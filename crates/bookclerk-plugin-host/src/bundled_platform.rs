//! Startup install of the platform plugins a release ships beside the host.
//!
//! A platform release keeps `plugins/<PluginKey fs-id>/` next to the host
//! binaries (the desktop app keeps it in its packaged runtime). Before the database adapter is discovered, the host installs any
//! of those trees that `$BOOKCLERK_FILES_DIR` lacks (or holds an untrusted or
//! different build of) and records them in the install ledger, so a fresh
//! files directory, such as a container volume, starts with working `sqlite`
//! and `local` plugins. See [`bookclerk_plugin_catalog::sync_bundled_platform`].

use std::path::{Path, PathBuf};

use bookclerk_plugin_catalog::{
    bundled_plugins_dir_beside, sync_bundled_platform, BundledSync, BundledSyncAction,
};

/// Files directories this process has already synced (or tried to), so repeat
/// database loads (config reload, scratch libraries) do not re-hash the bundle.
///
/// An async mutex held for the whole sync: a concurrent caller for the same
/// files directory waits for it instead of discovering plugins mid-install.
static ATTEMPTED: tokio::sync::Mutex<Vec<PathBuf>> = tokio::sync::Mutex::const_new(Vec::new());

/// Installs bundled platform plugins from beside the running executable into `files_dir`.
///
/// A no-op when no `plugins/` directory sits beside the executable (development
/// builds) or this process already tried `files_dir`. The hashing and copying
/// run on the blocking pool. Failures are logged and do not stop startup:
/// discovery then reports the missing or untrusted plugin the same way it
/// would without a bundle.
///
/// When something has to be installed the sync takes the files directory's
/// [`bookclerk_plugin_catalog::PluginMutationLock`], so the first call for a
/// files directory must not run while this process holds that lock.
///
/// # Arguments
///
/// * `files_dir` - Host `$BOOKCLERK_FILES_DIR` that owns `plugins/` and the install ledger.
pub async fn install_bundled_platform_plugins(files_dir: &Path) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(bundle) = bundled_plugins_dir_for(&exe) else {
        return;
    };
    let mut attempted = ATTEMPTED.lock().await;
    if attempted.iter().any(|dir| dir == files_dir) {
        return;
    }
    attempted.push(files_dir.to_path_buf());
    let files = files_dir.to_path_buf();
    let worker_bundle = bundle.clone();
    let result =
        tokio::task::spawn_blocking(move || sync_bundled_platform(&worker_bundle, &files)).await;
    match result {
        Ok(Ok(rows)) => rows.iter().for_each(log_row),
        Ok(Err(err)) => tracing::warn!(
            bundle = %bundle.display(),
            error = %err,
            "could not install the platform plugins bundled with this host"
        ),
        Err(err) => tracing::warn!(
            bundle = %bundle.display(),
            error = %err,
            "the bundled platform plugin install did not finish"
        ),
    }
}

/// The bundled platform plugins for the running executable: `plugins/`
/// beside it, else in the desktop app's packaged runtime
/// ([`bookclerk_sandbox::packaged_runtime_dirs`]), else none.
///
/// # Arguments
///
/// * `exe` - Path of the running executable.
///
/// # Returns
///
/// The first of those `plugins/` directories that exists.
fn bundled_plugins_dir_for(exe: &Path) -> Option<PathBuf> {
    bundled_plugins_dir_beside(exe).or_else(|| {
        bookclerk_sandbox::packaged_runtime_dirs(exe)
            .into_iter()
            // `bundled_plugins_dir_beside` looks beside a file, so name one
            // inside each runtime directory.
            .find_map(|runtime| bundled_plugins_dir_beside(&runtime.join("bookclerk-desktop")))
    })
}

/// Logs one sync outcome at a level that matches what the operator must do.
fn log_row(row: &BundledSync) {
    let alias = row.artifact.manifest_id;
    match &row.action {
        BundledSyncAction::Installed => {
            tracing::info!(%alias, "installed bundled platform plugin");
        }
        BundledSyncAction::Updated => {
            tracing::info!(%alias, "updated platform plugin from the bundled copy");
        }
        BundledSyncAction::Current => {
            tracing::debug!(%alias, "bundled platform plugin is current");
        }
        BundledSyncAction::AliasOccupied { path } => {
            tracing::warn!(
                %alias,
                occupant = %path.display(),
                "did not install the bundled platform plugin: another directory uses its name; move it to use the bundled one"
            );
        }
        BundledSyncAction::Failed { reason } => {
            tracing::warn!(
                %alias,
                %reason,
                "could not install the bundled platform plugin"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_app_finds_plugins_in_its_packaged_runtime() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let runtime = root.path().join("lib/Bookclerk/runtime");
        std::fs::create_dir_all(runtime.join("plugins")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        assert_eq!(
            bundled_plugins_dir_for(&bin.join("bookclerk-desktop")),
            Some(runtime.join("plugins"))
        );
        // The daemon does not look there.
        assert_eq!(bundled_plugins_dir_for(&bin.join("bookclerkd")), None);
        // `plugins/` beside the executable still wins.
        std::fs::create_dir_all(bin.join("plugins")).unwrap();
        assert_eq!(
            bundled_plugins_dir_for(&bin.join("bookclerk-desktop")),
            Some(bin.join("plugins"))
        );
    }
}
