//! Load `plugin.toml` from the plugin install root.

use std::path::Path;

use anyhow::{Context, Result};
use bookclerk_plugin_manifest::PluginManifest;

/// Reads and validates `plugin.toml` from the guest install root.
pub fn load_manifest(root: &Path) -> Result<PluginManifest> {
    let path = root.join("plugin.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    PluginManifest::parse(&text).map_err(|e| anyhow::anyhow!("{e}"))
}
