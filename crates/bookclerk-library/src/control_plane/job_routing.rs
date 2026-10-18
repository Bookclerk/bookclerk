//! Routing durable jobs to nodes that can run them, under cluster semaphores.
//!
//! A job records at admission the plugin instances the claiming node must
//! run ([`crate::JobPayload::requires`]): an instance the job is pinned to,
//! and for acquire every node-bound destination instance (a `local` output
//! is a disk on the nodes it is deployed to). A node may claim the job only
//! when each of those instances has a deployment on it that should run
//! (`present` or `placed`) and whose last observation there says the guest
//! is up (`running` or `healthy`), the same test delivery routing uses. A job
//! with no requirements stays claimable anywhere.
//!
//! Before the claim's compare-and-swap, the claimer takes one permit of
//! every cluster semaphore the job counts against
//! ([`super::semaphore`]): its resource class (`jobs:network`), each
//! required instance (`plugin-instance:<id>`), and each store account it
//! touches (`account:<plugin_key>:<account_id>`, one at a time). Permits live
//! as long as the job's lease, are renewed with its heartbeat, and are
//! released when the job is finalized; a crashed holder's permits expire and
//! are taken over with a new fencing token, after which the stale holder can
//! neither renew them nor finalize the job (whose own lease generation has
//! moved on as well).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use uuid::Uuid;

use super::deployments::{
    list_deployments_for_instance, load_deployment_for_node, load_observation, DeploymentStatus,
};
use super::instances::{list_all_plugin_instances, load_plugin_instance, PluginInstanceId};
use super::semaphore::{
    acquire_permits, release_permits, PermitAcquire, SemaphorePermit, SemaphoreRequest,
};
use crate::entities::accounts;
use crate::error::{LibraryError, Result};
use crate::models::{JobClaimCursor, JobKind, JobPayload, JobRecord, JobResourceClass};
use crate::store::LibraryStore;

/// Permits an account semaphore grants: one job per store account at a time.
pub const ACCOUNT_SEMAPHORE_LIMIT: u32 = 1;

/// Pending candidates read per page of a routed claim attempt (the claim
/// pages on until it claims a job or the ready rows run out).
pub const ROUTED_CLAIM_CANDIDATES: u64 = 16;

/// Name of the cluster semaphore for one resource class.
#[must_use]
pub fn class_semaphore(class: JobResourceClass) -> String {
    format!("jobs:{}", class.as_str())
}

/// Name of the cluster semaphore for one plugin instance.
#[must_use]
pub fn instance_semaphore(instance_id: &str) -> String {
    format!("plugin-instance:{instance_id}")
}

/// Name of the cluster semaphore for one store account.
#[must_use]
pub fn account_semaphore(plugin_key: &str, account_id: &str) -> String {
    format!("account:{plugin_key}:{account_id}")
}

/// Cluster-wide permit counts a routed claim enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobClaimLimits {
    /// Running jobs of the claimed resource class, cluster-wide.
    pub class: u32,
    /// Running jobs that require one plugin instance, cluster-wide.
    pub plugin_instance: u32,
}

/// One routed claim attempt by one worker.
#[derive(Debug, Clone)]
pub struct JobClaimRequest<'a> {
    /// Resource class the worker serves.
    pub resource_class: JobResourceClass,
    /// The claiming node (`node-identity.json`). Empty when unknown, in
    /// which case only jobs without requirements are claimable.
    pub node_id: &'a str,
    /// Worker lease owner written on the job row.
    pub owner: &'a str,
    /// Lease length; permits are taken for the same time.
    pub lease_secs: u64,
    /// Semaphore limits.
    pub limits: JobClaimLimits,
}

/// A claimed job and the semaphore permits it holds.
#[derive(Debug, Clone)]
pub struct RoutedJobClaim {
    /// The running job (its fence is on the record).
    pub job: JobRecord,
    /// Permits to renew with the job heartbeat and release at the end.
    pub permits: Vec<SemaphorePermit>,
}

/// The instances a new job must find on its claiming node.
///
/// `pinned` is the instance the job was pinned to; it is kept only when it
/// is a valid instance id with at least one deployment that should run, so a
/// job pinned to an undeployed instance is claimed anywhere and fails there
/// as before rather than waiting forever. For [`JobKind::Acquire`], every
/// instance of a plugin key in `node_bound_plugin_keys` that is deployed to
/// run somewhere is added: the acquire writes to that output, so it must run
/// where the output's disk is.
///
/// The required instances must share at least one node where all of them
/// are deployed to run; otherwise no node could ever claim the job, so it
/// is refused here instead of being left pending. Required instances are
/// never dropped to make a node fit: an acquire must write to every
/// deployed node-bound output.
///
/// # Errors
///
/// Returns [`LibraryError::Conflict`] when the required instances have no
/// common deployment node, or an error when a read fails.
pub async fn resolve_job_requirements(
    store: &LibraryStore,
    kind: JobKind,
    payload: &JobPayload,
    node_bound_plugin_keys: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let requires = desired_requirements(store, kind, payload, node_bound_plugin_keys).await?;
    if !requirements_routable(store, &requires).await? {
        return Err(LibraryError::Conflict(no_common_node_message(
            kind, &requires,
        )));
    }
    Ok(requires)
}

/// The requirement set [`resolve_job_requirements`] would record now,
/// without the common-node check.
pub(super) async fn desired_requirements(
    store: &LibraryStore,
    kind: JobKind,
    payload: &JobPayload,
    node_bound_plugin_keys: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut requires = BTreeSet::new();
    if let Some(pinned) = payload
        .plugin_instance_id
        .as_deref()
        .and_then(|id| PluginInstanceId::parse(id).ok())
    {
        if deployed_somewhere(store, &pinned).await? {
            requires.insert(pinned.as_str().to_string());
        }
    }
    if kind == JobKind::Acquire && !node_bound_plugin_keys.is_empty() {
        for instance in list_all_plugin_instances(store).await? {
            if node_bound_plugin_keys.contains(&instance.plugin_key)
                && deployed_somewhere(store, &instance.id).await?
            {
                requires.insert(instance.id.as_str().to_string());
            }
        }
    }
    Ok(requires.into_iter().collect())
}

/// True when some node is deployed to run every instance in `requires`
/// (always true for zero or one instance: [`desired_requirements`] keeps
/// only deployed instances).
pub(super) async fn requirements_routable(
    store: &LibraryStore,
    requires: &[String],
) -> Result<bool> {
    Ok(requires.len() <= 1 || !common_deployment_nodes(store, requires).await?.is_empty())
}

/// Operator-facing reason for requirements no node can meet.
pub(super) fn no_common_node_message(kind: JobKind, requires: &[String]) -> String {
    format!(
        "no node runs all of the plugin instances this {} job requires ({}); \
         deploy them to a common node",
        kind.as_str(),
        requires.join(", ")
    )
}

/// Nodes where every instance in `requires` has a deployment that should run.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn common_deployment_nodes(
    store: &LibraryStore,
    requires: &[String],
) -> Result<BTreeSet<String>> {
    let mut common: Option<BTreeSet<String>> = None;
    for raw in requires {
        let Ok(instance) = PluginInstanceId::parse(raw) else {
            return Ok(BTreeSet::new());
        };
        let nodes: BTreeSet<String> = list_deployments_for_instance(store, &instance)
            .await?
            .into_iter()
            .filter(|deployment| deployment.desired.runs())
            .map(|deployment| deployment.node_id)
            .collect();
        let next = match common {
            Some(prev) => prev.intersection(&nodes).cloned().collect(),
            None => nodes,
        };
        if next.is_empty() {
            return Ok(next);
        }
        common = Some(next);
    }
    Ok(common.unwrap_or_default())
}

/// True when some node has a deployment of `instance` that should run.
async fn deployed_somewhere(store: &LibraryStore, instance: &PluginInstanceId) -> Result<bool> {
    Ok(list_deployments_for_instance(store, instance)
        .await?
        .iter()
        .any(|deployment| deployment.desired.runs()))
}

/// True when `node_id` can run a job that requires `requires`.
///
/// Each instance needs a deployment on the node that should run and an
/// observation from the node with the guest up. An empty list is always
/// satisfied; an unknown node or an unparsable id never is.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn node_can_run_job(
    store: &LibraryStore,
    node_id: &str,
    requires: &[String],
) -> Result<bool> {
    if requires.is_empty() {
        return Ok(true);
    }
    if node_id.trim().is_empty() {
        return Ok(false);
    }
    for raw in requires {
        let Ok(instance) = PluginInstanceId::parse(raw) else {
            return Ok(false);
        };
        let Some(deployment) = load_deployment_for_node(store, &instance, node_id).await? else {
            return Ok(false);
        };
        if !deployment.desired.runs() {
            return Ok(false);
        }
        let up = load_observation(store, &deployment.deployment_id, node_id)
            .await?
            .is_some_and(|o| {
                matches!(
                    o.status,
                    DeploymentStatus::Running | DeploymentStatus::Healthy
                )
            });
        if !up {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The cluster semaphores `job` counts against under `limits`.
///
/// Every job takes its class semaphore and one per required instance. Scan
/// and acquire also take one per store account they touch: the payload's
/// account when it names one (limited to the pinned instance's plugin when
/// pinned), otherwise every account of the pinned plugin, or every account.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn job_semaphores(
    store: &LibraryStore,
    job: &JobRecord,
    limits: JobClaimLimits,
) -> Result<Vec<SemaphoreRequest>> {
    let mut requests = vec![SemaphoreRequest::new(
        class_semaphore(job.resource_class),
        limits.class,
    )];
    for instance in &job.payload.requires {
        requests.push(SemaphoreRequest::new(
            instance_semaphore(instance),
            limits.plugin_instance,
        ));
    }
    if matches!(job.kind, JobKind::Scan | JobKind::Acquire) {
        let pinned_key = match job
            .payload
            .plugin_instance_id
            .as_deref()
            .and_then(|id| PluginInstanceId::parse(id).ok())
        {
            Some(id) => load_plugin_instance(store, &id)
                .await?
                .map(|instance| instance.plugin_key),
            None => None,
        };
        let mut query = accounts::Entity::find();
        if let Some(account) = job.payload.account.as_deref() {
            query = query.filter(accounts::Column::AccountId.eq(account));
        }
        if let Some(key) = pinned_key.as_deref() {
            query = query.filter(accounts::Column::PluginKey.eq(key));
        }
        for row in query.all(store.db()).await.map_err(LibraryError::Orm)? {
            requests.push(SemaphoreRequest::new(
                account_semaphore(&row.plugin_key, &row.account_id),
                ACCOUNT_SEMAPHORE_LIMIT,
            ));
        }
    }
    Ok(requests)
}

/// Claims the best ready job this node can run, under the cluster semaphores.
///
/// Pages through the ready pending jobs of the class, best first
/// ([`ROUTED_CLAIM_CANDIDATES`] per page); skips those whose requirements
/// this node does not meet or whose account or instance semaphore is full;
/// takes the permits of the first one left and claims it with
/// [`LibraryStore::claim_job_by_id`]. A lost claim gives the permits back
/// and moves on. It stops at the first claim, when the rows run out, or when
/// the class semaphore itself is full (no further candidate could run).
///
/// # Returns
///
/// The claimed job and its permits, or `None` when nothing could be claimed.
///
/// # Errors
///
/// Returns an error when a read or write fails; permits taken for the
/// failing attempt are released on a best-effort basis.
pub async fn claim_routed_job(
    store: &LibraryStore,
    request: &JobClaimRequest<'_>,
) -> Result<Option<RoutedJobClaim>> {
    let class = class_semaphore(request.resource_class);
    let ttl = Duration::from_secs(request.lease_secs);
    // Many queued jobs share one requirement set (every acquire bound to the
    // same outputs), so each set is checked against this node once per call.
    let mut runnable: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    let mut after: Option<JobClaimCursor> = None;
    loop {
        let page = store
            .list_job_claim_candidates(
                request.resource_class,
                after.as_ref(),
                ROUTED_CLAIM_CANDIDATES,
            )
            .await?;
        for job in page.jobs {
            let can_run = match runnable.get(&job.payload.requires) {
                Some(known) => *known,
                None => {
                    let known =
                        node_can_run_job(store, request.node_id, &job.payload.requires).await?;
                    runnable.insert(job.payload.requires.clone(), known);
                    known
                }
            };
            if !can_run {
                continue;
            }
            let semaphores = job_semaphores(store, &job, request.limits).await?;
            let holder = format!("{}/{}/{}", request.owner, job.id, Uuid::new_v4().simple());
            let permits = match acquire_permits(store, &semaphores, &holder, ttl).await? {
                PermitAcquire::Granted(permits) => permits,
                PermitAcquire::Blocked(name) if name == class => return Ok(None),
                PermitAcquire::Blocked(_) => continue,
            };
            match store
                .claim_job_by_id(&job.id, request.owner, request.lease_secs)
                .await
            {
                Ok(Some(job)) => return Ok(Some(RoutedJobClaim { job, permits })),
                Ok(None) => release_permits(store, &permits).await?,
                Err(err) => {
                    let _ = release_permits(store, &permits).await;
                    return Err(err);
                }
            }
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(None),
        }
    }
}
