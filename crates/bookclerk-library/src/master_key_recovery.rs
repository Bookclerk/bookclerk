//! What an install's operator did about a recovery copy of its key.
//!
//! A key kept in the OS keychain never sits in a file a backup picks up, so
//! the desktop window asks its operator to save a recovery copy
//! ([`crate::export_master_key`]) and reminds them under Settings until they
//! do. The answer lives in `master-key-recovery.json` beside `master.key`:
//! which key it was given for (the key's SHA-256 fingerprint), whether a copy
//! was saved or the step skipped, and when. A record for another key (one
//! restored or replaced since) counts as no record, so a new key asks again.
//!
//! The file holds no secret: the fingerprint is the same one a `BCK3`
//! keychain pointer carries.

use std::path::{Path, PathBuf};

use rand::Rng as _;
use serde::{Deserialize, Serialize};

use crate::error::{LibraryError, Result};
use crate::master_key::{master_key_fingerprint, MasterKey};

/// File name of the record, beside `master.key` in the files directory.
pub const RECOVERY_RECORD_FILE_NAME: &str = "master-key-recovery.json";

/// What the operator chose about a recovery copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCopyChoice {
    /// The operator saved a recovery copy of this key.
    Saved,
    /// The operator skipped saving one, after a warning.
    Skipped,
}

/// The recorded answer for the key in use, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryCopyState {
    /// Nothing is recorded for this key: no copy saved, step not skipped.
    Unrecorded,
    /// The operator answered for this key.
    Recorded {
        /// What they chose.
        choice: RecoveryCopyChoice,
        /// When, in whole seconds since the Unix epoch (UTC).
        recorded_at: i64,
    },
}

/// The record as stored on disk.
#[derive(Debug, Serialize, Deserialize)]
struct RecoveryRecord {
    /// SHA-256 hex fingerprint of the key the answer was given for
    /// ([`master_key_fingerprint`]).
    fingerprint: String,
    /// What the operator chose.
    choice: RecoveryCopyChoice,
    /// When, in whole seconds since the Unix epoch (UTC).
    recorded_at: i64,
}

/// Path of the record in `files_dir`.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key`.
///
/// # Returns
///
/// `<files_dir>/master-key-recovery.json`.
#[must_use]
pub fn recovery_record_path(files_dir: &Path) -> PathBuf {
    files_dir.join(RECOVERY_RECORD_FILE_NAME)
}

/// Reads what the operator recorded about a recovery copy of `key`.
///
/// A missing or unreadable record, or one written for a different key,
/// reads as [`RecoveryCopyState::Unrecorded`]: the safe answer is to ask
/// again.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key`.
/// * `key` - the key in use.
///
/// # Returns
///
/// The recorded answer for `key`, or [`RecoveryCopyState::Unrecorded`].
#[must_use]
pub fn recovery_copy_state(files_dir: &Path, key: &MasterKey) -> RecoveryCopyState {
    let path = recovery_record_path(files_dir);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("could not read {}: {err}", path.display());
            }
            return RecoveryCopyState::Unrecorded;
        }
    };
    match serde_json::from_slice::<RecoveryRecord>(&raw) {
        Ok(record) if record.fingerprint == master_key_fingerprint(key) => {
            RecoveryCopyState::Recorded {
                choice: record.choice,
                recorded_at: record.recorded_at,
            }
        }
        Ok(_) => RecoveryCopyState::Unrecorded,
        Err(err) => {
            tracing::warn!("ignoring unreadable {}: {err}", path.display());
            RecoveryCopyState::Unrecorded
        }
    }
}

/// Records that the operator saved a recovery copy of `key`, or skipped it.
///
/// The record replaces any earlier one atomically (a temp file renamed over
/// it), so a crash leaves the old answer or the new one.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key`.
/// * `key` - the key in use, whose fingerprint the record names.
/// * `choice` - what the operator chose.
///
/// # Returns
///
/// The state now recorded.
///
/// # Errors
///
/// Returns an error when the record can't be written.
pub fn record_recovery_copy(
    files_dir: &Path,
    key: &MasterKey,
    choice: RecoveryCopyChoice,
) -> Result<RecoveryCopyState> {
    let recorded_at = chrono::Utc::now().timestamp();
    let record = RecoveryRecord {
        fingerprint: master_key_fingerprint(key),
        choice,
        recorded_at,
    };
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("failed to encode record: {e}")))?;
    let path = recovery_record_path(files_dir);
    write_atomic(&path, &bytes)?;
    Ok(RecoveryCopyState::Recorded {
        choice,
        recorded_at,
    })
}

/// Writes `bytes` to a temp file beside `path` and renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{RECOVERY_RECORD_FILE_NAME}.tmp-{}-{}",
        std::process::id(),
        rand::rngs::OsRng.gen::<u32>()
    ));
    let failed = |e: std::io::Error| {
        LibraryError::Other(anyhow::anyhow!("failed to write {}: {e}", path.display()))
    };
    if let Err(err) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(failed(err));
    }
    bookclerk_config::replace_file(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        failed(e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A key with every byte `fill`.
    fn key(fill: u8) -> MasterKey {
        MasterKey::from_bytes([fill; 32])
    }

    #[test]
    fn nothing_is_recorded_on_a_fresh_install() {
        let dir = tempdir().unwrap();
        assert_eq!(
            recovery_copy_state(dir.path(), &key(1)),
            RecoveryCopyState::Unrecorded
        );
    }

    #[test]
    fn a_recorded_answer_reads_back_for_the_same_key() {
        let dir = tempdir().unwrap();
        let written =
            record_recovery_copy(dir.path(), &key(1), RecoveryCopyChoice::Skipped).unwrap();
        assert_eq!(recovery_copy_state(dir.path(), &key(1)), written);
        let later = record_recovery_copy(dir.path(), &key(1), RecoveryCopyChoice::Saved).unwrap();
        assert!(matches!(
            recovery_copy_state(dir.path(), &key(1)),
            RecoveryCopyState::Recorded {
                choice: RecoveryCopyChoice::Saved,
                ..
            }
        ));
        assert_eq!(recovery_copy_state(dir.path(), &key(1)), later);
        // No temp file is left behind.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![RECOVERY_RECORD_FILE_NAME]);
    }

    #[test]
    fn an_answer_for_another_key_asks_again() {
        let dir = tempdir().unwrap();
        record_recovery_copy(dir.path(), &key(1), RecoveryCopyChoice::Saved).unwrap();
        assert_eq!(
            recovery_copy_state(dir.path(), &key(2)),
            RecoveryCopyState::Unrecorded
        );
    }

    #[test]
    fn a_damaged_record_asks_again() {
        let dir = tempdir().unwrap();
        std::fs::write(recovery_record_path(dir.path()), b"{not json").unwrap();
        assert_eq!(
            recovery_copy_state(dir.path(), &key(1)),
            RecoveryCopyState::Unrecorded
        );
    }

    #[test]
    fn an_unreadable_record_asks_again() {
        let dir = tempdir().unwrap();
        // A directory where the file should be: reading fails, not NotFound.
        std::fs::create_dir(recovery_record_path(dir.path())).unwrap();
        assert_eq!(
            recovery_copy_state(dir.path(), &key(1)),
            RecoveryCopyState::Unrecorded
        );
    }

    #[test]
    fn a_failed_write_reports_it_and_leaves_no_temp_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("gone");
        assert!(record_recovery_copy(&missing, &key(1), RecoveryCopyChoice::Saved).is_err());
        // Renaming onto a directory fails after the temp file is written.
        std::fs::create_dir(recovery_record_path(dir.path())).unwrap();
        std::fs::write(recovery_record_path(dir.path()).join("child"), b"x").unwrap();
        assert!(record_recovery_copy(dir.path(), &key(1), RecoveryCopyChoice::Saved).is_err());
        let temps = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp-")
            })
            .count();
        assert_eq!(temps, 0);
    }
}
