//! Load `plugin.toml` from the plugin install root.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use bookclerk_plugin_manifest::PluginManifest;

/// Join `root` / `plugin.toml` and require the result stays under `root`.
fn join_plugin_toml(root: &Path) -> Result<PathBuf> {
    if root.as_os_str().is_empty() {
        bail!("refusing empty plugin root");
    }
    let root_norm = match bookclerk_sandbox::canonicalize(root) {
        Ok(c) => c,
        Err(_) => root.to_path_buf(),
    };
    let path = root_norm.join("plugin.toml");
    for comp in path.components() {
        if matches!(comp, Component::ParentDir) {
            bail!("refusing path with '..': {}", path.display());
        }
    }
    if !path.starts_with(&root_norm) {
        bail!(
            "path {} escapes plugin root {}",
            path.display(),
            root_norm.display()
        );
    }
    Ok(path)
}

/// Reads and validates `plugin.toml` from the guest install root.
pub fn load_manifest(root: &Path) -> Result<PluginManifest> {
    let path = join_plugin_toml(root)?;
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    PluginManifest::parse(&text).map_err(|e| anyhow::anyhow!("{e}"))
}
