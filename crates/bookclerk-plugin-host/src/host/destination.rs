//! [`StorageBackend`] adapter over an external output plugin process.
//!
//! Destinations speak Cap'n Proto `api_version = 3` only. The host never grants
//! the guest filesystem access to acquire scratch or the output library.
//! Credentials are injected as spawn env when the `secrets` binding is granted.

use std::sync::Arc;

use bookclerk_config::{normalize_storage_prefix, Config};
use bookclerk_plugin_sdk::{BindingValues, PRODUCT_API_VERSION};
use bookclerk_storage::{load_s3_credentials, S3Credentials, StorageBackend, StorageError};
use sea_orm::DatabaseConnection;
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::protocol::OutputS3ContextDto;
use crate::rpc_session::{PluginSession, PluginStorage};
use crate::Result as PluginResult;

/// Manifest id of the platform S3 output plugin (`s3`).
const S3_PLUGIN_ID: &str = "s3";
/// Manifest id of the platform local-filesystem destination guest.
const LOCAL_PLUGIN_ID: &str = "local";

/// Which single-slot backend a deployed destination owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeployedKind {
    /// Platform S3 output.
    S3,
    /// Platform local-filesystem output.
    Local,
    /// Another storage guest. It has a session and no `s3()` / `local()` slot.
    Other,
}

/// A storage guest that has been spawned and not yet published.
pub(crate) struct SpawnedDestination {
    /// Guest session.
    pub session: Arc<PluginSession>,
    /// Storage backend when this guest is S3 or local.
    backend: Option<Arc<dyn StorageBackend>>,
    /// Which acquire slot this backend fills.
    kind: DeployedKind,
}

/// One reconciler-spawned destination, addressed by plugin instance id.
#[derive(Clone)]
struct DeployedSlot {
    /// Guest session.
    session: Arc<PluginSession>,
    /// Storage backend when this guest is S3 or local.
    backend: Option<Arc<dyn StorageBackend>>,
    /// Which acquire slot this backend fills.
    kind: DeployedKind,
}

/// Long-lived external output plugins loaded at host startup.
#[derive(Default, Clone)]
pub struct DestinationRegistry {
    /// Transitional S3 backend. A deployed instance lives in [`Self::deployed`].
    s3: Option<Arc<dyn StorageBackend>>,
    /// Transitional local backend. A deployed instance lives in [`Self::deployed`].
    local: Option<Arc<dyn StorageBackend>>,
    /// Transitional plugin sessions keyed by `(plugin_id, account_id)`.
    plugin_sessions: std::collections::HashMap<String, Arc<PluginSession>>,
    /// Reconciler sessions keyed by plugin instance id.
    ///
    /// Two instances of one plugin key each keep their own guest. Reload copies
    /// these slots and leaves transitional sessions behind.
    deployed: std::collections::HashMap<String, DeployedSlot>,
}

impl DestinationRegistry {
    /// External S3 output backend, when exactly one is loaded.
    ///
    /// Two deployed S3 instances return `None`. Acquire uses
    /// [`Self::require_s3`] so that case fails closed instead of falling
    /// through to `[output.s3]`.
    #[must_use]
    pub fn s3(&self) -> Option<Arc<dyn StorageBackend>> {
        self.require_s3().ok().flatten()
    }

    /// The S3 backend, or an error when more than one deployed instance owns it.
    ///
    /// # Errors
    ///
    /// Returns an error when two deployed S3 instances would otherwise overwrite
    /// one slot.
    pub fn require_s3(&self) -> std::result::Result<Option<Arc<dyn StorageBackend>>, String> {
        self.require_kind(DeployedKind::S3, &self.s3, "s3")
    }

    /// External local-filesystem output backend, when exactly one is loaded.
    #[must_use]
    pub fn local(&self) -> Option<Arc<dyn StorageBackend>> {
        self.require_local().ok().flatten()
    }

    /// The local backend, or an error when more than one deployed instance owns it.
    ///
    /// # Errors
    ///
    /// Returns an error when two deployed local instances would otherwise
    /// overwrite one slot.
    pub fn require_local(&self) -> std::result::Result<Option<Arc<dyn StorageBackend>>, String> {
        self.require_kind(DeployedKind::Local, &self.local, "local")
    }

    /// One deployed backend of `kind`, else the transitional slot.
    fn require_kind(
        &self,
        kind: DeployedKind,
        transitional: &Option<Arc<dyn StorageBackend>>,
        label: &str,
    ) -> std::result::Result<Option<Arc<dyn StorageBackend>>, String> {
        let deployed: Vec<_> = self
            .deployed
            .values()
            .filter(|slot| slot.kind == kind)
            .filter_map(|slot| slot.backend.clone())
            .collect();
        match deployed.len() {
            0 => Ok(transitional.clone()),
            1 => Ok(deployed.into_iter().next()),
            count => Err(format!(
                "{count} {label} destination instances are deployed; refusing to pick one"
            )),
        }
    }

    /// Plugin session for `plugin_id` and `account_id`, when that guest was loaded.
    ///
    /// `plugin_id` may be the canonical PluginKey (how sessions are stored) or
    /// a display alias. Aliases are installation-unique; two occupants sharing
    /// an alias is invalid state and fails closed rather than returning a twin.
    #[must_use]
    pub fn plugin_session(&self, plugin_id: &str, account_id: &str) -> Option<Arc<PluginSession>> {
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
    ) -> std::result::Result<Arc<PluginSession>, String> {
        if let Some(slot) = self.deployed.get(plugin_id) {
            if slot.session.account_id() == account_id {
                return Ok(Arc::clone(&slot.session));
            }
        }
        let mut hits: Vec<Arc<PluginSession>> = Vec::new();
        let mut push = |session: &Arc<PluginSession>| {
            if hits.iter().any(|existing| Arc::ptr_eq(existing, session)) {
                return;
            }
            let exact = session.instance_key() == crate::plugin_instance_key(plugin_id, account_id);
            let alias = session.account_id() == account_id
                && crate::identity_matches_occupancy(session.id(), session.alias(), plugin_id);
            if exact || alias {
                hits.push(Arc::clone(session));
            }
        };
        for slot in self.deployed.values() {
            push(&slot.session);
        }
        for session in self.plugin_sessions.values() {
            push(session);
        }
        match hits.len() {
            1 => Ok(hits.remove(0)),
            0 => Err(format!(
                "no plugin session for plugin `{plugin_id}` (guest not loaded)"
            )),
            count => Err(format!(
                "plugin `{plugin_id}` matches {count} destination sessions; pass a plugin instance id. candidates: {}",
                hits.iter()
                    .map(|session| session.instance_key())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Records the local-filesystem output backend after a successful spawn.
    pub(crate) fn set_local(&mut self, dest: Arc<dyn StorageBackend>) {
        self.local = Some(dest);
    }

    /// Records a plugin session used for `JobRunner.job` invocations.
    pub(crate) fn set_plugin_session(&mut self, session: Arc<PluginSession>) {
        self.plugin_sessions
            .insert(session.instance_key().to_string(), session);
    }

    /// Records a reconciler-spawned guest under `plugin_instance_id`.
    ///
    /// A second instance of the same plugin key does not replace this slot.
    fn note_deployed_instance(
        &mut self,
        plugin_instance_id: &str,
        session: Arc<PluginSession>,
        backend: Option<Arc<dyn StorageBackend>>,
        kind: DeployedKind,
    ) {
        if plugin_instance_id.is_empty() {
            return;
        }
        self.deployed.insert(
            plugin_instance_id.to_string(),
            DeployedSlot {
                session,
                backend,
                kind,
            },
        );
    }

    /// Drops the deployed slot for `plugin_instance_id`.
    ///
    /// The session is dropped with the slot. Callers that still track the
    /// guest separately must drop that handle too, or the process stays up.
    pub(crate) fn remove_deployed(&mut self, plugin_instance_id: &str) {
        self.deployed.remove(plugin_instance_id);
    }

    /// Records `spawned` unless another deployed backend of the same kind
    /// already owns acquire.
    ///
    /// The second S3 or local instance is not stored. Its session is dropped
    /// so the process exits, and the error becomes the observation.
    pub(crate) fn publish_spawned(
        &mut self,
        plugin_instance_id: &str,
        spawned: SpawnedDestination,
    ) -> std::result::Result<Arc<PluginSession>, String> {
        let kind = spawned.kind;
        self.note_deployed_instance(
            plugin_instance_id,
            Arc::clone(&spawned.session),
            spawned.backend,
            kind,
        );
        if matches!(kind, DeployedKind::S3 | DeployedKind::Local) {
            let count = self
                .deployed
                .values()
                .filter(|slot| slot.kind == kind)
                .count();
            if count > 1 {
                self.deployed.remove(plugin_instance_id);
                let label = match kind {
                    DeployedKind::S3 => "s3",
                    DeployedKind::Local => "local",
                    DeployedKind::Other => "storage",
                };
                return Err(format!(
                    "another {label} destination instance is already deployed; refusing to pick one"
                ));
            }
        }
        Ok(spawned.session)
    }

    /// Copies deployed sessions from this registry onto `candidate`.
    ///
    /// `deployed_instances` is the set of present plugin instance ids. Reload
    /// builds `candidate` without those plugin keys. Only sessions the
    /// reconciler bound to one of those ids are copied, including the S3 or
    /// local backend they own. A transitional session for the same key is left
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

/// Discover and spawn external output plugins.
///
/// Every enabled destination guest goes through the generic
/// [`PluginSession`] front door; there is no direct-native or in-process
/// fallback when a guest cannot start.
///
/// # Errors
///
/// Returns an error when discovery fails or an **enabled** destination guest
/// fails to spawn / `open`. Guests that are not `api_version = 3` are skipped
/// with a warning.
pub async fn load_external_destinations(
    config: &Config,
    db: Option<&DatabaseConnection>,
) -> PluginResult<DestinationRegistry> {
    load_external_destinations_with_store(config, db, None, &std::collections::BTreeSet::new())
        .await
}

/// [`load_external_destinations`] with an open library and deployment skips.
///
/// `store` supplies instance documents. `skip` plugin keys are left to the
/// deployment reconciler. Database connect bootstrap is not involved.
///
/// # Errors
///
/// Returns an error when discovery fails or an enabled destination guest fails
/// to spawn.
pub async fn load_external_destinations_with_store(
    config: &Config,
    db: Option<&DatabaseConnection>,
    store: Option<&bookclerk_library::LibraryStore>,
    skip: &std::collections::BTreeSet<String>,
) -> PluginResult<DestinationRegistry> {
    let mut registry = DestinationRegistry::default();
    let plugins = crate::discover_plugins(config)?;
    let storage: Vec<_> = plugins
        .into_iter()
        .filter(|plugin| plugin.manifest.has_entrypoint(crate::Entrypoint::Storage))
        .collect();

    if config.output.s3.enabled {
        let spec = crate::occupancy_spec(&config.output.s3.plugin, S3_PLUGIN_ID);
        match crate::resolve_plugin_slot(&storage, spec)? {
            Some(plugin) if !plugin.alias().eq_ignore_ascii_case(S3_PLUGIN_ID) => {
                return Err(crate::PluginError::message(format!(
                    "[output.s3].plugin `{spec}` is not an s3 destination (alias {})",
                    plugin.alias()
                )));
            }
            Some(plugin) if plugin.manifest.api_version != PRODUCT_API_VERSION => {
                tracing::warn!(
                    id = %plugin.manifest.id,
                    plugin_key = %plugin.plugin_key().canonical(),
                    api_version = plugin.manifest.api_version,
                    "output plugin is not api_version 3; skipping"
                );
            }
            Some(plugin) if skip.contains(plugin.plugin_key().canonical()) => {
                tracing::info!(
                    plugin_key = %plugin.plugin_key().canonical(),
                    "skipping S3 destination owned by a local deployment"
                );
            }
            Some(plugin) => {
                let (storage_backend, session) = spawn_s3_guest(plugin, config, db, store)
                    .await
                    .map_err(|err| {
                        crate::PluginError::message(format!(
                            "failed to start S3 output plugin guest: {err}"
                        ))
                    })?;
                tracing::info!(
                    id = %plugin.manifest.id,
                    plugin_key = %plugin.plugin_key().canonical(),
                    path = %plugin.command.display(),
                    "loaded external S3 output plugin"
                );
                registry.s3 = Some(Arc::new(storage_backend));
                registry.set_plugin_session(session);
            }
            None => {
                tracing::debug!(
                    spec,
                    "S3 output enabled but no matching storage plugin is installed"
                );
            }
        }
    }

    if config.output.local.enabled {
        let spec = crate::occupancy_spec(&config.output.local.plugin, LOCAL_PLUGIN_ID);
        match crate::resolve_plugin_slot(&storage, spec)? {
            Some(plugin) if !plugin.alias().eq_ignore_ascii_case(LOCAL_PLUGIN_ID) => {
                return Err(crate::PluginError::message(format!(
                    "[output.local].plugin `{spec}` is not a local destination (alias {})",
                    plugin.alias()
                )));
            }
            Some(plugin) if skip.contains(plugin.plugin_key().canonical()) => {
                tracing::info!(
                    plugin_key = %plugin.plugin_key().canonical(),
                    "skipping local destination owned by a local deployment"
                );
            }
            Some(plugin) => {
                super::destination_local::try_load_local(plugin, config, store, &mut registry)
                    .await
                    .map_err(|err| {
                        crate::PluginError::message(format!(
                            "failed to start local output plugin guest: {err}"
                        ))
                    })?;
            }
            None => {
                tracing::debug!(
                    spec,
                    "local output enabled but no matching storage plugin is installed"
                );
            }
        }
    }
    Ok(registry)
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
    if plugin.alias().eq_ignore_ascii_case(S3_PLUGIN_ID) {
        let db = store.map(bookclerk_library::LibraryStore::db);
        let (storage_backend, session) =
            spawn_s3_guest_prepared(plugin, config, db, prepared).await?;
        return Ok(SpawnedDestination {
            session,
            backend: Some(Arc::new(storage_backend)),
            kind: DeployedKind::S3,
        });
    }
    if plugin.alias().eq_ignore_ascii_case(LOCAL_PLUGIN_ID) {
        let (storage, session) =
            super::destination_local::spawn_local_prepared(plugin, config, prepared).await?;
        return Ok(SpawnedDestination {
            session,
            backend: Some(Arc::new(storage)),
            kind: DeployedKind::Local,
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
        session,
        backend: None,
        kind: DeployedKind::Other,
    })
}

/// Spawns the S3 destination as an external Cap'n Proto guest.
async fn spawn_s3_guest(
    plugin: &DiscoveredPlugin,
    config: &Config,
    db: Option<&DatabaseConnection>,
    store: Option<&bookclerk_library::LibraryStore>,
) -> PluginResult<(PluginStorage, Arc<PluginSession>)> {
    let table = crate::settings_table(config, plugin);
    let transitional = toml_to_json(&toml::Value::Table(table));
    let prepared = crate::instance_bindings::prepare_open_bindings(
        store,
        &config.paths().files_dir,
        plugin,
        transitional,
    )
    .await?;
    spawn_s3_guest_prepared(plugin, config, db, prepared).await
}

/// Spawns the S3 guest with bindings the caller already resolved.
async fn spawn_s3_guest_prepared(
    plugin: &DiscoveredPlugin,
    config: &Config,
    db: Option<&DatabaseConnection>,
    prepared: crate::instance_bindings::PreparedOpen,
) -> PluginResult<(PluginStorage, Arc<PluginSession>)> {
    if prepared.from_instance {
        return spawn_s3_from_instance(plugin, config, db, prepared).await;
    }
    let config_json = prepared.spawn_config_table;
    let s3_config = config.output.s3.clone();
    let prefix = normalize_storage_prefix(s3_config.prefix.trim());
    let credentials = resolve_host_credentials(db)
        .await
        .map_err(|err| crate::PluginError::message(err.to_string()))?;
    let ctx = OutputS3ContextDto {
        plugin_data_dir: String::new(),
        bucket: s3_config.bucket.clone(),
        prefix,
        region: s3_config.region.clone(),
        endpoint: s3_config.endpoint.clone(),
        force_path_style: s3_config.force_path_style,
        credentials: None,
    };
    let grant = crate::consent::spawn_grant(&config.paths().files_dir, plugin)?;
    let mut extra_env = Vec::new();
    if crate::is_first_party_s3_output(plugin)
        && crate::consent::grant_has_binding(&grant, "secrets")
    {
        if let Some(creds) = &credentials {
            extra_env.push((
                bookclerk_storage::ENV_AWS_ACCESS_KEY_ID,
                std::ffi::OsString::from(&creds.access_key_id),
            ));
            extra_env.push((
                bookclerk_storage::ENV_AWS_SECRET_ACCESS_KEY,
                std::ffi::OsString::from(&creds.secret_access_key),
            ));
            if let Some(token) = &creds.session_token {
                extra_env.push((
                    bookclerk_storage::ENV_AWS_SESSION_TOKEN,
                    std::ffi::OsString::from(token),
                ));
            }
        }
    }
    let session = Arc::new(
        PluginSession::spawn_for_account_with_env(
            plugin,
            config,
            config_json,
            crate::OPERATOR_ACCOUNT,
            &extra_env,
        )
        .await?,
    );
    let open_bindings = if prepared.from_instance {
        prepared.bindings
    } else {
        BindingValues::config(
            bookclerk_plugin_sdk::ExtensibleConfig::json_from(&ctx)
                .map_err(|err| crate::PluginError::message(err.to_string()))?,
        )
    };
    session.open(open_bindings).await?;
    Ok((PluginStorage::new(Arc::clone(&session)), session))
}

/// Spawns a deployed S3 guest from the instance document.
///
/// Bucket, region, prefix, endpoint, and `forcePathStyle` come from the
/// document. Credentials stay in `SECRETS`. When the document has none, the
/// operator `encrypted_secrets` row or `BOOKCLERK_AWS_*` is copied into that
/// binding for this open. They are not written into `CONFIG` or the spawn
/// config table.
async fn spawn_s3_from_instance(
    plugin: &DiscoveredPlugin,
    config: &Config,
    db: Option<&DatabaseConnection>,
    prepared: crate::instance_bindings::PreparedOpen,
) -> PluginResult<(PluginStorage, Arc<PluginSession>)> {
    let context = instance_s3_context(&prepared)?;
    let operator = operator_s3_credentials(db).await?;
    let secrets = s3_open_secrets(&prepared, operator.as_ref())?;
    let endpoint = context
        .get("endpoint")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(str::to_string);
    let services = crate::SessionServices {
        deployed_s3_endpoint: Some(endpoint),
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

/// Bucket and region from the instance document, with no credentials.
pub(crate) fn instance_s3_context(
    prepared: &crate::instance_bindings::PreparedOpen,
) -> PluginResult<serde_json::Value> {
    let body = &prepared.granted_config;
    let bucket = json_text(body, &["bucket"]);
    let region = json_text(body, &["region"]);
    if bucket.is_empty() || region.is_empty() {
        return Err(crate::PluginError::message(
            "deployed s3 instance config is missing bucket or region; [output.s3] is not the authority",
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
        credentials: None,
    };
    serde_json::to_value(&ctx).map_err(|err| crate::PluginError::message(err.to_string()))
}

/// `SECRETS` for one deployed S3 open.
///
/// Instance secrets win. Otherwise the operator credential pair is injected
/// into this open only.
pub(crate) fn s3_open_secrets(
    prepared: &crate::instance_bindings::PreparedOpen,
    operator: Option<&S3Credentials>,
) -> PluginResult<bookclerk_plugin_sdk::ExtensibleConfig> {
    if credentials_from_secrets(&prepared.bindings.secrets).is_some() {
        return Ok(prepared.bindings.secrets.clone());
    }
    let Some(credentials) = operator else {
        return Err(crate::PluginError::message(
            "deployed s3 instance has no credentials in SECRETS, BOOKCLERK_AWS_*, or operator encrypted_secrets",
        ));
    };
    bookclerk_config::register_secret(&credentials.access_key_id);
    bookclerk_config::register_secret(&credentials.secret_access_key);
    if let Some(token) = &credentials.session_token {
        bookclerk_config::register_secret(token);
    }
    let mut value = prepared
        .bindings
        .secrets
        .json_value()
        .unwrap_or_else(|_| serde_json::json!({}));
    if !value.is_object() {
        value = serde_json::json!({});
    }
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "accessKeyId".into(),
            serde_json::Value::String(credentials.access_key_id.clone()),
        );
        object.insert(
            "secretAccessKey".into(),
            serde_json::Value::String(credentials.secret_access_key.clone()),
        );
        if let Some(token) = &credentials.session_token {
            object.insert(
                "sessionToken".into(),
                serde_json::Value::String(token.clone()),
            );
        }
    }
    Ok(bookclerk_plugin_sdk::ExtensibleConfig::json(&value))
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

/// Operator AWS keys for one open: `BOOKCLERK_AWS_*`, else the operator secret row.
pub(crate) async fn operator_s3_credentials(
    db: Option<&DatabaseConnection>,
) -> PluginResult<Option<S3Credentials>> {
    resolve_host_credentials(db)
        .await
        .map_err(|err| crate::PluginError::message(err.to_string()))
}

/// Resolves AWS keys from `BOOKCLERK_AWS_*` env, else unseals the operator `encrypted_secrets` row (process DEK).
async fn resolve_host_credentials(
    db: Option<&DatabaseConnection>,
) -> std::result::Result<Option<S3Credentials>, StorageError> {
    if let (Ok(access), Ok(secret)) = (
        std::env::var(bookclerk_storage::ENV_AWS_ACCESS_KEY_ID),
        std::env::var(bookclerk_storage::ENV_AWS_SECRET_ACCESS_KEY),
    ) {
        let session = std::env::var(bookclerk_storage::ENV_AWS_SESSION_TOKEN).ok();
        return Ok(Some(S3Credentials {
            access_key_id: access,
            secret_access_key: secret,
            session_token: session,
            label: None,
        }));
    }
    if let Some(db) = db {
        return load_s3_credentials(db).await;
    }
    Ok(None)
}

/// Converts plugin settings TOML to JSON for guest spawn; invalid values become `null`.
fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use crate::protocol::{OutputS3ContextDto, S3CredentialsDto};

    #[test]
    fn s3_destination_json_omits_paths_and_secrets() {
        let ctx = OutputS3ContextDto {
            plugin_data_dir: String::new(),
            bucket: "library".into(),
            prefix: "audiobooks".into(),
            region: "us-east-1".into(),
            endpoint: None,
            force_path_style: false,
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
            from_instance: true,
            config_revision: Some(1),
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
        let kept = super::s3_open_secrets(&camel, None).expect("instance secrets win");
        assert_eq!(kept.json_value().expect("json")["accessKeyId"], "AKIATEST");
        assert!(context.get("credentials").is_none(), "{context}");
        assert!(!context.to_string().contains("AKIASECRET"), "{context}");
    }

    #[test]
    fn instance_s3_secrets_fall_back_to_operator_credentials() {
        let prepared = prepared(
            serde_json::json!({"bucket": "library", "region": "us-east-1"}),
            serde_json::json!({}),
        );
        let err = super::s3_open_secrets(&prepared, None).expect_err("missing");
        assert!(err.to_string().contains("encrypted_secrets"), "{err}");

        let secrets = super::s3_open_secrets(
            &prepared,
            Some(&bookclerk_storage::S3Credentials {
                access_key_id: "AKIATEST".into(),
                secret_access_key: "secret".into(),
                session_token: None,
                label: None,
            }),
        )
        .expect("secrets");
        let value = secrets.json_value().expect("json");
        assert_eq!(value["accessKeyId"], "AKIATEST");
        assert!(super::instance_s3_context(&prepared)
            .expect("context")
            .get("credentials")
            .is_none());
    }

    #[tokio::test]
    async fn operator_row_is_visible_when_the_database_is_passed() {
        let _lock = bookclerk_config::ProcessEnvGuard::enter();
        let previous = AwsEnv::clear();
        let files = tempfile::tempdir().expect("files");
        bookclerk_library::configure_master_key(files.path()).expect("dek");
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("sqlite");
        bookclerk_storage::save_s3_credentials(
            &db,
            &bookclerk_storage::S3Credentials {
                access_key_id: "AKIADB".into(),
                secret_access_key: "from-db".into(),
                session_token: None,
                label: None,
            },
        )
        .await
        .expect("save operator row");

        let from_db = super::operator_s3_credentials(Some(&db))
            .await
            .expect("db")
            .expect("operator row");
        assert_eq!(from_db.access_key_id, "AKIADB");
        assert!(
            super::operator_s3_credentials(None)
                .await
                .expect("no env")
                .is_none(),
            "db=None must not see the operator encrypted_secrets row"
        );
        drop(previous);
    }

    struct AwsEnv {
        access: Option<String>,
        secret: Option<String>,
        token: Option<String>,
    }

    impl AwsEnv {
        #[allow(unsafe_code)]
        fn clear() -> Self {
            let saved = Self {
                access: std::env::var(bookclerk_storage::ENV_AWS_ACCESS_KEY_ID).ok(),
                secret: std::env::var(bookclerk_storage::ENV_AWS_SECRET_ACCESS_KEY).ok(),
                token: std::env::var(bookclerk_storage::ENV_AWS_SESSION_TOKEN).ok(),
            };
            unsafe {
                std::env::remove_var(bookclerk_storage::ENV_AWS_ACCESS_KEY_ID);
                std::env::remove_var(bookclerk_storage::ENV_AWS_SECRET_ACCESS_KEY);
                std::env::remove_var(bookclerk_storage::ENV_AWS_SESSION_TOKEN);
            }
            saved
        }
    }

    impl Drop for AwsEnv {
        fn drop(&mut self) {
            restore(bookclerk_storage::ENV_AWS_ACCESS_KEY_ID, &self.access);
            restore(bookclerk_storage::ENV_AWS_SECRET_ACCESS_KEY, &self.secret);
            restore(bookclerk_storage::ENV_AWS_SESSION_TOKEN, &self.token);
        }
    }

    #[allow(unsafe_code)]
    fn restore(key: &str, value: &Option<String>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
