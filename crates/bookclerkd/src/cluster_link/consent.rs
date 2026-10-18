//! Host consent for owner plugins, per cluster link (H-4 design, section 5;
//! step 8).
//!
//! [`LinkSupervisor`] keeps one link's [`RemoteCatalog`] current and tells
//! the host client which plugins it may claim jobs for:
//!
//! - on every connection and on each `deployment.changed`,
//!   `install.changed`, `placement.changed` and `configuration.changed`
//!   wake it re-reads `deployments.list`, records what the owner asks to
//!   run (`remote_plugin_consents`, the owner's approved grant as the
//!   request), auto-approves a Bookclerk platform plugin only when this
//!   host's own provenance check says the owner's artifact is the bundled
//!   one, and evaluates consent;
//! - for each consented instance it materializes the version (an owner
//!   `blob:` archive comes over the signed artifact endpoint and is checked
//!   against its digest before it is stored; `https:` archives are fetched
//!   by the materializer itself; `file:` URLs are refused) and reads the
//!   instance's `CONFIG` and `SECRETS` with `instance.config`;
//! - it reports each deployment as `materialized`, or `error` with the
//!   consent state (`awaiting_host_consent`, `awaiting_host_reapproval`,
//!   `invalid_grant`) or the failure as the detail;
//! - every [`CONSENT_POLL`] and whenever [`notify_consent_changed`] is
//!   called (the API's approve and revoke) it re-reads the consent rows, so
//!   an approval or a revoke from the CLI takes effect without a wake.
//!   Losing consent cancels the plugin's held jobs and gives them back.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bookclerk_library::control_plane::{
    list_remote_plugin_consents, load_artifact_blob, put_artifact_blob,
    record_remote_plugin_request, PluginArtifact, PluginVersion, RemotePluginConsent,
    ARTIFACT_BLOB_SCHEME, REMOTE_CONSENT_PLATFORM,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_client::{HostClientHandle, AWAITING_HOST_CONSENT};
use bookclerk_node_proto::{Body, RemoteDeployment, RemoteVersion};
use bookclerk_plugin_host::{
    approve_remote_request, host_artifact, materialize_version, remote_consent_state,
    remote_platform_auto_approval, PluginGrant, RemoteConsentState, RemoteNarrowing,
};
use tokio::sync::{watch, Notify};

use super::remote_bindings::{RemoteCatalog, RemoteInstanceConfig, RemoteInstanceEntry};

/// How often consent rows are re-read without a notification.
pub const CONSENT_POLL: Duration = Duration::from_secs(5);

/// Wakes that make a host re-read what its owner deployed to it.
const REFRESH_WAKES: &[&str] = &[
    "deployment.changed",
    "install.changed",
    "placement.changed",
    "configuration.changed",
];

/// Notified when this daemon's API approves or revokes a consent.
static CONSENT_CHANGED: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Tells every link supervisor in this process to re-read consent now.
pub fn notify_consent_changed() {
    CONSENT_CHANGED.notify_waiters();
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
        let mut wakes = self.handle.subscribe_wakes();
        let mut poll = tokio::time::interval(CONSENT_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stale = true;
        loop {
            if stale && self.handle.is_connected() {
                stale = self.refresh().await.is_err();
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
                }
                wake = wakes.recv() => match wake {
                    Ok(wake) => {
                        let kind = wake.get("type").and_then(serde_json::Value::as_str);
                        if kind.is_some_and(|kind| REFRESH_WAKES.contains(&kind)) {
                            stale = true;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => stale = true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                () = CONSENT_CHANGED.notified() => self.reevaluate().await,
                _ = poll.tick() => {
                    self.reevaluate().await;
                    // A refresh that failed (owner unreachable mid-way) is
                    // retried on the poll.
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
    /// consent is fetched, one that loses it is dropped.
    async fn reevaluate(&self) {
        let rows = match list_remote_plugin_consents(&self.library, Some(self.cluster_id())).await {
            Ok(rows) => rows,
            Err(err) => {
                // Fail closed: nothing stays claimable on an unreadable row.
                tracing::warn!(cluster_id = self.cluster_id(), error = %err, "consent rows unreadable");
                self.handle.set_consented_plugins(BTreeSet::new());
                return;
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
            let _ = self.refresh().await;
        }
    }

    /// Re-reads what the owner deployed here and rebuilds the catalog.
    async fn refresh(&self) -> Result<(), ()> {
        let Some(frame) = self.handle.request(Body::DeploymentsList {}).await else {
            return Err(());
        };
        let Body::Deployments { items } = frame.body else {
            tracing::warn!(
                cluster_id = self.cluster_id(),
                "owner refused deployments.list"
            );
            return Err(());
        };
        let mut entries = BTreeMap::new();
        for item in items {
            let entry = self.evaluate(&item).await;
            self.observe(&item, &entry).await;
            entries.insert(entry.instance_id.clone(), entry);
        }
        self.catalog.replace(entries);
        self.publish();
        Ok(())
    }

    /// One deployment's catalog entry: its consent, and when consented its
    /// materialized tree and bindings.
    async fn evaluate(&self, item: &RemoteDeployment) -> RemoteInstanceEntry {
        let mut entry = RemoteInstanceEntry {
            instance_id: item.instance_id.clone(),
            plugin_key: item.plugin_key.clone(),
            default_instance: item.default_instance,
            consent: RemoteConsentState::Pending,
            root: None,
            config: None,
            problem: None,
        };
        let Some(remote) = item.version.as_ref() else {
            entry.problem = Some("the owner has no active version of this plugin".into());
            return entry;
        };
        if remote.grant_json.trim().is_empty() {
            entry.problem = Some("the owner's operator has not approved this version".into());
            return entry;
        }
        let row = match record_remote_plugin_request(
            &self.library,
            self.cluster_id(),
            &item.plugin_key,
            &remote.version,
            &remote.grant_json,
        )
        .await
        {
            Ok(row) => row,
            Err(err) => {
                entry.consent = RemoteConsentState::Invalid;
                entry.problem = Some(format!("the owner's request was not recorded: {err}"));
                return entry;
            }
        };
        let version = match plugin_version(&item.plugin_key, remote) {
            Ok(version) => version,
            Err(problem) => {
                entry.consent = RemoteConsentState::Invalid;
                entry.problem = Some(problem);
                return entry;
            }
        };
        let row = self.auto_approve_platform(row, &version).await;
        entry.consent = consent_state(&row);
        let RemoteConsentState::Consented(grant) = &entry.consent else {
            return entry;
        };
        match self.materialize(&version, grant).await {
            Ok(root) => entry.root = Some(root),
            Err(problem) => {
                entry.problem = Some(problem);
                return entry;
            }
        }
        match self.instance_config(&item.instance_id).await {
            Ok(config) => entry.config = Some(config),
            Err(problem) => entry.problem = Some(problem),
        }
        entry
    }

    /// Approves an unapproved platform plugin when this host bundles the
    /// owner's artifact; answers the row as it stands afterwards.
    async fn auto_approve_platform(
        &self,
        row: RemotePluginConsent,
        version: &PluginVersion,
    ) -> RemotePluginConsent {
        if row.is_approved() {
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
            Some(&row.requested_version),
            &RemoteNarrowing::default(),
            REMOTE_CONSENT_PLATFORM,
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

    /// Materializes `version` into this host's version cache.
    async fn materialize(
        &self,
        version: &PluginVersion,
        grant: &PluginGrant,
    ) -> Result<PathBuf, String> {
        let artifact = host_artifact(version).map_err(|err| err.to_string())?;
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
        } else if !artifact.url.starts_with("https:") {
            return Err("only blob: and https: archives are fetched from an owner".into());
        }
        let mut local = version.clone();
        local.grant_json = serde_json::to_string(grant).map_err(|err| err.to_string())?;
        materialize_version(&self.library, &self.files_dir, &local)
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

    /// Reports one deployment's state to the owner.
    async fn observe(&self, item: &RemoteDeployment, entry: &RemoteInstanceEntry) {
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
        if !matches!(answer, Some(ref frame) if matches!(frame.body, Body::Accepted {})) {
            tracing::debug!(
                deployment_id = %item.deployment_id,
                "owner did not accept the observation"
            );
        }
    }
}

/// A consent row's state.
pub(crate) fn consent_state(row: &RemotePluginConsent) -> RemoteConsentState {
    remote_consent_state(
        &row.plugin_key,
        &row.requested_grant_json,
        row.approved_request_json.as_deref(),
        row.grant_json.as_deref(),
    )
}

/// The owner's version as this host's materializer reads it.
fn plugin_version(plugin_key: &str, remote: &RemoteVersion) -> Result<PluginVersion, String> {
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
