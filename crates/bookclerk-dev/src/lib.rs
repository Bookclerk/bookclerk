//! Dev workflow helpers for building and staging first-party plugin guests.

pub mod package;
pub mod plugins;
mod ui;

pub use ui::ensure_ui_dist;

/// Ensure the pinned Cloudflare `workerd` binary is present under `target/<profile>/`.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `release` - When true, install under `target/release/`; otherwise `target/debug/`.
///
/// # Returns
///
/// Absolute path to the ensured `workerd` binary.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn ensure_workerd_for_profile(
    root: &std::path::Path,
    release: bool,
) -> anyhow::Result<std::path::PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let dir = root.join("target").join(profile);
    bookclerk_workerd::ensure_workerd(&dir)
}

/// Resolves the Cargo workspace root from `CARGO_MANIFEST_DIR` or cwd.
///
/// # Returns
///
/// Absolute path to the Cargo workspace root (parent of `crates/`).
///
/// # Errors
///
/// Returns an error when the crate is not nested under `crates/` as expected.
pub fn workspace_root() -> anyhow::Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| anyhow::anyhow!("bookclerk-dev manifest has no parent"))?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("bookclerk-dev is not under workspace crates/"))?
        .to_path_buf())
}

/// Default directory for packaged release artifacts under the workspace.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
///
/// # Returns
///
/// Absolute path to the artifacts directory.
pub fn default_artifacts(root: &std::path::Path) -> std::path::PathBuf {
    std::env::var_os("BOOKCLERK_PLUGIN_ARTIFACTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("target").join("plugin-artifacts"))
}

/// Dev files root: `$BOOKCLERK_FILES_DIR`, else `<workspace>/BookclerkFiles`.
pub fn default_files_dir() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("BOOKCLERK_FILES_DIR") {
        return std::path::PathBuf::from(path);
    }
    workspace_root()
        .map(|root| root.join("BookclerkFiles"))
        .unwrap_or_else(|_| std::path::PathBuf::from("BookclerkFiles"))
}

/// Wipe a Bookclerk files directory (config, DB, master.key, plugins, caches).
///
/// # Arguments
///
/// * `files_dir` - Bookclerk files directory to wipe and recreate.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn reset_files_dir(files_dir: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::{bail, Context};

    if files_dir.as_os_str().is_empty() {
        bail!("refusing to reset empty BOOKCLERK_FILES_DIR");
    }
    // Guard against wiping a filesystem root if misconfigured.
    if files_dir
        .components()
        .filter(|c| !matches!(c, std::path::Component::RootDir))
        .count()
        < 1
    {
        bail!(
            "refusing to reset suspicious files dir {}",
            files_dir.display()
        );
    }
    if files_dir.exists() {
        std::fs::remove_dir_all(files_dir)
            .with_context(|| format!("remove {}", files_dir.display()))?;
    }
    std::fs::create_dir_all(files_dir)
        .with_context(|| format!("recreate {}", files_dir.display()))?;
    Ok(())
}

/// Workspace version from `CARGO_PKG_VERSION` (matches `[workspace.package].version`).
#[must_use]
pub fn workspace_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_files_dir_recreates_empty_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("BookclerkFiles");
        std::fs::create_dir_all(root.join("plugins")).unwrap();
        std::fs::write(root.join("library.db"), b"stale").unwrap();
        std::fs::write(root.join("config.toml"), b"x = 1").unwrap();
        reset_files_dir(&root).expect("reset");
        assert!(root.is_dir());
        assert!(!root.join("library.db").exists());
        assert!(!root.join("config.toml").exists());
    }

    #[test]
    fn reset_files_dir_refuses_filesystem_root() {
        assert!(reset_files_dir(std::path::Path::new("/")).is_err());
    }
}
