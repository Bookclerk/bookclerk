//! `CONFIG` and `SECRETS` for one plugin instance.
//!
//! A guest is always spawned for a plugin instance. Its `CONFIG` is the
//! manifest's `[[settings]]` defaults overlaid with the instance document and
//! its `SECRETS` are the document's secret refs, unsealed. There is no
//! `config.toml` fallback: a plugin with no instance document does not spawn.

use std::path::Path;

use bookclerk_library::control_plane::{
    load_plugin_instance, resolve_instance_bindings, InstanceBindingGrant, PluginInstanceId,
};
use bookclerk_library::{LibraryError, LibraryStore};
use bookclerk_plugin_sdk::{BindingValues, ExtensibleConfig};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::Result;

/// Bindings passed to `PluginWorker.open`, plus the spawn config table.
#[derive(Debug, Clone)]
pub struct PreparedOpen {
    /// Values for `PluginWorker.open`.
    pub bindings: BindingValues,
    /// JSON object passed into [`crate::PluginSession::spawn_with`].
    ///
    /// The session still applies [`crate::spawn_config_for_grant`].
    pub spawn_config_table: Value,
    /// JSON object the guest receives when the grant includes `config`.
    pub granted_config: Value,
    /// Document revision this open applies.
    pub config_revision: i64,
}

/// Builds open bindings from a deployment's already resolved payloads.
///
/// Does not look up an instance by plugin key.
#[must_use]
pub fn prepared_open_from_resolved(
    config: ExtensibleConfig,
    secrets: ExtensibleConfig,
    config_revision: i64,
) -> PreparedOpen {
    let granted_config = config
        .json_value()
        .unwrap_or_else(|_| Value::Object(Default::default()));
    PreparedOpen {
        bindings: BindingValues {
            config,
            secrets,
            ..BindingValues::default()
        },
        spawn_config_table: granted_config.clone(),
        granted_config,
        config_revision,
    }
}

/// `config` / `secrets` flags of the plugin's covering grant.
#[must_use]
pub fn binding_grant_flags(grant: &crate::PluginGrant) -> InstanceBindingGrant {
    InstanceBindingGrant {
        config: crate::grant_has_binding(grant, "config"),
        secrets: crate::grant_has_binding(grant, "secrets"),
    }
}

/// Resolves open bindings for `instance_id` of `plugin`.
///
/// The covering grant comes from `plugin-grants.json`; the settings schema
/// from the installed manifest. A missing document, a dangling secret ref, or
/// a missing required setting is an error. Document keys the installed
/// manifest no longer declares are dropped and logged.
///
/// # Errors
///
/// Returns an error when the grant cannot be loaded, the instance is not this
/// plugin's, or its document cannot be resolved.
pub async fn prepare_open_for_instance(
    store: &LibraryStore,
    files_dir: &Path,
    plugin: &DiscoveredPlugin,
    instance_id: &PluginInstanceId,
) -> Result<PreparedOpen> {
    let instance = load_plugin_instance(store, instance_id)
        .await
        .map_err(library_err)?
        .ok_or_else(|| {
            crate::PluginError::message(format!("plugin instance `{instance_id}` was not found"))
        })?;
    if instance.plugin_key != plugin.plugin_key().canonical() {
        return Err(crate::PluginError::message(format!(
            "plugin instance `{instance_id}` belongs to `{}`, not `{}`",
            instance.plugin_key,
            plugin.plugin_key().canonical()
        )));
    }
    let grant = crate::spawn_grant(files_dir, plugin)?;
    let flags = binding_grant_flags(&grant);
    let schema = plugin.manifest.settings_schema();
    let resolved = resolve_instance_bindings(store, instance_id, &flags, &schema)
        .await
        .map_err(library_err)?;
    if !resolved.dropped_keys.is_empty() {
        tracing::warn!(
            plugin_key = %instance.plugin_key,
            plugin_instance_id = %instance_id,
            dropped = ?resolved.dropped_keys,
            "instance document keys not declared by the installed manifest were dropped"
        );
    }
    Ok(prepared_open_from_resolved(
        resolved.config,
        resolved.secrets,
        resolved.config_revision,
    ))
}

/// Maps a library error onto the host error type.
fn library_err(err: LibraryError) -> crate::PluginError {
    crate::PluginError::message(err.to_string())
}
