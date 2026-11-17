//! Removes isolate state directories that a killed launcher left behind.
//!
//! Each isolate session gets a fresh state directory under the guest's
//! `TMPDIR` (see [`crate::config::workerd_state_dir`]), which the launcher
//! removes when it returns. When the host stops a session by killing the
//! launcher, as it does when a plugin goes to sleep, that cleanup never
//! runs, and every start of a sleeping plugin would leave about half a
//! megabyte in the plugin's `tmp` until its disk budget refuses to start it.
//!
//! So each launcher holds an exclusive lock on [`OWNER_LOCK_FILE`] inside its
//! state directory for as long as it lives; the operating system drops the
//! lock when the process dies, however it dies. Before allocating its own
//! directory, a launcher removes every sibling directory of the same plugin
//! whose lock it can take, and every one older than [`UNLOCKED_GRACE`] that
//! never got a lock file.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{plugin_root_prefix, workerd_state_base};

/// The lock file a live launcher holds inside its state directory.
pub const OWNER_LOCK_FILE: &str = ".owner.lock";

/// How old a state directory without a lock file must be before it counts
/// as abandoned: well past the moment between creating a directory and
/// locking it.
pub const UNLOCKED_GRACE: Duration = Duration::from_secs(120);

/// Proof that this process owns a state directory; dropping it (or the
/// process dying) releases the lock.
#[derive(Debug)]
pub struct StateDirOwner {
    /// The locked file, held open for the life of the session.
    _lock: fs::File,
}

/// Takes the owner lock on `state_dir`.
///
/// # Arguments
///
/// * `state_dir` - The session's freshly created state directory.
///
/// # Errors
///
/// Returns an error when the lock file cannot be created or locked.
pub fn claim(state_dir: &Path) -> Result<StateDirOwner> {
    let path = state_dir.join(OWNER_LOCK_FILE);
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(StateDirOwner { _lock: file }),
        Err(fs::TryLockError::WouldBlock) => {
            anyhow::bail!("{} is already locked", path.display())
        }
        Err(fs::TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("lock {}", path.display()))
        }
    }
}

/// Removes the abandoned state directories of the plugin at `plugin_root`.
///
/// Only directories named for this plugin root are considered, links are
/// never followed, and a directory a live launcher holds is left alone.
/// Failures are skipped: a directory that cannot be removed now is tried
/// again by the next start.
///
/// # Arguments
///
/// * `plugin_root` - The plugin install directory the state is named for.
///
/// # Returns
///
/// The directories removed.
pub fn sweep_abandoned(plugin_root: &Path) -> Vec<PathBuf> {
    let base = workerd_state_base(plugin_root);
    let prefix = format!("w{}", plugin_root_prefix(plugin_root));
    let removed = sweep_under(&base, &prefix, UNLOCKED_GRACE);
    if !removed.is_empty() {
        tracing::info!(
            count = removed.len(),
            "removed abandoned workerd state directories"
        );
    }
    removed
}

/// [`sweep_abandoned`] over `base`, for directories named `prefix…`.
fn sweep_under(base: &Path, prefix: &str, grace: Duration) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        // `DirEntry::file_type` does not follow links.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let dir = entry.path();
        if abandoned(&dir, grace) && fs::remove_dir_all(&dir).is_ok() {
            removed.push(dir);
        }
    }
    removed
}

/// Whether no live launcher owns `dir`.
fn abandoned(dir: &Path, grace: Duration) -> bool {
    let lock = dir.join(OWNER_LOCK_FILE);
    match fs::symlink_metadata(&lock) {
        Ok(meta) if meta.file_type().is_file() => {
            let Ok(file) = fs::OpenOptions::new().write(true).open(&lock) else {
                return false;
            };
            // Taking the lock means its owner is gone; it is released when
            // `file` drops, before the directory is removed.
            file.try_lock().is_ok()
        }
        // Not a regular file: not ours to judge.
        Ok(_) => false,
        // No lock yet: abandoned only once it is clearly not being set up.
        Err(_) => fs::symlink_metadata(dir)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > grace),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(base: &Path, name: &str) -> PathBuf {
        let dir = base.join(name);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("workerd-config.capnp"), b"x").expect("write");
        dir
    }

    #[test]
    fn a_dead_owners_directory_is_removed_and_a_live_one_kept() {
        let base = tempfile::tempdir().expect("tempdir");
        let live = make(base.path(), "wabc0001");
        let _owner = claim(&live).expect("claim");
        let dead = make(base.path(), "wabc0002");
        drop(claim(&dead).expect("claim"));

        let removed = sweep_under(base.path(), "wabc", UNLOCKED_GRACE);
        assert_eq!(removed, vec![dead.clone()]);
        assert!(live.is_dir());
        assert!(!dead.exists());
    }

    #[test]
    fn an_unlocked_directory_waits_out_the_grace() {
        let base = tempfile::tempdir().expect("tempdir");
        let fresh = make(base.path(), "wabc0003");
        assert!(sweep_under(base.path(), "wabc", UNLOCKED_GRACE).is_empty());
        assert!(fresh.is_dir(), "may be a launcher about to lock it");
        assert_eq!(
            sweep_under(base.path(), "wabc", Duration::ZERO),
            vec![fresh.clone()]
        );
    }

    #[test]
    fn other_plugins_and_files_are_left_alone() {
        let base = tempfile::tempdir().expect("tempdir");
        let other = make(base.path(), "wfff0001");
        fs::write(base.path().join("wabc-file"), b"x").expect("write");
        assert!(sweep_under(base.path(), "wabc", Duration::ZERO).is_empty());
        assert!(other.is_dir());
        assert!(base.path().join("wabc-file").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_never_followed() {
        let base = tempfile::tempdir().expect("tempdir");
        let target = tempfile::tempdir().expect("tempdir");
        fs::write(target.path().join("keep"), b"x").expect("write");
        std::os::unix::fs::symlink(target.path(), base.path().join("wabc0004")).expect("link");
        assert!(sweep_under(base.path(), "wabc", Duration::ZERO).is_empty());
        assert!(target.path().join("keep").is_file());
    }

    #[test]
    fn a_dead_session_of_this_plugin_is_swept_from_the_state_base() {
        // A root no other test or plugin shares, so its prefix is unique.
        let root = tempfile::tempdir().expect("tempdir");
        let live = crate::config::workerd_state_dir(root.path()).expect("state dir");
        let _owner = claim(&live).expect("claim");
        let dead = crate::config::workerd_state_dir(root.path()).expect("state dir");
        drop(claim(&dead).expect("claim"));

        assert_eq!(sweep_abandoned(root.path()), vec![dead.clone()]);
        assert!(!dead.exists());
        assert!(live.is_dir());
        fs::remove_dir_all(&live).expect("cleanup");
    }

    #[test]
    fn a_second_claim_on_a_live_directory_fails() {
        let base = tempfile::tempdir().expect("tempdir");
        let dir = make(base.path(), "wabc0005");
        let _owner = claim(&dir).expect("claim");
        assert!(claim(&dir).is_err());
    }
}
