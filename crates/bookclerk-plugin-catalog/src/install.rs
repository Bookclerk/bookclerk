//! Secure plugin installer state machine.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;

use crate::coordinate::{PackageCoordinate, RegistrySource};
use crate::error::{CatalogError, Result};
use crate::extract::{
    extract_archive, require_mutable_child, require_under, safe_join, sha256_file, write_file,
};
use crate::identity::{PluginKey, PluginProvenance};
use crate::kind::RuntimeIdentity;
use crate::ledger::{record_install, restore_ledger_entry, InstallLedger, InstallLedgerEntry};
use crate::manifest::{parse_sha256_hex, BookclerkPackageManifest};
use crate::mutation_lock::{acquire_plugins_lock, PluginMutationLock};
use crate::receipt::InstallReceipt;
use crate::target::{host_bookclerk_target, select_target, ArchiveFormat};
use crate::trust::TrustPolicy;
use bookclerk_plugin_manifest::{NetworkMode, PluginFamily, PluginManifest};

/// Host-owned directory under `$FILES_DIR` for held plugin-state during remove.
///
/// Not inside a plugin-controlled install tree. Used so `--purge-state` can
/// rename `plugin-state/<fs-id>` aside and restore it on rollback.
pub const PLUGIN_HOLD_DIR: &str = ".plugin-hold";

/// Download / install limits.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Maximum HTTP redirects permitted while fetching a package artifact (`5`).
pub const MAX_REDIRECTS: u32 = 5;
/// Hard cap on downloaded artifact size in bytes.
pub const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Options for [`Installer::install_from_manifest`].
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Directory under which plugin install folders are created.
    pub plugins_root: PathBuf,
    /// Host target triple used to select release artifacts.
    pub target: Option<String>,
    /// When true, resolve and verify without writing an install.
    pub dry_run: bool,
    /// When true, overwrite an existing install of the **same** [`PluginKey`]
    /// at the **same** alias.
    ///
    /// Does not allow a different PluginKey to seize an already-used alias,
    /// and does not rename an installed PluginKey's alias. Alias changes are
    /// not supported by ordinary install/update/replace.
    pub replace: bool,
    /// When true, refuse network fetches (local/cache only).
    pub offline: bool,
    /// Trust policy applied to community packages (digest still required).
    pub trust: TrustPolicy,
    /// Skip health spawn (caller runs health separately).
    pub skip_health: bool,
    /// When true, persist a covering consent grant after install.
    pub approve_capabilities: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            plugins_root: PathBuf::from("plugins"),
            target: None,
            dry_run: false,
            replace: false,
            offline: false,
            trust: TrustPolicy::default(),
            skip_health: true,
            approve_capabilities: false,
        }
    }
}

/// Result of a successful (or dry-run) install.
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    /// Filesystem path of the installed plugin directory.
    pub plugin_root: PathBuf,
    /// Install receipt written under the plugin root after activation.
    pub receipt: InstallReceipt,
    /// When true, resolve and verify without writing an install.
    pub dry_run: bool,
    /// Previous install kept aside until [`Installer::commit`] (or restored by
    /// [`Installer::rollback`]). Present only for replace installs.
    pub previous: Option<PathBuf>,
    /// Ledger row that existed for this PluginKey before activation, if any.
    pub previous_ledger: Option<InstallLedgerEntry>,
    /// Files directory that owns `install-ledger.json` (`plugins_root` parent).
    pub files_dir: Option<PathBuf>,
}

/// Installer: resolve → download → verify → extract → activate.
pub struct Installer;

impl Installer {
    /// PluginKey this installer would assign for `coordinate` + runtime alias.
    ///
    /// Host/CLI alias preflight uses the same identity the activate step will
    /// write so a foreign occupant cannot be compared against a different key.
    ///
    /// # Errors
    ///
    /// Returns when the coordinate cannot form a canonical [`PluginKey`].
    pub fn plugin_key_for(
        coordinate: &PackageCoordinate,
        runtime_id: &str,
        plugins_root: &Path,
    ) -> Result<PluginKey> {
        match &coordinate.source {
            RegistrySource::LocalArchive => {
                PluginKey::from_install_path(Path::new(&coordinate.name), runtime_id).or_else(
                    |_| PluginKey::from_install_path(&plugins_root.join(runtime_id), runtime_id),
                )
            }
            _ => PluginKey::from_coordinate(coordinate, runtime_id),
        }
    }

    /// Coordinate used when installing from a local archive path.
    #[must_use]
    pub fn local_archive_coordinate(
        archive: &Path,
        manifest: &BookclerkPackageManifest,
    ) -> PackageCoordinate {
        PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: manifest
                .coordinate
                .as_ref()
                .map(|c| c.version.clone())
                .unwrap_or_else(|| "0.0.0".into()),
        }
    }

    /// Install from an already-validated package manifest (fixture / adapter output).
    ///
    /// # Arguments
    ///
    /// * `manifest` - Validated Bookclerk package manifest for this version.
    /// * `coordinate` - Source-qualified coordinate being installed.
    /// * `opts` - Plugins root, target, dry-run, replace, trust, and consent flags.
    ///
    /// # Returns
    ///
    /// [`InstallOutcome`] with plugin root, receipt, and optional previous install.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when validation, download, digest, extract, or
    /// activation fails (or when offline/trust policy refuses the package).
    pub fn install_from_manifest(
        manifest: &BookclerkPackageManifest,
        coordinate: &PackageCoordinate,
        opts: &InstallOptions,
    ) -> Result<InstallOutcome> {
        let _lock = acquire_plugins_lock(&opts.plugins_root)?;
        Self::install_from_manifest_locked(manifest, coordinate, opts)
    }

    /// Install while already holding the host-local plugin mutation lock.
    ///
    /// Production callers that also run configured-discovery preflight, health,
    /// and commit/rollback must acquire [`PluginMutationLock`] once for
    /// `$FILES_DIR` and pass it here so the lock covers the full local
    /// transaction. Do not acquire a second lock around health.
    ///
    /// # Errors
    ///
    /// Returns when `lock` does not cover `opts.plugins_root`, or when
    /// validation, download, digest, extract, or activation fails.
    pub fn install_from_manifest_with_lock(
        lock: &PluginMutationLock,
        manifest: &BookclerkPackageManifest,
        coordinate: &PackageCoordinate,
        opts: &InstallOptions,
    ) -> Result<InstallOutcome> {
        lock.require_plugins_root(&opts.plugins_root)?;
        Self::install_from_manifest_locked(manifest, coordinate, opts)
    }

    /// Install/update body. The caller already holds the host mutation lock.
    fn install_from_manifest_locked(
        manifest: &BookclerkPackageManifest,
        coordinate: &PackageCoordinate,
        opts: &InstallOptions,
    ) -> Result<InstallOutcome> {
        manifest.validate_for_install()?;
        let artifact = select_target(
            &manifest.artifacts,
            |a| a.target.as_str(),
            opts.target.as_deref(),
        )?;
        let target = artifact.bookclerk_target();
        let runtime = manifest.runtime();
        validate_plugin_id(&runtime.id)?;

        let incoming_key = Self::plugin_key_for(coordinate, &runtime.id, &opts.plugins_root)?;
        let dest = safe_join(&opts.plugins_root, Path::new(&incoming_key.fs_id()))?;
        let files_dir = opts.plugins_root.parent().map(Path::to_path_buf);
        // Fail closed on a malformed/unreadable trust ledger *before* any
        // install-tree mutation (staging, backup rename, activate).
        let ledger = match files_dir.as_ref() {
            Some(dir) => Some(InstallLedger::load(dir)?),
            None => None,
        };
        reject_alias_collision(
            &opts.plugins_root,
            ledger.as_ref(),
            &incoming_key,
            &runtime.id,
            opts.replace,
        )?;
        reject_alias_change(
            &opts.plugins_root,
            &dest,
            ledger.as_ref(),
            &incoming_key,
            &runtime.id,
        )?;
        let previous_ledger = ledger
            .as_ref()
            .and_then(|loaded| loaded.get(&incoming_key).cloned());
        if dest.exists() {
            if let Ok(existing) = InstallReceipt::load(&dest) {
                if existing.runtime.id.eq_ignore_ascii_case(&runtime.id)
                    && existing.runtime.kind != runtime.kind
                {
                    return Err(CatalogError::message(format!(
                        "plugin id `{}` is already installed as a {} plugin at {}; \
                         ids must be unique across kinds within one host plugin namespace",
                        runtime.id,
                        existing.runtime.kind.as_str(),
                        dest.display()
                    )));
                }
                if let Ok(existing_key) = existing.plugin_key() {
                    if existing_key != incoming_key {
                        return Err(CatalogError::message(format!(
                            "install directory {} belongs to `{existing_key}`; refusing `{incoming_key}`",
                            dest.display()
                        )));
                    }
                }
                let conflict = existing.runtime != runtime
                    || existing.coordinate.source.kind_name() != coordinate.source.kind_name()
                    || existing.coordinate.name != coordinate.name;
                if conflict && !opts.replace {
                    return Err(CatalogError::message(format!(
                        "runtime id `{}` already installed from {}; pass --replace",
                        runtime.id, existing.coordinate
                    )));
                }
                // Capability widening requires explicit approval even on --replace /
                // updates (replace must not bypass --approve-capabilities).
                if existing.requested_sandbox.network != manifest.sandbox.network
                    && !opts.approve_capabilities
                {
                    return Err(CatalogError::message(format!(
                        "update requests network `{}` (was `{}`); pass --approve-capabilities",
                        manifest.sandbox.network, existing.requested_sandbox.network
                    )));
                }
            } else if !opts.replace && dest.join("plugin.toml").exists() {
                return Err(CatalogError::message(format!(
                    "plugin `{}` already exists without receipt; pass --replace",
                    runtime.id
                )));
            }
        }

        opts.trust.check_unverified_publisher_allowed()?;

        let expected = parse_sha256_hex(&artifact.archive_sha256)?;
        let _ = expected;

        if opts.dry_run {
            let receipt = build_receipt(
                manifest,
                coordinate,
                artifact,
                &target,
                None,
                &dest,
                &incoming_key,
                PluginProvenance::VerifiedInstalled,
                true,
            )?;
            return Ok(InstallOutcome {
                plugin_root: dest,
                receipt,
                dry_run: true,
                previous: None,
                previous_ledger: None,
                files_dir,
            });
        }

        // Trusted root is plugins_root — create it before canonicalize-based checks.
        // (Previously create_dir_all(.staging) created this as a side effect.)
        fs::create_dir_all(&opts.plugins_root)?;
        let staging_parent_path = opts.plugins_root.join(".staging");
        // Never treat a pre-existing `.staging` symlink as the trusted root.
        let staging_parent = require_mutable_child(&opts.plugins_root, &staging_parent_path)?;
        fs::create_dir_all(&staging_parent)?;
        let staging_parent = require_mutable_child(&opts.plugins_root, &staging_parent_path)?;
        let staging_path = staging_parent.join(format!("{}.{}", runtime.id, std::process::id()));
        let staging = require_mutable_child(&opts.plugins_root, &staging_path)?;
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;

        let archive_path = staging.join("download.archive");
        let archive_path = require_mutable_child(&opts.plugins_root, &archive_path)?;
        download_to(&artifact.url, &archive_path, opts.offline)?;

        let actual = sha256_file(&archive_path)?;
        if !actual.eq_ignore_ascii_case(&artifact.archive_sha256) {
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(format!(
                "archive digest mismatch: expected {}, got {actual}",
                artifact.archive_sha256
            )));
        }

        let extract_root = staging.join("root");
        let extract_root = require_mutable_child(&opts.plugins_root, &extract_root)?;
        fs::create_dir_all(&extract_root)?;
        let format = if artifact.url.ends_with(".zip") || target.starts_with("windows-") {
            ArchiveFormat::Zip
        } else {
            ArchiveFormat::TarGz
        };
        extract_archive(&archive_path, format, &extract_root)?;

        let plugin_src = if artifact.archive_root == "." || artifact.archive_root.is_empty() {
            extract_root.clone()
        } else {
            safe_join(&extract_root, Path::new(&artifact.archive_root))?
        };
        let plugin_toml = plugin_src.join("plugin.toml");
        if !plugin_toml.is_file() {
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(
                "archive missing plugin.toml after extract",
            ));
        }
        let exe = safe_join(&plugin_src, Path::new(&artifact.executable))?;
        if !exe.is_file() {
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(format!(
                "archive missing executable {}",
                artifact.executable
            )));
        }
        let exe_digest = sha256_file(&exe)?;
        if let Some(expected_exe) = &artifact.executable_sha256 {
            if !exe_digest.eq_ignore_ascii_case(expected_exe) {
                let _ = fs::remove_dir_all(&staging);
                return Err(CatalogError::message("executable digest mismatch"));
            }
        }

        // Validate plugin.toml binds id/kind/sandbox/command to the package manifest.
        let toml_text = fs::read_to_string(&plugin_toml)?;
        validate_plugin_toml(
            &toml_text,
            &runtime,
            &manifest.sandbox.network,
            &artifact.executable,
        )?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&exe)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&exe, perms)?;
        }

        let backup = if dest.exists() {
            let bak_path = staging_parent.join(format!("{}.backup", incoming_key.fs_id()));
            let bak = require_mutable_child(&opts.plugins_root, &bak_path)?;
            if bak.exists() {
                fs::remove_dir_all(&bak)?;
            }
            fs::rename(&dest, &bak)?;
            Some(bak)
        } else {
            None
        };

        let restore_tree = |backup: &Option<PathBuf>| {
            let _ = fs::remove_dir_all(&dest);
            if let Some(bak) = backup {
                let _ = fs::rename(bak, &dest);
            }
        };

        // Move extracted plugin tree into place (contents of plugin_src → dest).
        if let Err(e) = copy_dir_all(&plugin_src, &dest) {
            restore_tree(&backup);
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(format!(
                "activate install failed: {e}"
            )));
        }

        let receipt = match build_receipt(
            manifest,
            coordinate,
            artifact,
            &target,
            Some(exe_digest),
            &dest,
            &incoming_key,
            PluginProvenance::VerifiedInstalled,
            false,
        ) {
            Ok(receipt) => receipt,
            Err(e) => {
                restore_tree(&backup);
                let _ = fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        if let Err(e) = receipt.store(&dest) {
            restore_tree(&backup);
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        if let Some(dir) = files_dir.as_ref() {
            if let Err(e) = record_install(
                dir,
                &incoming_key,
                &coordinate.name,
                &runtime.id,
                &receipt.manifest_sha256,
                &receipt.payload_root_sha256,
                PluginProvenance::VerifiedInstalled,
            ) {
                restore_tree(&backup);
                let _ = restore_ledger_entry(dir, &incoming_key, previous_ledger.clone());
                let _ = fs::remove_dir_all(&staging);
                return Err(e);
            }
        }

        // Drop staging (download/extract). Leave `previous` backup for
        // commit/rollback after the caller's health check.
        let _ = fs::remove_dir_all(&staging);

        Ok(InstallOutcome {
            plugin_root: dest,
            receipt,
            dry_run: false,
            previous: backup,
            previous_ledger,
            files_dir,
        })
    }

    /// Discard a replace backup after a successful health check (or when none).
    ///
    /// The new plugin tree and ledger row are already active before this runs.
    /// Failure here is an operational cleanup error: the installation succeeded,
    /// but a previous-version backup may remain under `plugins/.staging`. Callers
    /// must surface that and must not claim a fully clean success.
    ///
    /// Retry-safe: a missing backup is treated as already cleaned up.
    ///
    /// # Errors
    ///
    /// Returns when the leftover backup directory cannot be deleted.
    pub fn commit(outcome: &InstallOutcome) -> Result<()> {
        if let Some(bak) = &outcome.previous {
            if bak.exists() {
                remove_dir_retry(bak)?;
            }
        }
        Ok(())
    }

    /// Restore the previous install (or delete a failed first install) after a
    /// failed health check.
    ///
    /// Rollback is **monotonic and retry-safe**:
    ///
    /// * Never delete `plugin_root` unless a usable previous-version backup
    ///   exists that can replace it.
    /// * If the backup was already consumed and the destination contains the old
    ///   tree, treat the tree phase as done and retry remaining ledger repair.
    /// * First-install rollback may delete the new tree; a missing destination
    ///   on retry is success for the tree phase.
    ///
    /// # Errors
    ///
    /// Returns when the tree cannot be restored/removed or the ledger cannot
    /// be repaired. A failed call must not destroy an already-restored tree.
    pub fn rollback(outcome: &InstallOutcome) -> Result<()> {
        restore_tree_for_rollback(outcome)?;
        restore_ledger_for_rollback(outcome)
    }

    /// Install from a local archive path using an explicit manifest.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn install_local_archive(
        archive: &Path,
        manifest: &BookclerkPackageManifest,
        opts: &InstallOptions,
    ) -> Result<InstallOutcome> {
        let _lock = acquire_plugins_lock(&opts.plugins_root)?;
        Self::install_local_archive_locked(archive, manifest, opts)
    }

    /// Local-archive install while already holding the host mutation lock.
    ///
    /// # Errors
    ///
    /// Returns when `lock` does not cover `opts.plugins_root` or install fails.
    pub fn install_local_archive_with_lock(
        lock: &PluginMutationLock,
        archive: &Path,
        manifest: &BookclerkPackageManifest,
        opts: &InstallOptions,
    ) -> Result<InstallOutcome> {
        lock.require_plugins_root(&opts.plugins_root)?;
        Self::install_local_archive_locked(archive, manifest, opts)
    }

    /// Local-archive body. The caller already holds the host mutation lock.
    fn install_local_archive_locked(
        archive: &Path,
        manifest: &BookclerkPackageManifest,
        opts: &InstallOptions,
    ) -> Result<InstallOutcome> {
        let coordinate = Self::local_archive_coordinate(archive, manifest);
        // Rewrite artifact URL to the local file for the selected target.
        let mut m = manifest.clone();
        let host = opts
            .target
            .clone()
            .unwrap_or_else(|| host_bookclerk_target().to_string());
        for art in &mut m.artifacts {
            if crate::target::normalize_target(&art.target).unwrap_or(art.target.as_str())
                == crate::target::normalize_target(&host).unwrap_or(host.as_str())
            {
                art.url = format!("file://{}", archive.display());
                // Recompute digest from local file.
                art.archive_sha256 = sha256_file(archive)?;
            }
        }
        Self::install_from_manifest_locked(&m, &coordinate, opts)
    }

    /// Remove an installed plugin directory; optionally purge `plugin-state/`.
    ///
    /// `spec` is a canonical PluginKey or an alias unique in this host plugin
    /// namespace. Duplicate aliases on disk fail closed.
    ///
    /// Removal binds durable identity from **host-owned evidence plus
    /// filesystem placement** before any mutation: parseable `plugin.toml`
    /// alias, the unique matching `install-ledger.json` row, and (for
    /// `pk-*` directories) `basename == PluginKey.fs_id()`. A syntactically
    /// valid `receipt.json` PluginKey is metadata and must agree; it cannot
    /// redirect remove or `--purge-state` onto another PluginKey.
    ///
    /// `--purge-state` **holds** `plugin-state/<fs-id>` under
    /// `$FILES_DIR/.plugin-hold/` until commit. Rollback restores tree, state,
    /// and ledger. Commit deletes the held tree then the held state.
    ///
    /// # Errors
    ///
    /// Returns when the plugin is missing, durable identity cannot be
    /// established, the trust ledger cannot be read or written, or the tree
    /// or state cannot be held, restored, or finalized.
    pub fn remove(plugins_root: &Path, spec: &str, purge_state: bool) -> Result<()> {
        let _lock = acquire_plugins_lock(plugins_root)?;
        Self::remove_locked(plugins_root, spec, purge_state)
    }

    /// Remove while already holding the host-local plugin mutation lock.
    ///
    /// # Errors
    ///
    /// Returns when `lock` does not cover `plugins_root` or remove fails.
    pub fn remove_with_lock(
        lock: &PluginMutationLock,
        plugins_root: &Path,
        spec: &str,
        purge_state: bool,
    ) -> Result<()> {
        lock.require_plugins_root(plugins_root)?;
        Self::remove_locked(plugins_root, spec, purge_state)
    }

    /// Remove body. The caller already holds the host mutation lock.
    fn remove_locked(plugins_root: &Path, spec: &str, purge_state: bool) -> Result<()> {
        let dest = resolve_installed_dir(plugins_root, spec)?;
        let files_dir = plugins_root.parent();
        let mut ledger = match files_dir {
            Some(dir) => Some(InstallLedger::load(dir)?),
            None => None,
        };
        let key = resolve_remove_plugin_key(&dest, ledger.as_ref())?;
        let previous_ledger = ledger.as_ref().and_then(|loaded| loaded.get(&key).cloned());

        fs::create_dir_all(plugins_root)?;
        let staging_parent_path = plugins_root.join(".staging");
        let staging_parent = require_mutable_child(plugins_root, &staging_parent_path)?;
        fs::create_dir_all(&staging_parent)?;
        let staging_parent = require_mutable_child(plugins_root, &staging_parent_path)?;
        let tree_hold = unique_hold_path(&staging_parent, &format!("{}.removing", key.fs_id()));
        let tree_hold = require_mutable_child(plugins_root, &tree_hold)?;
        rename_retry(&dest, &tree_hold)?;

        let state_dest = files_dir.map(|dir| dir.join("plugin-state").join(key.fs_id()));
        let mut state_hold: Option<PathBuf> = None;
        if purge_state {
            if let (Some(dir), Some(state)) = (files_dir, state_dest.as_ref()) {
                if state.exists() {
                    let hold_parent = dir.join(PLUGIN_HOLD_DIR);
                    fs::create_dir_all(&hold_parent)?;
                    let hold =
                        unique_hold_path(&hold_parent, &format!("{}.state-removing", key.fs_id()));
                    if let Err(err) = rename_retry(state, &hold) {
                        return Err(remove_after_hold_failure(RemoveHoldRestore {
                            tree_hold: &tree_hold,
                            dest: &dest,
                            state_hold: None,
                            state_dest: None,
                            files_dir: dir,
                            key: &key,
                            previous_ledger,
                            err,
                            what: "plugin state hold",
                        }));
                    }
                    state_hold = Some(hold);
                }
            }
        }

        if let (Some(dir), Some(ledger)) = (files_dir, ledger.as_mut()) {
            ledger.remove(&key);
            if let Err(err) = ledger.store(dir) {
                return Err(remove_after_hold_failure(RemoveHoldRestore {
                    tree_hold: &tree_hold,
                    dest: &dest,
                    state_hold: state_hold.as_deref(),
                    state_dest: state_dest.as_deref(),
                    files_dir: dir,
                    key: &key,
                    previous_ledger,
                    err,
                    what: "install ledger",
                }));
            }
        }

        match remove_dir_retry(&tree_hold) {
            Ok(()) => {}
            Err(_) if !tree_hold.exists() && !dest.exists() => {}
            Err(err) => {
                if let Some(dir) = files_dir {
                    return Err(remove_after_hold_failure(RemoveHoldRestore {
                        tree_hold: &tree_hold,
                        dest: &dest,
                        state_hold: state_hold.as_deref(),
                        state_dest: state_dest.as_deref(),
                        files_dir: dir,
                        key: &key,
                        previous_ledger,
                        err,
                        what: "held install tree",
                    }));
                }
                match restore_held_tree(&tree_hold, &dest) {
                    Ok(()) => return Err(err),
                    Err(restore_err) => {
                        return Err(CatalogError::message(format!(
                            "{err}; also failed to restore install tree: {restore_err}"
                        )));
                    }
                }
            }
        }

        if let Some(hold) = &state_hold {
            if hold.exists() {
                if let Err(err) = remove_dir_retry(hold) {
                    return Err(CatalogError::message(format!(
                        "plugin `{key}` was removed from the install tree and ledger, but \
                         purging held plugin state failed: {err}. Held state remains at {}. \
                         Re-run remove --purge-state after resolving the error; do not treat \
                         removal as fully complete.",
                        hold.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Resolves `spec` to an install directory under `plugins_root`.
fn resolve_installed_dir(plugins_root: &Path, spec: &str) -> Result<PathBuf> {
    if let Ok(key) = PluginKey::parse(spec) {
        let dest = safe_join(plugins_root, Path::new(&key.fs_id()))?;
        if dest.exists() {
            return Ok(dest);
        }
        return Err(CatalogError::message(format!(
            "plugin `{spec}` is not installed under {}",
            plugins_root.display()
        )));
    }
    let mut matches = Vec::new();
    let alias_legacy = safe_join(plugins_root, Path::new(spec))?;
    if alias_legacy.exists() {
        matches.push(alias_legacy);
    }
    if let Ok(entries) = fs::read_dir(plugins_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !path.join("plugin.toml").is_file() {
                continue;
            }
            let toml_alias = std::fs::read_to_string(path.join("plugin.toml"))
                .ok()
                .and_then(|t| PluginManifest::parse(&t).ok())
                .map(|m| m.id);
            let receipt_alias = InstallReceipt::load(&path).ok().map(|r| r.runtime.id);
            // Host `plugin.toml` is the alias source of truth. A receipt may
            // confirm the same alias but cannot redirect lookup onto another id.
            let id_match = match (&toml_alias, &receipt_alias) {
                (Some(toml), Some(receipt)) => {
                    toml.eq_ignore_ascii_case(spec) && receipt.eq_ignore_ascii_case(toml)
                }
                (Some(toml), None) => toml.eq_ignore_ascii_case(spec),
                (None, Some(receipt)) => receipt.eq_ignore_ascii_case(spec),
                (None, None) => false,
            };
            if id_match && !matches.iter().any(|p| p == &path) {
                matches.push(path);
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.pop().expect("len 1")),
        0 => Err(CatalogError::message(format!(
            "plugin `{spec}` is not installed under {}",
            plugins_root.display()
        ))),
        _ => Err(CatalogError::message(format!(
            "duplicate plugin alias `{spec}` is invalid installation state; \
             remove or repair one of: {}",
            matches
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Builds an [`InstallReceipt`] from the package manifest, chosen artifact, and content hashes.
#[allow(clippy::too_many_arguments)]
fn build_receipt(
    manifest: &BookclerkPackageManifest,
    coordinate: &PackageCoordinate,
    artifact: &crate::manifest::ArtifactTarget,
    target: &str,
    exe_digest: Option<String>,
    dest: &Path,
    plugin_key: &PluginKey,
    provenance: PluginProvenance,
    dry_run: bool,
) -> Result<InstallReceipt> {
    let registry_url = match &coordinate.source {
        RegistrySource::Cargo { registry_url } => Some(registry_url.clone()),
        RegistrySource::Npm { registry_url } => Some(registry_url.clone()),
        RegistrySource::Pypi { simple_url } => Some(simple_url.clone()),
        RegistrySource::Static { index_url } => Some(index_url.clone()),
        RegistrySource::LocalArchive => None,
    };
    let (manifest_sha256, payload_root_sha256) = if dry_run {
        (String::new(), String::new())
    } else {
        (
            crate::payload::manifest_sha256(dest)?,
            crate::payload::payload_root_sha256(dest)?,
        )
    };
    Ok(InstallReceipt {
        schema_version: InstallReceipt::SCHEMA_VERSION,
        plugin_key: plugin_key.canonical().to_string(),
        provenance,
        coordinate: coordinate.clone(),
        version: coordinate.version.clone(),
        registry_url,
        artifact_url: artifact.url.clone(),
        target: target.to_string(),
        archive_sha256: artifact.archive_sha256.clone(),
        manifest_sha256,
        payload_root_sha256,
        executable_sha256: exe_digest.or_else(|| artifact.executable_sha256.clone()),
        protocol: manifest.effective_protocol(),
        api_version: manifest.api_version,
        runtime: manifest.runtime(),
        requested_sandbox: manifest.sandbox.clone(),
        approved_network: manifest.sandbox.network.clone(),
        installed_at: Utc::now(),
        update_constraint: None,
    })
}

/// Reject plugin ids that fail the strict grammar (also blocks path escape).
fn validate_plugin_id(id: &str) -> Result<()> {
    bookclerk_plugin_manifest::validate_plugin_id(id)
        .map_err(|e| CatalogError::message(e.to_string()))
}

/// Checks the extracted `plugin.toml` against the package identity.
///
/// The manifest must parse as a v3 [`PluginManifest`], carry the package id,
/// derive the package [`PluginKind`] from its exported entrypoints / triggers,
/// request the same network mode as the package `sandbox`, and name the
/// packaged executable as its `command`.
fn validate_plugin_toml(
    text: &str,
    runtime: &RuntimeIdentity,
    expected_network: &str,
    expected_exe: &str,
) -> Result<()> {
    let manifest: PluginManifest = toml::from_str(text)?;
    if manifest.id != runtime.id {
        return Err(CatalogError::message(format!(
            "plugin.toml id `{}` does not match package id `{}`",
            manifest.id, runtime.id
        )));
    }
    let families = manifest.families();
    let package_family = PluginFamily::parse(runtime.kind.as_str());
    if !package_family.is_some_and(|family| families.contains(&family)) {
        return Err(CatalogError::message(format!(
            "plugin.toml entrypoints derive families [{}], which do not include package kind `{}`",
            families
                .iter()
                .map(|f| f.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            runtime.kind
        )));
    }
    let network = match manifest.capabilities.network.mode {
        NetworkMode::Deny => "none",
        NetworkMode::Outbound => "outbound",
    };
    let mode_label = match manifest.capabilities.network.mode {
        NetworkMode::Deny => "deny",
        NetworkMode::Outbound => "outbound",
    };
    let expected = match expected_network {
        "" | "outbound" => "outbound",
        "none" | "deny" => "none",
        other => other,
    };
    if network != expected {
        return Err(CatalogError::message(format!(
            "plugin.toml capabilities.network.mode `{mode_label}` does not match package sandbox \
             `{expected}`"
        )));
    }
    let command = manifest
        .command
        .as_deref()
        .and_then(Path::to_str)
        .ok_or_else(|| CatalogError::message("plugin.toml missing command"))?;
    if !command_matches_executable(command, expected_exe) {
        return Err(CatalogError::message(format!(
            "plugin.toml command `{command}` does not match package executable `{expected_exe}`"
        )));
    }
    Ok(())
}

/// True when `command` and `executable` share a file name (ignoring a leading `./`).
fn command_matches_executable(command: &str, executable: &str) -> bool {
    let cmd = Path::new(command.trim_start_matches("./"));
    let exe = Path::new(executable.trim_start_matches("./"));
    match (cmd.file_name(), exe.file_name()) {
        (Some(a), Some(b)) => a == b,
        _ => command == executable,
    }
}

/// Rejects installing `incoming_alias` when a different PluginKey already owns it.
fn reject_alias_collision(
    plugins_root: &Path,
    ledger: Option<&InstallLedger>,
    incoming_key: &PluginKey,
    incoming_alias: &str,
    replace: bool,
) -> Result<()> {
    let replace_hint = if replace {
        " (`--replace` updates the existing PluginKey only)"
    } else {
        ""
    };
    for (key, alias, path) in installed_alias_owners(plugins_root)? {
        if !alias.eq_ignore_ascii_case(incoming_alias) || key == *incoming_key {
            continue;
        }
        return Err(CatalogError::message(format!(
            "plugin alias `{incoming_alias}` is already owned by `{key}` at {}; \
             a different PluginKey cannot take this alias{replace_hint}",
            path.display()
        )));
    }
    if let Some(ledger) = ledger {
        for row in &ledger.artifacts {
            if row.manifest_id.eq_ignore_ascii_case(incoming_alias)
                && row.plugin_key != incoming_key.canonical()
            {
                return Err(CatalogError::message(format!(
                    "plugin alias `{incoming_alias}` is already owned by `{}` in the install ledger; \
                     a different PluginKey cannot take this alias{replace_hint}",
                    row.plugin_key
                )));
            }
        }
    }
    Ok(())
}

/// Rejects installing a new alias onto an already-installed [`PluginKey`].
///
/// Ordinary update/replace cannot rename aliases because product configuration
/// (`[sources.<id>]`, `[integrations.<id>]`, enablement) remains keyed by alias.
fn reject_alias_change(
    plugins_root: &Path,
    dest: &Path,
    ledger: Option<&InstallLedger>,
    incoming_key: &PluginKey,
    incoming_alias: &str,
) -> Result<()> {
    for existing in existing_aliases_for_plugin_key(plugins_root, dest, ledger, incoming_key)? {
        if !existing.eq_ignore_ascii_case(incoming_alias) {
            return Err(CatalogError::message(format!(
                "plugin `{incoming_key}` is already installed as `{existing}`; refusing alias \
                 change to `{incoming_alias}`. Alias changes are not supported by ordinary \
                 install/update/replace and require an explicit future migration mechanism"
            )));
        }
    }
    Ok(())
}

/// Known aliases for `incoming_key` from the dest tree, install occupancy, and ledger.
fn existing_aliases_for_plugin_key(
    plugins_root: &Path,
    dest: &Path,
    ledger: Option<&InstallLedger>,
    incoming_key: &PluginKey,
) -> Result<Vec<String>> {
    let mut aliases = Vec::new();
    let mut push_unique = |alias: String| {
        if !aliases
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&alias))
        {
            aliases.push(alias);
        }
    };
    if dest.exists() {
        if let Some(alias) = alias_from_install_dir(dest) {
            push_unique(alias);
        }
    }
    for (key, alias, _) in installed_alias_owners(plugins_root)? {
        if key == *incoming_key {
            push_unique(alias);
        }
    }
    if let Some(row) = ledger.and_then(|loaded| loaded.get(incoming_key)) {
        push_unique(row.manifest_id.clone());
    }
    Ok(aliases)
}

/// Receipt alias, otherwise parseable `plugin.toml` `id`.
fn alias_from_install_dir(dest: &Path) -> Option<String> {
    InstallReceipt::load(dest)
        .ok()
        .map(|receipt| receipt.runtime.id)
        .or_else(|| {
            fs::read_to_string(dest.join("plugin.toml"))
                .ok()
                .and_then(|text| PluginManifest::parse(&text).ok())
                .map(|manifest| manifest.id)
        })
}

/// Restores (or confirms) the install tree for [`Installer::rollback`].
///
/// Mutation operands are validated with [`require_mutable_child`] so a
/// destination leaf symlink cannot redirect delete/rename onto a sibling or
/// the plugins root. Missing destinations remain a no-op for first-install
/// rollback.
fn restore_tree_for_rollback(outcome: &InstallOutcome) -> Result<()> {
    let plugins_root = outcome.plugin_root.parent().ok_or_else(|| {
        CatalogError::message(format!(
            "cannot rollback {}: install path has no parent",
            outcome.plugin_root.display()
        ))
    })?;
    // Preserve the intended directory entry; do not follow a leaf symlink.
    let dest = require_mutable_child(plugins_root, &outcome.plugin_root)?;
    match &outcome.previous {
        Some(bak) if bak.exists() => {
            let bak = require_mutable_child(plugins_root, bak)?;
            restore_update_tree_from_backup(plugins_root, &dest, &bak)
        }
        Some(_) => {
            if dest.exists() {
                Ok(())
            } else {
                Err(CatalogError::message(format!(
                    "cannot rollback {}: previous-version backup is gone and the destination \
                     is missing",
                    dest.display()
                )))
            }
        }
        None => {
            if dest.exists() {
                remove_dir_retry(&dest)?;
            }
            Ok(())
        }
    }
}

/// Moves the failed new tree aside and puts the backup back without deleting
/// `dest` unless a usable backup is in place.
///
/// `dest` and `backup` must already be lexical mutable children of
/// `plugins_root` (see [`require_mutable_child`]).
fn restore_update_tree_from_backup(plugins_root: &Path, dest: &Path, backup: &Path) -> Result<()> {
    // Re-validate against the trusted plugins root before any rename/delete.
    let dest = require_mutable_child(plugins_root, dest)?;
    let backup = require_mutable_child(plugins_root, backup)?;
    if dest.exists() {
        fs::create_dir_all(plugins_root)?;
        let staging_parent_path = plugins_root.join(".staging");
        let staging_parent = require_mutable_child(plugins_root, &staging_parent_path)?;
        fs::create_dir_all(&staging_parent)?;
        let staging_parent = require_mutable_child(plugins_root, &staging_parent_path)?;
        let dest_name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".into());
        let aside = unique_hold_path(&staging_parent, &format!("{dest_name}.rollback-new"));
        let aside = require_mutable_child(plugins_root, &aside)?;
        rename_retry(&dest, &aside)?;
        match fs::rename(&backup, &dest) {
            Ok(()) => {
                let _ = remove_dir_retry(&aside);
                Ok(())
            }
            Err(err) => {
                let restore = restore_held_tree(&aside, &dest);
                Err(CatalogError::message(format!(
                    "failed to restore previous plugin tree from {}: {err}{}",
                    backup.display(),
                    match restore {
                        Ok(()) => String::new(),
                        Err(restore_err) =>
                            format!("; also failed to put the new tree back: {restore_err}"),
                    }
                )))
            }
        }
    } else {
        fs::rename(&backup, &dest).map_err(|err| {
            CatalogError::message(format!(
                "failed to restore previous plugin tree from {} to {}: {err}",
                backup.display(),
                dest.display()
            ))
        })
    }
}

/// Repairs the host install ledger as the last rollback phase.
fn restore_ledger_for_rollback(outcome: &InstallOutcome) -> Result<()> {
    let key = outcome.receipt.plugin_key().ok().or_else(|| {
        outcome
            .previous_ledger
            .as_ref()
            .and_then(|row| PluginKey::parse(&row.plugin_key).ok())
    });
    if let (Some(dir), Some(key)) = (outcome.files_dir.as_ref(), key.as_ref()) {
        restore_ledger_entry(dir, key, outcome.previous_ledger.clone())?;
    }
    Ok(())
}

/// Exactly one durable [`PluginKey`] for removal, before any tree mutation.
///
/// Host-owned `install-ledger.json` plus filesystem placement are the trust
/// anchors. `receipt.json` is plugin-controlled metadata: a syntactically
/// valid PluginKey is accepted only when it agrees with the unique ledger
/// owner and, for `pk-*` directories, `basename == PluginKey.fs_id()`.
/// Missing or malformed receipts may still recover from a parseable
/// `plugin.toml` alias, exactly one matching ledger row, and a consistent
/// install path. Ambiguous or inconsistent evidence fails closed.
fn resolve_remove_plugin_key(dest: &Path, ledger: Option<&InstallLedger>) -> Result<PluginKey> {
    let alias = installed_manifest_alias(dest)?;
    let Some(ledger) = ledger else {
        return Err(CatalogError::message(format!(
            "cannot establish a durable PluginKey for alias `{alias}` at {}; no host install \
             ledger is available. Removal is refused before mutating the plugin tree",
            dest.display()
        )));
    };
    let matches: Vec<&InstallLedgerEntry> = ledger
        .artifacts
        .iter()
        .filter(|row| row.manifest_id.eq_ignore_ascii_case(&alias))
        .collect();
    let ledger_key = match matches.len() {
        1 => PluginKey::parse(&matches[0].plugin_key).map_err(|err| {
            CatalogError::message(format!(
                "cannot establish a durable PluginKey for alias `{alias}`: install ledger row \
                 `{err}`"
            ))
        })?,
        0 => {
            return Err(CatalogError::message(format!(
                "cannot establish a durable PluginKey for alias `{alias}` at {}; the host \
                 install ledger has no matching row. Removal is refused before mutating the \
                 plugin tree",
                dest.display()
            )));
        }
        n => {
            return Err(CatalogError::message(format!(
                "cannot establish a durable PluginKey for alias `{alias}` at {}; the host \
                 install ledger has {n} matching rows. Removal is refused before mutating the \
                 plugin tree",
                dest.display()
            )));
        }
    };
    require_key_derived_path(dest, &ledger_key)?;
    match InstallReceipt::load(dest) {
        Ok(receipt) => {
            if !receipt.runtime.id.eq_ignore_ascii_case(&alias) {
                return Err(CatalogError::message(format!(
                    "install receipt alias `{}` does not match plugin.toml alias `{alias}` at \
                     {}; removal is refused before mutating the plugin tree",
                    receipt.runtime.id,
                    dest.display()
                )));
            }
            match receipt.plugin_key() {
                Ok(receipt_key) => {
                    if receipt_key != ledger_key {
                        return Err(CatalogError::message(format!(
                            "install receipt PluginKey `{receipt_key}` does not match host ledger \
                             PluginKey `{ledger_key}` for alias `{alias}` at {}; receipt \
                             metadata cannot redirect remove onto another PluginKey. Removal is \
                             refused before mutating the plugin tree",
                            dest.display()
                        )));
                    }
                    require_key_derived_path(dest, &receipt_key)?;
                }
                Err(_) => {
                    // Malformed receipt PluginKey: recover from unique ledger + path.
                }
            }
        }
        Err(err) if err.is_receipt_not_found() => {}
        Err(_) => {}
    }
    Ok(ledger_key)
}

/// Parseable `plugin.toml` alias for a managed install directory.
fn installed_manifest_alias(dest: &Path) -> Result<String> {
    let path = dest.join("plugin.toml");
    let text = fs::read_to_string(&path).map_err(|err| {
        CatalogError::message(format!(
            "cannot establish a durable PluginKey for {}; plugin.toml is missing or unreadable \
             ({err}). Removal is refused before mutating the plugin tree",
            dest.display()
        ))
    })?;
    let manifest = PluginManifest::parse(&text).map_err(|err| {
        CatalogError::message(format!(
            "cannot establish a durable PluginKey for {}; plugin.toml alias is not parseable \
             ({err}). Removal is refused before mutating the plugin tree",
            dest.display()
        ))
    })?;
    Ok(manifest.id)
}

/// For `pk-*` install directories, require `basename == key.fs_id()`.
fn require_key_derived_path(dest: &Path, key: &PluginKey) -> Result<()> {
    let Some(name) = dest.file_name().and_then(|n| n.to_str()) else {
        return Err(CatalogError::message(format!(
            "cannot establish a durable PluginKey for {}; install directory name is not \
             Unicode. Removal is refused before mutating the plugin tree",
            dest.display()
        )));
    };
    if name.starts_with("pk-") && name != key.fs_id() {
        return Err(CatalogError::message(format!(
            "install directory {} does not match PluginKey filesystem id `{}`; receipt or \
             placement cannot select another PluginKey. Removal is refused before mutating the \
             plugin tree",
            dest.display(),
            key.fs_id()
        )));
    }
    Ok(())
}

/// Unused name under `parent` for a hold/aside directory.
fn unique_hold_path(parent: &Path, base: &str) -> PathBuf {
    let candidate = parent.join(base);
    let Ok(candidate) = require_under(parent, &candidate) else {
        return parent.join(base);
    };
    if !candidate.exists() {
        return candidate;
    }
    for n in 1..128 {
        let candidate = parent.join(format!("{base}-{n}"));
        let Ok(candidate) = require_under(parent, &candidate) else {
            continue;
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    let fallback = parent.join(format!("{base}-{}", std::process::id()));
    require_under(parent, &fallback).unwrap_or(fallback)
}

/// Moves a held install tree back to `dest` after a failed remove step.
///
/// Both operands are validated with [`require_mutable_child`] under their
/// immediate parents so a leaf symlink cannot redirect rename onto a peer or
/// the trusted root. Missing `dest` with a present hold still renames; an
/// already-restored destination remains a no-op when the hold is gone.
fn restore_held_tree(hold: &Path, dest: &Path) -> Result<()> {
    let hold_root = hold.parent().ok_or_else(|| {
        CatalogError::message(format!(
            "cannot restore held tree {}: path has no parent",
            hold.display()
        ))
    })?;
    let dest_root = dest.parent().ok_or_else(|| {
        CatalogError::message(format!(
            "cannot restore to {}: path has no parent",
            dest.display()
        ))
    })?;
    let hold = require_mutable_child(hold_root, hold)?;
    let dest = require_mutable_child(dest_root, dest)?;
    if dest.exists() {
        if hold.exists() {
            return Err(CatalogError::message(format!(
                "cannot restore {} because {} already exists",
                hold.display(),
                dest.display()
            )));
        }
        return Ok(());
    }
    if !hold.exists() {
        return Err(CatalogError::message(format!(
            "held install tree {} is missing; cannot restore {}",
            hold.display(),
            dest.display()
        )));
    }
    rename_retry(&hold, &dest)
}

/// Inputs for restoring tree/state/ledger after a failed remove step.
struct RemoveHoldRestore<'a> {
    /// Held install tree.
    tree_hold: &'a Path,
    /// Original install destination.
    dest: &'a Path,
    /// Held plugin-state directory, if `--purge-state` moved it.
    state_hold: Option<&'a Path>,
    /// Original plugin-state destination.
    state_dest: Option<&'a Path>,
    /// Host `$FILES_DIR`.
    files_dir: &'a Path,
    /// Durable identity being removed.
    key: &'a PluginKey,
    /// Ledger row to restore, if any.
    previous_ledger: Option<InstallLedgerEntry>,
    /// The step that failed.
    err: CatalogError,
    /// Label for the failed step.
    what: &'a str,
}

/// Restores tree, optional held plugin-state, and ledger after a remove step
/// failed while the tree (and maybe state) was held.
fn remove_after_hold_failure(ctx: RemoveHoldRestore<'_>) -> CatalogError {
    let ledger_restore = restore_ledger_entry(ctx.files_dir, ctx.key, ctx.previous_ledger);
    let tree_restore = restore_held_tree(ctx.tree_hold, ctx.dest);
    let state_restore = match (ctx.state_hold, ctx.state_dest) {
        (Some(hold), Some(state)) => Some(restore_held_tree(hold, state)),
        _ => None,
    };
    let mut msg = format!(
        "failed to update {} during plugin remove: {}",
        ctx.what, ctx.err
    );
    match ledger_restore {
        Ok(()) => {}
        Err(restore_err) => {
            msg.push_str(&format!(
                "; also failed to restore install ledger: {restore_err}"
            ));
        }
    }
    match tree_restore {
        Ok(()) => {}
        Err(restore_err) => {
            msg.push_str(&format!(
                "; also failed to restore install tree: {restore_err}"
            ));
        }
    }
    if let Some(state_restore) = state_restore {
        match state_restore {
            Ok(()) => {}
            Err(restore_err) => {
                msg.push_str(&format!(
                    "; also failed to restore plugin state: {restore_err}"
                ));
            }
        }
    }
    CatalogError::message(msg)
}

/// Installed (PluginKey, alias, path) under `plugins_root`.
fn installed_alias_owners(plugins_root: &Path) -> Result<Vec<(PluginKey, String, PathBuf)>> {
    let mut out = Vec::new();
    if !plugins_root.is_dir() {
        return Ok(out);
    }
    let entries = match fs::read_dir(plugins_root) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !path.join("plugin.toml").is_file() {
            continue;
        }
        let receipt = InstallReceipt::load(&path).ok();
        let alias = receipt.as_ref().map(|r| r.runtime.id.clone()).or_else(|| {
            fs::read_to_string(path.join("plugin.toml"))
                .ok()
                .and_then(|t| PluginManifest::parse(&t).ok())
                .map(|m| m.id)
        });
        let Some(alias) = alias else {
            continue;
        };
        let key = receipt
            .and_then(|r| r.plugin_key().ok())
            .or_else(|| PluginKey::from_install_path(&path, &alias).ok());
        let Some(key) = key else {
            continue;
        };
        out.push((key, alias, path));
    }
    Ok(out)
}

/// Copies a `file://` or local path, or HTTPS/localhost HTTP, refusing oversized bodies.
fn download_to(url: &str, dest: &Path, offline: bool) -> Result<()> {
    if let Some(path) = url.strip_prefix("file://") {
        fs::copy(path, dest)?;
        return Ok(());
    }
    if url.starts_with('/') || Path::new(url).exists() {
        fs::copy(url, dest)?;
        return Ok(());
    }
    if offline {
        return Err(CatalogError::message(
            "offline install requires a local file:// artifact url",
        ));
    }
    if !url.starts_with("https://")
        && !url.starts_with("http://127.0.0.1")
        && !url.starts_with("http://localhost")
    {
        return Err(CatalogError::message(
            "remote install requires https:// (or localhost http for fixtures)",
        ));
    }
    let _ = MAX_REDIRECTS;
    let _ = DOWNLOAD_TIMEOUT;
    let mut response = ureq::get(url)
        .header(
            "User-Agent",
            concat!("bookclerk/", env!("CARGO_PKG_VERSION"), " (plugin-install)"),
        )
        .call()
        .map_err(|e| CatalogError::message(format!("download failed: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(CatalogError::message(format!(
            "download HTTP {status} for {url}"
        )));
    }
    let mut buf = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(CatalogError::message("download exceeds size limit"));
    }
    write_file(dest, &buf)?;
    Ok(())
}

/// Recursively copies `src` into `dest`, creating missing parent directories.
fn copy_dir_all(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(src).unwrap();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = match safe_join(dest, rel) {
            Ok(p) => p,
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    err.to_string(),
                ));
            }
        };
        if entry.file_type().is_dir() {
            fs::create_dir_all(&out)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &out)?;
        }
    }
    Ok(())
}

/// Retries `remove_dir_all` up to five times (50 ms apart) for transient Windows locks.
fn remove_dir_retry(path: &Path) -> Result<()> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(CatalogError::message(format!(
            "refusing remove path with '..': {}",
            path.display()
        )));
    }
    let mut last = None;
    for _ in 0..5 {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(CatalogError::message(format!(
        "failed to remove {}: {}",
        path.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}

/// Retries `rename` up to five times (50 ms apart) for transient Windows locks.
fn rename_retry(from: &Path, to: &Path) -> Result<()> {
    for p in [from, to] {
        if p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(CatalogError::message(format!(
                "refusing rename path with '..': {}",
                p.display()
            )));
        }
    }
    let mut last = None;
    for _ in 0..5 {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(CatalogError::message(format!(
        "failed to rename {} to {}: {}",
        from.display(),
        to.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::sha256_bytes;
    use crate::kind::{PluginKind, RuntimeIdentity};
    use crate::manifest::ArtifactTarget;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::Builder;

    /// Join `rel` under `root` with the CodeQL two-state path barrier.
    fn under_tmp(root: &Path, rel: impl AsRef<Path>) -> PathBuf {
        let joined = root.join(rel.as_ref());
        require_under(root, &joined).unwrap_or_else(|err| {
            panic!(
                "test path {} must stay under {}: {err}",
                joined.display(),
                root.display()
            )
        })
    }

    /// Re-check an absolute path stays under `root` before FS predicates/sinks.
    fn path_under(root: &Path, path: impl AsRef<Path>) -> PathBuf {
        require_under(root, path.as_ref()).unwrap_or_else(|err| {
            panic!(
                "test path {} must stay under {}: {err}",
                path.as_ref().display(),
                root.display()
            )
        })
    }

    fn make_named_archive(dir: &Path, filename: &str, id: &str) -> (PathBuf, String) {
        let archive = under_tmp(dir, filename);
        {
            let file = fs::File::create(&archive).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = Builder::new(enc);
            let toml = format!(
                "api_version = 3\nid = \"{id}\"\ncommand = \"./echo\"\nentrypoints = [\"cli\"]\n\
                 [capabilities.network]\nmode = \"outbound\"\n"
            );
            let toml = toml.into_bytes();
            let mut h = tar::Header::new_gnu();
            h.set_size(toml.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, "plugin.toml", toml.as_slice())
                .unwrap();
            let bin = b"#!/bin/sh\necho ok\n";
            let mut h2 = tar::Header::new_gnu();
            h2.set_size(bin.len() as u64);
            h2.set_mode(0o755);
            h2.set_cksum();
            tar.append_data(&mut h2, "echo", &bin[..]).unwrap();
            let data = b"packaged";
            let mut h3 = tar::Header::new_gnu();
            h3.set_size(data.len() as u64);
            h3.set_mode(0o644);
            h3.set_cksum();
            tar.append_data(&mut h3, "data/packaged.txt", &data[..])
                .unwrap();
            let enc = tar.into_inner().unwrap();
            enc.finish().unwrap();
        }
        let digest = sha256_file(&archive).unwrap();
        (archive, digest)
    }

    fn make_echo_archive(dir: &Path) -> (PathBuf, String) {
        make_named_archive(dir, "echo.tar.gz", "echo")
    }

    fn echo_install_opts(plugins: PathBuf, replace: bool) -> InstallOptions {
        InstallOptions {
            plugins_root: plugins,
            replace,
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        }
    }

    fn echo_manifest(
        id: &str,
        digest: String,
        url: String,
        target: &str,
    ) -> BookclerkPackageManifest {
        BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: id.into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url,
                archive_sha256: digest,
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        }
    }

    fn dir_names(path: &Path) -> Vec<String> {
        if !path.is_dir() {
            return Vec::new();
        }
        let mut names: Vec<String> = fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn corrupt_receipt(dest: &Path) {
        fs::write(under_tmp(dest, "receipt.json"), b"{not-valid-receipt").unwrap();
    }

    #[test]
    fn atomic_install_and_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let (archive, digest) = make_echo_archive(tmp.path());
        let target = host_bookclerk_target();
        let manifest = BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url: format!("file://{}", archive.display()),
                archive_sha256: digest,
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        };
        let plugins = under_tmp(tmp.path(), "plugins");
        let opts = InstallOptions {
            plugins_root: plugins.clone(),
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let out = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        assert!(under_tmp(&out.plugin_root, "plugin.toml").is_file());
        Installer::commit(&out).unwrap();
        let receipt = InstallReceipt::load(&out.plugin_root).unwrap();
        assert_eq!(receipt.runtime.id, "echo");

        // Digest mismatch fails.
        let mut bad = manifest.clone();
        bad.artifacts[0].archive_sha256 = sha256_bytes(b"nope");
        // pad to 64 hex
        bad.artifacts[0].archive_sha256 = format!("{:0>64}", "ab");
        assert!(Installer::install_from_manifest(&bad, &coord, &opts).is_err());
    }

    #[test]
    fn rejects_invalid_plugin_id() {
        let err = validate_plugin_id("../evil").unwrap_err();
        assert!(err.to_string().contains("plugin id"), "{err}");
        let err = validate_plugin_id("/abs").unwrap_err();
        assert!(err.to_string().contains("plugin id"), "{err}");
        let err = validate_plugin_id("a-b").unwrap_err();
        assert!(err.to_string().contains("lowercase"), "{err}");
        let err = validate_plugin_id("a").unwrap_err();
        assert!(err.to_string().contains("2–32"), "{err}");
        validate_plugin_id("echo").unwrap();
        validate_plugin_id("my_store").unwrap();
    }

    #[test]
    fn rejects_same_plugin_key_different_kind_on_install() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        let (archive, digest) = make_echo_archive(tmp.path());
        let target = host_bookclerk_target();
        let mut manifest = BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url: format!("file://{}", archive.display()),
                archive_sha256: digest.clone(),
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        };
        let opts = InstallOptions {
            plugins_root: plugins,
            replace: true,
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        manifest.kind = PluginKind::Source;
        let err = Installer::install_from_manifest(&manifest, &coord, &opts)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unique") || err.contains("already installed"),
            "{err}"
        );
    }

    #[test]
    fn same_alias_different_plugin_keys_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        let (archive_a, digest_a) = make_echo_archive(tmp.path());
        let archive_b = under_tmp(tmp.path(), "echo-b.tar.gz");
        fs::copy(&archive_a, &archive_b).unwrap();
        let target = host_bookclerk_target();
        let manifest_for = |digest: String, url: String| BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url,
                archive_sha256: digest,
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        };
        let opts = InstallOptions {
            plugins_root: plugins.clone(),
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let coord_a = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive_a.display().to_string(),
            version: "1.0.0".into(),
        };
        let coord_b = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive_b.display().to_string(),
            version: "1.0.0".into(),
        };
        let a = Installer::install_from_manifest(
            &manifest_for(digest_a.clone(), format!("file://{}", archive_a.display())),
            &coord_a,
            &opts,
        )
        .unwrap();
        Installer::commit(&a).unwrap();
        let err = Installer::install_from_manifest(
            &manifest_for(digest_a.clone(), format!("file://{}", archive_b.display())),
            &coord_b,
            &opts,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already owned"), "{err}");
        assert!(err.contains("PluginKey"), "{err}");

        let mut replace_opts = opts.clone();
        replace_opts.replace = true;
        let err = Installer::install_from_manifest(
            &manifest_for(digest_a, format!("file://{}", archive_b.display())),
            &coord_b,
            &replace_opts,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already owned"), "{err}");
        assert!(err.contains("--replace"), "{err}");
    }

    #[test]
    fn same_plugin_key_update_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        let (archive, digest) = make_echo_archive(tmp.path());
        let target = host_bookclerk_target();
        let manifest = BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url: format!("file://{}", archive.display()),
                archive_sha256: digest,
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        };
        let opts = InstallOptions {
            plugins_root: plugins,
            replace: true,
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let first = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        Installer::commit(&first).unwrap();
        let second = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        assert_eq!(first.plugin_root, second.plugin_root);
        assert_eq!(first.receipt.plugin_key, second.receipt.plugin_key);
        assert_eq!(second.receipt.runtime.id, "echo");
        Installer::commit(&second).unwrap();
    }

    #[test]
    fn failed_first_install_rollback_removes_tree_and_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        let (archive, digest) = make_echo_archive(tmp.path());
        let target = host_bookclerk_target();
        let manifest = BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url: format!("file://{}", archive.display()),
                archive_sha256: digest,
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        };
        let opts = InstallOptions {
            plugins_root: plugins,
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let out = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        let files_dir = tmp.path();
        let key = out.receipt.plugin_key().unwrap();
        assert!(under_tmp(&out.plugin_root, "plugin.toml").is_file());
        assert!(InstallLedger::load(files_dir).unwrap().get(&key).is_some());
        assert!(out.previous.is_none());
        Installer::rollback(&out).unwrap();
        assert!(!path_under(tmp.path(), &out.plugin_root).exists());
        assert!(InstallLedger::load(files_dir).unwrap().get(&key).is_none());
    }

    #[test]
    fn failed_update_rollback_restores_old_tree_and_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        let (archive, digest) = make_echo_archive(tmp.path());
        let target = host_bookclerk_target();
        let manifest = BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url: format!("file://{}", archive.display()),
                archive_sha256: digest,
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        };
        let opts = InstallOptions {
            plugins_root: plugins,
            replace: true,
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let first = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        Installer::commit(&first).unwrap();
        fs::write(under_tmp(&first.plugin_root, "marker.txt"), b"keep-me").unwrap();
        let files_dir = tmp.path();
        let key = first.receipt.plugin_key().unwrap();
        let old_payload = first.receipt.payload_root_sha256.clone();
        let second = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        assert!(second.previous.is_some());
        assert!(!under_tmp(&second.plugin_root, "marker.txt").is_file());
        Installer::rollback(&second).unwrap();
        assert!(under_tmp(&first.plugin_root, "marker.txt").is_file());
        let restored = InstallLedger::load(files_dir).unwrap();
        let row = restored.get(&key).expect("ledger row");
        assert_eq!(row.payload_root_sha256, old_payload);
    }

    #[test]
    fn malformed_trust_ledger_aborts_before_tree_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        let (archive, digest) = make_echo_archive(tmp.path());
        let target = host_bookclerk_target();
        let manifest = BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url: format!("file://{}", archive.display()),
                archive_sha256: digest,
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        };
        let opts = InstallOptions {
            plugins_root: plugins.clone(),
            replace: true,
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let first = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        Installer::commit(&first).unwrap();
        fs::write(under_tmp(&first.plugin_root, "marker.txt"), b"untouched").unwrap();
        let toml_before = fs::read(under_tmp(&first.plugin_root, "plugin.toml")).unwrap();
        let mut tree_before: Vec<_> = fs::read_dir(&plugins)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        tree_before.sort();

        let ledger_path = InstallLedger::path(tmp.path());
        let garbage = b"{not-valid-install-ledger";
        fs::write(&ledger_path, garbage).unwrap();

        let err = Installer::install_from_manifest(&manifest, &coord, &opts)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("install-ledger")
                || err.contains("expected")
                || err.contains("json")
                || err.contains("EOF")
                || err.contains("key must be"),
            "{err}"
        );

        assert_eq!(
            fs::read(&ledger_path).unwrap(),
            garbage,
            "malformed ledger must not be rewritten"
        );
        assert_eq!(
            fs::read(under_tmp(&first.plugin_root, "plugin.toml")).unwrap(),
            toml_before
        );
        assert_eq!(
            fs::read(under_tmp(&first.plugin_root, "marker.txt")).unwrap(),
            b"untouched"
        );
        assert!(path_under(tmp.path(), &first.plugin_root).is_dir());
        let mut tree_after: Vec<_> = fs::read_dir(&plugins)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        tree_after.sort();
        assert_eq!(tree_before, tree_after);
        if under_tmp(&plugins, ".staging").is_dir() {
            let leftover: Vec<_> = fs::read_dir(under_tmp(&plugins, ".staging"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert!(
                leftover.is_empty(),
                "malformed ledger must not leave a replacement in .staging: {leftover:?}"
            );
        }
    }

    #[test]
    fn replace_does_not_copy_install_root_data_tmp_into_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        let (archive, digest) = make_echo_archive(tmp.path());
        let target = host_bookclerk_target();
        let manifest = BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url: format!("file://{}", archive.display()),
                archive_sha256: digest,
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        };
        let opts = InstallOptions {
            plugins_root: plugins,
            replace: true,
            trust: TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let first = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        Installer::commit(&first).unwrap();
        fs::create_dir_all(under_tmp(&first.plugin_root, "data")).unwrap();
        fs::write(under_tmp(&first.plugin_root, "data/old-state"), b"stale").unwrap();
        let second = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        Installer::commit(&second).unwrap();
        assert!(!under_tmp(&second.plugin_root, "data/old-state").exists());
        assert!(under_tmp(&second.plugin_root, "data/packaged.txt").is_file());
    }

    #[test]
    fn same_plugin_key_alias_change_is_rejected_even_with_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        let (archive, digest) = make_named_archive(tmp.path(), "echo.tar.gz", "echo");
        let target = host_bookclerk_target();
        let file_url = |p: &Path| format!("file://{}", p.display());
        let opts = echo_install_opts(plugins.clone(), false);
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let first = Installer::install_from_manifest(
            &echo_manifest("echo", digest.clone(), file_url(&archive), target),
            &coord,
            &opts,
        )
        .unwrap();
        Installer::commit(&first).unwrap();

        let toml_before = fs::read(under_tmp(&first.plugin_root, "plugin.toml")).unwrap();
        let receipt_before = fs::read(under_tmp(&first.plugin_root, "receipt.json")).unwrap();
        let ledger_path = InstallLedger::path(tmp.path());
        let ledger_before = fs::read(&ledger_path).unwrap();
        let tree_before = dir_names(&plugins);
        let staging_before = dir_names(&under_tmp(&plugins, ".staging"));

        let (archive2, digest2) = make_named_archive(tmp.path(), "echo.tar.gz", "echo2");
        assert_eq!(archive, archive2);
        let coord2 = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive2.display().to_string(),
            version: "1.0.0".into(),
        };
        let incoming = echo_manifest("echo2", digest2.clone(), file_url(&archive2), target);
        let err = Installer::install_from_manifest(&incoming, &coord2, &opts)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing alias change"), "{err}");
        assert!(err.contains("not supported by ordinary"), "{err}");
        assert!(err.contains("migration"), "{err}");

        let mut replace_opts = opts.clone();
        replace_opts.replace = true;
        let err = Installer::install_from_manifest(&incoming, &coord2, &replace_opts)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing alias change"), "{err}");
        assert!(err.contains("not supported by ordinary"), "{err}");
        assert!(
            !err.contains("pass --replace"),
            "replace must not be offered as an alias-rename path: {err}"
        );

        assert_eq!(
            fs::read(under_tmp(&first.plugin_root, "plugin.toml")).unwrap(),
            toml_before
        );
        assert_eq!(
            fs::read(under_tmp(&first.plugin_root, "receipt.json")).unwrap(),
            receipt_before
        );
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        assert_eq!(dir_names(&plugins), tree_before);
        assert_eq!(dir_names(&under_tmp(&plugins, ".staging")), staging_before);
        let receipt = InstallReceipt::load(&first.plugin_root).unwrap();
        assert_eq!(receipt.runtime.id, "echo");
        assert_eq!(
            receipt.plugin_key().unwrap(),
            first.receipt.plugin_key().unwrap()
        );
        let ledger = InstallLedger::load(tmp.path()).unwrap();
        assert_eq!(
            ledger
                .get(&first.receipt.plugin_key().unwrap())
                .unwrap()
                .manifest_id,
            "echo"
        );

        let (other, other_digest) = make_named_archive(tmp.path(), "other.tar.gz", "other");
        let other_coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: other.display().to_string(),
            version: "1.0.0".into(),
        };
        let other_out = Installer::install_from_manifest(
            &echo_manifest("other", other_digest, file_url(&other), target),
            &other_coord,
            &opts,
        )
        .unwrap();
        Installer::commit(&other_out).unwrap();

        let (archive3, digest3) = make_named_archive(tmp.path(), "echo.tar.gz", "other");
        let coord3 = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive3.display().to_string(),
            version: "1.0.0".into(),
        };
        let err = Installer::install_from_manifest(
            &echo_manifest("other", digest3, file_url(&archive3), target),
            &coord3,
            &replace_opts,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already owned"), "{err}");
    }

    fn installed_echo(
        tmp: &Path,
    ) -> (
        PathBuf,
        PathBuf,
        PluginKey,
        BookclerkPackageManifest,
        PackageCoordinate,
        InstallOptions,
    ) {
        let plugins = under_tmp(tmp, "plugins");
        let (archive, digest) = make_echo_archive(tmp);
        let target = host_bookclerk_target();
        let opts = echo_install_opts(plugins.clone(), true);
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let manifest = echo_manifest(
            "echo",
            digest,
            format!("file://{}", archive.display()),
            target,
        );
        let first = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        Installer::commit(&first).unwrap();
        let key = first.receipt.plugin_key().unwrap();
        (plugins, first.plugin_root, key, manifest, coord, opts)
    }

    #[test]
    fn remove_with_valid_receipt_drops_tree_and_ledger_row() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, key, _, _, _) = installed_echo(tmp.path());
        assert!(path_under(tmp.path(), &dest).is_dir());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_some());

        Installer::remove(&plugins, "echo", false).unwrap();
        assert!(!path_under(tmp.path(), &dest).exists());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_none());
    }

    #[test]
    fn remove_recovers_plugin_key_from_unique_ledger_row() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, key, manifest, coord, opts) = installed_echo(tmp.path());
        corrupt_receipt(&dest);
        assert!(InstallReceipt::load(&dest).is_err());

        Installer::remove(&plugins, "echo", false).unwrap();
        assert!(!path_under(tmp.path(), &dest).exists());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_none());

        let again = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        Installer::commit(&again).unwrap();
        assert!(path_under(tmp.path(), &again.plugin_root).is_dir());
        assert_eq!(again.receipt.runtime.id, "echo");
        assert!(InstallLedger::load(tmp.path())
            .unwrap()
            .get(&again.receipt.plugin_key().unwrap())
            .is_some());
    }

    #[test]
    fn remove_recovered_plugin_key_purges_state_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, key, _, _, _) = installed_echo(tmp.path());
        let state = dest_state(tmp.path(), &key);
        write_state_marker(&state, "keep-me-not");
        corrupt_receipt(&dest);

        Installer::remove(&plugins, "echo", true).unwrap();
        assert!(!path_under(tmp.path(), &dest).exists());
        assert!(!path_under(tmp.path(), &state).exists());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_none());
    }

    #[test]
    fn remove_malformed_receipt_without_ledger_identity_leaves_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, key, _, _, _) = installed_echo(tmp.path());
        corrupt_receipt(&dest);
        let mut ledger = InstallLedger::load(tmp.path()).unwrap();
        ledger.remove(&key);
        ledger.store(tmp.path()).unwrap();
        let toml_before = fs::read(under_tmp(&dest, "plugin.toml")).unwrap();

        let err = Installer::remove(&plugins, "echo", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cannot establish a durable PluginKey"),
            "{err}"
        );
        assert!(err.contains("before mutating"), "{err}");
        assert!(path_under(tmp.path(), &dest).is_dir());
        assert_eq!(
            fs::read(under_tmp(&dest, "plugin.toml")).unwrap(),
            toml_before
        );
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_none());
    }

    #[test]
    fn remove_malformed_receipt_with_ambiguous_ledger_leaves_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, key, _, _, _) = installed_echo(tmp.path());
        corrupt_receipt(&dest);
        let mut ledger = InstallLedger::load(tmp.path()).unwrap();
        let mut extra = ledger.get(&key).unwrap().clone();
        extra.plugin_key =
            PluginKey::from_install_path(&under_tmp(tmp.path(), "other-archive.tar.gz"), "echo")
                .unwrap()
                .canonical()
                .to_string();
        ledger.artifacts.push(extra);
        ledger.store(tmp.path()).unwrap();
        let toml_before = fs::read(under_tmp(&dest, "plugin.toml")).unwrap();

        let err = Installer::remove(&plugins, "echo", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cannot establish a durable PluginKey"),
            "{err}"
        );
        assert!(
            err.contains("matching rows")
                || err.contains("ambiguous")
                || err.contains("2 matching"),
            "{err}"
        );
        assert!(path_under(tmp.path(), &dest).is_dir());
        assert_eq!(
            fs::read(under_tmp(&dest, "plugin.toml")).unwrap(),
            toml_before
        );
    }

    #[test]
    fn remove_malformed_trust_ledger_leaves_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, _, _, _, _) = installed_echo(tmp.path());
        let ledger_path = InstallLedger::path(tmp.path());
        let garbage = b"{not-valid-install-ledger";
        fs::write(&ledger_path, garbage).unwrap();
        let toml_before = fs::read(under_tmp(&dest, "plugin.toml")).unwrap();

        let err = Installer::remove(&plugins, "echo", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("install-ledger")
                || err.contains("expected")
                || err.contains("json")
                || err.contains("EOF")
                || err.contains("key must be"),
            "{err}"
        );
        assert_eq!(fs::read(&ledger_path).unwrap(), garbage);
        assert!(path_under(tmp.path(), &dest).is_dir());
        assert_eq!(
            fs::read(under_tmp(&dest, "plugin.toml")).unwrap(),
            toml_before
        );
    }

    #[test]
    fn remove_ledger_write_failure_restores_held_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, key, _, _, _) = installed_echo(tmp.path());
        let toml_before = fs::read(under_tmp(&dest, "plugin.toml")).unwrap();
        let receipt_before = fs::read(under_tmp(&dest, "receipt.json")).unwrap();
        let ledger_before = fs::read(InstallLedger::path(tmp.path())).unwrap();
        fs::create_dir_all(under_tmp(tmp.path(), "install-ledger.json.tmp")).unwrap();

        let err = Installer::remove(&plugins, "echo", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("install ledger")
                || err.contains("Is a directory")
                || err.contains("directory"),
            "{err}"
        );
        assert!(
            path_under(tmp.path(), &dest).is_dir(),
            "held tree must be restored after ledger write failure"
        );
        assert_eq!(
            fs::read(under_tmp(&dest, "plugin.toml")).unwrap(),
            toml_before
        );
        assert_eq!(
            fs::read(under_tmp(&dest, "receipt.json")).unwrap(),
            receipt_before
        );
        assert_eq!(
            fs::read(InstallLedger::path(tmp.path())).unwrap(),
            ledger_before
        );
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_some());
        if under_tmp(&plugins, ".staging").is_dir() {
            let leftover = dir_names(&under_tmp(&plugins, ".staging"));
            assert!(
                leftover.iter().all(|n| !n.contains("removing")),
                "restore must not leave a .removing hold: {leftover:?}"
            );
        }
    }

    /// Rewrites `receipt.json` `plugin_key` while leaving the rest parseable.
    fn rewrite_receipt_plugin_key(dest: &Path, key: &PluginKey) {
        let path = under_tmp(dest, "receipt.json");
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["plugin_key"] = serde_json::Value::String(key.canonical().to_string());
        fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }

    /// Installs `id` from a uniquely named local archive.
    fn installed_named(tmp: &Path, filename: &str, id: &str) -> (PathBuf, PathBuf, PluginKey) {
        let plugins = under_tmp(tmp, "plugins");
        let (archive, digest) = make_named_archive(tmp, filename, id);
        let target = host_bookclerk_target();
        let opts = echo_install_opts(plugins, false);
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let out = Installer::install_from_manifest(
            &echo_manifest(id, digest, format!("file://{}", archive.display()), target),
            &coord,
            &opts,
        )
        .unwrap();
        Installer::commit(&out).unwrap();
        let key = out.receipt.plugin_key().unwrap();
        (out.plugin_root, dest_state(tmp, &key), key)
    }

    /// `$FILES_DIR/plugin-state/<fs-id>` for a test plugin.
    fn dest_state(tmp: &Path, key: &PluginKey) -> PathBuf {
        under_tmp(tmp, Path::new("plugin-state").join(key.fs_id()))
    }

    /// Writes a marker file under `plugin-state/<fs-id>/data`.
    fn write_state_marker(state: &Path, marker: &str) {
        fs::create_dir_all(state).unwrap();
        fs::create_dir_all(under_tmp(state, "data")).unwrap();
        fs::write(under_tmp(state, "data/marker"), marker.as_bytes()).unwrap();
    }

    #[test]
    fn remove_tampered_receipt_cannot_redirect_to_other_plugin_key() {
        let tmp = tempfile::tempdir().unwrap();
        let (dest_a, state_a, key_a) = installed_named(tmp.path(), "a.tar.gz", "plugina");
        let (dest_b, state_b, key_b) = installed_named(tmp.path(), "b.tar.gz", "pluginb");
        write_state_marker(&state_a, "state-a");
        write_state_marker(&state_b, "state-b");
        rewrite_receipt_plugin_key(&dest_a, &key_b);
        let plugins = under_tmp(tmp.path(), "plugins");
        let toml_a = fs::read(under_tmp(&dest_a, "plugin.toml")).unwrap();
        let toml_b = fs::read(under_tmp(&dest_b, "plugin.toml")).unwrap();
        let ledger_before = fs::read(InstallLedger::path(tmp.path())).unwrap();

        let err = Installer::remove(&plugins, "plugina", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("before mutating"), "{err}");
        assert!(
            err.contains("does not match host ledger") || err.contains("cannot redirect"),
            "{err}"
        );
        assert!(path_under(tmp.path(), &dest_a).is_dir());
        assert!(path_under(tmp.path(), &dest_b).is_dir());
        assert_eq!(fs::read(under_tmp(&dest_a, "plugin.toml")).unwrap(), toml_a);
        assert_eq!(fs::read(under_tmp(&dest_b, "plugin.toml")).unwrap(), toml_b);
        assert_eq!(
            fs::read(InstallLedger::path(tmp.path())).unwrap(),
            ledger_before
        );
        assert!(InstallLedger::load(tmp.path())
            .unwrap()
            .get(&key_a)
            .is_some());
        assert!(InstallLedger::load(tmp.path())
            .unwrap()
            .get(&key_b)
            .is_some());
        assert_eq!(
            fs::read(under_tmp(&state_a, "data/marker")).unwrap(),
            b"state-a"
        );
        assert_eq!(
            fs::read(under_tmp(&state_b, "data/marker")).unwrap(),
            b"state-b"
        );
    }

    #[test]
    fn remove_receipt_key_mismatching_directory_fs_id_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let (dest, _, key) = installed_named(tmp.path(), "echo.tar.gz", "echo");
        let foreign =
            PluginKey::from_install_path(&under_tmp(tmp.path(), "other.tar.gz"), "echo").unwrap();
        assert_ne!(foreign.fs_id(), dest.file_name().unwrap().to_string_lossy());
        rewrite_receipt_plugin_key(&dest, &foreign);
        let plugins = under_tmp(tmp.path(), "plugins");
        let err = Installer::remove(&plugins, "echo", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("before mutating"), "{err}");
        assert!(path_under(tmp.path(), &dest).is_dir());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_some());
    }

    #[test]
    fn remove_directory_fs_id_mismatching_ledger_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let (dest, _, key) = installed_named(tmp.path(), "echo.tar.gz", "echo");
        let plugins = under_tmp(tmp.path(), "plugins");
        let dest = path_under(tmp.path(), &dest);
        let renamed = under_tmp(&plugins, "pk-ffffffffffffffffffffffffffffffff");
        fs::rename(&dest, &renamed).unwrap();
        let err = Installer::remove(&plugins, "echo", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("before mutating"), "{err}");
        assert!(
            err.contains("filesystem id") || err.contains("does not match"),
            "{err}"
        );
        assert!(path_under(tmp.path(), &renamed).is_dir());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_some());
    }

    #[test]
    fn remove_without_purge_leaves_state() {
        let tmp = tempfile::tempdir().unwrap();
        let (dest, state, key) = installed_named(tmp.path(), "echo.tar.gz", "echo");
        write_state_marker(&state, "keep");
        Installer::remove(&under_tmp(tmp.path(), "plugins"), "echo", false).unwrap();
        assert!(!path_under(tmp.path(), &dest).exists());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_none());
        assert_eq!(fs::read(under_tmp(&state, "data/marker")).unwrap(), b"keep");
    }

    #[test]
    fn remove_purge_state_deletes_tree_ledger_and_state() {
        let tmp = tempfile::tempdir().unwrap();
        let (dest, state, key) = installed_named(tmp.path(), "echo.tar.gz", "echo");
        write_state_marker(&state, "gone");
        Installer::remove(&under_tmp(tmp.path(), "plugins"), "echo", true).unwrap();
        assert!(!path_under(tmp.path(), &dest).exists());
        assert!(!path_under(tmp.path(), &state).exists());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_none());
    }

    #[test]
    fn remove_purge_state_restores_state_when_ledger_write_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let (dest, state, key) = installed_named(tmp.path(), "echo.tar.gz", "echo");
        write_state_marker(&state, "keep-state");
        let toml_before = fs::read(under_tmp(&dest, "plugin.toml")).unwrap();
        fs::create_dir_all(under_tmp(tmp.path(), "install-ledger.json.tmp")).unwrap();
        let err = Installer::remove(&under_tmp(tmp.path(), "plugins"), "echo", true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("install ledger")
                || err.contains("Is a directory")
                || err.contains("directory"),
            "{err}"
        );
        assert!(path_under(tmp.path(), &dest).is_dir());
        assert_eq!(
            fs::read(under_tmp(&dest, "plugin.toml")).unwrap(),
            toml_before
        );
        assert_eq!(
            fs::read(under_tmp(&state, "data/marker")).unwrap(),
            b"keep-state"
        );
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_some());
        let hold = under_tmp(tmp.path(), PLUGIN_HOLD_DIR);
        if path_under(tmp.path(), &hold).is_dir() {
            let leftover = dir_names(&hold);
            assert!(
                leftover.iter().all(|n| !n.contains("state-removing")),
                "restore must not leave a state hold: {leftover:?}"
            );
        }
        fs::remove_dir_all(under_tmp(tmp.path(), "install-ledger.json.tmp")).unwrap();
        Installer::remove(&under_tmp(tmp.path(), "plugins"), "echo", true).unwrap();
        assert!(!path_under(tmp.path(), &dest).exists());
        assert!(!path_under(tmp.path(), &state).exists());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_none());
    }

    #[test]
    fn rollback_retries_ledger_without_deleting_restored_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, key, manifest, coord, mut opts) = installed_echo(tmp.path());
        fs::write(under_tmp(&dest, "old-marker"), b"v1").unwrap();
        opts.replace = true;
        let second = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        assert!(second
            .previous
            .as_ref()
            .is_some_and(|p| path_under(tmp.path(), p).exists()));
        assert!(!under_tmp(&second.plugin_root, "old-marker").is_file());
        fs::create_dir_all(under_tmp(tmp.path(), "install-ledger.json.tmp")).unwrap();

        let err = Installer::rollback(&second).unwrap_err().to_string();
        assert!(
            err.contains("install-ledger")
                || err.contains("Is a directory")
                || err.contains("directory"),
            "{err}"
        );
        assert!(
            path_under(tmp.path(), &second.plugin_root).is_dir(),
            "restored tree must survive a ledger restore failure"
        );
        assert_eq!(
            fs::read(under_tmp(&second.plugin_root, "old-marker")).unwrap(),
            b"v1"
        );

        let err = Installer::rollback(&second).unwrap_err().to_string();
        assert!(
            path_under(tmp.path(), &second.plugin_root).is_dir(),
            "retry must not delete the restored destination when the backup is gone"
        );
        assert_eq!(
            fs::read(under_tmp(&second.plugin_root, "old-marker")).unwrap(),
            b"v1"
        );
        assert!(
            err.contains("directory") || err.contains("install-ledger"),
            "{err}"
        );

        fs::remove_dir_all(under_tmp(tmp.path(), "install-ledger.json.tmp")).unwrap();
        Installer::rollback(&second).unwrap();
        assert!(under_tmp(&second.plugin_root, "old-marker").is_file());
        assert!(InstallLedger::load(tmp.path()).unwrap().get(&key).is_some());
        assert_eq!(
            dir_names(&plugins)
                .iter()
                .filter(|n| n.starts_with("pk-"))
                .count(),
            1
        );
    }

    #[test]
    fn first_install_rollback_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let (archive, digest) = make_echo_archive(tmp.path());
        let plugins = under_tmp(tmp.path(), "plugins");
        let opts = echo_install_opts(plugins, false);
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let out = Installer::install_from_manifest(
            &echo_manifest(
                "echo",
                digest,
                format!("file://{}", archive.display()),
                host_bookclerk_target(),
            ),
            &coord,
            &opts,
        )
        .unwrap();
        assert!(out.previous.is_none());
        Installer::rollback(&out).unwrap();
        assert!(!path_under(tmp.path(), &out.plugin_root).exists());
        Installer::rollback(&out).unwrap();
        assert!(!path_under(tmp.path(), &out.plugin_root).exists());
        assert!(InstallLedger::load(tmp.path())
            .unwrap()
            .get(&out.receipt.plugin_key().unwrap())
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn first_install_rollback_refuses_dest_symlink_to_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let (archive, digest) = make_echo_archive(tmp.path());
        let plugins = under_tmp(tmp.path(), "plugins");
        let opts = echo_install_opts(plugins.clone(), false);
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let out = Installer::install_from_manifest(
            &echo_manifest(
                "echo",
                digest,
                format!("file://{}", archive.display()),
                host_bookclerk_target(),
            ),
            &coord,
            &opts,
        )
        .unwrap();
        assert!(out.previous.is_none());
        let dest = out.plugin_root.clone();
        let sibling = under_tmp(&plugins, "sibling-keep");
        fs::create_dir_all(&sibling).unwrap();
        let sentinel = under_tmp(&sibling, "sentinel.txt");
        fs::write(&sentinel, b"keep-sibling").unwrap();
        // Replace the install entry with a leaf symlink to the sibling.
        fs::remove_dir_all(&dest).unwrap();
        std::os::unix::fs::symlink(&sibling, &dest).unwrap();

        let err = Installer::rollback(&out).unwrap_err().to_string();
        assert!(
            err.contains("symlink") || err.contains("mutation"),
            "expected symlink refusal, got {err}"
        );
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep-sibling");
        assert!(dest.is_symlink());
        assert!(sibling.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn first_install_rollback_refuses_dest_symlink_to_plugins_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (archive, digest) = make_echo_archive(tmp.path());
        let plugins = under_tmp(tmp.path(), "plugins");
        let opts = echo_install_opts(plugins.clone(), false);
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let out = Installer::install_from_manifest(
            &echo_manifest(
                "echo",
                digest,
                format!("file://{}", archive.display()),
                host_bookclerk_target(),
            ),
            &coord,
            &opts,
        )
        .unwrap();
        let dest = out.plugin_root.clone();
        let root_sentinel = under_tmp(&plugins, "root-sentinel.txt");
        fs::write(&root_sentinel, b"keep-root").unwrap();
        fs::remove_dir_all(&dest).unwrap();
        std::os::unix::fs::symlink(&plugins, &dest).unwrap();

        let err = Installer::rollback(&out).unwrap_err().to_string();
        assert!(
            err.contains("symlink") || err.contains("mutation") || err.contains("strict child"),
            "expected refusal of plugins-root alias, got {err}"
        );
        assert_eq!(fs::read(&root_sentinel).unwrap(), b"keep-root");
        assert!(plugins.is_dir());
        assert!(dest.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn update_rollback_refuses_symlinked_dest_preserving_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let (plugins, dest, _key, manifest, coord, mut opts) = installed_echo(tmp.path());
        fs::write(under_tmp(&dest, "old-marker"), b"v1").unwrap();
        opts.replace = true;
        let second = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        let bak = second.previous.clone().expect("backup from replace");
        assert!(path_under(tmp.path(), &bak).exists());
        assert!(fs::read(under_tmp(&bak, "old-marker")).unwrap() == b"v1");

        let sibling = under_tmp(&plugins, "peer-keep");
        fs::create_dir_all(&sibling).unwrap();
        let sentinel = under_tmp(&sibling, "peer.txt");
        fs::write(&sentinel, b"peer-ok").unwrap();
        // Swap the new install tree for a symlink to the peer.
        fs::remove_dir_all(&second.plugin_root).unwrap();
        std::os::unix::fs::symlink(&sibling, &second.plugin_root).unwrap();

        let err = Installer::rollback(&second).unwrap_err().to_string();
        assert!(
            err.contains("symlink") || err.contains("mutation"),
            "expected symlink refusal, got {err}"
        );
        assert_eq!(fs::read(&sentinel).unwrap(), b"peer-ok");
        assert!(path_under(tmp.path(), &bak).is_dir());
        assert_eq!(fs::read(under_tmp(&bak, "old-marker")).unwrap(), b"v1");
        assert!(second.plugin_root.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn restore_held_tree_refuses_symlinked_hold_source() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = under_tmp(tmp.path(), "plugins");
        fs::create_dir_all(&plugins).unwrap();
        let staging = under_tmp(&plugins, ".staging");
        fs::create_dir_all(&staging).unwrap();
        let real_hold = under_tmp(&staging, "real-hold");
        fs::create_dir_all(&real_hold).unwrap();
        let sentinel = under_tmp(&real_hold, "held.txt");
        fs::write(&sentinel, b"held-bytes").unwrap();
        let link_hold = under_tmp(&staging, "link-hold");
        std::os::unix::fs::symlink(&real_hold, &link_hold).unwrap();
        let dest = under_tmp(&plugins, "restore-dest");

        let err = restore_held_tree(&link_hold, &dest)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("symlink") || err.contains("mutation"),
            "expected held symlink refusal, got {err}"
        );
        assert_eq!(fs::read(&sentinel).unwrap(), b"held-bytes");
        assert!(real_hold.is_dir());
        assert!(!dest.exists());
        assert!(link_hold.is_symlink());
    }

    #[test]
    fn commit_cleanup_failure_is_surfaced() {
        let tmp = tempfile::tempdir().unwrap();
        let bak = under_tmp(tmp.path(), "not-a-dir-backup");
        fs::write(&bak, b"file").unwrap();
        let outcome = InstallOutcome {
            plugin_root: under_tmp(tmp.path(), "dest"),
            receipt: InstallReceipt::load(&{
                let (dest, _, _) = installed_named(tmp.path(), "echo.tar.gz", "echo");
                dest
            })
            .unwrap(),
            dry_run: false,
            previous: Some(bak),
            previous_ledger: None,
            files_dir: Some(tmp.path().to_path_buf()),
        };
        let err = Installer::commit(&outcome).unwrap_err().to_string();
        assert!(
            err.contains("failed to remove")
                || err.contains("Not a directory")
                || err.contains("not a directory"),
            "{err}"
        );
    }

    /// Minimal v3 `plugin.toml` for an integration guest.
    fn echo_toml(command: &str, network_mode: &str, entrypoints: &str) -> String {
        format!(
            "api_version = 3\nid = \"echo\"\ncommand = \"{command}\"\n\
             entrypoints = [{entrypoints}]\n[capabilities.network]\nmode = \"{network_mode}\"\n"
        )
    }

    #[test]
    fn toml_binds_family_network_and_command() {
        let runtime = RuntimeIdentity::new(PluginKind::Integration, "echo");
        validate_plugin_toml(
            &echo_toml("./echo", "deny", "\"cli\""),
            &runtime,
            "none",
            "echo",
        )
        .unwrap();
        let bad_net = echo_toml("./echo", "outbound", "\"cli\"");
        assert!(validate_plugin_toml(&bad_net, &runtime, "none", "echo").is_err());
        let bad_cmd = echo_toml("./other", "outbound", "\"cli\"");
        assert!(validate_plugin_toml(&bad_cmd, &runtime, "outbound", "echo").is_err());
        // A storefront-only manifest is not an integration package.
        let bad_family = echo_toml("./echo", "deny", "\"storefront\"");
        let err = validate_plugin_toml(&bad_family, &runtime, "none", "echo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("package kind `integration`"), "{err}");
        // Legacy `kind` keys are rejected by the strict v3 parser.
        let legacy =
            "api_version = 3\nid = \"echo\"\nkind = \"integration\"\ncommand = \"./echo\"\n\
                      entrypoints = [\"cli\"]\n[capabilities.network]\nmode = \"deny\"\n";
        assert!(validate_plugin_toml(legacy, &runtime, "none", "echo").is_err());
    }
}
