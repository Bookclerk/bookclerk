//! Host-owned install ledger (trust anchor outside plugin-controlled trees).
//!
//! Plugin-authored bytes — including `receipt.json` beside `plugin.toml` — cannot
//! manufacture Bookclerk platform trust. Discovery treats an install as
//! [`crate::PluginProvenance::PlatformBundled`] only when this ledger, which
//! lives at `$FILES_DIR/install-ledger.json`, records the exact PluginKey and
//! payload digests for a known platform artifact.
//!
//! The ledger is host-local, like the install tree and
//! [`crate::PLUGIN_MUTATION_LOCK_FILE`]. It is not a cluster-wide inventory
//! and is never locked through PostgreSQL/D1 advisory locks.
//!
//! The receipt remains useful metadata (version, archive URL, sandbox snapshot).
//! It does **not** establish platform or first-party authority by itself.

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{CatalogError, Result};
use crate::identity::{PluginKey, PluginProvenance};

/// Filename under `$BOOKCLERK_FILES_DIR` for the host install ledger.
pub const INSTALL_LEDGER_FILE: &str = "install-ledger.json";

/// Schema version for [`InstallLedger`].
pub const INSTALL_LEDGER_SCHEMA_VERSION: u32 = 1;

/// Host-owned record of one verified installed artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallLedgerEntry {
    /// Canonical [`PluginKey`].
    pub plugin_key: String,
    /// Package coordinate name recorded at install/stage time.
    pub package_name: String,
    /// Manifest alias (`sqlite`, `local`, …).
    pub manifest_id: String,
    /// SHA-256 of the installed `plugin.toml` bytes.
    pub manifest_sha256: String,
    /// Deterministic payload root over immutable packaged files.
    pub payload_root_sha256: String,
    /// Host-evaluated provenance at the time the ledger row was written.
    pub provenance: PluginProvenance,
    /// RFC 3339 time when the host recorded this artifact.
    pub recorded_at: DateTime<Utc>,
}

/// Host-owned map of PluginKey → expected digests for verified installs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallLedger {
    /// DTO schema version.
    pub schema_version: u32,
    /// One row per PluginKey (last write wins).
    #[serde(default)]
    pub artifacts: Vec<InstallLedgerEntry>,
}

impl Default for InstallLedger {
    fn default() -> Self {
        Self {
            schema_version: INSTALL_LEDGER_SCHEMA_VERSION,
            artifacts: Vec::new(),
        }
    }
}

impl InstallLedger {
    /// Absolute path to `install-ledger.json` under `files_dir`.
    #[must_use]
    pub fn path(files_dir: &Path) -> PathBuf {
        files_dir.join(INSTALL_LEDGER_FILE)
    }

    /// Loads the ledger, or an empty ledger when the file is missing.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or parsed,
    /// or when `files_dir` contains `..` / cannot be contained.
    /// A missing file (or missing files dir) is not an error.
    pub fn load(files_dir: &Path) -> Result<Self> {
        let Some(path) = resolved_ledger_path(files_dir)? else {
            return Ok(Self::default());
        };
        // `files_dir` is the operator `$BOOKCLERK_FILES_DIR`, not plugin-supplied.
        // codeql[rust/path-injection]
        match fs::read_to_string(&path) {
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(CatalogError::message(format!(
                "read {}: {err}",
                path.display()
            ))),
            Ok(text) => {
                let ledger: Self = serde_json::from_str(&text)?;
                if ledger.schema_version != INSTALL_LEDGER_SCHEMA_VERSION {
                    return Err(CatalogError::message(format!(
                        "unsupported install ledger schema {}",
                        ledger.schema_version
                    )));
                }
                Ok(ledger)
            }
        }
    }

    /// Lookup by canonical PluginKey.
    #[must_use]
    pub fn get(&self, plugin_key: &PluginKey) -> Option<&InstallLedgerEntry> {
        let canonical = plugin_key.canonical();
        self.artifacts
            .iter()
            .find(|row| row.plugin_key == canonical)
    }

    /// Inserts or replaces the row for `entry.plugin_key`.
    pub fn upsert(&mut self, entry: InstallLedgerEntry) {
        if let Some(existing) = self
            .artifacts
            .iter_mut()
            .find(|row| row.plugin_key == entry.plugin_key)
        {
            *existing = entry;
        } else {
            self.artifacts.push(entry);
        }
    }

    /// Removes the row for `plugin_key` if present.
    pub fn remove(&mut self, plugin_key: &PluginKey) -> bool {
        let canonical = plugin_key.canonical();
        let before = self.artifacts.len();
        self.artifacts.retain(|row| row.plugin_key != canonical);
        self.artifacts.len() != before
    }

    /// Atomically writes the ledger under `files_dir`.
    ///
    /// # Errors
    ///
    /// Returns when the directory cannot be created or the file cannot be written.
    pub fn store(&self, files_dir: &Path) -> Result<()> {
        if files_dir
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(CatalogError::message(
                "refusing install-ledger path with '..' in files_dir",
            ));
        }
        fs::create_dir_all(files_dir)?;
        let Some(final_path) = resolved_ledger_path(files_dir)? else {
            return Err(CatalogError::message(format!(
                "install-ledger files_dir vanished: {}",
                files_dir.display()
            )));
        };
        let tmp = final_path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        // Operator `$BOOKCLERK_FILES_DIR`, not plugin-supplied.
        // codeql[rust/path-injection]
        fs::write(&tmp, text)?;
        // codeql[rust/path-injection]
        fs::rename(&tmp, &final_path)?;
        Ok(())
    }
}

/// Contained `$FILES_DIR/install-ledger.json`, or `None` when `files_dir` is missing.
///
/// Rejects `..` in `files_dir` and requires the resolved path to stay under the
/// canonical files dir. Missing files-dir is treated as an empty ledger.
///
/// # Errors
///
/// Returns when `files_dir` contains `..`, cannot be canonicalized (except
/// NotFound), or the joined ledger path escapes that directory.
fn resolved_ledger_path(files_dir: &Path) -> Result<Option<PathBuf>> {
    if files_dir
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(CatalogError::message(
            "refusing install-ledger path with '..' in files_dir",
        ));
    }
    let base = match files_dir.canonicalize() {
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(CatalogError::message(format!(
                "canonicalize {}: {err}",
                files_dir.display()
            )));
        }
        Ok(base) => base,
    };
    let path = base.join(INSTALL_LEDGER_FILE);
    if !path.starts_with(&base) {
        return Err(CatalogError::message(
            "install-ledger path escaped files_dir",
        ));
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) if name == INSTALL_LEDGER_FILE => Ok(Some(path)),
        _ => Err(CatalogError::message(
            "refusing unexpected install-ledger filename",
        )),
    }
}

/// Records (or updates) a host-verified install in the files-dir ledger.
///
/// # Errors
///
/// Returns when the ledger cannot be loaded or stored, or `plugin_key` is empty.
pub fn record_install(
    files_dir: &Path,
    plugin_key: &PluginKey,
    package_name: &str,
    manifest_id: &str,
    manifest_sha256: &str,
    payload_root_sha256: &str,
    provenance: PluginProvenance,
) -> Result<()> {
    if plugin_key.canonical().is_empty() {
        return Err(CatalogError::message(
            "refusing to record an empty PluginKey in the install ledger",
        ));
    }
    let mut ledger = InstallLedger::load(files_dir)?;
    ledger.upsert(InstallLedgerEntry {
        plugin_key: plugin_key.canonical().to_string(),
        package_name: package_name.to_string(),
        manifest_id: manifest_id.to_string(),
        manifest_sha256: manifest_sha256.to_string(),
        payload_root_sha256: payload_root_sha256.to_string(),
        provenance,
        recorded_at: Utc::now(),
    });
    ledger.store(files_dir)
}

/// Restores `previous` for `plugin_key`, or deletes the row when `previous` is `None`.
///
/// # Errors
///
/// Returns when the ledger cannot be loaded or stored.
pub fn restore_ledger_entry(
    files_dir: &Path,
    plugin_key: &PluginKey,
    previous: Option<InstallLedgerEntry>,
) -> Result<()> {
    let mut ledger = InstallLedger::load(files_dir)?;
    match previous {
        Some(entry) => ledger.upsert(entry),
        None => {
            ledger.remove(plugin_key);
        }
    }
    ledger.store(files_dir)
}

/// True when `entry` records the exact current payload for `plugin_key`.
#[must_use]
pub fn entry_matches(
    entry: &InstallLedgerEntry,
    plugin_key: &PluginKey,
    manifest_sha256: &str,
    payload_root_sha256: &str,
) -> bool {
    entry.plugin_key == plugin_key.canonical()
        && entry.manifest_sha256.eq_ignore_ascii_case(manifest_sha256)
        && entry
            .payload_root_sha256
            .eq_ignore_ascii_case(payload_root_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_files_dir_is_empty_ledger() {
        let missing = std::env::temp_dir().join("bookclerk-no-such-files-dir-install-ledger");
        let _ = fs::remove_dir_all(&missing);
        let ledger = InstallLedger::load(&missing).unwrap();
        assert!(ledger.artifacts.is_empty());
    }

    #[test]
    fn load_rejects_parent_dir_components_in_files_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let sneaky = tmp.path().join("a").join("..").join("b");
        let err = InstallLedger::load(&sneaky).unwrap_err().to_string();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn store_and_load_round_trip_under_canonical_files_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = InstallLedger::default();
        ledger.upsert(InstallLedgerEntry {
            plugin_key: "path:file:///tmp/demo#demo".into(),
            package_name: "demo".into(),
            manifest_id: "demo".into(),
            manifest_sha256: "aa".into(),
            payload_root_sha256: "bb".into(),
            provenance: PluginProvenance::LocalDevelopment,
            recorded_at: Utc::now(),
        });
        ledger.store(tmp.path()).unwrap();
        let loaded = InstallLedger::load(tmp.path()).unwrap();
        assert_eq!(loaded.artifacts.len(), 1);
        assert_eq!(loaded.artifacts[0].plugin_key, "path:file:///tmp/demo#demo");
    }
}
