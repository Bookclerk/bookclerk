//! [`StorageBackend`] adapter over an external output plugin process.
//!
//! Destinations speak Cap'n Proto `api_version = 2` only. The host never grants
//! the guest filesystem access to acquire scratch or the output library.
//! Credentials are injected as spawn env when the `secrets` binding is granted.

use std::sync::Arc;

use bookclerk_config::{normalize_storage_prefix, Config};
use bookclerk_plugin_sdk::{DestinationContext, PRODUCT_API_VERSION};
use bookclerk_storage::{load_s3_credentials, S3Credentials, StorageBackend, StorageError};
use sea_orm::DatabaseConnection;
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::protocol::OutputS3ContextDto;
use crate::rpc_session::{PluginSession, PluginStorage};
use crate::Result as PluginResult;

/// Manifest id of the platform S3 output plugin (`s3`).
const S3_PLUGIN_ID: &str = "s3";

/// Long-lived external output plugins loaded at host startup.
#[derive(Default, Clone)]
pub struct DestinationRegistry {
    /// Spawned S3 output backend when `[output.s3].enabled` and describe succeeded.
    s3: Option<Arc<dyn StorageBackend>>,
    /// Spawned local-filesystem output backend when that plugin loaded.
    local: Option<Arc<dyn StorageBackend>>,
    /// Plugin sessions keyed by `(plugin_id, account_id)`.
    plugin_sessions: std::collections::HashMap<String, Arc<PluginSession>>,
}

impl DestinationRegistry {
    /// External S3 output backend, when loaded.
    #[must_use]
    pub fn s3(&self) -> Option<Arc<dyn StorageBackend>> {
        self.s3.clone()
    }

    /// External local-filesystem output backend, when loaded.
    #[must_use]
    pub fn local(&self) -> Option<Arc<dyn StorageBackend>> {
        self.local.clone()
    }

    /// Plugin session for `plugin_id` and `account_id`, when that guest was loaded.
    #[must_use]
    pub fn plugin_session(&self, plugin_id: &str, account_id: &str) -> Option<Arc<PluginSession>> {
        self.plugin_sessions
            .get(&crate::plugin_instance_key(plugin_id, account_id))
            .cloned()
    }

    /// Records the local-filesystem output backend after a successful spawn.
    pub(crate) fn set_local(&mut self, dest: Arc<dyn StorageBackend>) {
        self.local = Some(dest);
    }

    /// Records a plugin session used for `JobHandler` invocations.
    pub(crate) fn set_plugin_session(&mut self, session: Arc<PluginSession>) {
        self.plugin_sessions
            .insert(session.instance_key().to_string(), session);
    }
}

/// Discover and spawn external output plugins.
///
/// # Errors
///
/// Returns an error when discovery fails. Individual guests that are not
/// `api_version = 2` or fail to start are skipped with a warning.
pub async fn load_external_destinations(
    config: &Config,
    db: Option<&DatabaseConnection>,
) -> PluginResult<DestinationRegistry> {
    let mut registry = DestinationRegistry::default();
    for plugin in crate::discover_plugins(config)? {
        if plugin.manifest.kind != crate::PluginKind::Output {
            continue;
        }
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            tracing::warn!(
                id = %plugin.manifest.id,
                api_version = plugin.manifest.api_version,
                "output plugin is not api_version 2; skipping"
            );
            continue;
        }
        if plugin.manifest.id == S3_PLUGIN_ID {
            if !config.output.s3.enabled {
                tracing::debug!(id = %plugin.manifest.id, "S3 output disabled in config; skipping external plugin");
                continue;
            }
            match spawn_s3_guest(&plugin, config, db).await {
                Ok((storage, session)) => {
                    tracing::info!(
                        id = %plugin.manifest.id,
                        path = %plugin.command.display(),
                        "loaded external S3 output plugin (api_version 2)"
                    );
                    registry.s3 = Some(Arc::new(storage));
                    registry.set_plugin_session(session);
                }
                Err(err) => {
                    tracing::warn!(
                        id = %plugin.manifest.id,
                        %err,
                        "failed to start S3 output plugin guest; falling back to in-process backend"
                    );
                }
            }
            continue;
        }
        super::destination_local::try_load_local(&plugin, config, &mut registry).await;
    }
    Ok(registry)
}

/// Spawns the S3 destination as an external Cap'n Proto guest.
async fn spawn_s3_guest(
    plugin: &DiscoveredPlugin,
    config: &Config,
    db: Option<&DatabaseConnection>,
) -> PluginResult<(PluginStorage, Arc<PluginSession>)> {
    let table = crate::settings_table(config, plugin);
    let config_json = toml_to_json(&toml::Value::Table(table));
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
    let grant = crate::consent::spawn_grant(&config.paths().files_dir, &plugin.manifest)?;
    let mut extra_env = Vec::new();
    if crate::consent::grant_has_binding(&grant, "secrets") {
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
    session
        .ensure_destination(DestinationContext {
            json: serde_json::to_string(&ctx).map_err(crate::PluginError::Json)?,
        })
        .await?;
    Ok((PluginStorage::new(Arc::clone(&session)), session))
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
                access_key_id: "AKIASECRET".into(),
                secret_access_key: "wJalr".into(),
                session_token: None,
            }),
            ..ctx
        };
        let leaked_json = serde_json::to_string(&leaked).unwrap();
        assert!(leaked_json.contains("pluginDataDir"), "{leaked_json}");
        assert!(leaked_json.contains("AKIASECRET"), "{leaked_json}");
    }
}
