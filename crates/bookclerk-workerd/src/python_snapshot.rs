//! Python start-up snapshots for workerd isolates.
//!
//! A Python isolate spends seconds loading Pyodide and importing the plugin
//! before it can answer. workerd can save the isolate's memory after those
//! imports (a "dedicated" snapshot) and start later isolates from it instead.
//! The host keeps one snapshot per installed plugin version, read-only to the
//! guest, and hands the launcher two paths through the environment:
//!
//! - [`LOAD_ENV`]: the stored snapshot to start from, when the host has one.
//! - [`SAVE_ENV`]: an empty staging directory the jail may write. When there
//!   is nothing to load, or the stored snapshot fails to start, the launcher
//!   starts workerd saving a fresh snapshot there, and renames it to
//!   [`READY_FILE`] once the isolate answered. The host promotes only a
//!   ready file into its store.
//!
//! Neither path is a secret, and a missing or broken snapshot only costs the
//! slow start: the launcher always falls back to starting without one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable naming the stored snapshot file to start from.
pub const LOAD_ENV: &str = "BOOKCLERK_PYTHON_SNAPSHOT_LOAD";

/// Environment variable naming the staging directory a fresh snapshot is
/// saved into.
pub const SAVE_ENV: &str = "BOOKCLERK_PYTHON_SNAPSHOT_SAVE";

/// The file name workerd writes a saved snapshot under, inside the snapshot
/// directory.
pub const SAVED_FILE: &str = "snapshot.bin";

/// The file name a saved snapshot is renamed to once the isolate it came
/// from answered, so the host never promotes a half-written file.
pub const READY_FILE: &str = "snapshot.ready";

/// The largest snapshot the host promotes, in bytes. A dedicated snapshot of
/// a plugin with the Bookclerk SDK is about 45 MB; anything far past that is
/// refused rather than stored.
pub const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;

/// What the host asked the launcher to do with snapshots for this start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotPlan {
    /// The stored snapshot to start from, when the host has one.
    pub load: Option<PathBuf>,
    /// The staging directory a fresh snapshot may be saved into.
    pub save: Option<PathBuf>,
}

/// One way to start workerd for a Python isolate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotStart {
    /// Start from the stored snapshot at this path.
    Load(PathBuf),
    /// Start normally and save a snapshot into this staging directory.
    Save(PathBuf),
    /// Start normally without snapshots.
    Plain,
}

impl SnapshotPlan {
    /// Reads the plan from [`LOAD_ENV`] and [`SAVE_ENV`].
    ///
    /// A value that is empty or not an absolute path is ignored, so a bad
    /// value only costs the fast start.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            load: absolute_env(LOAD_ENV),
            save: absolute_env(SAVE_ENV),
        }
    }

    /// The starts to try, in order: loading the stored snapshot when there
    /// is one, then saving a fresh one when a staging directory was given,
    /// then a plain start.
    ///
    /// # Returns
    ///
    /// At least one start; the last is the fallback when every earlier one
    /// fails to become ready.
    #[must_use]
    pub fn starts(&self) -> Vec<SnapshotStart> {
        let mut starts = Vec::with_capacity(2);
        if let Some(load) = &self.load {
            starts.push(SnapshotStart::Load(load.clone()));
        }
        starts.push(match &self.save {
            Some(save) => SnapshotStart::Save(save.clone()),
            None => SnapshotStart::Plain,
        });
        starts
    }
}

impl SnapshotStart {
    /// The `workerd serve` arguments for this start.
    ///
    /// # Returns
    ///
    /// No arguments for [`SnapshotStart::Plain`], or for a load path without
    /// a parent directory or file name.
    #[must_use]
    pub fn serve_args(&self) -> Vec<OsString> {
        match self {
            Self::Load(file) => {
                let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
                    return Vec::new();
                };
                vec![
                    flag("--python-snapshot-dir=", dir.as_os_str()),
                    flag("--python-load-snapshot=", name),
                ]
            }
            Self::Save(dir) => vec![
                flag("--python-snapshot-dir=", dir.as_os_str()),
                OsString::from("--python-save-snapshot"),
            ],
            Self::Plain => Vec::new(),
        }
    }
}

/// Marks the snapshot saved into `staging` as complete.
///
/// Called once the isolate that saved it answered, so the file is whole.
///
/// # Arguments
///
/// * `staging` - The directory workerd saved the snapshot into.
///
/// # Errors
///
/// Returns the I/O error when there is no saved snapshot or it cannot be
/// renamed.
pub fn mark_ready(staging: &Path) -> std::io::Result<()> {
    std::fs::rename(staging.join(SAVED_FILE), staging.join(READY_FILE))
}

/// Reads `name` as an absolute path, or `None`.
fn absolute_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// `prefix` followed by `value`, without a lossy conversion.
fn flag(prefix: &str, value: &std::ffi::OsStr) -> OsString {
    let mut arg = OsString::from(prefix);
    arg.push(value);
    arg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(path: &str) -> PathBuf {
        std::env::temp_dir().join(path)
    }

    #[test]
    fn a_stored_snapshot_is_tried_first_then_saved_again() {
        let plan = SnapshotPlan {
            load: Some(abs("store/key.bin")),
            save: Some(abs("staging")),
        };
        assert_eq!(
            plan.starts(),
            vec![
                SnapshotStart::Load(abs("store/key.bin")),
                SnapshotStart::Save(abs("staging")),
            ]
        );
    }

    #[test]
    fn without_a_stored_snapshot_the_first_start_saves_one() {
        let plan = SnapshotPlan {
            load: None,
            save: Some(abs("staging")),
        };
        assert_eq!(plan.starts(), vec![SnapshotStart::Save(abs("staging"))]);
        assert_eq!(SnapshotPlan::default().starts(), vec![SnapshotStart::Plain]);
        let load_only = SnapshotPlan {
            load: Some(abs("store/key.bin")),
            save: None,
        };
        assert_eq!(
            load_only.starts(),
            vec![
                SnapshotStart::Load(abs("store/key.bin")),
                SnapshotStart::Plain
            ]
        );
    }

    #[test]
    fn the_serve_arguments_name_the_directory_and_file() {
        let load = SnapshotStart::Load(abs("store/key.bin")).serve_args();
        assert_eq!(
            load,
            vec![
                flag("--python-snapshot-dir=", abs("store").as_os_str()),
                OsString::from("--python-load-snapshot=key.bin"),
            ]
        );
        let save = SnapshotStart::Save(abs("staging")).serve_args();
        assert_eq!(
            save,
            vec![
                flag("--python-snapshot-dir=", abs("staging").as_os_str()),
                OsString::from("--python-save-snapshot"),
            ]
        );
        assert!(SnapshotStart::Plain.serve_args().is_empty());
        assert!(SnapshotStart::Load(PathBuf::from("/"))
            .serve_args()
            .is_empty());
    }

    #[test]
    fn a_saved_snapshot_is_marked_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(mark_ready(dir.path()).is_err(), "nothing saved yet");
        std::fs::write(dir.path().join(SAVED_FILE), b"snap").expect("write");
        mark_ready(dir.path()).expect("ready");
        assert!(!dir.path().join(SAVED_FILE).exists());
        assert_eq!(
            std::fs::read(dir.path().join(READY_FILE)).expect("read"),
            b"snap"
        );
    }
}
