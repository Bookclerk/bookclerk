//! [`StorageBackend`] adapter over an external output plugin process.
//!
//! Destinations speak Cap'n Proto `api_version = 3` only and are spawned for
//! a plugin instance whose document supplies bucket, prefix, and endpoint.
//! The host never grants the guest filesystem access to acquire scratch or
//! the output library. Credentials travel in `SECRETS`, never in `CONFIG`.

use std::sync::Arc;

use bookclerk_config::{Config, DestinationNaming, NamingProfile, OutputBackendKind};
use bookclerk_storage::StorageBackend;

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
    /// Key layout from the instance document.
    settings: DestinationSettings,
}

/// Key layout settings the host reads from a deployed destination's instance
/// document: the `local` / `s3` plugins declare `prefix` and the naming
/// overrides as `[[settings]]`, and acquire needs them to plan storage keys.
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

/// One reconciler-spawned destination, addressed by plugin instance id.
#[derive(Clone)]
struct DeployedSlot {
    /// Guest session.
    session: Arc<PluginSession>,
    /// Storage backend when this guest is S3 or local.
    backend: Option<Arc<dyn StorageBackend>>,
    /// Which acquire slot this backend fills.
    kind: DeployedKind,
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
        self.require_kind(DeployedKind::S3, "s3")
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
        self.require_kind(DeployedKind::Local, "local")
    }

    /// Key layout settings of the deployed destination of `kind`, when exactly
    /// one is deployed.
    #[must_use]
    pub fn settings_for(&self, kind: OutputBackendKind) -> Option<DestinationSettings> {
        let wanted = match kind {
            OutputBackendKind::Local => DeployedKind::Local,
            OutputBackendKind::S3 => DeployedKind::S3,
        };
        let mut matches = self.deployed.values().filter(|slot| slot.kind == wanted);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first.settings.clone())
    }

    /// One deployed backend of `kind`, else the transitional slot.
    fn require_kind(
        &self,
        kind: DeployedKind,
        label: &str,
    ) -> std::result::Result<Option<Arc<dyn StorageBackend>>, String> {
        let deployed: Vec<_> = self
            .deployed
            .values()
            .filter(|slot| slot.kind == kind)
            .filter_map(|slot| slot.backend.clone())
            .collect();
        match deployed.len() {
            0 => Ok(None),
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

    /// Records a reconciler-spawned guest under `plugin_instance_id`.
    ///
    /// A second instance of the same plugin key does not replace this slot.
    fn note_deployed_instance(
        &mut self,
        plugin_instance_id: &str,
        session: Arc<PluginSession>,
        backend: Option<Arc<dyn StorageBackend>>,
        kind: DeployedKind,
        settings: DestinationSettings,
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
                settings,
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
            spawned.settings,
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
    if plugin.alias().eq_ignore_ascii_case(S3_PLUGIN_ID) {
        let (storage_backend, session) = spawn_s3_from_instance(plugin, config, prepared).await?;
        return Ok(SpawnedDestination {
            session,
            backend: Some(Arc::new(storage_backend)),
            kind: DeployedKind::S3,
            settings,
        });
    }
    if plugin.alias().eq_ignore_ascii_case(LOCAL_PLUGIN_ID) {
        let (storage, session) =
            super::destination_local::spawn_local_prepared(plugin, config, prepared).await?;
        return Ok(SpawnedDestination {
            session,
            backend: Some(Arc::new(storage)),
            kind: DeployedKind::Local,
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
        session,
        backend: None,
        kind: DeployedKind::Other,
        settings,
    })
}

/// Spawns a deployed S3 guest from the instance document.
///
/// Bucket, region, prefix, endpoint, and `force_path_style` come from the
/// document. Credentials are the instance's `access_key_id` /
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
    let endpoint = context
        .get("endpoint")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(str::to_string);
    let services = crate::SessionServices {
        deployed_endpoint: Some(endpoint),
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
