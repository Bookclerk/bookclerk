//! What a host node knows of the instances an owner cluster deployed to
//! it, and the bindings a handed job runs with (H-4 design, step 8).
//!
//! [`RemoteCatalog`] is per link and in memory only: [`super::consent`]
//! fills it from `deployments.list` and `instance.config` and re-evaluates
//! each entry's host consent. [`host_job_bindings`] turns one catalog
//! entry plus the secrets the owner handed with a job into what the guest
//! is opened with: `CONFIG`, `SECRETS` (only when the consented grant
//! carries the `secrets` binding) and the job's store credentials. Nothing
//! here is written to disk, and every type holding secret bytes redacts
//! its `Debug` and zeroizes on drop.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use bookclerk_library::control_plane::PluginVersion;
use bookclerk_library::secrets::secret_kind;
use bookclerk_node_client::{HostJobCtx, HostJobError, AWAITING_HOST_CONSENT};
use bookclerk_node_proto::SecretValue;
use bookclerk_plugin_abi::{AccountCredential, ExtensibleConfig};
use bookclerk_plugin_host::{
    binding_grant_flags, prepared_open_from_resolved, PluginGrant, PreparedOpen, RemoteConsentState,
};
use zeroize::Zeroize as _;

/// Error kind for a handed job whose instance this host cannot open.
pub const INSTANCE_UNAVAILABLE: &str = "instance_unavailable";

/// An instance's `CONFIG` and `SECRETS` as the owner served them.
#[derive(Clone)]
pub struct RemoteInstanceConfig {
    /// Instance document revision.
    pub revision: i64,
    /// `CONFIG` (JSON object).
    pub config: serde_json::Value,
    /// `SECRETS` (JSON object bytes), opened with this node's box key;
    /// `None` when the instance has none or the owner withheld them.
    pub secrets: Option<Arc<SecretValue>>,
    /// True when the owner withheld secrets this node is not trusted with.
    pub secrets_withheld: bool,
}

impl std::fmt::Debug for RemoteInstanceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteInstanceConfig")
            .field("revision", &self.revision)
            .field("secrets", &self.secrets.as_ref().map(|_| "<redacted>"))
            .field("secrets_withheld", &self.secrets_withheld)
            .finish_non_exhaustive()
    }
}

/// One instance an owner deployed to this host.
#[derive(Debug, Clone)]
pub struct RemoteInstanceEntry {
    /// Owner plugin instance id.
    pub instance_id: String,
    /// The instance's plugin key.
    pub plugin_key: String,
    /// True for the plugin's default instance (unpinned jobs use it).
    pub default_instance: bool,
    /// This host's consent to run it.
    pub consent: RemoteConsentState,
    /// Materialized plugin tree, once consented and fetched.
    pub root: Option<PathBuf>,
    /// The owner's version as materialized here, its grant the consented
    /// one; set with `root`.
    pub version: Option<PluginVersion>,
    /// The instance's bindings, once consented and fetched.
    pub config: Option<RemoteInstanceConfig>,
    /// Why it cannot run, when it cannot (observation detail).
    pub problem: Option<String>,
}

impl RemoteInstanceEntry {
    /// True when a job may run on this entry: consented, materialized and
    /// its bindings fetched.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self.consent, RemoteConsentState::Consented(_))
            && self.root.is_some()
            && self.version.is_some()
            && self.config.is_some()
            && self.problem.is_none()
    }
}

/// The instances one owner cluster deployed to this host, by instance id.
#[derive(Debug, Default)]
pub struct RemoteCatalog {
    /// Entries by instance id.
    entries: RwLock<BTreeMap<String, RemoteInstanceEntry>>,
}

impl RemoteCatalog {
    /// Reads the entries (recovering from a poisoned lock: each write
    /// replaces whole entries).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, RemoteInstanceEntry>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes the entries.
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, RemoteInstanceEntry>> {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replaces every entry.
    pub fn replace(&self, entries: BTreeMap<String, RemoteInstanceEntry>) {
        *self.write() = entries;
    }

    /// Every entry.
    #[must_use]
    pub fn entries(&self) -> Vec<RemoteInstanceEntry> {
        self.read().values().cloned().collect()
    }

    /// One entry.
    #[must_use]
    pub fn get(&self, instance_id: &str) -> Option<RemoteInstanceEntry> {
        self.read().get(instance_id).cloned()
    }

    /// Sets the consent of every entry of `plugin_key`; an entry that loses
    /// consent drops its fetched bindings. Answers whether any entry
    /// changed from or to consented.
    pub fn set_consent(&self, plugin_key: &str, consent: &RemoteConsentState) -> bool {
        let mut changed = false;
        for entry in self
            .write()
            .values_mut()
            .filter(|entry| entry.plugin_key == plugin_key)
        {
            let was = matches!(entry.consent, RemoteConsentState::Consented(_));
            let now = matches!(consent, RemoteConsentState::Consented(_));
            if was != now || (now && entry.consent != *consent) {
                changed = true;
            }
            entry.consent = consent.clone();
            if !now {
                entry.config = None;
            }
        }
        changed
    }

    /// Plugin keys whose every deployed instance here can run now: the
    /// keys a host client may claim jobs for.
    ///
    /// Per plugin, not per instance, on purpose: the claim filter the owner
    /// applies is a set of plugin keys, and a handed job names its plugins,
    /// not the instance it is pinned to. Admitting a plugin while one of
    /// its instances is not ready would let a job pinned to that instance
    /// be claimed and then fail ([`INSTANCE_UNAVAILABLE`]
    /// spends an attempt). Withholding the plugin instead gives jobs held
    /// on its ready instances back unspent; they are claimed again once
    /// every instance is ready (a consented instance that is not ready is
    /// retried with backoff).
    #[must_use]
    pub fn consented_keys(&self) -> BTreeSet<String> {
        let entries = self.read();
        let mut keys: BTreeSet<String> = entries
            .values()
            .filter(|entry| entry.is_ready())
            .map(|entry| entry.plugin_key.clone())
            .collect();
        keys.retain(|key| {
            entries
                .values()
                .filter(|entry| &entry.plugin_key == key)
                .all(RemoteInstanceEntry::is_ready)
        });
        keys
    }
}

/// What one handed job's guest is opened with.
pub struct HostJobBindings {
    /// Owner plugin instance id.
    pub instance_id: String,
    /// The instance's plugin key.
    pub plugin_key: String,
    /// The consented grant (the owner's grant narrowed by this host); the
    /// guest is spawned under exactly this grant.
    pub grant: PluginGrant,
    /// Materialized plugin tree.
    pub root: PathBuf,
    /// The version materialized at `root`.
    pub version: PluginVersion,
    /// `CONFIG` and `SECRETS` for `PluginWorker.open`.
    pub open: PreparedOpen,
    /// Store credentials for the job's accounts, from the owner's handed
    /// secrets; empty without the `secrets` binding.
    pub credentials: Vec<AccountCredential>,
}

impl std::fmt::Debug for HostJobBindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostJobBindings")
            .field("instance_id", &self.instance_id)
            .field("plugin_key", &self.plugin_key)
            .field("root", &self.root)
            .field("config_revision", &self.open.config_revision)
            .field("secrets", &"<redacted>")
            .field("credentials", &self.credentials.len())
            .finish_non_exhaustive()
    }
}

impl Drop for HostJobBindings {
    fn drop(&mut self) {
        self.open.bindings.secrets.payload.zeroize();
        for credential in &mut self.credentials {
            credential.credentials.zeroize();
        }
    }
}

/// The catalog entry a handed job runs on: its pinned instance, or else
/// the default instance of its source plugin. The source plugin is the one
/// the owner named, and must be one of the job's plugins; without it (an
/// older owner) only a job of exactly one plugin has one. The job's other
/// plugins are destinations it writes and never select the guest.
fn job_entry(catalog: &RemoteCatalog, ctx: &HostJobCtx) -> Option<RemoteInstanceEntry> {
    let pinned = ctx
        .job
        .job
        .payload
        .get("plugin_instance_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty());
    if let Some(instance_id) = pinned {
        return catalog
            .get(instance_id)
            .filter(|entry| ctx.job.plugin_keys.contains(&entry.plugin_key));
    }
    let plugin_key = match ctx.job.source_plugin_key.as_deref() {
        Some(source) => ctx.job.plugin_keys.iter().find(|key| *key == source)?,
        None => {
            let [plugin_key] = ctx.job.plugin_keys.as_slice() else {
                return None;
            };
            plugin_key
        }
    };
    catalog
        .entries()
        .into_iter()
        .find(|entry| &entry.plugin_key == plugin_key && entry.default_instance)
}

/// Builds the bindings a handed job's guest is opened with, failing closed
/// when its instance is not deployed here, not consented, or not fetched.
///
/// # Errors
///
/// Returns [`AWAITING_HOST_CONSENT`] when the instance is not consented and
/// [`INSTANCE_UNAVAILABLE`] when it is unknown, unfetched, or ambiguous.
pub fn host_job_bindings(
    catalog: &RemoteCatalog,
    ctx: &HostJobCtx,
) -> Result<HostJobBindings, HostJobError> {
    let unavailable = |detail: &str| HostJobError::new(INSTANCE_UNAVAILABLE, detail.to_string());
    let entry = job_entry(catalog, ctx)
        .ok_or_else(|| unavailable("the job's plugin instance is not deployed to this host"))?;
    let RemoteConsentState::Consented(grant) = &entry.consent else {
        return Err(HostJobError::new(
            AWAITING_HOST_CONSENT,
            "this host's operator has not approved the plugin",
        ));
    };
    if !entry.is_ready() {
        return Err(unavailable("the plugin instance is not ready on this host"));
    }
    let (Some(root), Some(version), Some(config)) = (
        entry.root.clone(),
        entry.version.clone(),
        entry.config.as_ref(),
    ) else {
        return Err(unavailable("the plugin instance is not ready on this host"));
    };
    let flags = binding_grant_flags(grant);
    let config_payload = if flags.config && config.config.is_object() {
        ExtensibleConfig::json(&config.config)
    } else {
        ExtensibleConfig::json(&serde_json::Value::Object(serde_json::Map::new()))
    };
    let secrets_payload = match (&config.secrets, flags.secrets) {
        (Some(secrets), true) => ExtensibleConfig {
            schema_version: 1,
            media_type: bookclerk_plugin_abi::JSON_MEDIA_TYPE.into(),
            payload: secrets.expose().to_vec(),
        },
        _ => ExtensibleConfig::default(),
    };
    let credentials = if flags.secrets {
        ctx.secrets
            .refs()
            .filter(|secret| {
                secret.kind == secret_kind::SOURCE_AUTH && secret.provider == entry.plugin_key
            })
            .filter_map(|secret| {
                ctx.secrets.get(secret).map(|value| AccountCredential {
                    account_id: secret.account_id.clone(),
                    credentials: value.expose().to_vec(),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(HostJobBindings {
        instance_id: entry.instance_id.clone(),
        plugin_key: entry.plugin_key.clone(),
        grant: (**grant).clone(),
        root,
        version,
        open: prepared_open_from_resolved(config_payload, secrets_payload, config.revision),
        credentials,
    })
}
