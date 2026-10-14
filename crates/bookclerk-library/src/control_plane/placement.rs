//! Placement: how many hosts run a plugin instance, and which hosts may.
//!
//! An operator writes intent per instance in `plugin_placements`: a replica
//! count and label constraints. The scheduler, a singleton that holds the
//! [`SCHEDULER_LEASE`] cluster lease, turns that intent into
//! `plugin_deployments` rows (the same rows `plugins enable` writes by
//! hand), and each host's reconciler converges on its own rows as before.
//! An instance with no placement row is managed by hand; the scheduler never
//! touches it.
//!
//! A host is a candidate for an instance when:
//!
//! - its daemon is live: its `host:<host_id>` queue consumer heartbeated
//!   within the event catalog TTL;
//! - its labels satisfy every constraint (`name` or `name=value` must be
//!   present, `!name…` must be absent). Every host also carries the implicit
//!   labels `host=<host_id>` and `target=<target>`;
//! - for a released plugin, the active release has an artifact for the
//!   host's target.
//!
//! The planner keeps replicas where they already run, drops replicas on
//! hosts that stopped qualifying (a dead host, a removed label), and fills
//! the rest on the least-loaded candidates. Deployment writes are
//! compare-and-swap, so a scheduler that lost its lease mid-pass can at worst
//! place one replica too many, which the next pass removes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::deployments::{
    ensure_plugin_deployment, list_all_deployments, list_deployments_for_instance,
    replace_plugin_deployment, DeploymentReplace, DesiredState, PluginDeployment,
};
use super::documents::ConfigActor;
use super::host_queue::{notify_all_hosts, HostMessage, HOST_CONSUMER_PREFIX};
use super::identity::{list_hosts, load_host, HostRecord};
use super::instances::{load_plugin_instance, PluginInstanceId};
use super::releases::load_active_release;
use crate::entities::{cluster_leases, hosts, plugin_placements};
use crate::error::{LibraryError, Result};
use crate::models::EVENT_SUBSCRIBER_HEARTBEAT_TTL_SECS;
use crate::store::LibraryStore;

/// Name of the cluster lease the scheduler runs under.
pub const SCHEDULER_LEASE: &str = "scheduler";

/// How long a scheduler lease lasts without renewal.
pub const SCHEDULER_LEASE_TTL: Duration = Duration::from_secs(60);

/// Largest replica count an operator may ask for.
pub const MAX_REPLICAS: u32 = 64;

/// Most operator labels one host may carry.
pub const MAX_HOST_LABELS: usize = 32;

/// Most constraints one placement may carry.
pub const MAX_PLACEMENT_CONSTRAINTS: usize = 16;

/// Longest label or constraint (without its `!`), in bytes.
const MAX_LABEL_BYTES: usize = 63;

/// Implicit label name every host carries with its own id.
pub const HOST_LABEL: &str = "host";

/// Implicit label name every host carries with its artifact target.
pub const TARGET_LABEL: &str = "target";

/// What an operator asks for: how many hosts, and which may qualify.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlacementSpec {
    /// Hosts that should run the instance. Zero keeps the placement and
    /// stops every replica.
    pub replicas: u32,
    /// Label constraints every chosen host must satisfy.
    pub constraints: Vec<String>,
}

impl PlacementSpec {
    /// Validated copy with constraints trimmed, sorted, and deduplicated.
    ///
    /// # Errors
    ///
    /// Returns an error for a replica count over [`MAX_REPLICAS`], too many
    /// constraints, or a constraint outside the label grammar.
    pub fn normalized(&self) -> Result<Self> {
        if self.replicas > MAX_REPLICAS {
            return Err(invalid(&format!("replicas must be at most {MAX_REPLICAS}")));
        }
        let constraints = normalize(&self.constraints);
        if constraints.len() > MAX_PLACEMENT_CONSTRAINTS {
            return Err(invalid(&format!(
                "at most {MAX_PLACEMENT_CONSTRAINTS} placement constraints"
            )));
        }
        for constraint in &constraints {
            validate_constraint(constraint)?;
        }
        Ok(Self {
            replicas: self.replicas,
            constraints,
        })
    }
}

/// A stored placement: the operator's intent plus the last pass's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPlacement {
    /// Placed instance.
    pub plugin_instance_id: PluginInstanceId,
    /// Requested replicas and constraints.
    pub spec: PlacementSpec,
    /// Compare-and-swap revision of the intent.
    pub revision: i64,
    /// RFC 3339 time of the last intent write.
    pub updated_at: String,
    /// Audit identity of the last intent writer.
    pub updated_by: String,
    /// Replicas the last scheduler pass placed.
    pub placed: u32,
    /// Why the last pass placed fewer than requested; empty when satisfied.
    pub detail: String,
    /// RFC 3339 time the scheduler last changed the status; empty before
    /// the first pass.
    pub scheduled_at: String,
}

/// Outcome of [`put_plugin_placement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementWrite {
    /// The intent is stored at the returned revision.
    Applied(PluginPlacement),
    /// `expected_revision` was not current (0 when no placement exists).
    Conflict {
        /// Revision currently stored.
        current_revision: i64,
    },
}

/// Error for a value outside the placement grammar.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("invalid configuration: {detail}"))
}

/// Only an operator writes placement intent and host labels.
fn authorize(actor: &ConfigActor) -> Result<()> {
    match actor {
        ConfigActor::Operator { .. } => Ok(()),
        ConfigActor::Bootstrap
        | ConfigActor::Scheduler
        | ConfigActor::Administrator { .. }
        | ConfigActor::Member { .. } => Err(LibraryError::Other(anyhow::anyhow!(
            "unauthorized configuration write: operator authority is required"
        ))),
    }
}

/// Trimmed, non-empty, sorted, deduplicated.
fn normalize(values: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = values
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    set.into_iter().collect()
}

/// True when every byte is a lowercase letter, digit, or one of `._/-`.
fn label_part_ok(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'/' | b'-')
        })
}

/// Checks the label grammar: `name` or `name=value`, lowercase letters,
/// digits, and `._/-`, the name starting with a letter or digit, at most 63
/// bytes.
///
/// # Errors
///
/// Returns an error when `label` does not fit the grammar.
pub fn validate_label(label: &str) -> Result<()> {
    let (name, value) = match label.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (label, None),
    };
    let starts_ok = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if label.len() > MAX_LABEL_BYTES
        || !starts_ok
        || !label_part_ok(name)
        || value.is_some_and(|v| !label_part_ok(v))
    {
        return Err(invalid(&format!(
            "label `{label}` must be `name` or `name=value` using lowercase letters, digits, \
             and `._/-` (at most {MAX_LABEL_BYTES} bytes)"
        )));
    }
    Ok(())
}

/// Checks an operator label: the grammar of [`validate_label`], and not one
/// of the implicit names every host already carries.
///
/// # Errors
///
/// Returns an error for a malformed or reserved label.
pub fn validate_host_label(label: &str) -> Result<()> {
    validate_label(label)?;
    let name = label.split_once('=').map_or(label, |(name, _)| name);
    if name == HOST_LABEL || name == TARGET_LABEL {
        return Err(invalid(&format!(
            "label name `{name}` is implicit on every host and cannot be assigned"
        )));
    }
    Ok(())
}

/// Checks a constraint: a label, optionally prefixed with `!` to exclude it.
///
/// # Errors
///
/// Returns an error when the label part does not fit the grammar.
pub fn validate_constraint(constraint: &str) -> Result<()> {
    validate_label(constraint.strip_prefix('!').unwrap_or(constraint))
}

/// A host's operator labels plus `host=<id>` and `target=<target>`.
#[must_use]
pub fn effective_host_labels(host: &HostRecord) -> BTreeSet<String> {
    let mut labels: BTreeSet<String> = host.labels.iter().cloned().collect();
    labels.insert(format!(
        "{HOST_LABEL}={}",
        host.host_id.to_ascii_lowercase()
    ));
    if !host.target.is_empty() {
        labels.insert(format!("{TARGET_LABEL}={}", host.target));
    }
    labels
}

/// True when `labels` satisfy every constraint.
#[must_use]
pub fn labels_satisfy(labels: &BTreeSet<String>, constraints: &[String]) -> bool {
    constraints
        .iter()
        .all(|constraint| match constraint.strip_prefix('!') {
            Some(excluded) => !labels.contains(excluded),
            None => labels.contains(constraint.as_str()),
        })
}

/// Maps a stored row.
fn placement_from_model(row: plugin_placements::Model) -> Result<PluginPlacement> {
    let constraints: Vec<String> = serde_json::from_str(&row.constraints_json)
        .map_err(|err| invalid(&format!("stored placement constraints: {err}")))?;
    Ok(PluginPlacement {
        plugin_instance_id: PluginInstanceId::parse(&row.plugin_instance_id)?,
        spec: PlacementSpec {
            replicas: u32::try_from(row.replicas).unwrap_or(0),
            constraints,
        },
        revision: row.revision,
        updated_at: row.updated_at,
        updated_by: row.updated_by,
        placed: u32::try_from(row.placed).unwrap_or(0),
        detail: row.detail,
        scheduled_at: row.scheduled_at,
    })
}

/// The placement of `instance_id`, if it has one.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_plugin_placement(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<Option<PluginPlacement>> {
    plugin_placements::Entity::find_by_id(instance_id.as_str().to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(placement_from_model)
        .transpose()
}

/// Every placement, by instance id.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_plugin_placements(store: &LibraryStore) -> Result<Vec<PluginPlacement>> {
    plugin_placements::Entity::find()
        .order_by_asc(plugin_placements::Column::PluginInstanceId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(placement_from_model)
        .collect()
}

/// Writes placement intent for an instance.
///
/// `expected_revision` is `None` (or `Some(0)`) to create a placement and the
/// current revision to replace one. Every host is woken so the scheduler,
/// wherever it runs, applies the change.
///
/// # Errors
///
/// Returns an error when the actor is not an operator, the spec is invalid,
/// the instance is missing, or the write fails.
pub async fn put_plugin_placement(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
    spec: &PlacementSpec,
    expected_revision: Option<i64>,
) -> Result<PlacementWrite> {
    authorize(actor)?;
    let spec = spec.normalized()?;
    if load_plugin_instance(store, instance_id).await?.is_none() {
        return Err(LibraryError::NotFound(format!(
            "plugin instance {instance_id}"
        )));
    }
    let constraints_json = serde_json::to_string(&spec.constraints)
        .map_err(|err| invalid(&format!("placement constraints: {err}")))?;
    let now = Utc::now().to_rfc3339();
    let current = load_plugin_placement(store, instance_id).await?;
    let expected = expected_revision.unwrap_or(0);
    let current_revision = current.as_ref().map_or(0, |p| p.revision);
    if expected != current_revision {
        return Ok(PlacementWrite::Conflict { current_revision });
    }
    let replicas = i64::from(spec.replicas);
    if current.is_none() {
        let model = plugin_placements::ActiveModel {
            plugin_instance_id: Set(instance_id.as_str().to_string()),
            replicas: Set(replicas),
            constraints_json: Set(constraints_json),
            revision: Set(1),
            updated_at: Set(now),
            updated_by: Set(actor.audit_id().to_string()),
            placed: Set(0),
            detail: Set(String::new()),
            scheduled_at: Set(String::new()),
        };
        if let Err(err) = plugin_placements::Entity::insert(model)
            .exec(store.db())
            .await
        {
            return match load_plugin_placement(store, instance_id).await? {
                Some(raced) => Ok(PlacementWrite::Conflict {
                    current_revision: raced.revision,
                }),
                None => Err(LibraryError::Orm(err)),
            };
        }
    } else {
        let res = plugin_placements::Entity::update_many()
            .col_expr(plugin_placements::Column::Replicas, Expr::value(replicas))
            .col_expr(
                plugin_placements::Column::ConstraintsJson,
                Expr::value(constraints_json),
            )
            .col_expr(
                plugin_placements::Column::Revision,
                Expr::value(expected + 1),
            )
            .col_expr(plugin_placements::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                plugin_placements::Column::UpdatedBy,
                Expr::value(actor.audit_id().to_string()),
            )
            .filter(plugin_placements::Column::PluginInstanceId.eq(instance_id.as_str()))
            .filter(plugin_placements::Column::Revision.eq(expected))
            .exec(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected != 1 {
            let current_revision = load_plugin_placement(store, instance_id)
                .await?
                .map_or(0, |p| p.revision);
            return Ok(PlacementWrite::Conflict { current_revision });
        }
    }
    let stored = load_plugin_placement(store, instance_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("placement {instance_id}")))?;
    notify_all_hosts(
        store,
        HostMessage::PlacementChanged {
            subject: format!("instance:{instance_id}"),
            revision: stored.revision,
        },
    )
    .await;
    Ok(PlacementWrite::Applied(stored))
}

/// Deletes an instance's placement and hands its deployments back to the
/// operator: each `placed` row becomes `present` and each `unplaced` row
/// `absent`, so every host keeps doing what it does now. Returns false when
/// there was no placement.
///
/// # Errors
///
/// Returns an error when the actor is not an operator or a write fails.
pub async fn delete_plugin_placement(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
) -> Result<bool> {
    authorize(actor)?;
    let res = plugin_placements::Entity::delete_by_id(instance_id.as_str().to_string())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    // Hand back after the placement row is gone, so a scheduler pass that
    // starts now finds no placement and leaves these rows alone.
    for row in list_deployments_for_instance(store, instance_id).await? {
        if !row.desired.is_scheduled() {
            continue;
        }
        let operation_id = format!("hand-back-{}-{}", row.deployment_id, row.revision);
        replace_plugin_deployment(
            store,
            actor,
            &row.deployment_id,
            row.revision,
            row.desired.handed_back(),
            &operation_id,
        )
        .await?;
    }
    if res.rows_affected > 0 {
        notify_all_hosts(
            store,
            HostMessage::PlacementChanged {
                subject: format!("instance:{instance_id}"),
                revision: 0,
            },
        )
        .await;
    }
    Ok(res.rows_affected > 0)
}

/// Fails when the scheduler owns `instance_id`'s deployments, so a
/// hand-written enable or disable is not silently undone on its next pass.
///
/// # Errors
///
/// Returns a conflict naming the placement when one exists.
pub async fn ensure_hand_managed(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<()> {
    if let Some(placement) = load_plugin_placement(store, instance_id).await? {
        return Err(LibraryError::Conflict(format!(
            "plugin instance {instance_id} is placed by the scheduler ({} replica(s){}); \
             change it with `bookclerk plugins deploy`, or drop the placement with \
             `bookclerk plugins deploy --manual` first",
            placement.spec.replicas,
            if placement.spec.constraints.is_empty() {
                String::new()
            } else {
                format!(" on {}", placement.spec.constraints.join(", "))
            }
        )));
    }
    Ok(())
}

/// Replaces a host's operator labels and wakes the scheduler.
///
/// # Errors
///
/// Returns an error when the actor is not an operator, a label is invalid or
/// reserved, there are too many labels, or the host is unknown.
pub async fn set_host_labels(
    store: &LibraryStore,
    actor: &ConfigActor,
    host_id: &str,
    labels: &[String],
) -> Result<HostRecord> {
    authorize(actor)?;
    let labels = normalize(labels);
    if labels.len() > MAX_HOST_LABELS {
        return Err(invalid(&format!("at most {MAX_HOST_LABELS} host labels")));
    }
    for label in &labels {
        validate_host_label(label)?;
    }
    let json =
        serde_json::to_string(&labels).map_err(|err| invalid(&format!("host labels: {err}")))?;
    let res = hosts::Entity::update_many()
        .col_expr(hosts::Column::LabelsJson, Expr::value(json))
        .filter(hosts::Column::HostId.eq(host_id))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if res.rows_affected == 0 {
        return Err(LibraryError::NotFound(format!("host {host_id}")));
    }
    notify_all_hosts(
        store,
        HostMessage::PlacementChanged {
            subject: format!("host:{host_id}"),
            revision: 0,
        },
    )
    .await;
    load_host(store, host_id).await
}

/// Hosts whose daemon is live: their `host:<host_id>` queue consumer row
/// heartbeated within the event catalog TTL.
///
/// # Errors
///
/// Returns an error when the catalog read fails.
pub async fn live_host_ids(store: &LibraryStore) -> Result<BTreeSet<String>> {
    Ok(store
        .list_live_event_subscriber_nodes(EVENT_SUBSCRIBER_HEARTBEAT_TTL_SECS)
        .await?
        .into_iter()
        .filter(|row| row.enabled)
        .filter_map(|row| {
            row.plugin_id
                .strip_prefix(HOST_CONSUMER_PREFIX)
                .map(str::to_string)
        })
        .collect())
}

/// A held cluster lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterLease {
    /// Lease name.
    pub name: String,
    /// Holder (a host id).
    pub holder: String,
    /// Fencing token; a new holder always gets a larger one.
    pub token: i64,
    /// Expiry in Unix milliseconds.
    pub expires_at_ms: i64,
}

/// Acquires or renews `name` for `holder` for `ttl`.
///
/// Returns `None` when another holder's lease has not expired, or when a
/// concurrent acquire won. Renewing keeps the token; taking over an expired
/// lease bumps it.
///
/// # Errors
///
/// Returns an error when the read or write fails.
pub async fn acquire_lease(
    store: &LibraryStore,
    name: &str,
    holder: &str,
    ttl: Duration,
) -> Result<Option<ClusterLease>> {
    let now_ms = Utc::now().timestamp_millis();
    let expires_at_ms = now_ms.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX));
    let renewed_at = Utc::now().to_rfc3339();
    let lease = |token| ClusterLease {
        name: name.to_string(),
        holder: holder.to_string(),
        token,
        expires_at_ms,
    };
    let Some(row) = cluster_leases::Entity::find_by_id(name.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        let model = cluster_leases::ActiveModel {
            name: Set(name.to_string()),
            holder: Set(holder.to_string()),
            token: Set(1),
            expires_at_ms: Set(expires_at_ms),
            renewed_at: Set(renewed_at),
        };
        return match cluster_leases::Entity::insert(model).exec(store.db()).await {
            Ok(_) => Ok(Some(lease(1))),
            // A concurrent first acquire inserted the row; this one lost.
            Err(err) => match load_lease(store, name).await? {
                Some(_) => Ok(None),
                None => Err(LibraryError::Orm(err)),
            },
        };
    };
    let token = if row.holder == holder {
        row.token
    } else if row.expires_at_ms <= now_ms {
        row.token + 1
    } else {
        return Ok(None);
    };
    let res = cluster_leases::Entity::update_many()
        .col_expr(
            cluster_leases::Column::Holder,
            Expr::value(holder.to_string()),
        )
        .col_expr(cluster_leases::Column::Token, Expr::value(token))
        .col_expr(
            cluster_leases::Column::ExpiresAtMs,
            Expr::value(expires_at_ms),
        )
        .col_expr(cluster_leases::Column::RenewedAt, Expr::value(renewed_at))
        .filter(cluster_leases::Column::Name.eq(name))
        .filter(cluster_leases::Column::Holder.eq(row.holder))
        .filter(cluster_leases::Column::Token.eq(row.token))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok((res.rows_affected == 1).then(|| lease(token)))
}

/// Gives up a held lease so another holder may take it at once.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn release_lease(store: &LibraryStore, lease: &ClusterLease) -> Result<()> {
    cluster_leases::Entity::update_many()
        .col_expr(cluster_leases::Column::ExpiresAtMs, Expr::value(0_i64))
        .filter(cluster_leases::Column::Name.eq(lease.name.as_str()))
        .filter(cluster_leases::Column::Holder.eq(lease.holder.as_str()))
        .filter(cluster_leases::Column::Token.eq(lease.token))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// The current row of lease `name`, held or expired.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_lease(store: &LibraryStore, name: &str) -> Result<Option<ClusterLease>> {
    Ok(cluster_leases::Entity::find_by_id(name.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(|row| ClusterLease {
            name: row.name,
            holder: row.holder,
            token: row.token,
            expires_at_ms: row.expires_at_ms,
        }))
}

/// One host as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCandidate {
    /// Host id.
    pub host_id: String,
    /// Effective labels ([`effective_host_labels`]).
    pub labels: BTreeSet<String>,
    /// Artifact target; empty when unknown.
    pub target: String,
    /// True when the host's daemon is live.
    pub live: bool,
    /// Present deployments the host already runs (all instances).
    pub load: usize,
}

/// What the planner decided for one instance.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlacementPlan {
    /// Hosts that should gain a present deployment.
    pub add: Vec<String>,
    /// Hosts whose present deployment should become absent.
    pub remove: Vec<String>,
    /// Replicas placed after the plan applies.
    pub placed: u32,
    /// Why fewer than requested are placed; empty when satisfied.
    pub detail: String,
}

/// Plans one instance. `release_targets` is the active release's artifact
/// targets, or `None` for a plugin with no release (a host-local tree).
/// `present` is the hosts that already run the instance.
#[must_use]
pub fn plan_placement(
    spec: &PlacementSpec,
    release_targets: Option<&BTreeSet<String>>,
    hosts: &[HostCandidate],
    present: &BTreeSet<String>,
) -> PlacementPlan {
    // Same rule as `PluginRelease::artifact_for`: an exact target, or a
    // target-independent (`any`) artifact. A host that has not reported its
    // target is not excluded; its reconciler reports a missing artifact.
    let target_ok = |host: &HostCandidate| match release_targets {
        Some(targets) => {
            host.target.is_empty() || targets.contains(&host.target) || targets.contains("any")
        }
        None => true,
    };
    let eligible = |host: &HostCandidate| {
        host.live && labels_satisfy(&host.labels, &spec.constraints) && target_ok(host)
    };
    let by_id: BTreeMap<&str, &HostCandidate> =
        hosts.iter().map(|h| (h.host_id.as_str(), h)).collect();
    let want = usize::try_from(spec.replicas).unwrap_or(usize::MAX);
    let mut plan = PlacementPlan::default();

    // Keep replicas on hosts that still qualify, least loaded first; drop
    // the rest (a dead host, a host that lost a label, or a scale-down).
    let mut keep: Vec<&HostCandidate> = Vec::new();
    for host_id in present {
        match by_id.get(host_id.as_str()) {
            Some(host) if eligible(host) => keep.push(host),
            _ => plan.remove.push(host_id.clone()),
        }
    }
    keep.sort_by(|a, b| a.load.cmp(&b.load).then_with(|| a.host_id.cmp(&b.host_id)));
    if keep.len() > want {
        for host in keep.split_off(want) {
            plan.remove.push(host.host_id.clone());
        }
    }

    // Fill on the least-loaded eligible hosts that do not run it yet.
    let mut candidates: Vec<&HostCandidate> = hosts
        .iter()
        .filter(|h| !present.contains(&h.host_id) && eligible(h))
        .collect();
    candidates.sort_by(|a, b| a.load.cmp(&b.load).then_with(|| a.host_id.cmp(&b.host_id)));
    let need = want.saturating_sub(keep.len());
    plan.add = candidates
        .iter()
        .take(need)
        .map(|h| h.host_id.clone())
        .collect();
    plan.remove.sort();
    let placed = keep.len() + plan.add.len();
    plan.placed = u32::try_from(placed).unwrap_or(u32::MAX);
    if placed < want {
        let eligible_count = hosts.iter().filter(|h| eligible(h)).count();
        plan.detail = format!(
            "{placed} of {} replica(s) placed: {eligible_count} live host(s) satisfy {}",
            spec.replicas,
            if spec.constraints.is_empty() {
                "the release's targets".to_string()
            } else {
                spec.constraints.join(", ")
            }
        );
    }
    plan
}

/// What one scheduler pass changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScheduleReport {
    /// Placements considered.
    pub placements: usize,
    /// Deployments set present.
    pub added: usize,
    /// Deployments set absent.
    pub removed: usize,
    /// Deployment writes that lost a compare-and-swap (retried next pass).
    pub conflicts: usize,
}

/// Sets `host_id`'s deployment of `instance_id` to `placed` (`true`) or
/// `unplaced`, creating the row when missing. Returns false on a
/// compare-and-swap loss.
async fn write_deployment(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    host_id: &str,
    existing: Option<&PluginDeployment>,
    placed: bool,
) -> Result<bool> {
    let actor = ConfigActor::Scheduler;
    let desired = DesiredState::scheduled(placed);
    let row = match existing {
        Some(row) => row.clone(),
        // `ensure` returns a row a concurrent writer created first; that row
        // may carry another state, so fall through to replace it.
        None => ensure_plugin_deployment(store, &actor, instance_id, host_id, desired).await?,
    };
    if row.desired == desired {
        return Ok(true);
    }
    let operation_id = format!("scheduler-{}-{}-{desired}", row.deployment_id, row.revision);
    Ok(!matches!(
        replace_plugin_deployment(
            store,
            &actor,
            &row.deployment_id,
            row.revision,
            desired,
            &operation_id,
        )
        .await?,
        DeploymentReplace::Conflict { .. }
    ))
}

/// One scheduler pass: plans every placement against the live hosts and
/// writes the deployments the plans call for. Call it only while holding
/// the [`SCHEDULER_LEASE`].
///
/// # Errors
///
/// Returns an error when a read fails or a deployment write is refused.
pub async fn schedule_placements(store: &LibraryStore) -> Result<ScheduleReport> {
    let placements = list_plugin_placements(store).await?;
    let mut report = ScheduleReport {
        placements: placements.len(),
        ..ScheduleReport::default()
    };
    if placements.is_empty() {
        return Ok(report);
    }
    let hosts = list_hosts(store).await?;
    let live = live_host_ids(store).await?;
    let deployments = list_all_deployments(store).await?;
    let mut load: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_instance: BTreeMap<String, Vec<PluginDeployment>> = BTreeMap::new();
    for deployment in deployments {
        if deployment.desired.runs() {
            *load.entry(deployment.host_id.clone()).or_default() += 1;
        }
        by_instance
            .entry(deployment.plugin_instance_id.as_str().to_string())
            .or_default()
            .push(deployment);
    }
    for placement in placements {
        let instance_id = &placement.plugin_instance_id;
        let Some(instance) = load_plugin_instance(store, instance_id).await? else {
            continue;
        };
        let release_targets =
            load_active_release(store, &instance.plugin_key)
                .await?
                .map(|release| {
                    release
                        .artifacts
                        .iter()
                        .map(|a| a.target.clone())
                        .collect::<BTreeSet<_>>()
                });
        let candidates: Vec<HostCandidate> = hosts
            .iter()
            .map(|host| HostCandidate {
                host_id: host.host_id.clone(),
                labels: effective_host_labels(host),
                target: host.target.clone(),
                live: live.contains(&host.host_id),
                load: load.get(&host.host_id).copied().unwrap_or(0),
            })
            .collect();
        let rows = by_instance.remove(instance_id.as_str()).unwrap_or_default();
        let present: BTreeSet<String> = rows
            .iter()
            .filter(|d| d.desired.runs())
            .map(|d| d.host_id.clone())
            .collect();
        let plan = plan_placement(
            &placement.spec,
            release_targets.as_ref(),
            &candidates,
            &present,
        );
        for host_id in &plan.remove {
            let row = rows.iter().find(|d| &d.host_id == host_id);
            if write_deployment(store, instance_id, host_id, row, false).await? {
                report.removed += 1;
                if let Some(n) = load.get_mut(host_id) {
                    *n = n.saturating_sub(1);
                }
            } else {
                report.conflicts += 1;
            }
        }
        for host_id in &plan.add {
            let row = rows.iter().find(|d| &d.host_id == host_id);
            if write_deployment(store, instance_id, host_id, row, true).await? {
                report.added += 1;
                *load.entry(host_id.clone()).or_default() += 1;
            } else {
                report.conflicts += 1;
            }
        }
        if plan.placed != placement.placed || plan.detail != placement.detail {
            plugin_placements::Entity::update_many()
                .col_expr(
                    plugin_placements::Column::Placed,
                    Expr::value(i64::from(plan.placed)),
                )
                .col_expr(plugin_placements::Column::Detail, Expr::value(plan.detail))
                .col_expr(
                    plugin_placements::Column::ScheduledAt,
                    Expr::value(Utc::now().to_rfc3339()),
                )
                .filter(plugin_placements::Column::PluginInstanceId.eq(instance_id.as_str()))
                .exec(store.db())
                .await
                .map_err(LibraryError::Orm)?;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, labels: &[&str], target: &str, live: bool, load: usize) -> HostCandidate {
        let mut set: BTreeSet<String> = labels.iter().map(|l| (*l).to_string()).collect();
        set.insert(format!("host={id}"));
        if !target.is_empty() {
            set.insert(format!("target={target}"));
        }
        HostCandidate {
            host_id: id.into(),
            labels: set,
            target: target.into(),
            live,
            load,
        }
    }

    fn spec(replicas: u32, constraints: &[&str]) -> PlacementSpec {
        PlacementSpec {
            replicas,
            constraints: constraints.iter().map(|c| (*c).to_string()).collect(),
        }
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|i| (*i).to_string()).collect()
    }

    #[test]
    fn fills_least_loaded_eligible_hosts() {
        let hosts = [
            host("a", &["nas"], "linux-x64-gnu", true, 3),
            host("b", &["nas"], "linux-x64-gnu", true, 0),
            host("c", &[], "linux-x64-gnu", true, 0),
            host("d", &["nas"], "linux-x64-gnu", false, 0),
        ];
        let plan = plan_placement(&spec(2, &["nas"]), None, &hosts, &BTreeSet::new());
        assert_eq!(plan.add, vec!["b", "a"]);
        assert!(plan.remove.is_empty());
        assert_eq!(plan.placed, 2);
        assert!(plan.detail.is_empty());
    }

    #[test]
    fn keeps_running_replicas_and_moves_off_dead_or_unlabelled_hosts() {
        let hosts = [
            host("a", &["nas"], "", true, 1),
            host("b", &["nas"], "", false, 1),
            host("c", &[], "", true, 1),
            host("d", &["nas"], "", true, 0),
        ];
        let plan = plan_placement(&spec(2, &["nas"]), None, &hosts, &set(&["a", "b", "c"]));
        assert_eq!(plan.remove, vec!["b", "c"]);
        assert_eq!(plan.add, vec!["d"]);
        assert_eq!(plan.placed, 2);
    }

    #[test]
    fn scales_down_from_the_most_loaded_and_to_zero() {
        let hosts = [
            host("a", &[], "", true, 5),
            host("b", &[], "", true, 1),
            host("c", &[], "", true, 2),
        ];
        let plan = plan_placement(&spec(1, &[]), None, &hosts, &set(&["a", "b", "c"]));
        assert_eq!(plan.remove, vec!["a", "c"]);
        assert!(plan.add.is_empty());
        let zero = plan_placement(&spec(0, &[]), None, &hosts, &set(&["a", "b"]));
        assert_eq!(zero.remove, vec!["a", "b"]);
        assert_eq!(zero.placed, 0);
        assert!(zero.detail.is_empty());
    }

    #[test]
    fn release_targets_and_exclusions_narrow_the_candidates() {
        let hosts = [
            host("mac", &[], "macos-arm64", true, 0),
            host("pi", &["slow"], "linux-arm64-gnu", true, 0),
            host("x86", &[], "linux-x64-gnu", true, 0),
            host("old", &[], "", true, 0),
        ];
        let targets = set(&["linux-x64-gnu", "linux-arm64-gnu"]);
        let plan = plan_placement(
            &spec(4, &["!slow"]),
            Some(&targets),
            &hosts,
            &BTreeSet::new(),
        );
        // An unknown target is not excluded; the host reports its own error.
        assert_eq!(plan.add, vec!["old", "x86"]);
        assert_eq!(plan.placed, 2);
        assert!(
            plan.detail.starts_with("2 of 4 replica(s) placed"),
            "{}",
            plan.detail
        );
        let pinned = plan_placement(&spec(1, &["host=pi"]), None, &hosts, &BTreeSet::new());
        assert_eq!(pinned.add, vec!["pi"]);
        // A workerd package ships one `any` artifact that runs everywhere.
        let any = plan_placement(
            &spec(4, &[]),
            Some(&set(&["any"])),
            &hosts,
            &BTreeSet::new(),
        );
        assert_eq!(any.placed, 4);
    }

    #[test]
    fn label_grammar() {
        for ok in ["nas", "disk=ssd", "zone=eu-west/1", "a.b_c-d"] {
            validate_label(ok).unwrap();
            validate_host_label(ok).unwrap();
            validate_constraint(&format!("!{ok}")).unwrap();
        }
        for bad in [
            "",
            "NAS",
            "-x",
            "a=",
            "=b",
            "a b",
            "a=b=c=D",
            &"x".repeat(64),
        ] {
            assert!(validate_label(bad).is_err(), "{bad}");
        }
        assert!(validate_host_label("host=abc").is_err());
        assert!(validate_host_label("target").is_err());
        validate_constraint("host=abc").unwrap();
        assert!(spec(MAX_REPLICAS + 1, &[]).normalized().is_err());
        let normalized = spec(1, &[" b ", "a", "b", ""]).normalized().unwrap();
        assert_eq!(normalized.constraints, vec!["a", "b"]);
    }
}
