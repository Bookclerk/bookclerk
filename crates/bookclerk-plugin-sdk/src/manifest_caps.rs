//! Derive the typed `describe()` capability block from an embedded manifest.

use bookclerk_plugin_abi::{PluginCapabilities, PluginError, PluginErrorCode};
use bookclerk_plugin_manifest::PluginManifest;

/// Parses `plugin.toml` source and returns the [`PluginCapabilities`] a guest
/// must advertise from `describe()`.
///
/// Keeping the manifest as the single source of truth means the host's
/// widening check (manifest vs describe vs operator grant) cannot drift: the
/// guest embeds the same file the host installed.
///
/// # Arguments
///
/// * `manifest_toml` - Contents of the plugin's `plugin.toml`, typically
///   `include_str!("../plugin.toml")`.
///
/// # Returns
///
/// The typed capability declaration in manifest order.
///
/// # Errors
///
/// Returns [`PluginErrorCode::Internal`] when the manifest does not parse or
/// fails validation — a build defect, not a runtime condition.
///
/// # Examples
///
/// ```
/// use bookclerk_plugin_sdk::manifest_capabilities;
///
/// let caps = manifest_capabilities(r#"
/// api_version = 3
/// id = "echo"
/// runtime = "native"
/// command = "./echo"
/// entrypoints = ["cli"]
///
/// [capabilities.network]
/// mode = "deny"
/// "#).unwrap();
/// assert_eq!(caps.entrypoints.len(), 1);
/// ```
pub fn manifest_capabilities(manifest_toml: &str) -> Result<PluginCapabilities, PluginError> {
    PluginManifest::parse(manifest_toml)
        .map(|m| m.capabilities())
        .map_err(|e| PluginError::new(PluginErrorCode::Internal, format!("plugin.toml: {e}")))
}
