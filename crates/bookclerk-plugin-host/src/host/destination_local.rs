//! [`StorageBackend`] adapter over the local filesystem output plugin process.
//!
//! Local output speaks Cap'n Proto `api_version = 3` only, spawned through the
//! same [`PluginSession`] front door as every other guest (`bookclerk-workerd`
//! fronting the native backend). There is no direct-native or in-process
//! fallback: a guest that cannot start is a load error.

use std::path::PathBuf;
use std::sync::Arc;

use bookclerk_config::{normalize_storage_prefix, Config};
use bookclerk_plugin_sdk::{BindingValues, PRODUCT_API_VERSION};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::protocol::OutputLocalContextDto;
use crate::rpc_session::{PluginSession, PluginStorage};
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

/// Spawns the local output guest when enabled and records it in `registry`.
///
/// # Errors
///
/// Returns the spawn / `open` error when the guest cannot start; the caller
/// decides whether the host may run without local output.
pub(crate) async fn try_load_local(
    plugin: &DiscoveredPlugin,
    config: &Config,
    registry: &mut super::destination::DestinationRegistry,
) -> PluginResult<()> {
    if plugin.manifest.id != LOCAL_PLUGIN_ID {
        return Ok(());
    }
    if !config.output.local.enabled {
        tracing::debug!(id = %plugin.manifest.id, "local output disabled in config; skipping external plugin");
        return Ok(());
    }
    if plugin.manifest.api_version != PRODUCT_API_VERSION {
        tracing::warn!(
            id = %plugin.manifest.id,
            api_version = plugin.manifest.api_version,
            "local output plugin is not api_version 2; skipping"
        );
        return Ok(());
    }
    let (storage, session) = spawn_local_guest(plugin, config).await?;
    tracing::info!(
        id = %plugin.manifest.id,
        path = %plugin.command.display(),
        "loaded external local output plugin (api_version 2)"
    );
    registry.set_local(Arc::new(storage));
    registry.set_plugin_session(session);
    Ok(())
}

/// Spawns the local destination as an external Cap'n Proto guest.
async fn spawn_local_guest(
    plugin: &DiscoveredPlugin,
    config: &Config,
) -> PluginResult<(PluginStorage, Arc<PluginSession>)> {
    let table = crate::settings_table(config, plugin);
    let config_json = toml_to_json(&toml::Value::Table(table));
    let root = resolved_local_output_root(config);
    let prefix = normalize_storage_prefix(config.output.local.prefix.trim());
    let extra_env = [(
        "BOOKCLERK_OUTPUT_LOCAL_ROOT",
        std::ffi::OsString::from(root.as_os_str()),
    )];
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
    let ctx = OutputLocalContextDto {
        plugin_data_dir: String::new(),
        root: String::new(),
        prefix,
    };
    session
        .open(BindingValues::config(
            bookclerk_plugin_sdk::ExtensibleConfig::json_from(&ctx)
                .map_err(|err| crate::PluginError::message(err.to_string()))?,
        ))
        .await?;
    Ok((PluginStorage::new(Arc::clone(&session)), session))
}

/// Converts a plugin settings TOML table to JSON for the guest spawn config; `Null` on failure.
fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_destination_json_omits_filesystem_paths() {
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
