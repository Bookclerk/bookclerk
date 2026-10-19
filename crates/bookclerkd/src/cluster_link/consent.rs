//! Host consent for owner plugins, per cluster link (H-4 design, section 5;
//! step 8).
//!
//! [`LinkSupervisor`] keeps one link's [`RemoteCatalog`] current and tells
//! the host client which plugins it may claim jobs for:
//!
//! - on every connection and on each `deployment.changed`,
//!   `install.changed`, `placement.changed` and `configuration.changed`
//!   wake it re-reads `deployments.list`, records what the owner asks to
//!   run (`remote_plugin_consents`, the owner's approved grant and the
//!   archive URL and digest for this host as the request), auto-approves a Bookclerk platform plugin only when this
//!   host's own provenance check says the owner's artifact is the bundled
//!   one, and evaluates consent;
//! - for each consented instance it materializes the version (an owner
//!   `blob:` archive comes over the signed artifact endpoint and is checked
//!   against its digest before it is stored; an `https:` archive is fetched
//!   by the materializer itself, only when its URL and digest are the ones
//!   this host's operator approved and its host (and every redirect
//!   hop's) is not local or private; `file:` URLs are refused) and reads the
//!   instance's `CONFIG` and `SECRETS` with `instance.config`;
//! - it reports each deployment as `materialized`, or `error` with the
//!   consent state (`awaiting_host_consent`, `awaiting_host_reapproval`,
//!   `invalid_grant`) or the failure as the detail;
//! - every [`CONSENT_POLL`] and whenever [`notify_consent_changed`] is
//!   called (the API's approve and revoke) it re-reads the consent rows, so
//!   an approval or a revoke from the CLI takes effect without a wake.
//!   Losing consent cancels the plugin's held jobs and gives them back. A
//!   change notified while a refresh runs is not lost (it is a generation
//!   counter, not a one-shot wake-up), and a refresh re-reads the consent
//!   rows after its slow steps (fetch, `instance.config`) and before it
//!   publishes, so a revoke that lands mid-refresh is never overwritten by
//!   the state the refresh read when it started;
//! - a refresh that could not finish (the owner unreachable, or a consented
//!   instance whose archive or `instance.config` failed) is retried with
//!   backoff ([`RETRY_MIN`] doubling to [`RETRY_MAX`]) until it does, so a
//!   transient failure never leaves a consented plugin unclaimable;
//! - an operator's revoke is recorded and holds: a platform plugin is not
//!   approved automatically again until the operator approves it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bookclerk_library::control_plane::{
    list_remote_plugin_consents, load_artifact_blob, put_artifact_blob,
    record_remote_plugin_request, PluginArtifact, PluginVersion, RemoteApprover, RemoteArtifactRef,
    RemotePluginConsent, ARTIFACT_BLOB_SCHEME,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_client::{HostClientHandle, AWAITING_HOST_CONSENT};
use bookclerk_node_proto::{Body, RemoteDeployment, RemoteVersion};
use bookclerk_plugin_host::{
    approve_remote_request, host_artifact, is_sha256_hex, materialize_owner_version,
    remote_platform_auto_approval, remote_row_consent_state, PluginGrant, RemoteConsentState,
    RemoteNarrowing,
};
use tokio::sync::watch;
use tokio::time::Instant;

use super::remote_bindings::{RemoteCatalog, RemoteInstanceConfig, RemoteInstanceEntry};

/// How often consent rows are re-read without a notification.
pub const CONSENT_POLL: Duration = Duration::from_secs(5);

/// First delay before an unfinished refresh is retried.
pub const RETRY_MIN: Duration = CONSENT_POLL;

/// Longest delay between retries of an unfinished refresh.
pub const RETRY_MAX: Duration = Duration::from_secs(300);

/// Wakes that make a host re-read what its owner deployed to it.
const REFRESH_WAKES: &[&str] = &[
    "deployment.changed",
    "install.changed",
    "placement.changed",
    "configuration.changed",
];

/// Bumped when this daemon's API approves or revokes a consent.
///
/// A `watch` generation rather than a `Notify`: a supervisor that is busy
/// in a refresh when the change lands still sees it (`changed()` is ready
/// on its next turn), where `Notify::notify_waiters` would wake only the
/// tasks already waiting and drop the change for everyone else.
static CONSENT_GENERATION: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::Sender::new(0));

/// Tells every link supervisor in this process to re-read consent now.
pub fn notify_consent_changed() {
    CONSENT_GENERATION.send_modify(|generation| *generation = generation.wrapping_add(1));
}

/// A receiver of [`notify_consent_changed`], marked as having seen every
/// change so far.
fn consent_changes() -> watch::Receiver<u64> {
    CONSENT_GENERATION.subscribe()
}

/// How a refresh ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refreshed {
    /// Every deployment was evaluated to a settled state.
    Settled,
    /// The owner did not answer, or an entry hit a failure that may pass
    /// (an archive or `instance.config` that could not be read, a request
    /// that could not be recorded): try again.
    Unfinished,
}

/// When an unfinished refresh is tried again: [`RETRY_MIN`] after the
/// first failure, doubling to [`RETRY_MAX`]; a settled refresh, a new
/// connection or a refresh wake resets it.
#[derive(Debug, Default)]
struct RetryBackoff {
    /// Unfinished refreshes in a row.
    failures: u32,
    /// Earliest time of the next try (`None`: now).
    next: Option<Instant>,
}

impl RetryBackoff {
    /// True when a retry may run at `now`.
    fn due(&self, now: Instant) -> bool {
        self.next.is_none_or(|at| now >= at)
    }

    /// The delay after `failures` unfinished refreshes in a row.
    fn delay(failures: u32) -> Duration {
        let factor = 1_u32 << failures.saturating_sub(1).min(16);
        RETRY_MIN.saturating_mul(factor).min(RETRY_MAX)
    }

    /// Records an unfinished refresh at `now`.
    fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.next = Some(now + Self::delay(self.failures));
    }

    /// Clears the backoff (settled, or something changed).
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// Applies a refresh outcome; answers whether a refresh is still owed.
    fn record(&mut self, outcome: Refreshed, now: Instant) -> bool {
        match outcome {
            Refreshed::Settled => {
                self.reset();
                false
            }
            Refreshed::Unfinished => {
                self.failed(now);
                true
            }
        }
    }
}

/// Keeps one link's catalog and consented plugin set current.
pub struct LinkSupervisor {
    /// This host's own database.
    pub library: LibraryStore,
    /// This host's files dir (the version cache lives under it).
    pub files_dir: PathBuf,
    /// The link's client.
    pub handle: HostClientHandle,
    /// The link's catalog, shared with the job runner.
    pub catalog: Arc<RemoteCatalog>,
    /// Payload digests of this host's own platform-bundled plugins, by
    /// plugin key (from this host's provenance check).
    pub platform_payloads: BTreeMap<String, String>,
}

impl LinkSupervisor {
    /// Runs until `shutdown` turns true.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut connections = self.handle.connections();
        let mut consent_changes = consent_changes();
        let mut wakes = self.handle.subscribe_wakes();
        let mut poll = tokio::time::interval(CONSENT_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stale = true;
        let mut backoff = RetryBackoff::default();
        loop {
            if stale && self.handle.is_connected() && backoff.due(Instant::now()) {
                let outcome = self.refresh().await;
                stale = backoff.record(outcome, Instant::now());
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                changed = connections.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    stale = true;
                    backoff.reset();
                }
                wake = wakes.recv() => match wake {
                    Ok(wake) => {
                        let kind = wake.get("type").and_then(serde_json::Value::as_str);
                        if kind.is_some_and(|kind| REFRESH_WAKES.contains(&kind)) {
                            stale = true;
                            backoff.reset();
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        stale = true;
                        backoff.reset();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                Ok(()) = consent_changes.changed() => {
                    if let Some(outcome) = self.reevaluate().await {
                        stale = backoff.record(outcome, Instant::now());
                    }
                }
                _ = poll.tick() => {
                    if let Some(outcome) = self.reevaluate().await {
                        stale = backoff.record(outcome, Instant::now());
                    }
                    // An unfinished refresh is retried at the top of the
                    // loop once its backoff is due; the poll's tick is what
                    // brings the loop round.
                }
            }
        }
    }

    /// The link's owner cluster id.
    fn cluster_id(&self) -> &str {
        self.handle.cluster_id()
    }

    /// Publishes the consented set to the client.
    fn publish(&self) {
        self.handle
            .set_consented_plugins(self.catalog.consented_keys());
    }

    /// Re-reads the consent rows and applies them; an instance that gains
    /// consent is fetched, one that loses it is dropped. Answers the
    /// outcome of the refresh a change ran, if one did.
    async fn reevaluate(&self) -> Option<Refreshed> {
        let rows = match list_remote_plugin_consents(&self.library, Some(self.cluster_id())).await {
            Ok(rows) => rows,
            Err(err) => {
                // Fail closed: nothing stays claimable on an unreadable row.
                tracing::warn!(cluster_id = self.cluster_id(), error = %err, "consent rows unreadable");
                self.handle.set_consented_plugins(BTreeSet::new());
                return None;
            }
        };
        let by_key: BTreeMap<&str, &RemotePluginConsent> = rows
            .iter()
            .map(|row| (row.plugin_key.as_str(), row))
            .collect();
        let mut changed = false;
        let keys: BTreeSet<String> = self
            .catalog
            .entries()
            .into_iter()
            .map(|entry| entry.plugin_key)
            .collect();
        for key in keys {
            let state = by_key
                .get(key.as_str())
                .map_or(RemoteConsentState::Pending, |row| consent_state(row));
            let consented = matches!(state, RemoteConsentState::Consented(_));
            if self.catalog.set_consent(&key, &state) {
                changed = true;
                if !consented {
                    tracing::info!(
                        cluster_id = self.cluster_id(),
                        plugin_key = %key,
                        consent = state.as_str(),
                        "owner plugin no longer consented"
                    );
                }
            }
        }
        // Withdraw first so a revoke cancels held work before any fetch;
        // the refresh then fetches what gained consent and reports both.
        self.publish();
        if changed {
            Some(self.refresh().await)
        } else {
            None
        }
    }

    /// Re-reads what the owner deployed here and rebuilds the catalog.
    /// Answers [`Refreshed::Unfinished`] when the owner did not answer or
    /// any entry hit a failure that may pass, so the caller retries.
    async fn refresh(&self) -> Refreshed {
        let Some(frame) = self.handle.request(Body::DeploymentsList {}).await else {
            return Refreshed::Unfinished;
        };
        let Body::Deployments { items } = frame.body else {
            tracing::warn!(
                cluster_id = self.cluster_id(),
                "owner refused deployments.list"
            );
            return Refreshed::Unfinished;
        };
        let mut entries = BTreeMap::new();
        let mut outcome = Refreshed::Settled;
        for item in &items {
            let (entry, evaluated) = self.evaluate(item).await;
            if evaluated == Refreshed::Unfinished {
                tracing::debug!(
                    cluster_id = self.cluster_id(),
                    instance_id = %entry.instance_id,
                    "owner plugin not ready yet; retrying"
                );
                outcome = Refreshed::Unfinished;
            }
            entries.insert(entry.instance_id.clone(), entry);
        }
        // Each entry's consent was read before its fetch and
        // `instance.config`; an operator may have revoked (or changed) it
        // since. Re-read the rows now, right before publishing, so a stale
        // `Consented` never replaces the revoke `reevaluate` already
        // published. A revoke after this read bumps the consent generation
        // and is applied by the loop as soon as this refresh returns.
        if self.reconcile(&mut entries).await {
            outcome = Refreshed::Unfinished;
        }
        self.catalog.replace(entries.clone());
        self.publish();
        for item in &items {
            let Some(entry) = entries.get(&item.instance_id) else {
                continue;
            };
            if !self.observe(item, entry).await {
                // Retried with the refresh's backoff, so the owner does not
                // keep a stale or missing observation until an unrelated
                // wake.
                outcome = Refreshed::Unfinished;
            }
        }
        outcome
    }

    /// Brings each of `entries` up to the consent rows as they stand now
    /// ([`reconcile_consent`]). Answers true when anything changed (the
    /// refresh then counts as unfinished, so a changed grant is fetched
    /// again and the owner is told the new state).
    ///
    /// Fails closed: when the rows cannot be read, every entry is marked
    /// not ready, so nothing is published as claimable on a stale read.
    async fn reconcile(&self, entries: &mut BTreeMap<String, RemoteInstanceEntry>) -> bool {
        match list_remote_plugin_consents(&self.library, Some(self.cluster_id())).await {
            Ok(rows) => {
                let fresh: BTreeMap<String, RemoteConsentState> = rows
                    .iter()
                    .map(|row| (row.plugin_key.clone(), consent_state(row)))
                    .collect();
                reconcile_consent(entries, &fresh)
            }
            Err(err) => {
                tracing::warn!(cluster_id = self.cluster_id(), error = %err, "consent rows unreadable");
                for entry in entries.values_mut() {
                    entry.problem = Some("this host's consent rows could not be read".into());
                }
                true
            }
        }
    }

    /// One deployment's catalog entry: its consent, and when consented its
    /// materialized tree and bindings. The outcome is
    /// [`Refreshed::Unfinished`] when the entry failed on something that may
    /// pass (recording the request, or a consented instance's archive or
    /// configuration), so it is evaluated again.
    async fn evaluate(&self, item: &RemoteDeployment) -> (RemoteInstanceEntry, Refreshed) {
        let (entry, transient) = self.evaluate_entry(item).await;
        let outcome = if transient
            || (matches!(entry.consent, RemoteConsentState::Consented(_)) && !entry.is_ready())
        {
            Refreshed::Unfinished
        } else {
            Refreshed::Settled
        };
        (entry, outcome)
    }

    /// [`Self::evaluate`] without the outcome; the flag marks a failure to
    /// record the request (a database write that may pass).
    async fn evaluate_entry(&self, item: &RemoteDeployment) -> (RemoteInstanceEntry, bool) {
        let mut entry = RemoteInstanceEntry {
            instance_id: item.instance_id.clone(),
            plugin_key: item.plugin_key.clone(),
            default_instance: item.default_instance,
            consent: RemoteConsentState::Pending,
            root: None,
            version: None,
            config: None,
            problem: None,
        };
        let Some(remote) = item.version.as_ref() else {
            entry.problem = Some("the owner has no active version of this plugin".into());
            return (entry, false);
        };
        if remote.grant_json.trim().is_empty() {
            entry.problem = Some("the owner's operator has not approved this version".into());
            return (entry, false);
        }
        let version = match plugin_version(&item.plugin_key, remote) {
            Ok(version) => version,
            Err(problem) => {
                entry.consent = RemoteConsentState::Invalid;
                entry.problem = Some(problem);
                return (entry, false);
            }
        };
        // The archive this host would fetch is part of what its operator
        // reviews, so another URL or digest needs approval again.
        let artifact = match host_artifact(&version) {
            Ok(artifact) => artifact,
            Err(err) => {
                entry.problem = Some(err.to_string());
                return (entry, false);
            }
        };
        let row = match record_remote_plugin_request(
            &self.library,
            self.cluster_id(),
            &item.plugin_key,
            &remote.version,
            &remote.grant_json,
            RemoteArtifactRef::of(artifact),
        )
        .await
        {
            Ok(row) => row,
            Err(err) => {
                entry.consent = RemoteConsentState::Invalid;
                entry.problem = Some(format!("the owner's request was not recorded: {err}"));
                return (entry, true);
            }
        };
        let row = self.auto_approve_platform(row, &version).await;
        entry.consent = consent_state(&row);
        let RemoteConsentState::Consented(grant) = &entry.consent else {
            return (entry, false);
        };
        match self.materialize(&version, grant, &row).await {
            Ok(root) => {
                let mut local = version.clone();
                match serde_json::to_string(&**grant) {
                    Ok(grant_json) => local.grant_json = grant_json,
                    Err(err) => {
                        entry.problem = Some(format!("the consented grant does not encode: {err}"));
                        return (entry, false);
                    }
                }
                entry.root = Some(root);
                entry.version = Some(local);
            }
            Err(problem) => {
                entry.problem = Some(problem);
                return (entry, false);
            }
        }
        match self.instance_config(&item.instance_id).await {
            Ok(config) => entry.config = Some(config),
            Err(problem) => entry.problem = Some(problem),
        }
        (entry, false)
    }

    /// Approves an unapproved platform plugin when this host bundles the
    /// owner's artifact and its operator has not revoked it; answers the
    /// row as it stands afterwards.
    async fn auto_approve_platform(
        &self,
        row: RemotePluginConsent,
        version: &PluginVersion,
    ) -> RemotePluginConsent {
        if !platform_auto_approval_allowed(&row) {
            return row;
        }
        let Ok(requested) = serde_json::from_str::<PluginGrant>(&row.requested_grant_json) else {
            return row;
        };
        let Ok(artifact) = host_artifact(version) else {
            return row;
        };
        if !remote_platform_auto_approval(
            &requested,
            &artifact.payload_root_sha256,
            self.platform_payloads
                .get(&row.plugin_key)
                .map(String::as_str),
        ) {
            return row;
        }
        match approve_remote_request(
            &self.library,
            &row.owner_cluster_id,
            &row.plugin_key,
            &row.request_digest(),
            &RemoteNarrowing::default(),
            RemoteApprover::Platform,
        )
        .await
        {
            Ok(approved) => approved,
            Err(err) => {
                tracing::warn!(plugin_key = %row.plugin_key, error = %err, "platform auto-approval failed");
                row
            }
        }
    }

    /// Materializes `version` into this host's version cache, but only the
    /// archive `row`'s approval names ([`owner_artifact_allowed`]).
    async fn materialize(
        &self,
        version: &PluginVersion,
        grant: &PluginGrant,
        row: &RemotePluginConsent,
    ) -> Result<PathBuf, String> {
        let artifact = host_artifact(version).map_err(|err| err.to_string())?;
        owner_artifact_allowed(row, artifact)?;
        if let Some(sha) = artifact.url.strip_prefix(ARTIFACT_BLOB_SCHEME) {
            let present = load_artifact_blob(&self.library, sha)
                .await
                .map_err(|err| err.to_string())?
                .is_some();
            if !present {
                let bytes =
                    self.handle.fetch_artifact(sha).await.map_err(|err| {
                        format!("the owner's archive could not be fetched: {err}")
                    })?;
                // Checked before it is stored or extracted.
                let actual: String = {
                    use sha2::Digest as _;
                    sha2::Sha256::digest(&bytes)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect()
                };
                if actual != sha || actual != artifact.archive_sha256 {
                    return Err("the owner's archive does not match its digest".into());
                }
                put_artifact_blob(&self.library, &bytes)
                    .await
                    .map_err(|err| err.to_string())?;
            }
        }
        let mut local = version.clone();
        local.grant_json = serde_json::to_string(grant).map_err(|err| err.to_string())?;
        materialize_owner_version(&self.library, &self.files_dir, &local)
            .await
            .map_err(|err| format!("the plugin could not be materialized: {err}"))
    }

    /// Reads an instance's `CONFIG` and `SECRETS` from the owner.
    async fn instance_config(&self, instance_id: &str) -> Result<RemoteInstanceConfig, String> {
        let frame = self
            .handle
            .request(Body::InstanceConfigGet {
                instance_id: instance_id.to_string(),
            })
            .await
            .ok_or_else(|| "the owner did not answer instance.config".to_string())?;
        match frame.body {
            Body::InstanceConfig {
                instance_id: answered,
                revision,
                config,
                sealed_secrets,
                secrets_withheld,
            } if answered == instance_id => {
                let secrets = match sealed_secrets {
                    Some(sealed) => Some(Arc::new(
                        self.handle
                            .open_instance_secrets(instance_id, revision, &sealed)
                            .map_err(|_| {
                                "the instance's secrets could not be opened".to_string()
                            })?,
                    )),
                    None => None,
                };
                Ok(RemoteInstanceConfig {
                    revision,
                    config,
                    secrets,
                    secrets_withheld,
                })
            }
            Body::Error { code, .. } => Err(format!("the owner refused instance.config: {code}")),
            _ => Err("unexpected answer to instance.config".into()),
        }
    }

    /// Reports one deployment's state to the owner; answers whether the
    /// owner accepted the observation.
    async fn observe(&self, item: &RemoteDeployment, entry: &RemoteInstanceEntry) -> bool {
        let (status, detail) = if entry.is_ready() {
            ("materialized", String::new())
        } else if let Some(problem) = &entry.problem {
            ("error", problem.clone())
        } else if matches!(entry.consent, RemoteConsentState::Pending) {
            ("error", AWAITING_HOST_CONSENT.to_string())
        } else {
            ("error", entry.consent.as_str().to_string())
        };
        let answer = self
            .handle
            .request(Body::Observation {
                deployment_id: item.deployment_id.clone(),
                status: status.into(),
                detail,
                applied_config_revision: entry.config.as_ref().map(|config| config.revision),
            })
            .await;
        let accepted = matches!(answer, Some(ref frame) if matches!(frame.body, Body::Accepted {}));
        if !accepted {
            tracing::debug!(
                deployment_id = %item.deployment_id,
                "owner did not accept the observation"
            );
        }
        accepted
    }
}

/// Applies the consent states read now (`fresh`, by plugin key; a key
/// without a row is [`RemoteConsentState::Pending`]) to `entries` that were
/// evaluated as consented against an earlier read.
///
/// A consented entry whose consent changed since (revoked, or approved
/// again with another grant) takes the fresh state and drops its
/// materialized tree and bindings: those were fetched under the old grant
/// (or none should be held at all), so the entry is not ready until a
/// refresh fetches them under the new one. An entry that was not evaluated
/// as consented is left alone: it is not claimable either way, its state
/// may come from the owner's version rather than the row (no active or
/// approved version), and a new approval reaches it through
/// `reevaluate`. Answers true when any entry changed.
pub(crate) fn reconcile_consent(
    entries: &mut BTreeMap<String, RemoteInstanceEntry>,
    fresh: &BTreeMap<String, RemoteConsentState>,
) -> bool {
    let mut changed = false;
    for entry in entries
        .values_mut()
        .filter(|entry| matches!(entry.consent, RemoteConsentState::Consented(_)))
    {
        let now = fresh
            .get(&entry.plugin_key)
            .cloned()
            .unwrap_or(RemoteConsentState::Pending);
        if now != entry.consent {
            entry.consent = now;
            entry.root = None;
            entry.config = None;
            changed = true;
        }
    }
    changed
}

/// True when a row may be approved automatically at all: not approved yet
/// and not revoked by this host's operator (a revoke holds until the
/// operator approves again; the store refuses it too).
pub(crate) fn platform_auto_approval_allowed(row: &RemotePluginConsent) -> bool {
    !row.is_approved() && !row.is_revoked()
}

/// A consent row's state.
pub(crate) fn consent_state(row: &RemotePluginConsent) -> RemoteConsentState {
    remote_row_consent_state(row)
}

/// Whether this host may fetch `artifact` for an owner: its URL and digest
/// must be the ones `row`'s approval names (the owner cannot repoint an
/// approved plugin), and it must be a `blob:` archive from the owner link
/// or an `https:` URL to a host that is not local or private
/// ([`bookclerk_plugin_catalog::check_owner_artifact_url`]).
pub(crate) fn owner_artifact_allowed(
    row: &RemotePluginConsent,
    artifact: &PluginArtifact,
) -> Result<(), String> {
    if !row.approved_artifact_matches(RemoteArtifactRef::of(artifact)) {
        return Err("the owner's archive is not the one this host's operator approved".into());
    }
    if artifact.url.starts_with(ARTIFACT_BLOB_SCHEME) {
        return Ok(());
    }
    if !artifact.url.starts_with("https:") {
        return Err("only blob: and https: archives are fetched from an owner".into());
    }
    bookclerk_plugin_catalog::check_owner_artifact_url(&artifact.url)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// The owner's version as this host's materializer reads it.
///
/// Every artifact's archive and payload digests must be canonical SHA-256
/// ([`is_sha256_hex`]) before anything else reads them: the payload digest
/// names this host's version-cache directory, so an owner-chosen `..` or
/// absolute value must never reach a path (the materializer refuses it
/// too).
///
/// # Errors
///
/// Returns the problem to report when the settings schema does not parse
/// or a digest is not 64 lowercase hex digits.
fn plugin_version(plugin_key: &str, remote: &RemoteVersion) -> Result<PluginVersion, String> {
    if remote.artifacts.iter().any(|artifact| {
        !is_sha256_hex(&artifact.archive_sha256) || !is_sha256_hex(&artifact.payload_root_sha256)
    }) {
        return Err("the owner's archive digests are not 64 lowercase hex digits".into());
    }
    let settings = serde_json::from_value(remote.settings.clone())
        .map_err(|_| "the owner's settings schema does not parse".to_string())?;
    Ok(PluginVersion {
        plugin_key: plugin_key.to_string(),
        version: remote.version.clone(),
        manifest_id: remote.manifest_id.clone(),
        manifest_toml: remote.manifest_toml.clone(),
        settings,
        artifacts: remote
            .artifacts
            .iter()
            .map(|artifact| PluginArtifact {
                target: artifact.target.clone(),
                url: artifact.url.clone(),
                format: artifact.format.clone(),
                archive_root: artifact.archive_root.clone(),
                archive_sha256: artifact.archive_sha256.clone(),
                payload_root_sha256: artifact.payload_root_sha256.clone(),
                executable: artifact.executable.clone(),
            })
            .collect(),
        publisher_verified: remote.publisher_verified,
        grant_json: remote.grant_json.clone(),
        coordinate: String::new(),
        installed_at: String::new(),
        installed_by: String::new(),
    })
}

/// Payload digests of this host's platform-bundled plugins, by key.
#[must_use]
pub fn platform_payloads(config: &bookclerk_config::Config) -> BTreeMap<String, String> {
    let Ok(plugins) = bookclerk_plugin_host::discover_plugins(config) else {
        return BTreeMap::new();
    };
    plugins
        .into_iter()
        .filter(|plugin| {
            plugin.identity.provenance
                == bookclerk_plugin_catalog::PluginProvenance::PlatformBundled
        })
        .map(|plugin| {
            (
                plugin.identity.plugin_key.canonical().to_string(),
                plugin.identity.artifact.payload_root_sha256.clone(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unfinished_refresh_backs_off_and_a_settled_one_resets() {
        let start = Instant::now();
        let mut backoff = RetryBackoff::default();
        assert!(backoff.due(start));
        assert!(backoff.record(Refreshed::Unfinished, start));
        assert!(!backoff.due(start));
        assert!(backoff.due(start + RETRY_MIN));
        assert!(backoff.record(Refreshed::Unfinished, start));
        assert!(!backoff.due(start + RETRY_MIN));
        assert!(backoff.due(start + RETRY_MIN * 2));
        for _ in 0..40 {
            backoff.failed(start);
        }
        assert!(backoff.due(start + RETRY_MAX));
        assert!(!backoff.due(start + RETRY_MAX - Duration::from_secs(1)));
        assert!(!backoff.record(Refreshed::Settled, start));
        assert!(backoff.due(start));
    }

    fn row() -> RemotePluginConsent {
        RemotePluginConsent {
            owner_cluster_id: "cluster-a".into(),
            plugin_key: "platform:bookclerk/bookclerk-plugin-database-sqlite".into(),
            requested_version: "1.0.0".into(),
            requested_grant_json: "{}".into(),
            requested_artifact_url: "https://registry.example/sqlite.tar.gz".into(),
            requested_artifact_sha256: "ab".repeat(32),
            requested_artifact_payload_sha256: "cd".repeat(32),
            version: None,
            approved_request_json: None,
            approved_artifact_url: None,
            approved_artifact_sha256: None,
            approved_artifact_payload_sha256: None,
            grant_json: None,
            approved_at: None,
            approved_by: None,
            revoked_at: None,
            updated_at: "2026-10-01T00:00:00Z".into(),
        }
    }

    /// An owner version whose one artifact has these digests.
    fn remote_version(archive_sha256: &str, payload_root_sha256: &str) -> RemoteVersion {
        RemoteVersion {
            version: "1.0.0".into(),
            manifest_id: "store".into(),
            manifest_toml: String::new(),
            settings: serde_json::to_value(bookclerk_plugin_abi::SettingsSchema::default())
                .unwrap(),
            artifacts: vec![bookclerk_node_proto::RemoteArtifact {
                target: bookclerk_plugin_host::host_bookclerk_target().to_string(),
                url: "https://registry.example/store.tar.gz".into(),
                format: "tar.gz".into(),
                archive_root: ".".into(),
                archive_sha256: archive_sha256.into(),
                payload_root_sha256: payload_root_sha256.into(),
                executable: "guest".into(),
            }],
            grant_json: "{}".into(),
            publisher_verified: false,
        }
    }

    /// An owner's digest that is not canonical SHA-256 (a `..` or absolute
    /// path, uppercase, empty) is refused before it can name a path.
    #[test]
    fn an_owner_digest_must_be_canonical_sha256() {
        let good = "ab".repeat(32);
        assert!(plugin_version("store", &remote_version(&good, &good)).is_ok());
        for bad in [
            "..",
            "../../etc",
            "/etc",
            "",
            &"AB".repeat(32),
            &"ab".repeat(33),
        ] {
            for version in [remote_version(bad, &good), remote_version(&good, bad)] {
                let problem = plugin_version("store", &version).unwrap_err();
                assert!(problem.contains("64 lowercase hex"), "{bad}: {problem}");
            }
        }
    }

    /// A ready entry of `plugin_key` consented with `grant`.
    fn ready_entry(
        instance_id: &str,
        plugin_key: &str,
        grant: &PluginGrant,
    ) -> RemoteInstanceEntry {
        RemoteInstanceEntry {
            instance_id: instance_id.into(),
            plugin_key: plugin_key.into(),
            default_instance: true,
            consent: RemoteConsentState::Consented(Box::new(grant.clone())),
            root: Some(PathBuf::from("/versions/store")),
            config: Some(RemoteInstanceConfig {
                revision: 1,
                config: serde_json::json!({}),
                secrets: None,
                secrets_withheld: false,
            }),
            problem: None,
        }
    }

    /// A revoke that lands while a refresh fetches is applied before the
    /// refresh publishes: the entry it evaluated as consented is not ready,
    /// so the plugin is not claimable, and an unchanged plugin is kept.
    #[test]
    fn a_revoke_during_refresh_is_not_overwritten() {
        let grant = PluginGrant {
            plugin_key: "store".into(),
            plugin_id: "store".into(),
            ..PluginGrant::empty()
        };
        let mut entries: BTreeMap<String, RemoteInstanceEntry> = [
            ("a".to_string(), ready_entry("a", "store", &grant)),
            ("b".to_string(), ready_entry("b", "other", &grant)),
        ]
        .into();
        let consented = RemoteConsentState::Consented(Box::new(grant.clone()));
        let unchanged: BTreeMap<String, RemoteConsentState> = [
            ("store".to_string(), consented.clone()),
            ("other".to_string(), consented.clone()),
        ]
        .into();
        assert!(!reconcile_consent(&mut entries, &unchanged));
        assert!(entries.values().all(RemoteInstanceEntry::is_ready));

        // `store` was revoked (its row reads as pending) mid-refresh.
        let revoked: BTreeMap<String, RemoteConsentState> = [
            ("store".to_string(), RemoteConsentState::Pending),
            ("other".to_string(), consented),
        ]
        .into();
        assert!(reconcile_consent(&mut entries, &revoked));
        let catalog = RemoteCatalog::default();
        catalog.replace(entries.clone());
        assert_eq!(
            catalog.consented_keys(),
            BTreeSet::from(["other".to_string()]),
            "the revoked plugin is not published as claimable"
        );
        assert!(entries["a"].config.is_none() && entries["a"].root.is_none());

        // A changed grant is not run with what the old one fetched.
        let mut entries: BTreeMap<String, RemoteInstanceEntry> =
            [("a".to_string(), ready_entry("a", "store", &grant))].into();
        let wider = PluginGrant {
            network_mode: "outbound".into(),
            ..grant.clone()
        };
        let regranted: BTreeMap<String, RemoteConsentState> = [(
            "store".to_string(),
            RemoteConsentState::Consented(Box::new(wider)),
        )]
        .into();
        assert!(reconcile_consent(&mut entries, &regranted));
        assert!(!entries["a"].is_ready());

        // Only a consented entry is downgraded: one evaluated as pending
        // (no approved version yet) is not made consented by its row.
        let mut entries: BTreeMap<String, RemoteInstanceEntry> = [(
            "a".to_string(),
            RemoteInstanceEntry {
                consent: RemoteConsentState::Pending,
                root: None,
                config: None,
                problem: Some("the owner has no active version of this plugin".into()),
                ..ready_entry("a", "store", &grant)
            },
        )]
        .into();
        assert!(!reconcile_consent(&mut entries, &unchanged));
        assert_eq!(entries["a"].consent, RemoteConsentState::Pending);
    }

    /// A consent change notified while no supervisor waits (one busy in a
    /// refresh) is still seen on its next turn.
    #[tokio::test]
    async fn a_consent_change_notified_mid_refresh_is_not_lost() {
        // Other tests in this process may notify too; only "seen" matters.
        let mut changes = consent_changes();
        notify_consent_changed();
        assert!(changes.has_changed().unwrap());
        tokio::time::timeout(Duration::from_secs(1), changes.changed())
            .await
            .expect("the change is pending")
            .unwrap();
    }

    #[test]
    fn a_revoked_platform_plugin_is_not_approved_automatically() {
        assert!(platform_auto_approval_allowed(&row()));
        let revoked = RemotePluginConsent {
            revoked_at: Some("2026-10-02T00:00:00Z".into()),
            ..row()
        };
        assert!(!platform_auto_approval_allowed(&revoked));
        let approved = RemotePluginConsent {
            version: Some("1.0.0".into()),
            approved_request_json: Some("{}".into()),
            grant_json: Some("{}".into()),
            approved_at: Some("2026-10-02T00:00:00Z".into()),
            approved_by: Some("platform".into()),
            ..row()
        };
        assert!(!platform_auto_approval_allowed(&approved));
    }

    /// `row()` approved for `url` (archive digest `ab…`, payload digest
    /// `cd…`), and that archive.
    fn approved_for(url: &str) -> (RemotePluginConsent, PluginArtifact) {
        let row = RemotePluginConsent {
            requested_artifact_url: url.into(),
            version: Some("1.0.0".into()),
            approved_request_json: Some("{}".into()),
            approved_artifact_url: Some(url.into()),
            approved_artifact_sha256: Some("ab".repeat(32)),
            approved_artifact_payload_sha256: Some("cd".repeat(32)),
            grant_json: Some("{}".into()),
            approved_at: Some("2026-10-02T00:00:00Z".into()),
            approved_by: Some("operator".into()),
            ..row()
        };
        let artifact = PluginArtifact {
            target: bookclerk_plugin_host::host_bookclerk_target().to_string(),
            url: url.into(),
            format: "tar.gz".into(),
            archive_root: ".".into(),
            archive_sha256: "ab".repeat(32),
            payload_root_sha256: "cd".repeat(32),
            executable: "guest".into(),
        };
        (row, artifact)
    }

    #[test]
    fn only_the_approved_archive_on_a_public_host_is_fetched() {
        let (row, artifact) = approved_for("https://registry.example/sqlite.tar.gz");
        assert_eq!(owner_artifact_allowed(&row, &artifact), Ok(()));
        let blob = format!("{ARTIFACT_BLOB_SCHEME}{}", "ab".repeat(32));
        let (row, artifact) = approved_for(&blob);
        assert_eq!(owner_artifact_allowed(&row, &artifact), Ok(()));

        // The owner repoints the approved plugin at another download.
        let (row, _) = approved_for("https://registry.example/sqlite.tar.gz");
        let (_, moved) = approved_for("https://registry.example/other.tar.gz");
        assert!(owner_artifact_allowed(&row, &moved)
            .unwrap_err()
            .contains("not the one"));
        let (row, mut changed) = approved_for("https://registry.example/sqlite.tar.gz");
        changed.archive_sha256 = "ef".repeat(32);
        assert!(owner_artifact_allowed(&row, &changed).is_err());
        // Same URL and archive, but the payload digest names another tree
        // in the version cache, which would be reused without a fetch.
        let (row, mut swapped) = approved_for("https://registry.example/sqlite.tar.gz");
        swapped.payload_root_sha256 = "ef".repeat(32);
        assert!(owner_artifact_allowed(&row, &swapped)
            .unwrap_err()
            .contains("not the one"));
        let unapproved = RemotePluginConsent {
            requested_artifact_url: artifact.url.clone(),
            ..super::tests::row()
        };
        assert!(owner_artifact_allowed(&unapproved, &artifact).is_err());

        // Even an approved URL is refused on a local or private host.
        for url in [
            "https://127.0.0.1/sqlite.tar.gz",
            "https://localhost:8787/sqlite.tar.gz",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/sqlite.tar.gz",
            "https://192.168.0.10/sqlite.tar.gz",
            "http://registry.example/sqlite.tar.gz",
            "file:///etc/passwd",
        ] {
            let (row, artifact) = approved_for(url);
            assert!(owner_artifact_allowed(&row, &artifact).is_err(), "{url}");
        }
    }
}
