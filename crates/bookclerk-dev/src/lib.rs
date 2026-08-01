//! Dev workflow helpers for building and staging first-party plugin guests.

pub mod package;
pub mod plugins;

pub fn workspace_root() -> anyhow::Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| anyhow::anyhow!("bookclerk-dev manifest has no parent"))?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("bookclerk-dev is not under workspace crates/"))?
        .to_path_buf())
}

pub fn default_artifacts(root: &std::path::Path) -> std::path::PathBuf {
    std::env::var_os("BOOKCLERK_PLUGIN_ARTIFACTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("target").join("plugin-artifacts"))
}

pub fn default_files_dir() -> std::path::PathBuf {
    std::env::var_os("BOOKCLERK_FILES_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/BookclerkFiles"))
}

/// Workspace version from `CARGO_PKG_VERSION` (matches `[workspace.package].version`).
#[must_use]
pub fn workspace_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
