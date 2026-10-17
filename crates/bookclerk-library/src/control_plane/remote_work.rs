//! Jobs run by a Host node over the node API (H-4 design, sections 3 and 5).
//!
//! A Host pulls work through the owner node holding its socket. Every call
//! here maps one work frame onto the claim and fence machinery the local
//! workers use, as a [`Claimant::Remote`]:
//!
//! - [`claim_remote_job`]: the routed claim (trust-filtered, under the
//!   cluster semaphores), then [`handoff_routed_job`] re-checks trust just
//!   before the job's secrets are unsealed for the node.
//! - [`renew_remote_job`]: the fenced job heartbeat plus the renewal of the
//!   job's semaphore permits. Trust and the node credential are re-read on
//!   every renewal.
//! - [`finish_remote_job`]: the fenced complete or fail, then the permits
//!   are given back.
//! - [`write_back_job_secret`]: a refreshed store token, accepted only for
//!   a secret the job was handed and only while the job's fence is live.
//!
//! The lease owner written on a Host's job is [`remote_lease_owner`], so a
//! fence the Host presents is the job id plus the lease generation, and no
//! other node can present it. Nothing here keeps per-connection state: a
//! Host that reconnects (to this or another owner node) presents its fences
//! again and each one is checked against the database, so a fence that
//! expired and was reclaimed while the Host was away is lost.

use std::time::Duration;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use zeroize::Zeroizing;

use super::job_routing::{
    claim_routed_job, job_semaphores, JobClaimLimits, JobClaimRequest, RoutedJobClaim,
};
use super::semaphore::{permits_held_by, release_permits, renew_permits, SemaphorePermit};
use super::trust::{handoff_routed_job, node_may_claim_job, node_trust, Claimant, NodeTrust};
use crate::entities::{accounts, encrypted_secrets};
use crate::error::{LibraryError, Result};
use crate::models::{JobFence, JobKind, JobRecord, JobResourceClass, JobState};
use crate::secrets::{
    build_sealed_record, get_secret, secret_account_type, unseal_secret, upsert_secret,
    EncryptedSecretRecord,
};
use crate::store::LibraryStore;

/// Prefix of the lease owner of a job a Host runs.
pub const REMOTE_LEASE_OWNER_PREFIX: &str = "host:";

/// Largest store token a Host may write back.
pub const MAX_WRITE_BACK_BYTES: usize = 64 * 1024;

/// Error kind recorded when a job's secrets cannot be unsealed for a Host.
pub const SECRETS_UNAVAILABLE: &str = "secrets_unavailable";

/// Lease owner of every job `node_id` runs over the node API.
#[must_use]
pub fn remote_lease_owner(node_id: &str) -> String {
    format!("{REMOTE_LEASE_OWNER_PREFIX}{node_id}")
}

/// The fence a Host holds for `job_id` at lease generation `generation`.
#[must_use]
pub fn remote_job_fence(node_id: &str, job_id: &str, generation: i64) -> JobFence {
    JobFence {
        job_id: job_id.to_string(),
        owner: remote_lease_owner(node_id),
        generation,
    }
}

/// Names one store-account secret a job is handed. Carries no secret
/// material.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobSecretRef {
    /// Secret kind (`source_auth`, `widevine`, ...).
    pub kind: String,
    /// Plugin key that owns the secret.
    pub provider: String,
    /// Store account id.
    pub account_id: String,
    /// Logical secret name.
    pub name: String,
}

/// One unsealed secret handed with a job. The value is zeroized on drop and
/// never shown by `Debug`.
#[derive(Clone)]
pub struct JobSecret {
    /// Which row the value belongs to.
    pub secret_ref: JobSecretRef,
    /// The unsealed bytes.
    pub value: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for JobSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobSecret")
            .field("secret_ref", &self.secret_ref)
            .field(
                "value",
                &format_args!("<redacted {} bytes>", self.value.len()),
            )
            .finish()
    }
}

/// What a Host's claim settles on: the routing and lease settings of the
/// owner node serving it.
#[derive(Debug, Clone, Copy)]
pub struct RemoteClaimSettings {
    /// Lease length; permits are taken for the same time.
    pub lease_secs: u64,
    /// Cluster semaphore limits.
    pub limits: JobClaimLimits,
}

/// A job claimed for a Host, with the secrets to seal for it.
#[derive(Debug)]
pub struct RemoteJobClaim {
    /// The running job (its fence is on the record).
    pub job: JobRecord,
    /// The account secrets the job needs, unsealed.
    pub secrets: Vec<JobSecret>,
}

/// Outcome of [`renew_remote_job`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteRenew {
    /// The lease and its permits were extended.
    Renewed {
        /// The operator asked to cancel the job.
        cancel: bool,
    },
    /// The fence no longer owns the job, a permit was taken over, or the
    /// node is no longer trusted with the job.
    Lost,
}

/// How a Host's job ended.
#[derive(Debug, Clone, Copy)]
pub enum RemoteOutcome<'a> {
    /// The job succeeded; the text is recorded as its progress.
    Complete(Option<&'a str>),
    /// The job failed.
    Fail {
        /// Stable error kind.
        kind: &'a str,
        /// Operator-facing message.
        message: &'a str,
    },
}

/// Outcome of [`finish_remote_job`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFinish {
    /// The job row was finalized and its permits given back.
    Done,
    /// The fence no longer owns the job.
    Lost,
    /// The request is not allowed for this job (the reason is stable).
    Refused(&'static str),
}

/// Outcome of [`write_back_job_secret`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBack {
    /// The secret was re-sealed and stored.
    Written,
    /// Refused; nothing was written (the reason is stable).
    Refused(&'static str),
}

/// Claims the best job `node_id` may run and unseals its secrets.
///
/// The claim is [`claim_routed_job`] as a [`Claimant::Remote`] with the
/// lease owner [`remote_lease_owner`]; [`handoff_routed_job`] then re-checks
/// trust, and only then are the job's account secrets
/// ([`job_secret_refs`]) unsealed. A job whose secrets cannot be unsealed
/// is failed with [`SECRETS_UNAVAILABLE`] (so it retries with backoff and
/// any node holding the database may take it) and `None` is returned.
///
/// # Errors
///
/// Returns an error when a read or write fails. Permits taken for a failed
/// attempt are released on a best-effort basis.
pub async fn claim_remote_job(
    store: &LibraryStore,
    node_id: &str,
    resource_class: JobResourceClass,
    settings: RemoteClaimSettings,
) -> Result<Option<RemoteJobClaim>> {
    let owner = remote_lease_owner(node_id);
    let request = JobClaimRequest {
        resource_class,
        node_id,
        claimant: Claimant::Remote,
        owner: &owner,
        lease_secs: settings.lease_secs,
        limits: settings.limits,
    };
    let Some(claim) = claim_routed_job(store, &request).await? else {
        return Ok(None);
    };
    let Some(claim) = handoff_routed_job(store, &request, claim).await? else {
        return Ok(None);
    };
    let RoutedJobClaim { job, permits, .. } = claim;
    let Some(fence) = job.fence() else {
        let _ = release_permits(store, &permits).await;
        return Ok(None);
    };
    match unseal_job_secrets(store, &job).await {
        Ok(secrets) => Ok(Some(RemoteJobClaim { job, secrets })),
        Err(err) => {
            tracing::warn!(
                job_id = %job.id,
                node_id,
                error = %err,
                "job secrets could not be unsealed for a host; failing the attempt"
            );
            let failed = store
                .fail_job(
                    &fence,
                    SECRETS_UNAVAILABLE,
                    "the job's secrets could not be unsealed for a host node",
                )
                .await;
            let _ = release_permits(store, &permits).await;
            failed?;
            Ok(None)
        }
    }
}

/// The store-account secrets `job` needs on a Host.
///
/// For a scan or acquire that names an account: every `integration` row of
/// `encrypted_secrets` for that account id under any plugin key that has an
/// `accounts` row with that id (the same rows the trust predicate checked).
/// Every other job is handed no secrets. Sorted, without duplicates.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn job_secret_refs(store: &LibraryStore, job: &JobRecord) -> Result<Vec<JobSecretRef>> {
    Ok(job_secret_records(store, job)
        .await?
        .into_iter()
        .filter_map(|record| record_ref(&record))
        .collect())
}

/// The `encrypted_secrets` rows behind [`job_secret_refs`].
async fn job_secret_records(
    store: &LibraryStore,
    job: &JobRecord,
) -> Result<Vec<EncryptedSecretRecord>> {
    if !matches!(job.kind, JobKind::Scan | JobKind::Acquire) {
        return Ok(Vec::new());
    }
    let Some(account) = job.payload.account.as_deref().filter(|a| !a.is_empty()) else {
        return Ok(Vec::new());
    };
    let plugin_keys: Vec<String> = accounts::Entity::find()
        .filter(accounts::Column::AccountId.eq(account))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|row| row.plugin_key)
        .collect();
    if plugin_keys.is_empty() {
        return Ok(Vec::new());
    }
    let rows = encrypted_secrets::Entity::find()
        .filter(encrypted_secrets::Column::AccountType.eq(secret_account_type::INTEGRATION))
        .filter(encrypted_secrets::Column::AccountId.eq(account))
        .filter(encrypted_secrets::Column::Provider.is_in(plugin_keys))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(provider) = row.provider.clone() else {
            continue;
        };
        if let Some(record) = get_secret(
            store.db(),
            &row.kind,
            Some(&provider),
            secret_account_type::INTEGRATION,
            Some(account),
            &row.name,
        )
        .await?
        {
            records.push(record);
        }
    }
    records.sort_by_key(record_ref);
    records.dedup_by(|a, b| record_ref(a) == record_ref(b));
    Ok(records)
}

/// The ref of an integration row that names its provider and account.
fn record_ref(record: &EncryptedSecretRecord) -> Option<JobSecretRef> {
    Some(JobSecretRef {
        kind: record.kind.clone(),
        provider: record.provider.clone()?,
        account_id: record.account_id.clone()?,
        name: record.name.clone(),
    })
}

/// Unseals every secret of [`job_secret_refs`].
async fn unseal_job_secrets(store: &LibraryStore, job: &JobRecord) -> Result<Vec<JobSecret>> {
    let mut secrets = Vec::new();
    for record in job_secret_records(store, job).await? {
        let Some(secret_ref) = record_ref(&record) else {
            continue;
        };
        let value = Zeroizing::new(unseal_secret(&record)?);
        secrets.push(JobSecret { secret_ref, value });
    }
    Ok(secrets)
}

/// The job `fence` owns right now, if it still owns one.
async fn fenced_job(store: &LibraryStore, fence: &JobFence) -> Result<Option<JobRecord>> {
    Ok(store.get_job(&fence.job_id).await?.filter(|job| {
        job.state == JobState::Running
            && job.lease_owner.as_deref() == Some(fence.owner.as_str())
            && job.lease_generation == fence.generation
    }))
}

/// The semaphore permits `node_id` holds for `job`, and whether every
/// semaphore the job counts against is among them.
async fn held_job_permits(
    store: &LibraryStore,
    node_id: &str,
    job: &JobRecord,
    limits: JobClaimLimits,
) -> Result<(Vec<SemaphorePermit>, bool)> {
    let prefix = format!("{}/{}/", remote_lease_owner(node_id), job.id);
    let mut held = Vec::new();
    let mut complete = true;
    let mut seen = std::collections::BTreeSet::new();
    for request in job_semaphores(store, job, limits).await? {
        if !seen.insert(request.name.clone()) {
            continue;
        }
        let found = permits_held_by(store, &request.name, request.limit, &prefix).await?;
        complete &= !found.is_empty();
        held.extend(found);
    }
    Ok((held, complete))
}

/// Extends the lease of the job `node_id` holds at `generation`, and its
/// semaphore permits, recording `progress` when given.
///
/// Trust and the node credential are re-read first: a node that is no
/// longer trusted with the job (revoked, owner demoted, account unlinked)
/// gets [`RemoteRenew::Lost`], and the lease then expires as for a crashed
/// worker. A fence that expired and was reclaimed is lost as well, as is a
/// job whose permit another holder took over.
///
/// # Errors
///
/// Returns an error when a read or write fails.
pub async fn renew_remote_job(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    settings: RemoteClaimSettings,
    progress: Option<&str>,
) -> Result<RemoteRenew> {
    let fence = remote_job_fence(node_id, job_id, generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(RemoteRenew::Lost);
    };
    if !node_may_claim_job(store, node_id, Claimant::Remote, &job).await? {
        return Ok(RemoteRenew::Lost);
    }
    if !store
        .heartbeat_job(&fence, settings.lease_secs, progress)
        .await?
    {
        return Ok(RemoteRenew::Lost);
    }
    let (permits, complete) = held_job_permits(store, node_id, &job, settings.limits).await?;
    let ttl = Duration::from_secs(settings.lease_secs);
    if !complete || !renew_permits(store, &permits, ttl).await? {
        return Ok(RemoteRenew::Lost);
    }
    Ok(RemoteRenew::Renewed {
        cancel: job.cancel_requested,
    })
}

/// Completes or fails the job `node_id` holds at `generation`, then gives
/// its semaphore permits back.
///
/// A failure is always accepted under a live fence, so the job retries
/// promptly. A completion also needs the node to still be trusted with the
/// job, and is refused for an acquire, which completes only when its
/// upload is committed.
///
/// # Errors
///
/// Returns an error when a read or write fails.
pub async fn finish_remote_job(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    limits: JobClaimLimits,
    outcome: RemoteOutcome<'_>,
) -> Result<RemoteFinish> {
    let fence = remote_job_fence(node_id, job_id, generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(RemoteFinish::Lost);
    };
    let finished = match outcome {
        RemoteOutcome::Complete(detail) => {
            if job.kind == JobKind::Acquire {
                return Ok(RemoteFinish::Refused("acquire_completes_on_upload"));
            }
            if !node_may_claim_job(store, node_id, Claimant::Remote, &job).await? {
                return Ok(RemoteFinish::Lost);
            }
            store.complete_job(&fence, detail).await?
        }
        RemoteOutcome::Fail { kind, message } => store.fail_job(&fence, kind, message).await?,
    };
    if !finished {
        return Ok(RemoteFinish::Lost);
    }
    let (permits, _) = held_job_permits(store, node_id, &job, limits).await?;
    release_permits(store, &permits).await?;
    Ok(RemoteFinish::Done)
}

/// Stores a refreshed store token a Host writes back.
///
/// Accepted only when all of these hold, checked immediately before the
/// write: the node still has a live credential and is still trusted with
/// the job; `generation` is the job's live lease generation for this node;
/// and `secret_ref` is one of the job's secrets ([`job_secret_refs`]), so it
/// was handed out with the job under that fence and still exists. The value
/// is re-sealed with this node's DEK under the same identity.
///
/// # Errors
///
/// Returns an error when a read, the seal, or the write fails.
pub async fn write_back_job_secret(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    secret_ref: &JobSecretRef,
    value: &[u8],
) -> Result<WriteBack> {
    if value.is_empty() || value.len() > MAX_WRITE_BACK_BYTES {
        return Ok(WriteBack::Refused("invalid_value"));
    }
    if node_trust(store, node_id, Claimant::Remote).await? == NodeTrust::Nothing {
        return Ok(WriteBack::Refused("node_untrusted"));
    }
    let fence = remote_job_fence(node_id, job_id, generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(WriteBack::Refused("fence_not_live"));
    };
    if !node_may_claim_job(store, node_id, Claimant::Remote, &job).await? {
        return Ok(WriteBack::Refused("node_untrusted"));
    }
    if !job_secret_refs(store, &job).await?.contains(secret_ref) {
        return Ok(WriteBack::Refused("secret_not_handed"));
    }
    let record = build_sealed_record(
        value,
        &secret_ref.kind,
        &secret_ref.provider,
        secret_account_type::INTEGRATION,
        &secret_ref.account_id,
        &secret_ref.name,
    )?;
    upsert_secret(store.db(), &record).await?;
    Ok(WriteBack::Written)
}
