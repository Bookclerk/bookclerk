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
    use std::path::Component;

    if files_dir.as_os_str().is_empty() {
        bail!("refusing empty files dir");
    }

    // Resolve cwd-relative and absolute operator paths without the spawn-helper
    // absolute-file contract.
    let absolute = if files_dir.is_absolute() {
        files_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .context("current_dir for relative BOOKCLERK_FILES_DIR")?
            .join(files_dir)
    };

    // Resolve `..` with filesystem semantics so a symlink component is not
    // lexically collapsed away (e.g. `link/../files` where `link -> real/child`
    // must target `real/files`, not a sibling of `link`).
    let normalized = resolve_files_dir_target(&absolute)?;

    // Guard against wiping a filesystem root (Unix `/`, Windows drive/UNC root).
    let depth = normalized
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if depth < 1 {
        bail!(
            "refusing to reset suspicious files dir {}",
            normalized.display()
        );
    }

    if let Ok(meta) = std::fs::symlink_metadata(&normalized) {
        if meta.file_type().is_symlink() {
            bail!(
                "refusing to reset files dir that is a symlink: {}",
                normalized.display()
            );
        }
        if meta.is_dir() {
            std::fs::remove_dir_all(&normalized)
                .with_context(|| format!("remove {}", normalized.display()))?;
        } else {
            std::fs::remove_file(&normalized)
                .with_context(|| format!("remove file {}", normalized.display()))?;
        }
    }
    std::fs::create_dir_all(&normalized)
        .with_context(|| format!("recreate {}", normalized.display()))?;
    Ok(())
}

/// Resolve an operator files-dir path with filesystem `..` semantics.
///
/// Existing prefixes are canonicalized before applying `..` so symlink
/// components are followed rather than lexically erased. A missing final
/// component is preserved (joined onto the resolved parent) without inventing
/// a different target. Ambiguous `..` after a symlink into a missing suffix is
/// refused.
fn resolve_files_dir_target(absolute: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::{bail, Context};
    use std::path::{Component, PathBuf};

    let mut cur = PathBuf::new();
    for comp in absolute.components() {
        match comp {
            Component::Prefix(p) => cur.push(p.as_os_str()),
            Component::RootDir => cur.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::Normal(c) => cur.push(c),
            Component::ParentDir => {
                match std::fs::symlink_metadata(&cur) {
                    Ok(_) => {
                        // Follow symlinks in the prefix, then take the real parent.
                        let mut canon = std::fs::canonicalize(&cur).with_context(|| {
                            format!("canonicalize files dir prefix {}", cur.display())
                        })?;
                        if !canon.pop() {
                            bail!(
                                "refusing files dir that escapes past root: {}",
                                absolute.display()
                            );
                        }
                        cur = canon;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        // Missing path: lexical pop only when no symlink ancestor
                        // remains in `cur` (otherwise `..` would be ambiguous).
                        if files_dir_prefix_has_symlink(&cur)? {
                            bail!(
                                "refusing ambiguous '..' after symlink toward missing path: {}",
                                cur.display()
                            );
                        }
                        if !cur.pop() {
                            bail!(
                                "refusing files dir that escapes past root: {}",
                                absolute.display()
                            );
                        }
                    }
                    Err(err) => {
                        return Err(err)
                            .with_context(|| format!("stat files dir prefix {}", cur.display()));
                    }
                }
            }
        }
    }
    Ok(cur)
}

/// True when any existing component of `path` is a symlink (leaf or ancestor).
fn files_dir_prefix_has_symlink(path: &std::path::Path) -> anyhow::Result<bool> {
    use anyhow::Context;

    let mut cursor = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("stat {}", cursor.display()));
            }
        }
        if !cursor.pop() {
            break;
        }
        // Stop at filesystem root / prefix.
        if cursor.components().all(|c| {
            matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Prefix(_)
            )
        }) {
            break;
        }
    }
    Ok(false)
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

    #[test]
    fn reset_files_dir_accepts_missing_leaf() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("missing-files");
        assert!(!root.exists());
        reset_files_dir(&root).expect("create missing");
        assert!(root.is_dir());
    }

    #[test]
    fn reset_files_dir_accepts_parent_dir_spelling() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("nested");
        std::fs::create_dir_all(nested.join("BookclerkFiles")).unwrap();
        std::fs::write(nested.join("BookclerkFiles").join("stale"), b"x").unwrap();
        let with_dotdot = nested
            .join("..")
            .join(nested.file_name().unwrap())
            .join("BookclerkFiles");
        reset_files_dir(&with_dotdot).expect("reset with ..");
        assert!(nested.join("BookclerkFiles").is_dir());
        assert!(!nested.join("BookclerkFiles").join("stale").exists());
    }

    #[test]
    fn reset_files_dir_accepts_cwd_relative() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("rel-files");
        std::fs::create_dir_all(root.join("plugins")).unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = reset_files_dir(std::path::Path::new("rel-files"));
        std::env::set_current_dir(&cwd).unwrap();
        result.expect("relative reset");
        assert!(root.is_dir());
        assert!(!root.join("plugins").exists());
    }

    #[cfg(unix)]
    #[test]
    fn reset_files_dir_refuses_symlink_leaf() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("keep"), b"safe").unwrap();
        let link = tmp.path().join("files-link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        assert!(reset_files_dir(&link).is_err());
        assert!(outside.path().join("keep").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn reset_files_dir_parent_dir_follows_symlink_prefix() {
        // `/tmp/test/link -> /tmp/test/real/child` + `link/../files` must reset
        // `/tmp/test/real/files`, never the unintended sibling `/tmp/test/files`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_child = tmp.path().join("real").join("child");
        std::fs::create_dir_all(&real_child).unwrap();
        let intended = tmp.path().join("real").join("files");
        std::fs::create_dir_all(&intended).unwrap();
        std::fs::write(intended.join("stale"), b"reset-me").unwrap();
        let unintended = tmp.path().join("files");
        std::fs::create_dir_all(&unintended).unwrap();
        std::fs::write(unintended.join("sentinel"), b"keep-me").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_child, &link).unwrap();

        let configured = link.join("..").join("files");
        reset_files_dir(&configured).expect("reset through symlink parent");

        assert!(
            intended.is_dir() && !intended.join("stale").exists(),
            "intended real/files must be reset"
        );
        assert!(
            unintended.join("sentinel").is_file(),
            "unintended sibling /files must never be deleted"
        );
        assert_eq!(
            std::fs::read(unintended.join("sentinel")).unwrap(),
            b"keep-me"
        );
    }
}
