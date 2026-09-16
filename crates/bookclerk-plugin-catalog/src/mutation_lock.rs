//! Host-local inter-process plugin mutation lock.
//!
//! Plugin deployment state is **host-local**. Shared Bookclerk application
//! data may live in a common database, but each host owns its own plugin
//! inventory, trust ledger, runtime state, and mutation lock.
//!
//! The lock file lives at `$FILES_DIR/.plugin-mutation.lock`. Two hosts with
//! distinct `$BOOKCLERK_FILES_DIR` values have independent locks and do not
//! coordinate through a shared PostgreSQL/D1 database. That is intentional:
//! the same alias and PluginKey may be installed on Host A and Host B
//! without cluster-wide serialization.
//!
//! A future stable `HostId` (for example `$FILES_DIR/host.json`) is expected
//! to map naturally onto this files-dir namespace for placement/scheduling.
//! This module does not persist a HostId.
//!
//! The lock is an OS advisory lock on an open file descriptor. The lock
//! *file* may remain after a crash; ownership is released when the process
//! exits and the descriptor is closed.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

use crate::error::{CatalogError, Result};

/// Filename under `$BOOKCLERK_FILES_DIR` for the host plugin mutation lock.
pub const PLUGIN_MUTATION_LOCK_FILE: &str = ".plugin-mutation.lock";

/// RAII exclusive lock covering one host plugin namespace (`$FILES_DIR`).
///
/// Hold this guard for the full local mutation transaction: configured alias
/// preflight, ledger read, tree/state hold, ledger write, health, and
/// commit or rollback. Drop (or process exit) releases the OS lock.
///
/// Binding the guard is required: `PluginMutationLock::acquire(dir)?;` without
/// an owning binding drops the lock immediately.
#[must_use = "the plugin mutation lock is released when this guard is dropped"]
#[derive(Debug)]
pub struct PluginMutationLock {
    /// Open lock file; the OS advisory lock is released when this is dropped.
    _file: File,
    /// Host files directory this lock serializes.
    files_dir: PathBuf,
}

impl PluginMutationLock {
    /// Absolute path to `.plugin-mutation.lock` under `files_dir`.
    #[must_use]
    pub fn path(files_dir: &Path) -> PathBuf {
        files_dir.join(PLUGIN_MUTATION_LOCK_FILE)
    }

    /// Acquires an exclusive inter-process lock for `files_dir`.
    ///
    /// Blocks until the lock is available. Creates `files_dir` and the lock
    /// file when missing. Does not take a shared-database or cluster lock.
    ///
    /// # Errors
    ///
    /// Returns when `files_dir` cannot be created, the lock file cannot be
    /// opened, or the OS refuses the advisory lock.
    pub fn acquire(files_dir: &Path) -> Result<Self> {
        if files_dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(CatalogError::message(
                "refusing plugin mutation lock path with '..' in files_dir",
            ));
        }
        std::fs::create_dir_all(files_dir)?;
        let path = Self::path(files_dir);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                CatalogError::message(format!(
                    "open plugin mutation lock {}: {err}",
                    path.display()
                ))
            })?;
        file.lock_exclusive().map_err(|err| {
            CatalogError::message(format!(
                "acquire plugin mutation lock {}: {err}",
                path.display()
            ))
        })?;
        Ok(Self {
            _file: file,
            files_dir: files_dir.to_path_buf(),
        })
    }

    /// Host files directory this lock owns.
    #[must_use]
    pub fn files_dir(&self) -> &Path {
        &self.files_dir
    }

    /// True when `plugins_root` is the `plugins/` directory under this lock.
    #[must_use]
    pub fn covers_plugins_root(&self, plugins_root: &Path) -> bool {
        plugins_root
            .parent()
            .is_some_and(|parent| parent == self.files_dir)
    }

    /// Rejects a plugins root that is not this lock's host namespace.
    ///
    /// # Errors
    ///
    /// Returns when `plugins_root` is not `$FILES_DIR/plugins` for this lock.
    pub fn require_plugins_root(&self, plugins_root: &Path) -> Result<()> {
        if self.covers_plugins_root(plugins_root) {
            Ok(())
        } else {
            Err(CatalogError::message(format!(
                "plugin mutation lock for {} does not cover plugins root {}",
                self.files_dir.display(),
                plugins_root.display()
            )))
        }
    }
}

/// Acquires the host mutation lock for `plugins_root`'s parent files dir.
///
/// # Errors
///
/// Returns when the lock cannot be taken. `None` when `plugins_root` has no
/// parent (no host files dir).
pub fn acquire_plugins_lock(plugins_root: &Path) -> Result<Option<PluginMutationLock>> {
    match plugins_root.parent() {
        Some(dir) => Ok(Some(PluginMutationLock::acquire(dir)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_is_files_dir_dotfile() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = PluginMutationLock::acquire(tmp.path()).unwrap();
        assert_eq!(
            PluginMutationLock::path(tmp.path()).file_name().unwrap(),
            PLUGIN_MUTATION_LOCK_FILE
        );
        assert!(lock.covers_plugins_root(&tmp.path().join("plugins")));
        assert!(!lock.covers_plugins_root(tmp.path()));
        assert!(PluginMutationLock::path(tmp.path()).is_file());
    }

    #[test]
    fn distinct_files_dirs_are_independent() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let lock_a = PluginMutationLock::acquire(a.path()).unwrap();
        let lock_b = PluginMutationLock::acquire(b.path()).unwrap();
        assert_ne!(lock_a.files_dir(), lock_b.files_dir());
    }
}
