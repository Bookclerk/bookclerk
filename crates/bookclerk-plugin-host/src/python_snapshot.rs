//! Host-side store of Python start-up snapshots, one per plugin version.
//!
//! A Python isolate takes seconds to start; started from a workerd snapshot
//! taken after its imports, it takes a fraction of a second (see
//! [`bookclerk_workerd::python_snapshot`]). The host keeps the snapshot in
//! `plugin-state/<fs-id>/snapshot/<key>/snapshot.bin`, where the key hashes the
//! pinned workerd release and every file of the installed plugin, so a new
//! plugin version or a new workerd never loads a stale snapshot.
//!
//! The guest's jail may read only the key directory it starts from (workerd
//! opens the snapshot's directory) and write only a fresh staging directory. The launcher saves a new snapshot there
//! when there is none to load, or the stored one did not start, and marks it
//! ready once the isolate answered. The host then promotes the ready file into
//! the store (see [`SnapshotSlot::promote_when_ready`]) and drops older keys.
//! A missing, broken or refused snapshot only costs the slow start.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookclerk_workerd::python_snapshot::{
    self as launcher, LOADED_FILE, MAX_SNAPSHOT_BYTES, READY_FILE,
};
use sha2::{Digest, Sha256};

use crate::DiscoveredPlugin;

/// Directory under a plugin's state root that holds its snapshots.
pub(crate) const SNAPSHOT_DIR: &str = "snapshot";

/// The file name of a stored snapshot inside its key directory.
const STORED_FILE: &str = "snapshot.bin";

/// The most files a plugin install may hold for the host to key a snapshot
/// on it; a larger install starts without one.
const MAX_KEYED_FILES: usize = 10_000;

/// The most bytes a plugin install may hold for the host to key a snapshot
/// on it; a larger install starts without one.
const MAX_KEYED_BYTES: u64 = 256 * 1024 * 1024;

/// How long the host waits for a staged snapshot to become ready before it
/// gives up and removes the staging directory: the launcher's two bounded
/// starts (30 s each) with room to spare.
const READY_WAIT: Duration = Duration::from_secs(90);

/// How old a staging directory must be before a later start removes it as
/// left behind; well past [`READY_WAIT`].
const STALE_STAGING: Duration = Duration::from_secs(600);

/// How often the host looks for a ready snapshot while it waits.
const READY_POLL: Duration = Duration::from_millis(250);

/// The snapshot paths for one Python guest start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotSlot {
    /// Where this plugin version's snapshot is stored
    /// (`<root>/<key>/snapshot.bin`).
    store: PathBuf,
    /// Whether [`Self::store`] holds a snapshot to start from.
    load: bool,
    /// The fresh, empty directory the launcher may save a snapshot into.
    staging: PathBuf,
}

impl SnapshotSlot {
    /// Plans the snapshot paths for one start of `plugin`, creating the
    /// store and a fresh staging directory.
    ///
    /// # Arguments
    ///
    /// * `state_root` - The plugin's host-managed state root
    ///   (`plugin-state/<fs-id>`).
    /// * `plugin` - The plugin to start.
    ///
    /// # Returns
    ///
    /// `None` for a plugin that is not Python, an install too large to key,
    /// or when the directories cannot be created; the guest then starts
    /// without a snapshot.
    pub(crate) fn plan(state_root: &Path, plugin: &DiscoveredPlugin) -> Option<Self> {
        if !plugin.manifest.declares_python() {
            return None;
        }
        let key = match install_key(&plugin.root) {
            Ok(Some(key)) => key,
            Ok(None) => {
                tracing::debug!(
                    plugin = %plugin.alias(),
                    "plugin install too large to key a Python snapshot"
                );
                return None;
            }
            Err(err) => {
                tracing::warn!(
                    plugin = %plugin.alias(),
                    error = %err,
                    "could not key a Python snapshot; starting without one"
                );
                return None;
            }
        };
        match Self::create(&state_root.join(SNAPSHOT_DIR), &key) {
            Ok(slot) => Some(slot),
            Err(err) => {
                tracing::warn!(
                    plugin = %plugin.alias(),
                    error = %err,
                    "could not prepare the Python snapshot directory; starting without one"
                );
                None
            }
        }
    }

    /// Creates the store directory and a staging directory for `key`,
    /// first removing staging directories older than [`STALE_STAGING`] that
    /// a host which stopped mid-wait left behind.
    fn create(root: &Path, key: &str) -> std::io::Result<Self> {
        create_private_dir(root)?;
        sweep_stale_staging(root, STALE_STAGING);
        let store = root.join(key).join(STORED_FILE);
        let load = std::fs::symlink_metadata(&store).is_ok_and(|meta| meta.file_type().is_file());
        let staging = root.join(format!("staging-{}", random_hex()));
        create_private_dir(&staging)?;
        Ok(Self {
            store,
            load,
            staging,
        })
    }

    /// The directory of the stored snapshot the guest's jail may read, when
    /// there is one. workerd opens the snapshot's directory, so the grant is
    /// the key directory, which holds nothing else.
    pub(crate) fn read_path(&self) -> Option<&Path> {
        if self.load {
            self.store.parent()
        } else {
            None
        }
    }

    /// The staging directory the guest's jail may write.
    pub(crate) fn write_path(&self) -> &Path {
        &self.staging
    }

    /// The launcher environment for this start.
    pub(crate) fn env(&self) -> Vec<(&'static str, OsString)> {
        let mut env = vec![(launcher::SAVE_ENV, self.staging.clone().into_os_string())];
        if self.load {
            env.push((launcher::LOAD_ENV, self.store.clone().into_os_string()));
        }
        env
    }

    /// Watches the staging directory in the background and promotes the
    /// snapshot once the launcher marks it ready, then removes the staging
    /// directory. Gives up after [`READY_WAIT`] (the start failed, or the
    /// launcher loaded the stored snapshot and saved nothing).
    pub(crate) fn promote_when_ready(self) {
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + READY_WAIT;
            while tokio::time::Instant::now() < deadline {
                if self.staging.join(READY_FILE).exists() || self.staging.join(LOADED_FILE).exists()
                {
                    break;
                }
                tokio::time::sleep(READY_POLL).await;
            }
            let slot = self;
            let _ = tokio::task::spawn_blocking(move || slot.promote()).await;
        });
    }

    /// Moves a ready snapshot into the store, drops older keys, and removes
    /// the staging directory.
    ///
    /// # Returns
    ///
    /// Whether a snapshot was promoted.
    fn promote(&self) -> bool {
        let ready = self.staging.join(READY_FILE);
        let promoted = match std::fs::symlink_metadata(&ready) {
            Ok(meta) if meta.file_type().is_file() && meta.len() <= MAX_SNAPSHOT_BYTES => {
                let stored = self
                    .store
                    .parent()
                    .map_or(Ok(()), create_private_dir)
                    .and_then(|()| std::fs::rename(&ready, &self.store));
                match stored {
                    Ok(()) => {
                        self.drop_other_keys();
                        tracing::info!(
                            snapshot = %self.store.display(),
                            bytes = meta.len(),
                            "stored a Python start-up snapshot"
                        );
                        true
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "could not store the Python snapshot");
                        false
                    }
                }
            }
            Ok(meta) if meta.file_type().is_file() => {
                tracing::warn!(
                    bytes = meta.len(),
                    "refusing a Python snapshot larger than the host keeps"
                );
                false
            }
            // Not a regular file (a link the jail planted), or nothing saved.
            Ok(_) | Err(_) => false,
        };
        let _ = std::fs::remove_dir_all(&self.staging);
        promoted
    }

    /// Removes snapshots stored under other keys (older plugin versions or
    /// workerd releases). Staging directories of other starts are left to
    /// their own watchers.
    fn drop_other_keys(&self) {
        let Some(key_dir) = self.store.parent() else {
            return;
        };
        let Some(root) = key_dir.parent() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let staging = entry.file_name().to_string_lossy().starts_with("staging-");
            if staging || path == key_dir {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    let _ = std::fs::remove_dir_all(&path);
                }
                Ok(_) => {
                    let _ = std::fs::remove_file(&path);
                }
                Err(_) => {}
            }
        }
    }
}

/// The snapshot key of a plugin install: a hash of the pinned workerd
/// release and every regular file under `root`, by relative path and
/// content.
///
/// # Returns
///
/// `None` when the install holds more than [`MAX_KEYED_FILES`] files or
/// [`MAX_KEYED_BYTES`] bytes.
///
/// # Errors
///
/// Returns the I/O error when the install cannot be walked or read.
fn install_key(root: &Path) -> std::io::Result<Option<String>> {
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
                files.push(path);
                if files.len() > MAX_KEYED_FILES || total > MAX_KEYED_BYTES {
                    return Ok(None);
                }
            }
            // Links are skipped: the jail reads the install, not their targets.
        }
    }
    files.sort();
    let mut hasher = Sha256::new();
    hasher.update(bookclerk_workerd::WORKERD_RELEASE_TAG.as_bytes());
    hasher.update([0]);
    let mut buf = vec![0u8; 64 * 1024];
    for path in files {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let name = relative.to_string_lossy();
        hasher.update(u64::try_from(name.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(name.as_bytes());
        let mut file = std::fs::File::open(&path)?;
        let mut length = 0u64;
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            length = length.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        }
        hasher.update(length.to_le_bytes());
    }
    Ok(Some(hex::encode(&hasher.finalize()[..16])))
}

/// Removes `staging-*` directories under `root` last modified more than
/// `age` ago.
fn sweep_stale_staging(root: &Path, age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("staging-") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|elapsed| elapsed > age);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Creates `dir` (and its parents) and makes it owner-only on Unix.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 32 random hex characters for a staging directory name.
fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
#[path = "python_snapshot_tests.rs"]
mod tests;
