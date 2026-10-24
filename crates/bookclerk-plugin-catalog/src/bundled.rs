//! Installs the platform plugins a release ships beside its host binaries.
//!
//! A platform release (`cargo package-platform`, the Docker image) carries
//! `plugins/<PluginKey fs-id>/` next to `bookclerk` and `bookclerkd`. A host
//! only trusts a platform plugin that `$FILES_DIR/install-ledger.json` records,
//! and a fresh or long-lived files directory (a container's `/config` volume)
//! has no such row. [`sync_bundled_platform`] closes that gap on startup: for
//! every [`PLATFORM_ARTIFACTS`] entry the bundle carries, it installs the tree
//! into `$FILES_DIR/plugins/<fs-id>` unless a trusted, byte-identical copy is
//! already there, and stamps the ledger row through [`stamp_platform_receipt`].
//!
//! The bundle is as trusted as the host executable it sits beside and is the
//! build of the platform plugins that host was released with, so it is
//! authoritative: a different installed build (newer or older) is replaced,
//! which keeps the host and its database adapter from the same release. The
//! bundled receipt is never consulted; the installed copy is re-hashed and
//! stamped by the host. Only the known platform packages are installed this
//! way, and a directory that another install owns under the same alias is left
//! alone.
//!
//! The steady state (everything current) takes no lock and writes nothing; the
//! host mutation lock is taken only when something has to be installed, and
//! the decision is re-checked under it.

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

/// Directory beside the host executable that holds bundled platform plugins.
pub const BUNDLED_PLUGINS_DIR: &str = "plugins";

/// Version recorded for a bundled manifest that declares none.
const UNVERSIONED: &str = "0.0.0";

/// Host-owned scratch directory under `$FILES_DIR/plugins` (shared with the installer).
const STAGING_DIR: &str = ".staging";

/// Name prefix of this module's staging trees under [`STAGING_DIR`].
const STAGING_PREFIX: &str = "bundled-";

/// Name suffix of this module's backup trees under [`STAGING_DIR`].
const BACKUP_SUFFIX: &str = ".bundled-backup";

/// What [`sync_bundled_platform`] did for one platform artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundledSyncAction {
    /// The files directory had no tree for this PluginKey; the bundled copy was installed.
    Installed,
    /// An untrusted or different tree was replaced by the bundled copy.
    Updated,
    /// The installed tree is trusted and byte-identical to the bundled copy.
    Current,
    /// Another directory under `$FILES_DIR/plugins` claims this alias; nothing was touched.
    AliasOccupied {
        /// The occupying directory.
        path: PathBuf,
    },
    /// This artifact could not be checked or installed; the others were still synced.
    Failed {
        /// Operator-facing reason.
        reason: String,
    },
}

/// Outcome of [`sync_bundled_platform`] for one bundled platform artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledSync {
    /// The platform package this row is about.
    pub artifact: &'static PlatformArtifact,
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
/// Artifacts the bundle does not carry get no row. When `bundle` is
/// `files_dir/plugins` itself (a release bundle used directly as the files
/// directory) there is nothing to copy and the result is empty. A failure for
/// one artifact is reported in its row and does not stop the others.
///
/// When an install is needed this blocks on the files directory's
/// [`PluginMutationLock`], so it must not be called while this process holds
/// that lock.
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
/// Returns an error only when the bundle or `files_dir/plugins` cannot be
/// resolved, or the mutation lock cannot be taken.
pub fn sync_bundled_platform(bundle: &Path, files_dir: &Path) -> Result<Vec<BundledSync>> {
    if !bundle.is_dir() {
        return Ok(Vec::new());
    }
    let plugins_root = files_dir.join("plugins");
    if plugins_root.is_dir() && bundle.canonicalize()? == plugins_root.canonicalize()? {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    let mut pending = Vec::new();
    for artifact in PLATFORM_ARTIFACTS {
        match inspect(artifact, bundle, files_dir, &plugins_root) {
            Ok(None) => {}
            Ok(Some(Inspection {
                plan: Plan::Install { .. },
                ..
            })) => pending.push(artifact),
            Ok(Some(found)) => rows.push(found.into_row()),
            Err(err) => rows.push(failed(artifact, &err)),
        }
    }
    if pending.is_empty() {
        return Ok(rows);
    }

    let _lock = PluginMutationLock::acquire(files_dir)?;
    fs::create_dir_all(&plugins_root)?;
    sweep_leftovers(&plugins_root);
    for artifact in pending {
        // Another process may have installed it while we waited for the lock.
        let row = match inspect(artifact, bundle, files_dir, &plugins_root) {
            Ok(None) => continue,
            Ok(Some(found)) => match found.plan {
                Plan::Install { existed } => match install(&found, files_dir, &plugins_root) {
                    Ok(()) => BundledSync {
                        artifact,
                        action: if existed {
                            BundledSyncAction::Updated
                        } else {
                            BundledSyncAction::Installed
                        },
                    },
                    Err(err) => failed(artifact, &err),
                },
                _ => found.into_row(),
            },
            Err(err) => failed(artifact, &err),
        };
        rows.push(row);
    }
    Ok(rows)
}

/// A [`BundledSyncAction::Failed`] row for `artifact`.
fn failed(artifact: &'static PlatformArtifact, err: &CatalogError) -> BundledSync {
    BundledSync {
        artifact,
        action: BundledSyncAction::Failed {
            reason: err.to_string(),
        },
    }
}

/// What has to happen for one artifact.
enum Plan {
    /// Trusted and byte-identical; nothing to do.
    Current,
    /// Another directory owns the alias; leave everything alone.
    AliasOccupied(PathBuf),
    /// Copy the bundled tree in; `existed` when a tree is already at the destination.
    Install {
        /// A tree (untrusted or a different build) occupies the destination.
        existed: bool,
    },
}

/// A bundled artifact, its digests, and what to do with it.
struct Inspection {
    /// The platform package.
    artifact: &'static PlatformArtifact,
    /// Bundled tree.
    src: PathBuf,
    /// Parsed bundled `plugin.toml`.
    manifest: PluginManifest,
    /// SHA-256 of the bundled `plugin.toml`.
    manifest_sha256: String,
    /// Payload root of the bundled tree.
    payload_root_sha256: String,
    /// Decision.
    plan: Plan,
}

impl Inspection {
    /// The result row for a plan that needs no install.
    fn into_row(self) -> BundledSync {
        let action = match self.plan {
            Plan::Current => BundledSyncAction::Current,
            Plan::AliasOccupied(path) => BundledSyncAction::AliasOccupied { path },
            Plan::Install { .. } => BundledSyncAction::Failed {
                reason: "install was not attempted".into(),
            },
        };
        BundledSync {
            artifact: self.artifact,
            action,
        }
    }
}

/// Reads and hashes the bundled tree for `artifact` and decides what to do.
///
/// `None` when the bundle does not carry the artifact.
fn inspect(
    artifact: &'static PlatformArtifact,
    bundle: &Path,
    files_dir: &Path,
    plugins_root: &Path,
) -> Result<Option<Inspection>> {
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
    let manifest_sha = manifest_sha256(&src)?;
    let payload_sha = payload_root_sha256(&src)?;

    let dest = plugins_root.join(&leaf);
    let plan = if let Some(path) = alias_occupant(plugins_root, &leaf, artifact.manifest_id)? {
        Plan::AliasOccupied(path)
    } else if fs::symlink_metadata(&dest).is_err() {
        Plan::Install { existed: false }
    } else if is_trusted_copy(&dest, files_dir, &manifest_sha, &payload_sha) {
        Plan::Current
    } else {
        Plan::Install { existed: true }
    };
    Ok(Some(Inspection {
        artifact,
        src,
        manifest,
        manifest_sha256: manifest_sha,
        payload_root_sha256: payload_sha,
        plan,
    }))
}

/// True when the ledger trusts `dest` as platform-bundled with exactly these digests.
fn is_trusted_copy(dest: &Path, files_dir: &Path, manifest_sha: &str, payload_sha: &str) -> bool {
    let Ok(manifest) = read_manifest(dest) else {
        return false;
    };
    let Ok(identity) = evaluate_install_in(dest, &manifest, Some(files_dir)) else {
        return false;
    };
    identity.provenance == PluginProvenance::PlatformBundled
        && identity
            .artifact
            .manifest_sha256
            .eq_ignore_ascii_case(manifest_sha)
        && identity
            .artifact
            .payload_root_sha256
            .eq_ignore_ascii_case(payload_sha)
}

/// Another top-level directory under `plugins_root` that claims `alias`.
///
/// A directory named after the alias counts even when its `plugin.toml` is
/// missing or unparseable (ownership cannot be established, so it is not
/// ours to shadow); any other directory counts when its manifest declares the
/// alias.
///
/// # Errors
///
/// Returns when `plugins_root` exists but cannot be listed.
fn alias_occupant(plugins_root: &Path, own_leaf: &str, alias: &str) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(plugins_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == own_leaf || name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if name.eq_ignore_ascii_case(alias) {
            return Ok(Some(path));
        }
        if let Ok(manifest) = read_manifest(&path) {
            if manifest.id.eq_ignore_ascii_case(alias) {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

/// Parses `<root>/plugin.toml`.
fn read_manifest(root: &Path) -> Result<PluginManifest> {
    let path = root.join("plugin.toml");
    let text = fs::read_to_string(&path)?;
    PluginManifest::parse(&text)
        .map_err(|err| CatalogError::message(format!("parse {}: {err}", path.display())))
}

/// Removes staging and backup trees a crashed sync left under `.staging`.
///
/// Called under the mutation lock. A backup is never the only good copy: if a
/// sync died after moving the old tree aside, this sync reinstalls from the
/// bundle anyway.
fn sweep_leftovers(plugins_root: &Path) {
    let Ok(staging) = require_mutable_child(plugins_root, &plugins_root.join(STAGING_DIR)) else {
        return;
    };
    let Ok(entries) = fs::read_dir(&staging) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(STAGING_PREFIX) || name.ends_with(BACKUP_SUFFIX)) {
            continue;
        }
        if let Ok(path) = require_mutable_child(plugins_root, &entry.path()) {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Copies the bundled tree in, swaps it into place, and stamps it.
///
/// The copy is staged under `.staging` and checked against the bundled
/// digests before the swap. A failed swap or stamp puts the previous tree
/// and ledger row back.
fn install(found: &Inspection, files_dir: &Path, plugins_root: &Path) -> Result<()> {
    let key = found.artifact.plugin_key();
    let leaf = key.fs_id();
    let staging_parent = require_mutable_child(plugins_root, &plugins_root.join(STAGING_DIR))?;
    fs::create_dir_all(&staging_parent)?;
    let staging = require_mutable_child(
        plugins_root,
        &staging_parent.join(format!("{STAGING_PREFIX}{leaf}.{}", std::process::id())),
    )?;
    if fs::symlink_metadata(&staging).is_ok() {
        fs::remove_dir_all(&staging)?;
    }
    let copied = copy_tree(&found.src, &staging).and_then(|()| {
        let manifest_ok = manifest_sha256(&staging)?.eq_ignore_ascii_case(&found.manifest_sha256);
        let payload_ok =
            payload_root_sha256(&staging)?.eq_ignore_ascii_case(&found.payload_root_sha256);
        if manifest_ok && payload_ok {
            Ok(())
        } else {
            Err(CatalogError::message(format!(
                "copy of bundled plugin {} does not match the bundled bytes",
                found.src.display()
            )))
        }
    });
    if let Err(err) = copied {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }

    let dest = require_mutable_child(plugins_root, &plugins_root.join(&leaf))?;
    let backup = require_mutable_child(
        plugins_root,
        &staging_parent.join(format!("{leaf}{BACKUP_SUFFIX}")),
    )?;
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
    let restore_tree = || {
        let _ = fs::remove_dir_all(&dest);
        if had_previous {
            let _ = fs::rename(&backup, &dest);
        }
    };
    if let Err(err) = fs::rename(&staging, &dest) {
        let _ = fs::remove_dir_all(&staging);
        restore_tree();
        return Err(err.into());
    }

    let previous_row = match InstallLedger::load(files_dir) {
        Ok(ledger) => ledger.get(&key).cloned(),
        Err(err) => {
            restore_tree();
            return Err(err);
        }
    };
    let version = found
        .manifest
        .version
        .clone()
        .unwrap_or_else(|| UNVERSIONED.into());
    // The staged copy hashed to the bundled bytes, so the bundled manifest is
    // the installed one.
    if let Err(err) = stamp_platform_receipt(
        &dest,
        files_dir,
        found.artifact.package_name,
        &found.manifest,
        &version,
    ) {
        restore_tree();
        let _ = restore_ledger_entry(files_dir, &key, previous_row);
        return Err(err);
    }
    if had_previous {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
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

    /// The local platform artifact row.
    fn local() -> &'static PlatformArtifact {
        &PLATFORM_ARTIFACTS[1]
    }

    /// Writes a tree for `id` with `version` and guest bytes `guest` at `root`.
    fn write_tree(root: &Path, id: &str, version: &str, guest: &[u8]) {
        let entrypoint = if id == "local" {
            "storage"
        } else {
            "databaseAdapter"
        };
        write_file(
            &root.join("plugin.toml"),
            format!(
                r#"
api_version = 3
id = "{id}"
version = "{version}"
runtime = "native"
command = "./guest"
entrypoints = ["{entrypoint}"]

[capabilities.network]
mode = "deny"
"#
            )
            .as_bytes(),
        )
        .unwrap();
        write_file(&root.join("guest"), guest).unwrap();
    }

    /// Writes a sqlite-shaped tree at `root`.
    fn write_sqlite(root: &Path, version: &str, guest: &[u8]) {
        write_tree(root, "sqlite", version, guest);
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

    /// Installed sqlite directory under `files`.
    fn sqlite_dest(files: &Path) -> PathBuf {
        files.join("plugins").join(sqlite().plugin_key().fs_id())
    }

    /// Discovery's view of the installed sqlite tree.
    fn provenance(files: &Path) -> PluginProvenance {
        let dest = sqlite_dest(files);
        let manifest = read_manifest(&dest).unwrap();
        evaluate_install_in(&dest, &manifest, Some(files))
            .unwrap()
            .provenance
    }

    /// The single row's action.
    fn only_action(rows: &[BundledSync]) -> &BundledSyncAction {
        assert_eq!(rows.len(), 1, "{rows:?}");
        &rows[0].action
    }

    #[test]
    fn installs_missing_tree_and_stamps_the_ledger() {
        let bundle = bundle_with_sqlite("0.1.0", b"guest-v1");
        let files = tempfile::tempdir().unwrap();
        let rows = sync_bundled_platform(bundle.path(), files.path()).unwrap();
        assert_eq!(rows[0].artifact, sqlite());
        assert_eq!(only_action(&rows), &BundledSyncAction::Installed);
        assert_eq!(provenance(files.path()), PluginProvenance::PlatformBundled);
        let staging = files.path().join("plugins").join(STAGING_DIR);
        assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
    }

    #[test]
    fn second_sync_is_current_and_takes_no_lock() {
        let bundle = bundle_with_sqlite("0.1.0", b"guest-v1");
        let files = tempfile::tempdir().unwrap();
        sync_bundled_platform(bundle.path(), files.path()).unwrap();
        // Holding the lock would deadlock a sync that tried to take it.
        let _held = PluginMutationLock::acquire(files.path()).unwrap();
        let rows = sync_bundled_platform(bundle.path(), files.path()).unwrap();
        assert_eq!(only_action(&rows), &BundledSyncAction::Current);
    }

    #[test]
    fn different_build_replaces_the_install() {
        let files = tempfile::tempdir().unwrap();
        sync_bundled_platform(bundle_with_sqlite("0.1.0", b"old").path(), files.path()).unwrap();
        let rows = sync_bundled_platform(bundle_with_sqlite("0.1.0", b"new").path(), files.path())
            .unwrap();
        assert_eq!(only_action(&rows), &BundledSyncAction::Updated);
        assert_eq!(
            fs::read(sqlite_dest(files.path()).join("guest")).unwrap(),
            b"new"
        );
        assert_eq!(provenance(files.path()), PluginProvenance::PlatformBundled);
    }

    #[test]
    fn bundle_is_authoritative_over_a_newer_install() {
        let files = tempfile::tempdir().unwrap();
        sync_bundled_platform(bundle_with_sqlite("0.2.0", b"newer").path(), files.path()).unwrap();
        let rows =
            sync_bundled_platform(bundle_with_sqlite("0.1.0", b"older").path(), files.path())
                .unwrap();
        assert_eq!(only_action(&rows), &BundledSyncAction::Updated);
        assert_eq!(
            fs::read(sqlite_dest(files.path()).join("guest")).unwrap(),
            b"older"
        );
    }

    #[test]
    fn modified_install_is_repaired() {
        let files = tempfile::tempdir().unwrap();
        let bundle = bundle_with_sqlite("0.1.0", b"bundled");
        sync_bundled_platform(bundle.path(), files.path()).unwrap();
        let dest = sqlite_dest(files.path());
        fs::write(dest.join("guest"), b"tampered").unwrap();
        assert_eq!(provenance(files.path()), PluginProvenance::Modified);
        let rows = sync_bundled_platform(bundle.path(), files.path()).unwrap();
        assert_eq!(only_action(&rows), &BundledSyncAction::Updated);
        assert_eq!(fs::read(dest.join("guest")).unwrap(), b"bundled");
        assert_eq!(provenance(files.path()), PluginProvenance::PlatformBundled);
    }

    #[test]
    fn alias_owned_by_another_tree_is_left_alone() {
        let files = tempfile::tempdir().unwrap();
        let other = files.path().join("plugins").join("third-party-db");
        write_sqlite(&other, "9.9.9", b"third-party");
        let rows =
            sync_bundled_platform(bundle_with_sqlite("0.1.0", b"guest").path(), files.path())
                .unwrap();
        assert_eq!(
            only_action(&rows),
            &BundledSyncAction::AliasOccupied { path: other }
        );
        assert!(!sqlite_dest(files.path()).exists());
        assert!(InstallLedger::load(files.path())
            .unwrap()
            .artifacts
            .is_empty());
    }

    #[test]
    fn unparseable_directory_named_after_the_alias_is_an_occupant() {
        let files = tempfile::tempdir().unwrap();
        let legacy = files.path().join("plugins").join("sqlite");
        fs::create_dir_all(&legacy).unwrap();
        let rows =
            sync_bundled_platform(bundle_with_sqlite("0.1.0", b"guest").path(), files.path())
                .unwrap();
        assert_eq!(
            only_action(&rows),
            &BundledSyncAction::AliasOccupied { path: legacy }
        );
    }

    #[test]
    fn one_broken_artifact_does_not_stop_the_other() {
        let bundle = tempfile::tempdir().unwrap();
        // sqlite's leaf holds a manifest with the wrong id; local is fine.
        write_tree(
            &bundle.path().join(sqlite().plugin_key().fs_id()),
            "local",
            "0.1.0",
            b"guest",
        );
        write_tree(
            &bundle.path().join(local().plugin_key().fs_id()),
            "local",
            "0.1.0",
            b"guest",
        );
        let files = tempfile::tempdir().unwrap();
        let rows = sync_bundled_platform(bundle.path(), files.path()).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let by = |a: &PlatformArtifact| rows.iter().find(|r| r.artifact == a).unwrap();
        assert!(matches!(
            by(sqlite()).action,
            BundledSyncAction::Failed { .. }
        ));
        assert_eq!(by(local()).action, BundledSyncAction::Installed);
        let ledger = InstallLedger::load(files.path()).unwrap();
        assert!(ledger.get(&local().plugin_key()).is_some());
        assert!(ledger.get(&sqlite().plugin_key()).is_none());
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

    #[test]
    fn leftovers_from_a_crashed_sync_are_swept() {
        let files = tempfile::tempdir().unwrap();
        let staging = files.path().join("plugins").join(STAGING_DIR);
        let stale = staging.join(format!("{STAGING_PREFIX}pk-dead.1"));
        let backup = staging.join(format!("pk-dead{BACKUP_SUFFIX}"));
        let foreign = staging.join("other.1");
        for dir in [&stale, &backup, &foreign] {
            fs::create_dir_all(dir).unwrap();
        }
        sync_bundled_platform(bundle_with_sqlite("0.1.0", b"g").path(), files.path()).unwrap();
        assert!(!stale.exists());
        assert!(!backup.exists());
        assert!(foreign.exists(), "the installer's own staging is not ours");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_in_bundle_is_refused() {
        let bundle = bundle_with_sqlite("0.1.0", b"guest");
        let leaf = bundle.path().join(sqlite().plugin_key().fs_id());
        std::os::unix::fs::symlink("/etc/passwd", leaf.join("link")).unwrap();
        let files = tempfile::tempdir().unwrap();
        let rows = sync_bundled_platform(bundle.path(), files.path()).unwrap();
        assert!(matches!(
            only_action(&rows),
            BundledSyncAction::Failed { .. }
        ));
        assert!(!sqlite_dest(files.path()).exists());
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
