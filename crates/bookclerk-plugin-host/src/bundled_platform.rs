//! Startup install of the platform plugins a release ships beside the host.
//!
//! A platform release keeps `plugins/<PluginKey fs-id>/` next to the host
//! binaries. Before the database adapter is discovered, the host installs any
//! of those trees that `$BOOKCLERK_FILES_DIR` lacks (or holds an older or
//! untrusted copy of) and records them in the install ledger, so a fresh
//! files directory, such as a container volume, starts with working
//! `sqlite` and `local` plugins. See
//! [`bookclerk_plugin_catalog::sync_bundled_platform`].

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use bookclerk_plugin_catalog::{
    bundled_plugins_dir_beside, sync_bundled_platform, BundledSyncAction,
};

/// Files directories this process has already synced, so repeat database
/// loads (config reload, scratch libraries) do not re-hash the bundle.
static SYNCED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Installs bundled platform plugins from beside the running executable into `files_dir`.
///
/// A no-op when no `plugins/` directory sits beside the executable (development
/// builds) or this process already synced `files_dir`. Failures are logged and
/// do not stop startup: discovery then reports the missing or untrusted plugin
/// the same way it would without a bundle.
///
/// The first call for a files directory takes its
/// [`bookclerk_plugin_catalog::PluginMutationLock`], so it must not run while
/// this process already holds that lock.
///
/// # Arguments
///
/// * `files_dir` - Host `$BOOKCLERK_FILES_DIR` that owns `plugins/` and the install ledger.
pub fn install_bundled_platform_plugins(files_dir: &Path) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(bundle) = bundled_plugins_dir_beside(&exe) else {
        return;
    };
    let mut synced = SYNCED.lock().unwrap_or_else(PoisonError::into_inner);
    if synced.iter().any(|dir| dir == files_dir) {
        return;
    }
    match sync_bundled_platform(&bundle, files_dir) {
        Ok(rows) => {
            for row in rows {
                let alias = row.artifact.manifest_id;
                let version = row.bundled_version.as_str();
                match &row.action {
                    BundledSyncAction::Installed => {
                        tracing::info!(%alias, %version, "installed bundled platform plugin");
                    }
                    BundledSyncAction::Updated => {
                        tracing::info!(%alias, %version, "updated platform plugin from the bundled copy");
                    }
                    BundledSyncAction::Current => {
                        tracing::debug!(%alias, %version, "bundled platform plugin is current");
                    }
                    BundledSyncAction::KeptNewer { installed } => {
                        tracing::warn!(
                            %alias,
                            bundled = %version,
                            %installed,
                            "kept the installed platform plugin: it is newer than the one bundled with this host"
                        );
                    }
                    BundledSyncAction::AliasOccupied { path } => {
                        tracing::warn!(
                            %alias,
                            occupant = %path.display(),
                            "did not install the bundled platform plugin: another install uses its name; move that directory to use the bundled one"
                        );
                    }
                }
            }
            synced.push(files_dir.to_path_buf());
        }
        Err(err) => {
            tracing::warn!(
                bundle = %bundle.display(),
                error = %err,
                "could not install the platform plugins bundled with this host"
            );
        }
    }
}
