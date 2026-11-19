//! [`StorageBackend`] adapter over an external output plugin process.
//!
//! Destinations speak Cap'n Proto `api_version = 3` only and are spawned for
//! a plugin instance whose document supplies bucket, prefix, and endpoint.
//! The host never grants the guest filesystem access to acquire scratch or
//! the output library. Credentials travel in `SECRETS`, never in `CONFIG`.

use std::sync::Arc;

use bookclerk_config::{
    Config, DestinationId, DestinationKeyStyle, DestinationNaming, NamingProfile,
};
use bookclerk_storage::StorageBackend;

use crate::discover::DiscoveredPlugin;
use crate::on_demand_storage::{HeldSession, OnDemandStorage};
use crate::protocol::OutputS3ContextDto;
use crate::rpc_session::{PluginSession, PluginStorage};
use crate::Result as PluginResult;

/// How a destination served by `plugin` names keys: file-system paths for
/// the first-party `local` plugin, object-store keys for every other one.
///
/// The destination's id is its plugin instance id, given when the guest is
/// published, so several instances of one plugin are separate destinations.
pub(crate) fn key_style_of(plugin: &DiscoveredPlugin) -> DestinationKeyStyle {
    if crate::is_first_party_local_output(plugin) {
        DestinationKeyStyle::Filesystem
    } else {
        DestinationKeyStyle::ObjectStore
    }
}

/// A storage guest that has been spawned and not yet published.
pub(crate) struct SpawnedDestination {
    /// Guest session.
    pub session: Arc<PluginSession>,
    /// Storage backend over `session`.
    backend: Arc<dyn StorageBackend>,
    /// How the destination names keys.
    key_style: DestinationKeyStyle,
    /// Key layout from the instance document.
    settings: DestinationSettings,
}

/// The parts of a spawned storage guest an on-demand proxy wraps.
pub(crate) struct OnDemandParts {
    /// Guest session.
    pub session: Arc<PluginSession>,
    /// Storage backend over `session`.
    pub backend: Arc<dyn StorageBackend>,
    /// How the destination names keys.
    pub key_style: DestinationKeyStyle,
    /// Key layout from the instance document.
    pub settings: DestinationSettings,
}

impl SpawnedDestination {
    /// Splits a storage guest into the parts an on-demand proxy wraps.
    #[must_use]
    pub(crate) fn on_demand_parts(self) -> OnDemandParts {
        OnDemandParts {
            session: self.session,
            backend: self.backend,
            key_style: self.key_style,
            settings: self.settings,
        }
    }
}

/// One deployed destination as acquire sees it.
#[derive(Clone)]
pub struct DeployedDestination {
    /// The destination's id: its plugin instance id.
    pub id: DestinationId,
    /// How the destination names keys.
    pub key_style: DestinationKeyStyle,
    /// Storage backend over the destination's guest.
    pub backend: Arc<dyn StorageBackend>,
    /// Key layout from the instance document.
    pub settings: DestinationSettings,
}

/// Key layout settings the host reads from a deployed destination's instance
/// document: a storage plugin declares `prefix` and the naming overrides
/// as `[[settings]]`, and acquire needs them to plan storage keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestinationSettings {
    /// Key prefix under the destination root or bucket.
    pub prefix: String,
    /// Naming overrides; unset values inherit global `[output]`.
    pub naming: DestinationNaming,
}

impl DestinationSettings {
    /// Reads the layout settings from a resolved `CONFIG` object.
    #[must_use]
    pub fn from_granted_config(granted: &serde_json::Value) -> Self {
        let text = |key: &str| {
            granted
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Self {
            prefix: text("prefix").unwrap_or_default(),
            naming: DestinationNaming {
                naming_profile: text("naming_profile")
                    .as_deref()
                    .and_then(NamingProfile::parse),
                folder_template: text("folder_template"),
                file_template: text("file_template"),
                chapter_file_template: text("chapter_file_template"),
            },
        }
    }
}

/// The identity of a guest session, kept while an on-demand guest sleeps so
/// session lookups still match it.
struct SessionIdentity {
    /// Owning account of the session.
    account_id: String,
    /// Plugin-instance key the session was spawned under.
    instance_key: String,
    /// Plugin id of the session.
    id: String,
    /// Display alias of the session.
    alias: String,
}

impl SessionIdentity {
    /// Copies the identity of a running session.
    fn of(session: &PluginSession) -> Self {
        Self {
            account_id: session.account_id().to_string(),
            instance_key: session.instance_key().to_string(),
            id: session.id().to_string(),
            alias: session.alias().to_string(),
        }
    }
}

/// How a deployed slot reaches its guest.
#[derive(Clone)]
enum SlotGuest {
    /// A session that runs until the deployment is retired.
    Resident(Arc<PluginSession>),
    /// A proxy that starts the guest on a call and stops it when idle. The
    /// slot holds no session, so a sleeping guest's process can end.
    OnDemand {
        /// The storage proxy.
        proxy: Arc<OnDemandStorage>,
        /// The session identity captured from the first guest.
        identity: Arc<SessionIdentity>,
    },
}

impl SlotGuest {
    /// Owning account of the guest's session.
    fn account_id(&self) -> &str {
        match self {
            Self::Resident(session) => session.account_id(),
            Self::OnDemand { identity, .. } => &identity.account_id,
        }
    }

    /// Plugin-instance key of the guest's session.
    fn instance_key(&self) -> &str {
        match self {
            Self::Resident(session) => session.instance_key(),
            Self::OnDemand { identity, .. } => &identity.instance_key,
        }
    }

    /// True when `plugin_id` names this guest by id or alias.
    fn matches(&self, plugin_id: &str) -> bool {
        match self {
            Self::Resident(session) => {
                crate::identity_matches_occupancy(session.id(), session.alias(), plugin_id)
            }
            Self::OnDemand { identity, .. } => {
                crate::identity_matches_occupancy(&identity.id, &identity.alias, plugin_id)
            }
        }
    }

    /// True when both reach the same guest.
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Resident(a), Self::Resident(b)) => Arc::ptr_eq(a, b),
            (Self::OnDemand { proxy: a, .. }, Self::OnDemand { proxy: b, .. }) => Arc::ptr_eq(a, b),
            (Self::Resident(_), Self::OnDemand { .. })
            | (Self::OnDemand { .. }, Self::Resident(_)) => false,
        }
    }

    /// A handle a caller can lend the guest's session from.
    fn handle(&self) -> DestinationSession {
        match self {
            Self::Resident(session) => DestinationSession::Resident(Arc::clone(session)),
            Self::OnDemand { proxy, .. } => DestinationSession::OnDemand(Arc::clone(proxy)),
        }
    }
}

/// A deployed destination's guest, as handed to a job that calls the
/// guest's entrypoints directly.
#[derive(Clone)]
pub enum DestinationSession {
    /// A guest that is always running.
    Resident(Arc<PluginSession>),
    /// A guest that sleeps while idle; [`Self::lend`] starts it.
    OnDemand(Arc<OnDemandStorage>),
}

impl DestinationSession {
    /// The guest's session, starting the guest when it sleeps; it stays
    /// awake until the returned value is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when an on-demand guest was retired or cannot start.
    pub async fn lend(&self) -> std::result::Result<HeldSession, String> {
        match self {
            Self::Resident(session) => Ok(HeldSession::resident(Arc::clone(session))),
            Self::OnDemand(proxy) => proxy.session().await,
        }
    }
}

/// One reconciler-spawned destination, addressed by plugin instance id.
#[derive(Clone)]
struct DeployedSlot {
    /// How the slot reaches its guest.
    guest: SlotGuest,
    /// Storage backend over the guest.
    backend: Arc<dyn StorageBackend>,
    /// The destination's id: the plugin instance id the slot is keyed by.
    id: DestinationId,
    /// How the destination names keys.
    key_style: DestinationKeyStyle,
    /// Key layout from the instance document.
    settings: DestinationSettings,
}

/// Long-lived external output plugins loaded at host startup.
#[derive(Default, Clone)]
pub struct DestinationRegistry {
    /// Sessions keyed by plugin instance id.
    ///
    /// Two instances of one plugin key each keep their own guest. Reload
    /// copies these slots.
    deployed: std::collections::HashMap<String, DeployedSlot>,
}

impl DestinationRegistry {
    /// Every deployed destination, one per plugin instance: destinations
    /// that name files as paths first, then the rest, each group by id.
    ///
    /// Several instances of one plugin are separate destinations.
    #[must_use]
    pub fn destinations(&self) -> Vec<DeployedDestination> {
        let mut out: Vec<DeployedDestination> = self
            .deployed
            .values()
            .map(|slot| DeployedDestination {
                id: slot.id.clone(),
                key_style: slot.key_style,
                backend: Arc::clone(&slot.backend),
                settings: slot.settings.clone(),
            })
            .collect();
        out.sort_by(|a, b| {
            b.key_style
                .is_filesystem()
                .cmp(&a.key_style.is_filesystem())
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// Plugin session for `plugin_id` and `account_id`, when that guest was loaded.
    ///
    /// `plugin_id` may be the canonical PluginKey (how sessions are stored) or
    /// a display alias. Aliases are installation-unique; two occupants sharing
    /// an alias is invalid state and fails closed rather than returning a twin.
    #[must_use]
    pub fn plugin_session(&self, plugin_id: &str, account_id: &str) -> Option<DestinationSession> {
        self.require_plugin_session(plugin_id, account_id).ok()
    }

    /// [`Self::plugin_session`] that reports same-key ambiguity.
    ///
    /// # Errors
    ///
    /// Returns an error when no guest is loaded, or when more than one session
    /// matches `plugin_id`.
    pub fn require_plugin_session(
        &self,
        plugin_id: &str,
        account_id: &str,
    ) -> std::result::Result<DestinationSession, String> {
        if let Some(slot) = self.deployed.get(plugin_id) {
            if slot.guest.account_id() == account_id {
                return Ok(slot.guest.handle());
            }
        }
        let mut hits: Vec<&SlotGuest> = Vec::new();
        for slot in self.deployed.values() {
            let guest = &slot.guest;
            if hits.iter().any(|existing| existing.same(guest)) {
                continue;
            }
            let exact = guest.instance_key() == crate::plugin_instance_key(plugin_id, account_id);
            let alias = guest.account_id() == account_id && guest.matches(plugin_id);
            if exact || alias {
                hits.push(guest);
            }
        }
        match hits.len() {
            1 => Ok(hits[0].handle()),
            0 => Err(format!(
                "no plugin session for plugin `{plugin_id}` (guest not loaded)"
            )),
            count => Err(format!(
                "plugin `{plugin_id}` matches {count} destination sessions; pass a plugin instance id. candidates: {}",
                hits.iter()
                    .map(|guest| guest.instance_key())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Records a reconciler-spawned guest under `plugin_instance_id`, which
    /// is also the destination's id.
    ///
    /// # Errors
    ///
    /// Returns an error when `plugin_instance_id` is not a valid
    /// destination id; nothing is recorded then.
    fn note_deployed_instance(
        &mut self,
        plugin_instance_id: &str,
        guest: SlotGuest,
        backend: Arc<dyn StorageBackend>,
        key_style: DestinationKeyStyle,
        settings: DestinationSettings,
    ) -> std::result::Result<(), String> {
        let id = DestinationId::new(plugin_instance_id)?;
        self.deployed.insert(
            plugin_instance_id.to_string(),
            DeployedSlot {
                guest,
                backend,
                id,
                key_style,
                settings,
            },
        );
        Ok(())
    }

    /// Drops the deployed slot for `plugin_instance_id`.
    ///
    /// The session is dropped with the slot. Callers that still track the
    /// guest separately must drop that handle too, or the process stays up.
    pub(crate) fn remove_deployed(&mut self, plugin_instance_id: &str) {
        self.deployed.remove(plugin_instance_id);
    }

    /// Records `spawned` as the destination `plugin_instance_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when `plugin_instance_id` is not a valid
    /// destination id. The session is dropped then, so the process exits,
    /// and the error becomes the observation.
    pub(crate) fn publish_spawned(
        &mut self,
        plugin_instance_id: &str,
        spawned: SpawnedDestination,
    ) -> std::result::Result<Arc<PluginSession>, String> {
        self.note_deployed_instance(
            plugin_instance_id,
            SlotGuest::Resident(Arc::clone(&spawned.session)),
            spawned.backend,
            spawned.key_style,
            spawned.settings,
        )?;
        Ok(spawned.session)
    }

    /// Records an on-demand storage proxy as the destination
    /// `plugin_instance_id`.
    ///
    /// `first` is the guest the deployment started; its session identity is
    /// kept for session lookups while the guest sleeps, and the slot keeps
    /// no reference to the session itself.
    ///
    /// # Errors
    ///
    /// Returns an error when `plugin_instance_id` is not a valid
    /// destination id.
    pub(crate) fn publish_on_demand(
        &mut self,
        plugin_instance_id: &str,
        first: &PluginSession,
        proxy: Arc<OnDemandStorage>,
        key_style: DestinationKeyStyle,
        settings: DestinationSettings,
    ) -> std::result::Result<(), String> {
        let backend: Arc<dyn StorageBackend> = Arc::clone(&proxy) as Arc<dyn StorageBackend>;
        self.note_deployed_instance(
            plugin_instance_id,
            SlotGuest::OnDemand {
                proxy,
                identity: Arc::new(SessionIdentity::of(first)),
            },
            backend,
            key_style,
            settings,
        )
    }

    /// Copies deployed sessions from this registry onto `candidate`.
    ///
    /// `deployed_instances` is the set of present plugin instance ids. Reload
    /// builds `candidate` without those plugin keys. Only sessions the
    /// reconciler bound to one of those ids are copied, including the
    /// backend they own. A transitional session for the same key is left
    /// behind.
    pub fn reattach_owned(
        &self,
        candidate: &mut Self,
        deployed_instances: &std::collections::BTreeSet<String>,
    ) {
        for (instance_id, slot) in &self.deployed {
            if !deployed_instances.contains(instance_id) {
                continue;
            }
            candidate.deployed.insert(instance_id.clone(), slot.clone());
        }
    }
}

/// Spawns one deployed storage plugin without touching the destination registry.
///
/// The caller publishes the result under the registry write lock. Spawn and
/// `open` stay outside that lock so acquire readers are not blocked for the
/// whole jail start.
///
/// `prepared` is the deployment's resolved bindings. This function does not
/// look up an instance by plugin key.
///
/// # Errors
///
/// Returns an error when the guest cannot start or `open` fails.
pub(crate) async fn spawn_deployed_storage(
    plugin: &DiscoveredPlugin,
    config: &Config,
    store: Option<&bookclerk_library::LibraryStore>,
    prepared: crate::instance_bindings::PreparedOpen,
) -> PluginResult<SpawnedDestination> {
    let settings = DestinationSettings::from_granted_config(&prepared.granted_config);
    let key_style = key_style_of(plugin);
    // First-party identity is the PluginKey plus verified provenance, never
    // the manifest alias: an `s3` or `local` alias twin spawns as a generic
    // storage guest with no implied endpoint grant or typed context.
    if crate::is_first_party_s3_output(plugin) {
        let (storage_backend, session) = spawn_s3_from_instance(plugin, config, prepared).await?;
        return Ok(SpawnedDestination {
            session,
            backend: Arc::new(storage_backend),
            key_style,
            settings,
        });
    }
    if crate::is_first_party_local_output(plugin) {
        let (storage, session) =
            super::destination_local::spawn_local_prepared(plugin, config, prepared).await?;
        return Ok(SpawnedDestination {
            session,
            backend: Arc::new(storage),
            key_style,
            settings,
        });
    }
    let session = Arc::new(
        PluginSession::spawn_with(
            plugin,
            config,
            prepared.spawn_config_table,
            crate::OPERATOR_ACCOUNT,
            &[],
            crate::SessionServices::from_outbox(store),
        )
        .await?,
    );
    session.open(prepared.bindings).await?;
    Ok(SpawnedDestination {
        backend: Arc::new(PluginStorage::new(Arc::clone(&session))),
        session,
        key_style,
        settings,
    })
}

/// Spawns a deployed S3 guest from the instance document.
///
/// Bucket, region, prefix, endpoint, `force_path_style` and
/// `notification_queue_url` come from the document. Credentials are the instance's `access_key_id` /
/// `secret_access_key` / `session_token` secrets, delivered in `SECRETS`;
/// without them the guest falls back to the AWS SDK default provider chain
/// inside its jail. Nothing is written into `CONFIG` or the spawn config
/// table.
async fn spawn_s3_from_instance(
    plugin: &DiscoveredPlugin,
    config: &Config,
    prepared: crate::instance_bindings::PreparedOpen,
) -> PluginResult<(PluginStorage, Arc<PluginSession>)> {
    let context = instance_s3_context(&prepared)?;
    let secrets = s3_open_secrets(&prepared);
    let services = crate::SessionServices {
        deployed_endpoint: s3_deployed_endpoint(plugin, &context),
        ..crate::SessionServices::default()
    };
    let session = Arc::new(
        PluginSession::spawn_with(
            plugin,
            config,
            context.clone(),
            crate::OPERATOR_ACCOUNT,
            &[],
            services,
        )
        .await?,
    );
    session
        .open(bookclerk_plugin_sdk::BindingValues {
            config: bookclerk_plugin_sdk::ExtensibleConfig::json(&context),
            secrets,
            ..bookclerk_plugin_sdk::BindingValues::default()
        })
        .await?;
    Ok((PluginStorage::new(Arc::clone(&session)), session))
}

/// The host-implied endpoint grant for an S3 spawn: the document's
/// `endpoint` for the verified first-party S3 package only
/// ([`crate::is_first_party_s3_output`]). An alias twin gets `None`.
pub(crate) fn s3_deployed_endpoint(
    plugin: &DiscoveredPlugin,
    context: &serde_json::Value,
) -> Option<Option<String>> {
    if !crate::is_first_party_s3_output(plugin) {
        return None;
    }
    Some(
        context
            .get("endpoint")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .map(str::to_string),
    )
}

/// Bucket and region from the instance document, with no credentials.
pub(crate) fn instance_s3_context(
    prepared: &crate::instance_bindings::PreparedOpen,
) -> PluginResult<serde_json::Value> {
    let body = &prepared.granted_config;
    let bucket = json_text(body, &["bucket"]);
    let region = json_text(body, &["region"]);
    if bucket.is_empty() || region.is_empty() {
        return Err(crate::PluginError::message(
            "s3 instance config is missing bucket or region; set them on the plugin instance",
        ));
    }
    let endpoint = {
        let text = json_text(body, &["endpoint"]);
        (!text.is_empty()).then_some(text)
    };
    let ctx = OutputS3ContextDto {
        plugin_data_dir: String::new(),
        bucket,
        prefix: json_text(body, &["prefix"]),
        region,
        endpoint,
        force_path_style: json_bool(body, &["forcePathStyle", "force_path_style"]),
        notification_queue_url: {
            let text = json_text(body, &["notification_queue_url"]);
            (!text.is_empty()).then_some(text)
        },
        credentials: None,
    };
    serde_json::to_value(&ctx).map_err(|err| crate::PluginError::message(err.to_string()))
}

/// `SECRETS` for one deployed S3 open: the instance's sealed credentials,
/// or an empty payload when the document names none.
#[must_use]
pub(crate) fn s3_open_secrets(
    prepared: &crate::instance_bindings::PreparedOpen,
) -> bookclerk_plugin_sdk::ExtensibleConfig {
    if credentials_from_secrets(&prepared.bindings.secrets).is_some() {
        return prepared.bindings.secrets.clone();
    }
    bookclerk_plugin_sdk::ExtensibleConfig::default()
}

/// Builds an [`OutputS3ContextDto`] credentials object from instance secrets.
fn credentials_from_secrets(
    secrets: &bookclerk_plugin_sdk::ExtensibleConfig,
) -> Option<serde_json::Value> {
    let value = secrets.json_value().ok()?;
    let access = json_text(&value, &["accessKeyId", "access_key_id"]);
    let secret = json_text(&value, &["secretAccessKey", "secret_access_key"]);
    if access.is_empty() || secret.is_empty() {
        return None;
    }
    let mut credentials = serde_json::json!({
        "accessKeyId": access,
        "secretAccessKey": secret,
    });
    let token = json_text(&value, &["sessionToken", "session_token"]);
    if !token.is_empty() {
        if let Some(object) = credentials.as_object_mut() {
            object.insert("sessionToken".into(), serde_json::Value::String(token));
        }
    }
    Some(credentials)
}

/// Bool among `keys`, accepting JSON bools and `true`/`false` strings.
fn json_bool(value: &serde_json::Value, keys: &[&str]) -> bool {
    for key in keys {
        match value.get(*key) {
            Some(serde_json::Value::Bool(flag)) => return *flag,
            Some(serde_json::Value::String(text)) => {
                if text.eq_ignore_ascii_case("true") {
                    return true;
                }
                if text.eq_ignore_ascii_case("false") {
                    return false;
                }
            }
            _ => {}
        }
    }
    false
}

/// First non-empty string among `keys`.
fn json_text(value: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(serde_json::Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::s3_deployed_endpoint;
    use crate::discover::DiscoveredPlugin;
    use crate::protocol::{OutputS3ContextDto, S3CredentialsDto};
    use bookclerk_plugin_catalog::{PluginKey, PluginProvenance};
    use bookclerk_plugin_manifest::PluginManifest;

    const S3_TOML: &str = r#"
api_version = 3
id = "s3"
version = "0.1.0"
runtime = "native"
command = "./guest"
entrypoints = ["storage"]

[capabilities.network]
mode = "outbound"

[vars]
"#;

    fn discovered(root: &std::path::Path) -> DiscoveredPlugin {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("plugin.toml"), S3_TOML).unwrap();
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").unwrap();
        let manifest = PluginManifest::parse(S3_TOML).unwrap();
        DiscoveredPlugin::try_new(manifest, root.to_path_buf(), command, None)
            .expect("test plugin tree must evaluate")
    }

    /// Defense in depth: the document endpoint is a host-implied TCP grant,
    /// so only the verified first-party S3 PluginKey may carry it. An alias
    /// twin (same manifest id, different key) gets nothing from the document.
    #[test]
    fn document_endpoint_is_granted_by_plugin_key_not_alias() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let mut real = discovered(a.path());
        let key = PluginKey::platform("bookclerk-plugin-destination-s3", "s3").unwrap();
        real.identity.plugin_key = key.clone();
        real.identity.artifact.plugin_key = key;
        real.identity.provenance = PluginProvenance::VerifiedStaged;
        let twin = discovered(b.path());
        assert_eq!(real.alias(), twin.alias());
        assert_ne!(real.plugin_key(), twin.plugin_key());
        let context = serde_json::json!({ "endpoint": "http://127.0.0.1:9000" });

        assert_eq!(
            s3_deployed_endpoint(&real, &context),
            Some(Some("http://127.0.0.1:9000".to_string()))
        );
        assert_eq!(
            s3_deployed_endpoint(&real, &serde_json::json!({})),
            Some(None),
            "a first-party instance with no endpoint grants nothing"
        );
        assert_eq!(
            s3_deployed_endpoint(&twin, &context),
            None,
            "an alias twin must not inherit the document endpoint grant"
        );
    }

    #[test]
    fn s3_destination_json_omits_paths_and_secrets() {
        let ctx = OutputS3ContextDto {
            plugin_data_dir: String::new(),
            bucket: "library".into(),
            prefix: "audiobooks".into(),
            region: "us-east-1".into(),
            endpoint: None,
            force_path_style: false,
            notification_queue_url: None,
            credentials: None,
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(
            !json.contains("pluginDataDir") && !json.contains("plugin_data_dir"),
            "{json}"
        );
        assert!(
            !json.contains("accessKeyId")
                && !json.contains("secretAccessKey")
                && !json.contains("AKIA"),
            "{json}"
        );
        assert!(!json.contains('/'), "{json}");

        let leaked = OutputS3ContextDto {
            plugin_data_dir: "/host/plugins/s3/data".into(),
            credentials: Some(S3CredentialsDto {
                access_key_id: ["AKIA", "SECRET"].concat(),
                secret_access_key: ["wJal", "r"].concat(),
                session_token: None,
            }),
            ..ctx
        };
        let leaked_json = serde_json::to_string(&leaked).unwrap();
        assert!(leaked_json.contains("pluginDataDir"), "{leaked_json}");
        assert!(leaked_json.contains("AKIASECRET"), "{leaked_json}");
    }

    fn prepared(config: serde_json::Value, secrets: serde_json::Value) -> crate::PreparedOpen {
        crate::PreparedOpen {
            bindings: bookclerk_plugin_sdk::BindingValues {
                config: bookclerk_plugin_sdk::ExtensibleConfig::json(&config),
                secrets: bookclerk_plugin_sdk::ExtensibleConfig::json(&secrets),
                ..bookclerk_plugin_sdk::BindingValues::default()
            },
            spawn_config_table: config.clone(),
            granted_config: config,
            config_revision: 1,
        }
    }

    #[test]
    fn instance_s3_context_requires_bucket_and_keeps_credentials_out() {
        let missing = prepared(
            serde_json::json!({"region": "us-east-1"}),
            serde_json::json!({}),
        );
        let err = super::instance_s3_context(&missing).expect_err("bucket");
        assert!(err.to_string().contains("bucket"), "{err}");

        let document = prepared(
            serde_json::json!({
                "bucket": "library",
                "region": "us-east-1",
                "force_path_style": true,
                "credentials": {"accessKeyId": "AKIASECRET", "secretAccessKey": "wJal"}
            }),
            serde_json::json!({}),
        );
        let context = super::instance_s3_context(&document).expect("context");
        assert_eq!(context["bucket"], "library");
        assert_eq!(context["forcePathStyle"], true);
        assert!(context.get("notificationQueueUrl").is_none());
        let queued = prepared(
            serde_json::json!({
                "bucket": "library",
                "region": "us-east-1",
                "notification_queue_url": "https://sqs.us-east-1.amazonaws.com/123456789012/q"
            }),
            serde_json::json!({}),
        );
        let queued_context = super::instance_s3_context(&queued).expect("queued context");
        assert_eq!(
            queued_context["notificationQueueUrl"],
            "https://sqs.us-east-1.amazonaws.com/123456789012/q"
        );
        let camel = prepared(
            serde_json::json!({
                "bucket": "library",
                "region": "us-east-1",
                "forcePathStyle": "true"
            }),
            serde_json::json!({"accessKeyId": "AKIATEST", "secretAccessKey": "secret"}),
        );
        let camel_context = super::instance_s3_context(&camel).expect("camel");
        assert_eq!(camel_context["forcePathStyle"], true);
        assert!(camel_context.get("credentials").is_none());
        let kept = super::s3_open_secrets(&camel);
        assert_eq!(kept.json_value().expect("json")["accessKeyId"], "AKIATEST");
        assert!(context.get("credentials").is_none(), "{context}");
        assert!(!context.to_string().contains("AKIASECRET"), "{context}");
    }

    #[test]
    fn s3_open_secrets_are_the_instance_secrets_or_nothing() {
        let without = prepared(
            serde_json::json!({"bucket": "library", "region": "us-east-1"}),
            serde_json::json!({}),
        );
        let empty = super::s3_open_secrets(&without);
        assert!(
            empty
                .json_value()
                .expect("json")
                .as_object()
                .is_none_or(|o| o.is_empty()),
            "no instance credentials means an empty SECRETS payload"
        );
        assert!(super::instance_s3_context(&without)
            .expect("context")
            .get("credentials")
            .is_none());

        let with = prepared(
            serde_json::json!({"bucket": "library", "region": "us-east-1"}),
            serde_json::json!({
                "access_key_id": "AKIATEST",
                "secret_access_key": "secret",
                "session_token": "tok"
            }),
        );
        let secrets = super::s3_open_secrets(&with);
        let value = secrets.json_value().expect("json");
        assert_eq!(value["access_key_id"], "AKIATEST");
        let credentials = super::credentials_from_secrets(&with.bindings.secrets).expect("creds");
        assert_eq!(credentials["accessKeyId"], "AKIATEST");
        assert_eq!(credentials["sessionToken"], "tok");
    }
}

#[cfg(test)]
mod registry_tests {
    //! Every deployed storage guest is a destination named by its plugin
    //! instance: ids, order, key styles and several instances of one plugin.

    use std::sync::Arc;
    use std::time::Duration;

    use bookclerk_config::DestinationKeyStyle;

    use super::{
        key_style_of, DestinationRegistry, DestinationSettings, SessionIdentity, SlotGuest,
    };
    use crate::discover::DiscoveredPlugin;
    use bookclerk_plugin_catalog::{PluginKey, PluginProvenance};
    use bookclerk_plugin_manifest::PluginManifest;

    /// Records an on-demand slot of plugin `plugin` under `instance`,
    /// storing into `dir`.
    fn deploy(
        registry: &mut DestinationRegistry,
        instance: &str,
        plugin: &str,
        key_style: DestinationKeyStyle,
        dir: &tempfile::TempDir,
    ) -> std::result::Result<(), String> {
        let (proxy, _first, _started) =
            crate::on_demand_storage::tests::proxy(dir, Duration::from_secs(60));
        let backend: Arc<dyn bookclerk_storage::StorageBackend> = proxy.clone();
        registry.note_deployed_instance(
            instance,
            SlotGuest::OnDemand {
                proxy,
                identity: Arc::new(SessionIdentity {
                    account_id: crate::OPERATOR_ACCOUNT.into(),
                    instance_key: instance.into(),
                    id: plugin.into(),
                    alias: plugin.into(),
                }),
            },
            backend,
            key_style,
            DestinationSettings {
                prefix: format!("{instance}/"),
                ..DestinationSettings::default()
            },
        )
    }

    #[tokio::test]
    async fn every_storage_instance_is_a_destination_paths_first() {
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let mut registry = DestinationRegistry::default();
        assert!(registry.destinations().is_empty());
        deploy(
            &mut registry,
            "a-r2",
            "r2",
            DestinationKeyStyle::ObjectStore,
            &dirs[0],
        )
        .unwrap();
        deploy(
            &mut registry,
            "z-local",
            "local",
            DestinationKeyStyle::Filesystem,
            &dirs[1],
        )
        .unwrap();
        deploy(
            &mut registry,
            "b-r2",
            "r2",
            DestinationKeyStyle::ObjectStore,
            &dirs[2],
        )
        .unwrap();

        let deployed = registry.destinations();
        let ids: Vec<&str> = deployed.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            ["z-local", "a-r2", "b-r2"],
            "path destinations first, then by instance id; two r2 instances are two destinations"
        );
        assert_eq!(deployed[0].key_style, DestinationKeyStyle::Filesystem);
        assert_eq!(deployed[2].key_style, DestinationKeyStyle::ObjectStore);
        assert_eq!(deployed[2].settings.prefix, "b-r2/");

        // A third-party destination stores through its backend like any other.
        deployed[2]
            .backend
            .put(
                "a/b.m4b",
                bytes::Bytes::from_static(b"audio"),
                bookclerk_storage::ObjectMeta::default(),
            )
            .await
            .unwrap();
        assert!(dirs[2].path().join("a/b.m4b").exists());

        registry.remove_deployed("a-r2");
        let ids: Vec<String> = registry
            .destinations()
            .iter()
            .map(|d| d.id.to_string())
            .collect();
        assert_eq!(ids, ["z-local", "b-r2"]);
    }

    #[tokio::test]
    async fn an_instance_id_that_is_not_a_destination_id_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = DestinationRegistry::default();
        let err = deploy(
            &mut registry,
            "Not/An/Id",
            "r2",
            DestinationKeyStyle::ObjectStore,
            &dir,
        )
        .unwrap_err();
        assert!(err.contains("invalid destination id"), "{err}");
        assert!(registry.destinations().is_empty());
    }

    #[tokio::test]
    async fn acquire_and_listing_use_every_deployed_destination() {
        let config = bookclerk_config::Config::default();
        let empty = DestinationRegistry::default();
        let Err(err) =
            crate::build_acquire_destinations(&config, &empty, &std::collections::BTreeMap::new())
        else {
            panic!("no destination must refuse an acquire");
        };
        assert!(err.to_string().contains("no output destination"), "{err}");
        let Err(err) = crate::build_storage_backend(&empty) else {
            panic!("no destination must refuse listing");
        };
        assert!(err.to_string().contains("no output destination"), "{err}");

        let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
        let mut registry = DestinationRegistry::default();
        deploy(
            &mut registry,
            "a-r2",
            "r2",
            DestinationKeyStyle::ObjectStore,
            &dirs[0],
        )
        .unwrap();
        let single = crate::build_storage_backend(&registry).unwrap();
        single
            .put(
                "one.m4b",
                bytes::Bytes::from_static(b"audio"),
                bookclerk_storage::ObjectMeta::default(),
            )
            .await
            .unwrap();
        assert!(dirs[0].path().join("one.m4b").exists());

        deploy(
            &mut registry,
            "z-local",
            "local",
            DestinationKeyStyle::Filesystem,
            &dirs[1],
        )
        .unwrap();
        let acquire = crate::build_acquire_destinations(
            &config,
            &registry,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            acquire.primary.as_str(),
            "z-local",
            "path destinations lead"
        );
        let ids: Vec<&str> = acquire.items.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["z-local", "a-r2"]);
        assert!(crate::build_storage_backend(&registry).is_ok());
    }

    /// A storage plugin's manifest under `root`, with `id`.
    fn discovered(root: &std::path::Path, id: &str) -> DiscoveredPlugin {
        let toml = format!(
            "api_version = 3\nid = \"{id}\"\nversion = \"0.1.0\"\nruntime = \"native\"\n\
             command = \"./guest\"\nentrypoints = [\"storage\"]\n\n\
             [capabilities.network]\nmode = \"outbound\"\n\n[vars]\n"
        );
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("plugin.toml"), &toml).unwrap();
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").unwrap();
        let manifest = PluginManifest::parse(&toml).unwrap();
        DiscoveredPlugin::try_new(manifest, root.to_path_buf(), command, None)
            .expect("test plugin tree must evaluate")
    }

    #[test]
    fn only_the_first_party_local_plugin_names_files_as_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut local = discovered(&dir.path().join("a"), "local");
        let key = PluginKey::platform("bookclerk-plugin-destination-local", "local").unwrap();
        local.identity.plugin_key = key.clone();
        local.identity.artifact.plugin_key = key;
        local.identity.provenance = PluginProvenance::VerifiedStaged;
        assert_eq!(key_style_of(&local), DestinationKeyStyle::Filesystem);

        // An alias twin of `local` stores object keys: it is not the
        // first-party package.
        let twin = discovered(&dir.path().join("b"), "local");
        assert_eq!(key_style_of(&twin), DestinationKeyStyle::ObjectStore);

        let r2 = discovered(&dir.path().join("c"), "r2");
        assert_eq!(key_style_of(&r2), DestinationKeyStyle::ObjectStore);
    }
}
