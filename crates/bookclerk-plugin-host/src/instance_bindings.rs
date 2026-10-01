//! `CONFIG` and `SECRETS` for one plugin instance.
//!
//! When the plugin key has an instance document, spawn uses that document.
//! Plugins with no instance keep the transitional `config.toml` table.

use std::path::Path;

use bookclerk_library::control_plane::{
    find_plugin_instance_by_key, instance_has_deployment, load_instance_config,
    resolve_instance_bindings, InstanceBindingGrant, ResolvedInstanceBindings,
    GRAPHICAUDIO_IMPORT_KEYS, GRAPHICAUDIO_MANIFEST_ID,
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
    /// True when `granted_config` came from an instance document.
    pub from_instance: bool,
    /// Document revision when `from_instance` is set.
    pub config_revision: Option<i64>,
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
        from_instance: true,
        config_revision: Some(config_revision),
    }
}

/// Resolves open bindings for `plugin`.
///
/// `store == None` always uses `transitional` (database connect bootstrap has
/// no library handle yet). A missing instance row, an instance with no
/// document, or an instance that already has a deployment also uses
/// `transitional`. Key lookup is only for plugins with no deployment. A
/// deployment spawn passes [`prepared_open_from_resolved`] instead. An
/// instance document that fails validation, grant checks, or secret
/// resolution is an error: this function does not fall back to
/// `settings_table_for`.
///
/// # Errors
///
/// Returns an error when the grant cannot be loaded or an instance document
/// cannot be resolved.
pub async fn prepare_open_bindings(
    store: Option<&LibraryStore>,
    files_dir: &Path,
    plugin: &DiscoveredPlugin,
    transitional: Value,
) -> Result<PreparedOpen> {
    let grant = crate::spawn_grant(files_dir, plugin)?;
    if let Some(store) = store {
        if let Some(resolved) =
            instance_document(store, plugin.plugin_key().canonical(), &grant).await?
        {
            let granted_config = resolved
                .config
                .json_value()
                .unwrap_or_else(|_| Value::Object(Default::default()));
            let revision = resolved.config_revision;
            return Ok(PreparedOpen {
                bindings: BindingValues {
                    config: resolved.config,
                    secrets: resolved.secrets,
                    ..BindingValues::default()
                },
                spawn_config_table: granted_config.clone(),
                granted_config,
                from_instance: true,
                config_revision: Some(revision),
            });
        }
    }
    let granted = crate::spawn_config_for_grant(&grant, transitional.clone());
    Ok(PreparedOpen {
        bindings: BindingValues::config(ExtensibleConfig::json(&granted)),
        spawn_config_table: transitional,
        granted_config: granted,
        from_instance: false,
        config_revision: None,
    })
}

/// Loads the instance document for `plugin_key` when one exists and no
/// deployment owns that instance.
///
/// Two instances may share a key. This lookup returns the oldest row, so it
/// is not used once a deployment exists. The reconciler carries that
/// deployment's instance id instead.
///
/// # Errors
///
/// Returns an error when the document exists but cannot be resolved. A missing
/// row, or an instance that has a deployment, is `Ok(None)`.
async fn instance_document(
    store: &LibraryStore,
    plugin_key: &str,
    grant: &crate::PluginGrant,
) -> Result<Option<ResolvedInstanceBindings>> {
    let Some(instance) = find_plugin_instance_by_key(store, plugin_key)
        .await
        .map_err(library_err)?
    else {
        return Ok(None);
    };
    if instance_has_deployment(store, &instance.id)
        .await
        .map_err(library_err)?
    {
        return Ok(None);
    }
    match load_instance_config(store, &instance.id).await {
        Ok(_) => {}
        Err(LibraryError::NotFound(_)) => return Ok(None),
        Err(err) => return Err(library_err(err)),
    }
    let flags = InstanceBindingGrant {
        config: crate::grant_has_binding(grant, "config"),
        secrets: crate::grant_has_binding(grant, "secrets"),
    };
    resolve_instance_bindings(store, &instance.id, &flags)
        .await
        .map(Some)
        .map_err(library_err)
}

/// Maps a library error onto the host error type.
fn library_err(err: LibraryError) -> crate::PluginError {
    crate::PluginError::message(err.to_string())
}

/// Dotted settings keys imported from `[sources.graphicaudio]`.
///
/// `sources.graphicaudio.enabled` stays file-backed.
pub fn graphicaudio_imported_setting_keys() -> &'static [&'static str] {
    const KEYS: &[&str] = &[
        "sources.graphicaudio.access",
        "sources.graphicaudio.base_url",
        "sources.graphicaudio.store_url",
        "sources.graphicaudio.bitrate",
        "sources.graphicaudio.container",
    ];
    let _ = GRAPHICAUDIO_IMPORT_KEYS;
    KEYS
}

/// True when `key` is a GraphicAudio setting owned by the instance document.
#[must_use]
pub fn is_graphicaudio_imported_setting(key: &str) -> bool {
    graphicaudio_imported_setting_keys().contains(&key)
}

/// Canonical plugin key for the installed GraphicAudio manifest, if any.
///
/// Discovery wins. The install ledger is the fallback when the tree is recorded
/// but not currently discovered.
///
/// # Errors
///
/// Returns an error when the install ledger cannot be read.
pub fn graphicaudio_plugin_key(config: &bookclerk_config::Config) -> Result<Option<String>> {
    if let Ok(plugins) = crate::discover_plugins(config) {
        if let Some(plugin) = plugins.iter().find(|plugin| {
            plugin
                .manifest
                .id
                .eq_ignore_ascii_case(GRAPHICAUDIO_MANIFEST_ID)
        }) {
            return Ok(Some(plugin.plugin_key().canonical().to_string()));
        }
    }
    let ledger = bookclerk_plugin_catalog::InstallLedger::load(&config.paths().files_dir)
        .map_err(|err| crate::PluginError::message(err.to_string()))?;
    Ok(ledger
        .artifacts
        .iter()
        .find(|row| {
            row.manifest_id
                .eq_ignore_ascii_case(GRAPHICAUDIO_MANIFEST_ID)
        })
        .map(|row| row.plugin_key.clone()))
}

/// True when GraphicAudio's instance document already exists.
///
/// # Errors
///
/// Returns an error when the instance row cannot be read.
pub async fn graphicaudio_document_exists(
    store: &LibraryStore,
    config: &bookclerk_config::Config,
) -> Result<bool> {
    let Some(plugin_key) = graphicaudio_plugin_key(config)? else {
        return Ok(false);
    };
    let Some(instance) = find_plugin_instance_by_key(store, &plugin_key)
        .await
        .map_err(library_err)?
    else {
        return Ok(false);
    };
    match load_instance_config(store, &instance.id).await {
        Ok(_) => Ok(true),
        Err(LibraryError::NotFound(_)) => Ok(false),
        Err(err) => Err(library_err(err)),
    }
}
