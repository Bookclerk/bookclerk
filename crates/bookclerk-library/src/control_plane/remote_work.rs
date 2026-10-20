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
//!   job's semaphore permits and of the tokens of the job's open uploads.
//!   Trust and the node credential are re-read on every renewal.
//! - [`finish_remote_job`]: the fenced complete or fail, then the permits
//!   are given back.
//! - [`release_remote_job`]: the Host gives a job back unrun (its operator
//!   no longer consents to the job's plugin), or a claim is given back when
//!   its `work` answer cannot reach the Host; the job returns to `pending`
//!   without spending the attempt, and the permits and handoff rows are
//!   given back.
//! - [`write_back_job_secret`]: a refreshed store token, accepted only for
//!   a secret handed with the job under the presented fence, and only while
//!   that fence is live.
//!
//! The lease owner written on a Host's job is [`remote_lease_owner`], so a
//! fence the Host presents is the job id plus the lease generation, and no
//! other node can present it. Nothing here keeps per-connection state: a
//! Host that reconnects (to this or another owner node) presents its fences
//! again and each one is checked against the database, so a fence that
//! expired and was reclaimed while the Host was away is lost. The secrets
//! handed under each fence are recorded in `job_secret_handoffs` for the
//! same reason.

use std::time::Duration;

use bookclerk_plugin_abi::{DbType, DbValue};
use chrono::Utc;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter};
use zeroize::Zeroizing;

use super::batch::{self, int, text};
use super::job_routing::{
    claim_routed_job, job_permit_holder_prefix, job_plugin_keys, job_semaphores,
    job_source_plugin_key, pinned_plugin_key, JobClaimFilter, JobClaimLimits, JobClaimRequest,
    RoutedJobClaim,
};
use super::semaphore::{
    permits_with_holder_prefix, release_permits, renew_permits, SemaphorePermit,
};
use super::trust::{
    accounts_named, handoff_routed_job, node_may_claim_job, node_trust, Claimant, NodeTrust,
};
use crate::entities::{encrypted_secrets, job_secret_handoffs};
use crate::error::{LibraryError, Result};
use crate::models::{JobFence, JobKind, JobRecord, JobResourceClass, JobState};
use crate::secrets::{
    build_sealed_record, forget_cached_secret, get_secret, secret_account_type, unseal_secret,
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
    /// The plugin keys the job runs ([`job_plugin_keys`]); empty when the
    /// job alone does not say (only possible without a claim filter).
    pub plugin_keys: Vec<String>,
    /// The one plugin whose source guest runs the job
    /// ([`job_source_plugin_key`]); always set under a claim filter.
    pub source_plugin_key: Option<String>,
    /// The claim resumed a suspended job ([`RoutedJobClaim::resumed`]).
    /// Pass it back to [`release_remote_job`] if the claim is given back.
    pub resumed: bool,
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
/// ([`job_secret_refs`]) unsealed. The refs handed are recorded under the
/// claim's lease generation before the secrets are returned, and are the
/// only ones [`write_back_job_secret`] accepts under that fence. A job whose
/// secrets cannot be unsealed is failed with [`SECRETS_UNAVAILABLE`] (so it
/// retries with backoff and any node holding the database may take it) and
/// `None` is returned.
///
/// `filter` is the Host's own narrowing ([`JobClaimFilter`]): only jobs of
/// its kinds whose every plugin it consented to, and whose one source
/// plugin can be told, are claimed.
///
/// # Errors
///
/// Returns an error when a read or write fails. Permits taken for a failed
/// attempt are released, and a claim whose plugin keys could not be read
/// or whose handoff could not be recorded is given back unspent (permits
/// included), on a best-effort basis.
pub async fn claim_remote_job(
    store: &LibraryStore,
    node_id: &str,
    resource_class: JobResourceClass,
    settings: RemoteClaimSettings,
    filter: Option<&JobClaimFilter>,
) -> Result<Option<RemoteJobClaim>> {
    let owner = remote_lease_owner(node_id);
    let request = JobClaimRequest {
        resource_class,
        node_id,
        claimant: Claimant::Remote,
        owner: &owner,
        lease_secs: settings.lease_secs,
        limits: settings.limits,
        filter,
    };
    let Some(claim) = claim_routed_job(store, &request).await? else {
        return Ok(None);
    };
    let Some(claim) = handoff_routed_job(store, &request, claim).await? else {
        return Ok(None);
    };
    let RoutedJobClaim {
        job,
        permits,
        resumed,
        ..
    } = claim;
    let Some(fence) = job.fence() else {
        let _ = release_permits(store, &permits).await;
        return Ok(None);
    };
    let plugin_keys = match job_plugin_keys(store, &job).await {
        Ok(keys) => keys
            .map(|keys| keys.into_iter().collect())
            .unwrap_or_default(),
        Err(err) => {
            // Nothing went out; give the claim back unspent.
            give_back_claim(store, &fence, &permits, resumed).await;
            return Err(err);
        }
    };
    let source_plugin_key = match job_source_plugin_key(store, &job).await {
        Ok(key) => key,
        Err(err) => {
            // Nothing went out; give the claim back unspent.
            give_back_claim(store, &fence, &permits, resumed).await;
            return Err(err);
        }
    };
    let canonical = match canonical_account(store, &job, source_plugin_key.as_deref()).await {
        Ok(canonical) => canonical,
        Err(err) => {
            // Nothing went out; give the claim back unspent.
            give_back_claim(store, &fence, &permits, resumed).await;
            return Err(err);
        }
    };
    match unseal_job_secrets(store, &job).await {
        Ok(secrets) => {
            // Trust can change while the secrets are read: check it once
            // more right before they are recorded and handed out. Every
            // later frame under the fence checks it again.
            let still_trusted = node_may_claim_job(store, node_id, Claimant::Remote, &job).await;
            if !matches!(still_trusted, Ok(true)) {
                give_back_claim(store, &fence, &permits, resumed).await;
                return still_trusted.map(|_| None);
            }
            match record_handoff(store, node_id, &fence, &secrets).await {
                Ok(true) => Ok(Some(RemoteJobClaim {
                    job: with_account(job, canonical),
                    secrets,
                    plugin_keys,
                    source_plugin_key,
                    resumed,
                })),
                Ok(false) => {
                    // The lease ran out or was lost while the secrets were
                    // read, and nothing went out. A fence that still holds
                    // the row gives it back unspent; the release is fenced,
                    // so a job already claimed again is left alone.
                    give_back_claim(store, &fence, &permits, resumed).await;
                    Ok(None)
                }
                Err(err) => {
                    // Nothing went out; give the claim back unspent.
                    give_back_claim(store, &fence, &permits, resumed).await;
                    Err(err)
                }
            }
        }
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

/// Gives a claim back unspent after a failure before anything was handed
/// to the Host: the job returns to `pending` without spending an attempt
/// (a resumed claim is resumable again, as
/// [`LibraryStore::release_job_claim`] does for `resumed`), and its
/// semaphore permits are released.
///
/// Best effort: the caller is already returning an error, so a failure
/// here is only logged; the lease and the permits then expire on their
/// own, after which the job is redelivered.
async fn give_back_claim(
    store: &LibraryStore,
    fence: &JobFence,
    permits: &[SemaphorePermit],
    resumed: bool,
) {
    if let Err(err) = store.release_job_claim(fence, resumed).await {
        tracing::warn!(
            job_id = %fence.job_id,
            error = %err,
            "a remote claim could not be given back; its lease will expire"
        );
    }
    if let Err(err) = release_permits(store, permits).await {
        tracing::warn!(
            job_id = %fence.job_id,
            error = %err,
            "a remote claim's permits could not be released; they will expire"
        );
    }
}

/// The store-account secrets `job` needs on a Host.
///
/// For a scan or acquire that names an account: every `integration` row of
/// `encrypted_secrets` for each store account the name reaches, by id or
/// label and ignoring case, as the executor resolves it (the same accounts
/// the trust predicate checked), limited to the pinned instance's plugin
/// when the job is pinned to one. Every other job is handed no secrets.
/// Sorted, without duplicates.
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
    // A job pinned to an instance runs only on that instance's plugin, so
    // it is handed only that plugin's accounts; a pin that names no
    // instance gets none.
    let pinned_key = pinned_plugin_key(store, job).await?;
    if job.payload.plugin_instance_id.is_some() && pinned_key.is_none() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for (provider, account_id) in accounts_named(store, account, pinned_key.as_deref()).await? {
        let rows = encrypted_secrets::Entity::find()
            .filter(encrypted_secrets::Column::AccountType.eq(secret_account_type::INTEGRATION))
            .filter(encrypted_secrets::Column::AccountId.eq(account_id.as_str()))
            .filter(encrypted_secrets::Column::Provider.eq(provider.as_str()))
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        for row in rows {
            if let Some(record) = get_secret(
                store.db(),
                &row.kind,
                Some(&provider),
                secret_account_type::INTEGRATION,
                Some(&account_id),
                &row.name,
            )
            .await?
            {
                records.push(record);
            }
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

/// Records the refs of `secrets` as handed to `node_id` under `fence`, and
/// drops the rows of the job's earlier generations.
///
/// One host batch, which every backend runs as a single transaction: it
/// first takes the job row with a fenced update, and every later statement
/// applies only while `fence` still owns the running job. A claim whose
/// lease was lost while its secrets were read therefore writes nothing,
/// and a newer claim's rows are left alone.
///
/// # Returns
///
/// `true` when the handoff was recorded; `false` when `fence` no longer
/// owns the running job.
///
/// # Errors
///
/// Returns an error when the batch fails.
pub(super) async fn record_handoff(
    store: &LibraryStore,
    node_id: &str,
    fence: &JobFence,
    secrets: &[JobSecret],
) -> Result<bool> {
    /// The fence still owns the running job, and its lease has not run out
    /// (compared as the reclaim compares it): secrets read for longer than
    /// the lease are not handed out for a job that may already be claimable
    /// again.
    const OWNED: &str = "EXISTS (SELECT 1 FROM jobs WHERE id = ? AND state = ? \
                          AND lease_owner = ? AND lease_generation = ? \
                          AND lease_expires_at > ?)";
    let now = Utc::now().to_rfc3339();
    let running = JobState::Running.as_str();
    let owned = || {
        vec![
            text(&fence.job_id),
            text(running),
            text(&fence.owner),
            int(fence.generation),
            text(&now),
        ]
    };
    let mut statements = vec![
        batch::exec(
            "UPDATE jobs SET updated_at = ?
              WHERE id = ? AND state = ? AND lease_owner = ? AND lease_generation = ?
                AND lease_expires_at > ?",
            [vec![text(&now)], owned()].concat(),
        ),
        batch::exec(
            &format!("DELETE FROM job_secret_handoffs WHERE job_id = ? AND {OWNED}"),
            [vec![text(&fence.job_id)], owned()].concat(),
        ),
    ];
    for secret in secrets {
        statements.push(batch::exec(
            &format!(
                "INSERT INTO job_secret_handoffs
                    (job_id, lease_generation, kind, provider, account_id, name, node_id,
                     handed_at)
                 SELECT ?, ?, ?, ?, ?, ?, ?, ? WHERE {OWNED}"
            ),
            [
                vec![
                    text(&fence.job_id),
                    int(fence.generation),
                    text(&secret.secret_ref.kind),
                    text(&secret.secret_ref.provider),
                    text(&secret.secret_ref.account_id),
                    text(&secret.secret_ref.name),
                    text(node_id),
                    text(&now),
                ],
                owned(),
            ]
            .concat(),
        ));
    }
    let operation_id = format!(
        "handoff-{}-{}-{}",
        fence.job_id,
        fence.generation,
        uuid::Uuid::new_v4()
    );
    let reply = batch::execute_host_batch(store, batch::request(&operation_id, statements)).await?;
    Ok(batch::rows_affected(&reply, 0) == 1)
}

/// Drops the handoff rows of `job_id` recorded under `generation` or an
/// earlier lease.
///
/// Callers run this after the claim is given back, when the job may
/// already be claimed again; a successor's lease has a higher generation,
/// so its rows are kept.
pub(super) async fn forget_handoffs(
    store: &LibraryStore,
    job_id: &str,
    generation: i64,
) -> Result<()> {
    job_secret_handoffs::Entity::delete_many()
        .filter(job_secret_handoffs::Column::JobId.eq(job_id))
        .filter(job_secret_handoffs::Column::LeaseGeneration.lte(generation))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// True when `secret_ref` was handed with the job under `fence`.
async fn was_handed(
    store: &LibraryStore,
    fence: &JobFence,
    secret_ref: &JobSecretRef,
) -> Result<bool> {
    handed_on(store.db(), fence, secret_ref).await
}

/// The one account id the job's account selector names (by id or label,
/// ignoring case, [`accounts_named`]) under its source plugin, when it
/// names exactly one and differs from it; `None` otherwise (no selector,
/// already canonical, or ambiguous).
///
/// # Errors
///
/// Returns an error when the read fails.
async fn canonical_account(
    store: &LibraryStore,
    job: &JobRecord,
    source_plugin_key: Option<&str>,
) -> Result<Option<String>> {
    let Some(selector) = job.payload.account.as_deref().filter(|a| !a.is_empty()) else {
        return Ok(None);
    };
    let named = accounts_named(store, selector, source_plugin_key).await?;
    let mut named = named.into_iter();
    Ok(match (named.next(), named.next()) {
        (Some((_, account_id)), None) if account_id != selector => Some(account_id),
        _ => None,
    })
}

/// `job` as handed to a remote node: its account selector replaced by the
/// `canonical` account id it resolved to ([`canonical_account`]), so the
/// node's credential lookup and its scan rows name the account by the id
/// the handed secrets and the owner's checks use. Only the handed copy
/// changes; the job row keeps the selector it was enqueued with.
fn with_account(mut job: JobRecord, canonical: Option<String>) -> JobRecord {
    if let Some(account_id) = canonical {
        job.payload.account = Some(account_id);
    }
    job
}

/// The job `fence` owns right now, if it still owns one: the job is
/// running under that owner and generation and its lease has not run out.
/// An expired lease no longer owns the job even before the reclaim runs
/// (it may be claimed again at any moment), so a node past its lease can
/// neither renew it nor write scan rows, open or commit uploads, or finish
/// the job, as the job-secret handoff gate (`record_handoff`) already
/// requires.
pub(super) async fn fenced_job(
    store: &LibraryStore,
    fence: &JobFence,
) -> Result<Option<JobRecord>> {
    let now = Utc::now();
    Ok(store.get_job(&fence.job_id).await?.filter(|job| {
        job.state == JobState::Running
            && job.lease_owner.as_deref() == Some(fence.owner.as_str())
            && job.lease_generation == fence.generation
            && job.lease_expires_at.is_some_and(|expires| expires > now)
    }))
}

/// The semaphore permits `node_id` holds for `job`, and whether every
/// semaphore the job counts against is among them.
///
/// The permits are found by holder: the prefix ([`job_permit_holder_prefix`])
/// names the job's current claim, generation included, so a permit left
/// over from an earlier claim of the same job by this node never matches
/// and expires with its own lease. Every permit the claim still holds is
/// returned, in any slot and whatever the semaphores the job would ask for
/// now: an account relabelled while the job runs keeps the permit the claim
/// took renewed. A semaphore the job counts against now but the claim does
/// not hold makes the set incomplete.
pub(super) async fn held_job_permits(
    store: &LibraryStore,
    node_id: &str,
    job: &JobRecord,
    limits: JobClaimLimits,
) -> Result<(Vec<SemaphorePermit>, bool)> {
    let prefix =
        job_permit_holder_prefix(&remote_lease_owner(node_id), &job.id, job.lease_generation);
    let held = permits_with_holder_prefix(store, &prefix).await?;
    let complete = job_semaphores(store, job, limits)
        .await?
        .iter()
        .all(|request| held.iter().any(|permit| permit.semaphore == request.name));
    Ok((held, complete))
}

/// Extends the lease of the job `node_id` holds at `generation`, and its
/// semaphore permits, recording `progress` when given.
///
/// Trust and the node credential are re-read first: a node that is no
/// longer trusted with the job (revoked, owner demoted, account unlinked)
/// gets [`RemoteRenew::Lost`], and the lease then expires as for a crashed
/// worker. A fence that expired and was reclaimed is lost as well, as is a
/// job whose permit another holder took over. Permits are checked before
/// the job lease is extended, so a renewal that answers `Lost` never pushes
/// the reclaim out; should a permit be taken over between that check and
/// its renewal, the just-extended lease is cut back to expire at once.
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
    let (permits, complete) = held_job_permits(store, node_id, &job, settings.limits).await?;
    if !complete {
        return Ok(RemoteRenew::Lost);
    }
    if !store
        .heartbeat_job(&fence, settings.lease_secs, progress)
        .await?
    {
        return Ok(RemoteRenew::Lost);
    }
    let ttl = Duration::from_secs(settings.lease_secs);
    if !renew_permits(store, &permits, ttl).await? {
        // A permit was taken over after the check above: hand the lease
        // straight back to the reclaimer instead of holding it a full term.
        store.heartbeat_job(&fence, 0, None).await?;
        return Ok(RemoteRenew::Lost);
    }
    // The renewed fence keeps the job's open uploads' tokens alive, so a
    // slow title is not cut off mid-way; nothing extends them once the
    // fence is lost.
    super::uploads::extend_job_uploads(store, node_id, job_id, generation).await?;
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
            if job.cancel_requested {
                // A cancelled job never succeeds, whatever the Host
                // reports; it ends cancelled.
                store
                    .fail_job(&fence, "cancelled", "cancelled by operator")
                    .await?
            } else {
                store.complete_job(&fence, detail).await?
            }
        }
        RemoteOutcome::Fail { kind, message } => store.fail_job(&fence, kind, message).await?,
    };
    if !finished {
        return Ok(RemoteFinish::Lost);
    }
    let (permits, _) = held_job_permits(store, node_id, &job, limits).await?;
    release_permits(store, &permits).await?;
    forget_handoffs(store, job_id, generation).await?;
    Ok(RemoteFinish::Done)
}

/// Gives back the job `node_id` holds at `generation` without running it:
/// the job returns to `pending`, and its semaphore permits and handoff rows
/// are released.
///
/// For a Host whose operator no longer consents to the job's plugin, and
/// for a `work` answer that cannot reach the Host (its connection was
/// superseded or is closing), so the job is claimable again at once instead
/// of only after its lease expires. `resumed` is the claim's
/// [`RemoteJobClaim::resumed`]: a resumed claim gets its resume flag back,
/// any other its attempt.
///
/// The attempt is not spent, unless the job carries a checkpoint (it may
/// have resumed, which spent none, or saved progress under this claim), in
/// which case it is kept as spent and the job resumes from the checkpoint.
/// Nothing is refunded twice: a release never lowers the count below what
/// the claim raised it by. The release is accepted from a node that is no
/// longer trusted, since it only gives work back.
///
/// # Returns
///
/// [`RemoteFinish::Done`] when the claim was given back;
/// [`RemoteFinish::Lost`] when the fence no longer owns the job.
///
/// # Errors
///
/// Returns an error when a read or write fails.
pub async fn release_remote_job(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    resumed: bool,
    limits: JobClaimLimits,
) -> Result<RemoteFinish> {
    let fence = remote_job_fence(node_id, job_id, generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(RemoteFinish::Lost);
    };
    let keep_spent = job.payload.checkpoint.is_some();
    let (permits, _) = held_job_permits(store, node_id, &job, limits).await?;
    // A checkpointed job keeps its attempt spent and resumes from the
    // checkpoint, as a resumed claim does.
    if !store
        .release_job_claim(&fence, resumed || keep_spent)
        .await?
    {
        return Ok(RemoteFinish::Lost);
    }
    release_permits(store, &permits).await?;
    forget_handoffs(store, job_id, generation).await?;
    Ok(RemoteFinish::Done)
}

/// Stores a refreshed store token a Host writes back.
///
/// Accepted only when all of these hold, checked immediately before the
/// write: the node still has a live credential and is still trusted with
/// the job; `generation` is the job's live lease generation for this node;
/// `secret_ref` was recorded as handed with the job under that generation
/// (a secret created for the account after the handoff is refused); and it
/// is still one of the job's secrets ([`job_secret_refs`]), so it still
/// exists and its account is still linked. The value is re-sealed with this
/// node's DEK under the same identity.
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
    if !was_handed(store, &fence, secret_ref).await?
        || !job_secret_refs(store, &job).await?.contains(secret_ref)
    {
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
    // The checks above read outside any transaction. The write is one
    // host batch, which every backend runs as a single transaction (D1
    // included, where interactive transactions are not available): it first
    // takes the job row with a fenced update, so a reclaim or a claim by
    // another node either committed before it (and the guards below match
    // nothing) or waits for it, and then updates the secret only while the
    // fence, the handoff row and the node credential are all still live.
    if !store_guarded_write_back(store, node_id, &fence, secret_ref, &record).await? {
        return Ok(WriteBack::Refused("fence_not_live"));
    }
    Ok(WriteBack::Written)
}

/// The job `node_id` holds at `generation`, while that fence is the job's
/// live lease and the node is still trusted with the job; `None` when it
/// is lost.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn live_remote_job(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
) -> Result<Option<JobRecord>> {
    let fence = remote_job_fence(node_id, job_id, generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(None);
    };
    if !node_may_claim_job(store, node_id, Claimant::Remote, &job).await? {
        return Ok(None);
    }
    Ok(Some(job))
}

/// Overwrites the sealed value of `secret_ref` with `record` in one host
/// batch, guarded on `fence` owning the running generation, the secret
/// having been handed under it, and `node_id` being an owner node or a host
/// node with an unrevoked credential.
///
/// Only an existing row is updated: a secret deleted since the handoff (its
/// account unlinked or revoked) is not brought back.
///
/// # Returns
///
/// `true` when the value was written; `false` when a guard failed.
///
/// # Errors
///
/// Returns an error when the batch fails.
async fn store_guarded_write_back(
    store: &LibraryStore,
    node_id: &str,
    fence: &JobFence,
    secret_ref: &JobSecretRef,
    record: &EncryptedSecretRecord,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let running = JobState::Running.as_str();
    let opt_text = |value: Option<&str>| value.map_or(DbValue::Null(DbType::Text), text);
    let opt_bytes = |value: Option<&[u8]>| {
        value.map_or(DbValue::Null(DbType::Bytes), |v| DbValue::Bytes(v.to_vec()))
    };
    let opt_int =
        |value: Option<u32>| value.map_or(DbValue::Null(DbType::Int64), |v| int(i64::from(v)));
    let operation_id = format!(
        "write-back-{}-{}-{}",
        fence.job_id,
        fence.generation,
        uuid::Uuid::new_v4()
    );
    let reply = batch::execute_host_batch(
        store,
        batch::request(
            &operation_id,
            vec![
                batch::exec(
                    "UPDATE jobs SET updated_at = ?
                      WHERE id = ? AND state = ? AND lease_owner = ? AND lease_generation = ?",
                    vec![
                        text(&now),
                        text(&fence.job_id),
                        text(running),
                        text(&fence.owner),
                        int(fence.generation),
                    ],
                ),
                batch::exec(
                    "UPDATE encrypted_secrets
                        SET format = ?, ciphertext = ?, kdf_algorithm = ?, kdf_salt = ?,
                            kdf_m_cost = ?, kdf_t_cost = ?, kdf_p_cost = ?,
                            cipher_algorithm = ?, cipher_nonce = ?, updated_at = ?
                      WHERE kind = ? AND provider = ? AND account_type = ?
                        AND account_id = ? AND name = ?
                        AND EXISTS (
                            SELECT 1 FROM jobs
                             WHERE id = ? AND state = ? AND lease_owner = ?
                               AND lease_generation = ?
                        )
                        AND EXISTS (
                            SELECT 1 FROM job_secret_handoffs
                             WHERE job_id = ? AND lease_generation = ? AND kind = ?
                               AND provider = ? AND account_id = ? AND name = ?
                        )
                        AND EXISTS (
                            SELECT 1 FROM nodes
                             WHERE nodes.node_id = ?
                               AND (nodes.kind = 'owner' OR EXISTS (
                                   SELECT 1 FROM node_credentials
                                    WHERE node_credentials.node_id = nodes.node_id
                                      AND node_credentials.revoked_at IS NULL
                               ))
                        )",
                    vec![
                        text(&record.format),
                        DbValue::Bytes(record.ciphertext.clone()),
                        opt_text(record.kdf_algorithm.as_deref()),
                        opt_bytes(record.kdf_salt.as_deref()),
                        opt_int(record.kdf_m_cost),
                        opt_int(record.kdf_t_cost),
                        opt_int(record.kdf_p_cost),
                        opt_text(record.cipher_algorithm.as_deref()),
                        opt_bytes(record.cipher_nonce.as_deref()),
                        text(&now),
                        text(&secret_ref.kind),
                        text(&secret_ref.provider),
                        text(secret_account_type::INTEGRATION),
                        text(&secret_ref.account_id),
                        text(&secret_ref.name),
                        text(&fence.job_id),
                        text(running),
                        text(&fence.owner),
                        int(fence.generation),
                        text(&fence.job_id),
                        int(fence.generation),
                        text(&secret_ref.kind),
                        text(&secret_ref.provider),
                        text(&secret_ref.account_id),
                        text(&secret_ref.name),
                        text(node_id),
                    ],
                ),
            ],
        ),
    )
    .await?;
    let written = batch::rows_affected(&reply, 0) == 1 && batch::rows_affected(&reply, 1) > 0;
    if written {
        forget_cached_secret(record);
    }
    Ok(written)
}

/// True when `secret_ref` was handed under `fence`, read on `db`.
async fn handed_on<C: ConnectionTrait>(
    db: &C,
    fence: &JobFence,
    secret_ref: &JobSecretRef,
) -> Result<bool> {
    let rows = job_secret_handoffs::Entity::find()
        .filter(job_secret_handoffs::Column::JobId.eq(fence.job_id.as_str()))
        .filter(job_secret_handoffs::Column::LeaseGeneration.eq(fence.generation))
        .filter(job_secret_handoffs::Column::Kind.eq(secret_ref.kind.as_str()))
        .filter(job_secret_handoffs::Column::Provider.eq(secret_ref.provider.as_str()))
        .filter(job_secret_handoffs::Column::AccountId.eq(secret_ref.account_id.as_str()))
        .filter(job_secret_handoffs::Column::Name.eq(secret_ref.name.as_str()))
        .count(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows > 0)
}
