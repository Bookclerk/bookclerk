//! Installs the platform plugins a release ships beside its host binaries.
//!
//! A platform release (`cargo package-platform`, the Docker image) carries
//! `plugins/<PluginKey fs-id>/` next to `bookclerk` and `bookclerkd`. A host
//! only trusts a platform plugin that `$FILES_DIR/install-ledger.json` records,
//! and a fresh or long-lived files directory (a container's `/config` volume)
//! has no such row. [`sync_bundled_platform`] closes that gap on startup: for
//! every [`PLATFORM_ARTIFACTS`] entry the bundle carries, it installs the tree
//! into `$FILES_DIR/plugins/<fs-id>` when it is missing, untrusted, or differs
//! from the bundled copy, and stamps the ledger row through
//! [`stamp_platform_receipt`].
//!
//! The bundle is as trusted as the host executable it sits beside, so its
//! receipt is never consulted: the installed copy is re-hashed and stamped by
//! the host. Only the two known platform packages are ever installed this way,
//! a newer trusted install is kept, and a tree that another PluginKey owns
//! under the same alias is left alone.

use std::fs;
use std::path::{Path, PathBuf};

use bookclerk_plugin_manifest::PluginManifest;

use crate::error::{CatalogError, Result};
use crate::evaluate::{evaluate_install_in, stamp_platform_receipt};
use crate::extract::{require_mutable_child, safe_join};
use crate::identity::{PlatformArtifact, PluginProvenance, PLATFORM_ARTIFACTS};
use crate::ledger::{restore_ledger_entry, InstallLedger};
use crate::mutation_lock::PluginMutationLock;
use crate::payload::{manifest_sha256, payload_root_sha256};
use crate::version::Version;

/// Directory beside the host executable that holds bundled platform plugins.
pub const BUNDLED_PLUGINS_DIR: &str = "plugins";

/// Version recorded for a bundled manifest that declares none.
const UNVERSIONED: &str = "0.0.0";

/// What [`sync_bundled_platform`] did for one platform artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundledSyncAction {
    /// The files directory had no tree for this PluginKey; the bundled copy was installed.
    Installed,
    /// An untrusted or different tree was replaced by the bundled copy.
    Updated,
    /// The installed tree is trusted and byte-identical to the bundled copy.
    Current,
    /// A trusted install with a newer version than the bundle was kept.
    KeptNewer {
        /// Version of the install that was kept.
        installed: String,
    },
    /// Another tree under `$FILES_DIR/plugins` claims this alias; nothing was touched.
    AliasOccupied {
        /// The occupying install directory.
        path: PathBuf,
    },
}

/// Outcome of [`sync_bundled_platform`] for one bundled platform artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledSync {
    /// The platform package this row is about.
    pub artifact: &'static PlatformArtifact,
    /// Version the bundled `plugin.toml` declares (`0.0.0` when it declares none).
    pub bundled_version: String,
    /// What was done.
    pub action: BundledSyncAction,
}

/// Bundled plugin directory for the host executable at `exe`, when it exists.
///
/// # Arguments
///
/// * `exe` - Path of the running host binary (`std::env::current_exe()`).
///
/// # Returns
///
/// `<dir of exe>/plugins` when that is a real directory (not a symlink);
/// `None` otherwise, which is the normal case for development builds.
#[must_use]
pub fn bundled_plugins_dir_beside(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?.join(BUNDLED_PLUGINS_DIR);
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => Some(dir),
        _ => None,
    }
}

/// Installs bundled platform plugins from `bundle` into `files_dir`.
///
/// Runs under the files directory's [`PluginMutationLock`]. Artifacts the
/// bundle does not carry are skipped and get no row in the result. When
/// `bundle` is `files_dir/plugins` itself (a release bundle used directly as
/// the files directory) there is nothing to copy and the result is empty.
///
/// # Arguments
///
/// * `bundle` - Directory holding `<PluginKey fs-id>/plugin.toml` trees.
/// * `files_dir` - Host `$BOOKCLERK_FILES_DIR` that owns `install-ledger.json`.
///
/// # Returns
///
/// One [`BundledSync`] per platform artifact found in `bundle`.
///
/// # Errors
///
/// Returns an error when the lock cannot be taken, a bundled tree is not the
/// platform package its directory names, contains a symlink, cannot be
/// copied, or the copy does not hash to the bundled bytes, or the ledger
/// cannot be written. A failed replacement puts the previous tree and ledger
/// row back before returning.
pub fn sync_bundled_platform(bundle: &Path, files_dir: &Path) -> Result<Vec<BundledSync>> {
    let mut out = Vec::new();
    if !bundle.is_dir() {
        return Ok(out);
    }
    let _lock = PluginMutationLock::acquire(files_dir)?;
    let plugins_root = files_dir.join("plugins");
    fs::create_dir_all(&plugins_root)?;
    if bundle.canonicalize()? == plugins_root.canonicalize()? {
        return Ok(out);
    }
    for artifact in PLATFORM_ARTIFACTS {
        if let Some(row) = sync_one(artifact, bundle, files_dir, &plugins_root)? {
            out.push(row);
        }
    }
    Ok(out)
}

/// Syncs one platform artifact; `None` when the bundle does not carry it.
fn sync_one(
    artifact: &'static PlatformArtifact,
    bundle: &Path,
    files_dir: &Path,
    plugins_root: &Path,
) -> Result<Option<BundledSync>> {
    let leaf = artifact.plugin_key().fs_id();
    let src = bundle.join(&leaf);
    match fs::symlink_metadata(&src) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(CatalogError::message(format!(
                "bundled platform plugin {} is not a directory",
                src.display()
            )))
        }
        Err(_) => return Ok(None),
    }
    let manifest = read_manifest(&src)?;
    if manifest.id != artifact.manifest_id {
        return Err(CatalogError::message(format!(
            "bundled plugin at {} declares id `{}`, expected `{}`",
            src.display(),
            manifest.id,
            artifact.manifest_id
        )));
    }
    let bundled_version = manifest
        .version
        .clone()
        .unwrap_or_else(|| UNVERSIONED.into());
    let src_manifest_sha = manifest_sha256(&src)?;
    let src_payload_sha = payload_root_sha256(&src)?;
    let row = |action| {
        Ok(Some(BundledSync {
            artifact,
            bundled_version: bundled_version.clone(),
            action,
        }))
    };

    if let Some(path) = alias_occupant(plugins_root, &leaf, artifact.manifest_id) {
        return row(BundledSyncAction::AliasOccupied { path });
    }

    let dest = plugins_root.join(&leaf);
    let existed = fs::symlink_metadata(&dest).is_ok();
    if existed {
        if let Some(installed) = trusted_install(&dest, files_dir) {
            if installed
                .manifest_sha256
                .eq_ignore_ascii_case(&src_manifest_sha)
                && installed
                    .payload_root_sha256
                    .eq_ignore_ascii_case(&src_payload_sha)
            {
                return row(BundledSyncAction::Current);
            }
            if is_newer(&installed.version, &bundled_version) {
                return row(BundledSyncAction::KeptNewer {
                    installed: installed.version,
                });
            }
        }
    }

    replace_tree(
        artifact,
        &src,
        &dest,
        plugins_root,
        (&src_manifest_sha, &src_payload_sha),
    )?;
    let installed = read_manifest(&dest)?;
    stamp_or_restore(artifact, &dest, files_dir, plugins_root, &installed)?;
    row(if existed {
        BundledSyncAction::Updated
    } else {
        BundledSyncAction::Installed
    })
}

/// Digests and version of an install the host already trusts as platform-bundled.
struct TrustedInstall {
    /// Declared version (`0.0.0` when none).
    version: String,
    /// SHA-256 of the installed `plugin.toml`.
    manifest_sha256: String,
    /// Payload root of the installed tree.
    payload_root_sha256: String,
}

/// The install at `dest` when the ledger trusts it as platform-bundled.
fn trusted_install(dest: &Path, files_dir: &Path) -> Option<TrustedInstall> {
    let manifest = read_manifest(dest).ok()?;
    let identity = evaluate_install_in(dest, &manifest, Some(files_dir)).ok()?;
    if identity.provenance != PluginProvenance::PlatformBundled {
        return None;
    }
    Some(TrustedInstall {
        version: identity.artifact.version,
        manifest_sha256: identity.artifact.manifest_sha256,
        payload_root_sha256: identity.artifact.payload_root_sha256,
    })
}

/// True when `installed` parses as a strictly newer version than `bundled`.
fn is_newer(installed: &str, bundled: &str) -> bool {
    match (Version::parse(installed), Version::parse(bundled)) {
        (Some(installed), Some(bundled)) => installed > bundled,
        _ => false,
    }
}

/// Another top-level install under `plugins_root` whose `plugin.toml` uses `alias`.
fn alias_occupant(plugins_root: &Path, own_leaf: &str, alias: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(plugins_root).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == own_leaf || name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(manifest) = read_manifest(&path) {
            if manifest.id.eq_ignore_ascii_case(alias) {
                return Some(path);
            }
        }
    }
    None
}

/// Parses `<root>/plugin.toml`.
fn read_manifest(root: &Path) -> Result<PluginManifest> {
    let path = root.join("plugin.toml");
    let text = fs::read_to_string(&path)?;
    PluginManifest::parse(&text)
        .map_err(|err| CatalogError::message(format!("parse {}: {err}", path.display())))
}

/// Copies `src` into a staging tree, checks its digests, and swaps it in at `dest`.
///
/// The previous tree, if any, is moved to `.staging` first and put back when
/// the swap fails. On success the backup is left for [`stamp_or_restore`].
fn replace_tree(
    artifact: &PlatformArtifact,
    src: &Path,
    dest: &Path,
    plugins_root: &Path,
    (manifest_sha, payload_sha): (&str, &str),
) -> Result<()> {
    let staging_parent = require_mutable_child(plugins_root, &plugins_root.join(".staging"))?;
    fs::create_dir_all(&staging_parent)?;
    let leaf = artifact.plugin_key().fs_id();
    let staging = require_mutable_child(
        plugins_root,
        &staging_parent.join(format!("bundled-{leaf}.{}", std::process::id())),
    )?;
    if fs::symlink_metadata(&staging).is_ok() {
        fs::remove_dir_all(&staging)?;
    }
    let copied = copy_tree(src, &staging).and_then(|()| {
        let manifest_ok = manifest_sha256(&staging)?.eq_ignore_ascii_case(manifest_sha);
        let payload_ok = payload_root_sha256(&staging)?.eq_ignore_ascii_case(payload_sha);
        if manifest_ok && payload_ok {
            Ok(())
        } else {
            Err(CatalogError::message(format!(
                "copy of bundled plugin {} does not match the bundled bytes",
                src.display()
            )))
        }
    });
    if let Err(err) = copied {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }

    let dest = require_mutable_child(plugins_root, dest)?;
    let backup = backup_path(plugins_root, &leaf)?;
    if fs::symlink_metadata(&backup).is_ok() {
        fs::remove_dir_all(&backup)?;
    }
    let had_previous = fs::symlink_metadata(&dest).is_ok();
    if had_previous {
        if let Err(err) = fs::rename(&dest, &backup) {
            let _ = fs::remove_dir_all(&staging);
            return Err(err.into());
        }
    }
    if let Err(err) = fs::rename(&staging, &dest) {
        let _ = fs::remove_dir_all(&staging);
        if had_previous {
            let _ = fs::rename(&backup, &dest);
        }
        return Err(err.into());
    }
    Ok(())
}

/// Stamps the receipt and ledger row for a freshly swapped-in tree.
///
/// On failure the new tree is removed and the previous tree and ledger row
/// are put back. On success the backup is deleted.
fn stamp_or_restore(
    artifact: &PlatformArtifact,
    dest: &Path,
    files_dir: &Path,
    plugins_root: &Path,
    manifest: &PluginManifest,
) -> Result<()> {
    let key = artifact.plugin_key();
    let backup = backup_path(plugins_root, &key.fs_id())?;
    let previous_row = InstallLedger::load(files_dir)?.get(&key).cloned();
    let version = manifest
        .version
        .clone()
        .unwrap_or_else(|| UNVERSIONED.into());
    match stamp_platform_receipt(dest, files_dir, artifact.package_name, manifest, &version) {
        Ok(_) => {
            if fs::symlink_metadata(&backup).is_ok() {
                let _ = fs::remove_dir_all(&backup);
            }
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_dir_all(dest);
            if fs::symlink_metadata(&backup).is_ok() {
                let _ = fs::rename(&backup, dest);
            }
            let _ = restore_ledger_entry(files_dir, &key, previous_row);
            Err(err)
        }
    }
}

/// `.staging/<fs-id>.bundled-backup` under `plugins_root`, refusing a symlinked leaf.
fn backup_path(plugins_root: &Path, leaf: &str) -> Result<PathBuf> {
    require_mutable_child(
        plugins_root,
        &plugins_root
            .join(".staging")
            .join(format!("{leaf}.bundled-backup")),
    )
}

/// Copies regular files and directories from `src` to `dest`, refusing symlinks.
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|err| CatalogError::message(err.to_string()))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|err| CatalogError::message(err.to_string()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = safe_join(dest, rel)?;
        let kind = entry.file_type();
        if kind.is_dir() {
            fs::create_dir_all(&out)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &out)?;
        } else {
            return Err(CatalogError::message(format!(
                "bundled plugin entry {} is not a regular file or directory",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::write_file;
    use crate::identity::PluginKey;

    /// The sqlite platform artifact row.
    fn sqlite() -> &'static PlatformArtifact {
        &PLATFORM_ARTIFACTS[0]
    }

    /// Writes a sqlite-shaped tree with `version` and guest bytes `guest` at `root`.
    fn write_sqlite(root: &Path, version: &str, guest: &[u8]) {
        write_file(
            &root.join("plugin.toml"),
            format!(
                r#"
api_version = 3
id = "sqlite"
version = "{version}"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"
"#
            )
            .as_bytes(),
        )
        .unwrap();
        write_file(&root.join("guest"), guest).unwrap();
    }

    /// A bundle dir holding a sqlite tree at its canonical leaf.
    fn bundle_with_sqlite(version: &str, guest: &[u8]) -> tempfile::TempDir {
        let bundle = tempfile::tempdir().unwrap();
        write_sqlite(
            &bundle.path().join(sqlite().plugin_key().fs_id()),
            version,
            guest,
        );
        bundle
    }

    /// Discovery's view of the installed sqlite tree.
    fn provenance(files: &Path) -> PluginProvenance {
        let dest = files.join("plugins").join(sqlite().plugin_key().fs_id());
        let manifest = read_manifest(&dest).unwrap();
        evaluate_install_in(&dest, &manifest, Some(files))
            .unwrap()
            .provenance
    }

    #[test]
    fn installs_missing_tree_and_stamps_the_ledger() {
        let bundle = bundle_with_sqlite("0.1.0", b"guest-v1");
        let files = tempfile::tempdir().unwrap();
        let rows = sync_bundled_platform(bundle.path(), files.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].artifact, sqlite());
        assert_eq!(rows[0].action, BundledSyncAction::Installed);
        assert_eq!(provenance(files.path()), PluginProvenance::PlatformBundled);
        let staging = files.path().join("plugins").join(".staging");
        assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
    }

    #[test]
    fn second_sync_is_current() {
        let bundle = bundle_with_sqlite("0.1.0", b"guest-v1");
        let files = tempfile::tempdir().unwrap();
        sync_bundled_platform(bundle.path(), files.path()).unwrap();
        let rows = sync_bundled_platform(bundle.path(), files.path()).unwrap();
        assert_eq!(rows[0].action, BundledSyncAction::Current);
    }

    #[test]
    fn same_version_rebuild_replaces_the_install() {
        let files = tempfile::tempdir().unwrap();
        sync_bundled_platform(bundle_with_sqlite("0.1.0", b"old").path(), files.path()).unwrap();
        let rows = sync_bundled_platform(bundle_with_sqlite("0.1.0", b"new").path(), files.path())
            .unwrap();
        assert_eq!(rows[0].action, BundledSyncAction::Updated);
        let dest = files
            .path()
            .join("plugins")
            .join(sqlite().plugin_key().fs_id());
        assert_eq!(fs::read(dest.join("guest")).unwrap(), b"new");
        assert_eq!(provenance(files.path()), PluginProvenance::PlatformBundled);
    }

    #[test]
    fn newer_trusted_install_is_kept() {
        let files = tempfile::tempdir().unwrap();
        sync_bundled_platform(bundle_with_sqlite("0.2.0", b"newer").path(), files.path()).unwrap();
        let rows =
            sync_bundled_platform(bundle_with_sqlite("0.1.0", b"older").path(), files.path())
                .unwrap();
        assert_eq!(
            rows[0].action,
            BundledSyncAction::KeptNewer {
                installed: "0.2.0".into()
            }
        );
        let dest = files
            .path()
            .join("plugins")
            .join(sqlite().plugin_key().fs_id());
        assert_eq!(fs::read(dest.join("guest")).unwrap(), b"newer");
    }

    #[test]
    fn modified_install_is_repaired_even_when_newer() {
        let files = tempfile::tempdir().unwrap();
        sync_bundled_platform(bundle_with_sqlite("0.2.0", b"newer").path(), files.path()).unwrap();
        let dest = files
            .path()
            .join("plugins")
            .join(sqlite().plugin_key().fs_id());
        fs::write(dest.join("guest"), b"tampered").unwrap();
        assert_eq!(provenance(files.path()), PluginProvenance::Modified);
        let rows =
            sync_bundled_platform(bundle_with_sqlite("0.1.0", b"bundled").path(), files.path())
                .unwrap();
        assert_eq!(rows[0].action, BundledSyncAction::Updated);
        assert_eq!(fs::read(dest.join("guest")).unwrap(), b"bundled");
        assert_eq!(provenance(files.path()), PluginProvenance::PlatformBundled);
    }

    #[test]
    fn alias_owned_by_another_tree_is_left_alone() {
        let files = tempfile::tempdir().unwrap();
        let other = files.path().join("plugins").join("sqlite");
        write_sqlite(&other, "9.9.9", b"third-party");
        let rows =
            sync_bundled_platform(bundle_with_sqlite("0.1.0", b"guest").path(), files.path())
                .unwrap();
        assert_eq!(
            rows[0].action,
            BundledSyncAction::AliasOccupied { path: other }
        );
        assert!(!files
            .path()
            .join("plugins")
            .join(sqlite().plugin_key().fs_id())
            .exists());
        assert!(InstallLedger::load(files.path())
            .unwrap()
            .artifacts
            .is_empty());
    }

    #[test]
    fn wrong_id_under_a_platform_leaf_is_refused() {
        let bundle = tempfile::tempdir().unwrap();
        let leaf = bundle.path().join(sqlite().plugin_key().fs_id());
        write_sqlite(&leaf, "0.1.0", b"guest");
        let text = fs::read_to_string(leaf.join("plugin.toml"))
            .unwrap()
            .replace("id = \"sqlite\"", "id = \"local\"");
        fs::write(leaf.join("plugin.toml"), text).unwrap();
        let files = tempfile::tempdir().unwrap();
        assert!(sync_bundled_platform(bundle.path(), files.path()).is_err());
        assert!(InstallLedger::load(files.path())
            .unwrap()
            .artifacts
            .is_empty());
    }

    #[test]
    fn non_platform_trees_in_the_bundle_are_ignored() {
        let bundle = tempfile::tempdir().unwrap();
        let key = PluginKey::platform("bookclerk-plugin-source-echo", "echo").unwrap();
        write_sqlite(&bundle.path().join(key.fs_id()), "0.1.0", b"guest");
        let files = tempfile::tempdir().unwrap();
        let rows = sync_bundled_platform(bundle.path(), files.path()).unwrap();
        assert!(rows.is_empty());
        assert!(InstallLedger::load(files.path())
            .unwrap()
            .artifacts
            .is_empty());
    }

    #[test]
    fn bundle_that_is_the_files_dir_is_a_no_op() {
        let files = tempfile::tempdir().unwrap();
        let plugins = files.path().join("plugins");
        write_sqlite(&plugins.join(sqlite().plugin_key().fs_id()), "0.1.0", b"g");
        let rows = sync_bundled_platform(&plugins, files.path()).unwrap();
        assert!(rows.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_in_bundle_is_refused() {
        let bundle = bundle_with_sqlite("0.1.0", b"guest");
        let leaf = bundle.path().join(sqlite().plugin_key().fs_id());
        std::os::unix::fs::symlink("/etc/passwd", leaf.join("link")).unwrap();
        let files = tempfile::tempdir().unwrap();
        assert!(sync_bundled_platform(bundle.path(), files.path()).is_err());
        assert!(!files
            .path()
            .join("plugins")
            .join(sqlite().plugin_key().fs_id())
            .exists());
    }

    #[test]
    fn bundled_dir_is_found_beside_the_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bookclerkd");
        assert_eq!(bundled_plugins_dir_beside(&exe), None);
        fs::create_dir(dir.path().join(BUNDLED_PLUGINS_DIR)).unwrap();
        assert_eq!(
            bundled_plugins_dir_beside(&exe),
            Some(dir.path().join(BUNDLED_PLUGINS_DIR))
        );
    }
}
