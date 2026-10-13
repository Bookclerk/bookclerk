//! [`StorageBackend`] adapter over the local filesystem output plugin process.
//!
//! Local output speaks Cap'n Proto `api_version = 3` only, spawned through the
//! same [`PluginSession`] front door as every other guest (`bookclerk-workerd`
//! fronting the native backend). There is no direct-native or in-process
//! fallback: a guest that cannot start is a load error.

use std::path::PathBuf;
use std::sync::Arc;

use bookclerk_config::Config;
use bookclerk_plugin_sdk::PRODUCT_API_VERSION;
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::rpc_session::{PluginSession, PluginStorage};
use crate::Result as PluginResult;

/// Spawns the local destination with bindings the deployment already resolved.
///
/// # Errors
///
/// Returns an error when the guest cannot start or `open` fails.
pub(crate) async fn spawn_local_prepared(
    plugin: &DiscoveredPlugin,
    config: &Config,
    prepared: crate::instance_bindings::PreparedOpen,
) -> PluginResult<(PluginStorage, Arc<PluginSession>)> {
    if plugin.manifest.api_version != PRODUCT_API_VERSION {
        return Err(crate::PluginError::message(format!(
            "local output plugin `{}` is not api_version 3",
            plugin.manifest.id
        )));
    }
    let config_json = prepared.spawn_config_table.clone();
    let mut services = crate::SessionServices::default();
    // The guest reads `root` / `prefix` from its `CONFIG`; the host only needs
    // the root for the jail write grant and the transport env.
    let root = instance_local_root(config, &prepared.granted_config)?;
    services.local_output_root = Some(root.clone());
    let extra_env: Vec<(&str, std::ffi::OsString)> = if crate::is_first_party_local_output(plugin) {
        vec![(
            "BOOKCLERK_OUTPUT_LOCAL_ROOT",
            std::ffi::OsString::from(root.as_os_str()),
        )]
    } else {
        Vec::new()
    };
    let session = Arc::new(
        PluginSession::spawn_with(
            plugin,
            config,
            config_json,
            crate::OPERATOR_ACCOUNT,
            &extra_env,
            services,
        )
        .await?,
    );
    session.open(prepared.bindings).await?;
    Ok((PluginStorage::new(Arc::clone(&session)), session))
}

/// Root from the instance document (`root` setting; relative paths resolve under the files directory).
pub(crate) fn instance_local_root(config: &Config, granted: &Value) -> PluginResult<PathBuf> {
    let text = granted
        .get("root")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            crate::PluginError::message(
                "local destination instance config is missing root; set it on the plugin instance",
            )
        })?;
    let root = PathBuf::from(text);
    if root.is_absolute() {
        Ok(root)
    } else {
        Ok(config.paths().files_dir.join(root))
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::OutputLocalContextDto;

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
