//! [`StorageBackend`] adapter over the local filesystem output plugin process.
//!
//! Local output speaks Cap'n Proto `api_version = 2` only. When
//! `bookclerk-workerd` is available the host wraps the native guest
//! (native-behind-workerd); otherwise it falls back to direct Cap'n Proto.

use std::path::PathBuf;
use std::sync::Arc;

use bookclerk_config::{normalize_storage_prefix, Config};
use bookclerk_plugin_sdk::v2::{DestinationContext, PRODUCT_API_VERSION};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::protocol::OutputLocalContextDto;
use crate::rpc_v2::{V2PluginSession, V2Storage};
use crate::Result as PluginResult;

/// Manifest id of the platform local-filesystem destination guest.
const LOCAL_PLUGIN_ID: &str = "local";

/// Absolute local output root: `output.local.root`, or that path joined to `files_dir` when relative.
fn resolved_local_output_root(config: &Config) -> PathBuf {
    let root = &config.output.local.root;
    if root.is_absolute() {
        root.clone()
    } else {
        config.paths().files_dir.join(root)
    }
}

/// Spawns the local output guest when enabled; logs and falls back in-process on failure.
pub(crate) async fn try_load_local(
    plugin: &DiscoveredPlugin,
    config: &Config,
    registry: &mut super::destination::DestinationRegistry,
) {
    if plugin.manifest.id != LOCAL_PLUGIN_ID {
        return;
    }
    if !config.output.local.enabled {
        tracing::debug!(id = %plugin.manifest.id, "local output disabled in config; skipping external plugin");
        return;
    }
    if plugin.manifest.api_version != PRODUCT_API_VERSION {
        tracing::warn!(
            id = %plugin.manifest.id,
            api_version = plugin.manifest.api_version,
            "local output plugin is not api_version 2; skipping"
        );
        return;
    }
    match spawn_v2_local(plugin, config).await {
        Ok((storage, session)) => {
            tracing::info!(
                id = %plugin.manifest.id,
                path = %plugin.command.display(),
                "loaded external local output plugin (api_version 2)"
            );
            registry.set_local(Arc::new(storage));
            registry.set_v2_session(session);
        }
        Err(err) => {
            tracing::warn!(
                id = %plugin.manifest.id,
                %err,
                "failed to start v2 local output plugin; falling back to in-process backend"
            );
        }
    }
}

/// Spawns the local destination as an ABI v2 Cap'n Proto guest.
async fn spawn_v2_local(
    plugin: &DiscoveredPlugin,
    config: &Config,
) -> PluginResult<(V2Storage, Arc<V2PluginSession>)> {
    let table = crate::settings_table(config, plugin);
    let config_json = toml_to_json(&toml::Value::Table(table));
    let root = resolved_local_output_root(config);
    let prefix = normalize_storage_prefix(config.output.local.prefix.trim());
    let extra_env = [(
        "BOOKCLERK_OUTPUT_LOCAL_ROOT",
        std::ffi::OsString::from(root.as_os_str()),
    )];
    let session = match crate::discover::resolve_workerd_runtime() {
        Ok(workerd) => {
            let mut wrapped = plugin.clone();
            wrapped.command = workerd;
            wrapped.manifest.runtime = crate::PluginRuntimeKind::Workerd;
            let mut env = extra_env.to_vec();
            env.push((
                "BOOKCLERK_NATIVE_BACKEND",
                std::ffi::OsString::from(plugin.command.as_os_str()),
            ));
            V2PluginSession::spawn_for_account_with_env(
                &wrapped,
                config,
                config_json.clone(),
                crate::OPERATOR_ACCOUNT,
                &env,
            )
            .await
        }
        Err(_) => {
            V2PluginSession::spawn_for_account_with_env(
                plugin,
                config,
                config_json,
                crate::OPERATOR_ACCOUNT,
                &extra_env,
            )
            .await
        }
    }?;
    let session = Arc::new(session);
    let ctx = OutputLocalContextDto {
        plugin_data_dir: String::new(),
        root: String::new(),
        prefix,
    };
    session
        .ensure_destination(DestinationContext {
            json: serde_json::to_string(&ctx)?,
        })
        .await?;
    Ok((V2Storage::new(Arc::clone(&session)), session))
}

/// Converts a plugin settings TOML table to JSON for guest handshake; `Null` on failure.
fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_local_destination_json_omits_filesystem_paths() {
        let ctx = OutputLocalContextDto {
            plugin_data_dir: String::new(),
            root: String::new(),
            prefix: "audiobooks".into(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(
            !json.contains("pluginDataDir") && !json.contains("plugin_data_dir"),
            "{json}"
        );
        assert!(!json.contains("root"), "{json}");
        assert!(!json.contains('/'), "{json}");
        assert!(!json.contains('\\'), "{json}");
    }
}
