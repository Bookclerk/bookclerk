//! Derive the typed `describe()` capability block from an embedded manifest.

use bookclerk_plugin_abi::{MediaSupport, PluginCapabilities, PluginError, PluginErrorCode};
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

/// Parses `plugin.toml` source and returns the [`MediaSupport`] a guest must
/// advertise from `describe()`.
///
/// The host refuses a storefront or metadata provider whose described media
/// differs from its manifest's `[media]` table, so deriving it from the
/// embedded manifest keeps the two equal.
///
/// # Arguments
///
/// * `manifest_toml` - Contents of the plugin's `plugin.toml`, typically
///   `include_str!("../plugin.toml")`.
///
/// # Returns
///
/// The `[media]` declaration in its wire shape; empty lists for a plugin
/// without the table.
///
/// # Errors
///
/// Returns [`PluginErrorCode::Internal`] when the manifest does not parse or
/// fails validation (including a storefront without `[media]`).
pub fn manifest_media(manifest_toml: &str) -> Result<MediaSupport, PluginError> {
    PluginManifest::parse(manifest_toml)
        .map(|m| m.media_support())
        .map_err(|e| PluginError::new(PluginErrorCode::Internal, format!("plugin.toml: {e}")))
}
