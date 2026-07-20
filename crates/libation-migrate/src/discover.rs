//! Locate classic Libation data files under a Libation Files directory.

use std::path::{Path, PathBuf};

use crate::error::{MigrateError, Result};

/// Paths discovered in a classic Libation Files directory.
#[derive(Debug, Clone)]
pub struct LibationSource {
    pub root: PathBuf,
    pub settings_json: Option<PathBuf>,
    pub accounts_settings: Option<PathBuf>,
    pub library_db: Option<PathBuf>,
    pub file_locations: Option<PathBuf>,
}

/// Discover importable files under `root`.
///
/// Accepts either a Libation Files directory or a directory that contains one
/// of the expected files (for Docker `/config` layouts).
pub fn discover_source(root: &Path) -> Result<LibationSource> {
    if !root.exists() {
        return Err(MigrateError::Source(format!(
            "{} does not exist",
            root.display()
        )));
    }

    let settings = first_existing(&[root.join("Settings.json"), root.join("settings.json")]);
    let accounts = first_existing(&[
        root.join("AccountsSettings.json"),
        root.join("accountsSettings.json"),
    ]);
    let db = first_existing(&[
        root.join("LibationContext.db"),
        root.join("libationContext.db"),
    ])
    .or_else(|| find_single_db(root));
    let locations = first_existing(&[
        root.join("FileLocationsV2.json"),
        root.join("FileLocations.json"),
    ]);

    if settings.is_none() && accounts.is_none() && db.is_none() {
        return Err(MigrateError::Source(format!(
            "{} has no Settings.json, AccountsSettings.json, or LibationContext.db",
            root.display()
        )));
    }

    Ok(LibationSource {
        root: root.to_path_buf(),
        settings_json: settings,
        accounts_settings: accounts,
        library_db: db,
        file_locations: locations,
    })
}

fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

fn find_single_db(root: &Path) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return None;
    };
    let mut dbs: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("db"))
        .collect();
    if dbs.len() == 1 {
        dbs.pop()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_expected_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Settings.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("AccountsSettings.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("LibationContext.db"), b"SQLite").unwrap();
        let src = discover_source(dir.path()).unwrap();
        assert!(src.settings_json.is_some());
        assert!(src.accounts_settings.is_some());
        assert!(src.library_db.is_some());
    }

    #[test]
    fn rejects_empty_dir() {
        let dir = tempdir().unwrap();
        let err = discover_source(dir.path()).unwrap_err();
        assert!(matches!(err, MigrateError::Source(_)));
    }
}
