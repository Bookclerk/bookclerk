//! Installed-plugin receipt / lock record.
//!
//! Digests on this record prove: the installed bytes match the artifact recorded
//! for this provenance-qualified package. They do **not** prove publisher
//! identity. There is no publisher PKI in this receipt.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use bookclerk_plugin_manifest::PluginManifest;

use crate::coordinate::PackageCoordinate;
use crate::error::{CatalogError, Result};
use crate::extract::require_under;
use crate::identity::{PluginKey, PluginProvenance};
use crate::kind::RuntimeIdentity;
use crate::manifest::SandboxRequest;

/// Filename for the receipt beside `plugin.toml`.
pub const RECEIPT_FILE: &str = "receipt.json";
/// Backup filename written beside `receipt.json` before an install/update overwrites it.
pub const RECEIPT_BACKUP: &str = "receipt.json.bak";

/// Record written after a successful install / update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReceipt {
    /// DTO schema version for CLI/UI JSON compatibility.
    pub schema_version: u32,
    /// Provenance-qualified logical plugin identity (no version, no hash).
    pub plugin_key: String,
    /// Host-evaluated provenance at install time (re-checked at discovery).
    pub provenance: PluginProvenance,
    /// Fully qualified package coordinate when version is known.
    pub coordinate: PackageCoordinate,
    /// Resolved or candidate package version string.
    pub version: String,
    /// Base URL of the package registry (for example crates.io or npm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,
    /// Resolved download URL used for this install.
    pub artifact_url: String,
    /// Host target triple used to select release artifacts.
    pub target: String,
    /// Lowercase hex SHA-256 of the downloadable archive bytes (install-time).
    pub archive_sha256: String,
    /// SHA-256 of the installed `plugin.toml` bytes.
    pub manifest_sha256: String,
    /// Deterministic Merkle-style root over immutable packaged files.
    pub payload_root_sha256: String,
    /// Optional SHA-256 of the extracted executable bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
    /// Host↔guest wire protocol id (for example `workers-rpc`).
    pub protocol: String,
    /// Plugin ABI version negotiated with the host.
    pub api_version: u32,
    /// Bookclerk runtime identity (kind + plugin id) when known.
    pub runtime: RuntimeIdentity,
    /// Publisher-requested sandbox snapshot recorded at install time.
    pub requested_sandbox: SandboxRequest,
    /// Host-approved network mode (may be stricter than requested).
    pub approved_network: String,
    /// RFC 3339 time when this install was activated.
    pub installed_at: DateTime<Utc>,
    /// Optional update constraint (for example pinned version range).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_constraint: Option<String>,
}

impl InstallReceipt {
    /// Receipt schema version for forward-compatible reads.
    pub const SCHEMA_VERSION: u32 = 2;

    /// Path to receipt inside an installed plugin directory.
    #[must_use]
    pub fn path_in(plugin_root: &Path) -> PathBuf {
        plugin_root.join(RECEIPT_FILE)
    }

    /// Parsed [`PluginKey`] from [`Self::plugin_key`].
    ///
    /// # Errors
    ///
    /// Returns an error when the stored key is not canonical.
    pub fn plugin_key(&self) -> Result<PluginKey> {
        PluginKey::parse(&self.plugin_key)
    }

    /// Host-stamped receipt for a Bookclerk platform artifact.
    #[must_use]
    pub fn platform_bundled(
        plugin_key: PluginKey,
        version: &str,
        manifest: &PluginManifest,
        manifest_sha256: String,
        payload_root_sha256: String,
        product: &str,
        package_name: &str,
    ) -> Self {
        use crate::kind::PluginKind;
        let family = manifest.primary_family();
        let kind = match family.as_str() {
            "source" => PluginKind::Source,
            "integration" => PluginKind::Integration,
            "output" => PluginKind::Output,
            _ => PluginKind::Database,
        };
        Self {
            schema_version: Self::SCHEMA_VERSION,
            plugin_key: plugin_key.canonical().to_string(),
            provenance: PluginProvenance::PlatformBundled,
            coordinate: PackageCoordinate {
                source: crate::coordinate::RegistrySource::LocalArchive,
                name: format!("{product}/{package_name}"),
                version: version.to_string(),
            },
            version: version.to_string(),
            registry_url: None,
            artifact_url: format!("platform:{product}/{package_name}"),
            target: crate::target::host_bookclerk_target().to_string(),
            archive_sha256: String::new(),
            manifest_sha256,
            payload_root_sha256,
            executable_sha256: None,
            protocol: crate::manifest::PROTOCOL_WORKERS_RPC.into(),
            api_version: manifest.api_version,
            runtime: RuntimeIdentity::new(kind, manifest.id.clone()),
            requested_sandbox: SandboxRequest {
                network: match manifest.capabilities.network.mode {
                    bookclerk_plugin_manifest::NetworkMode::Deny => "deny".into(),
                    bookclerk_plugin_manifest::NetworkMode::Outbound => "outbound".into(),
                },
            },
            approved_network: match manifest.capabilities.network.mode {
                bookclerk_plugin_manifest::NetworkMode::Deny => "deny".into(),
                bookclerk_plugin_manifest::NetworkMode::Outbound => "outbound".into(),
            },
            installed_at: Utc::now(),
            update_constraint: None,
        }
    }

    /// Load receipt from a plugin install directory.
    ///
    /// Operator-selected `plugin_root` may contain `..` spelling (resolved via
    /// canonicalize). Escaping or dangling leaf symlinks at `receipt.json` are
    /// hard errors; a true missing file is [`CatalogError::ReceiptNotFound`].
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::ReceiptNotFound`] when `receipt.json` is absent.
    /// Any other I/O or JSON failure is returned as an error (fail closed).
    pub fn load(plugin_root: &Path) -> Result<Self> {
        let root = match plugin_root.canonicalize() {
            Ok(root) => root,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(CatalogError::ReceiptNotFound);
            }
            Err(err) => {
                return Err(CatalogError::message(format!(
                    "could not canonicalize plugin root {}: {err}",
                    plugin_root.display()
                )));
            }
        };
        let path = require_under(&root, &Self::path_in(&root))?;
        match fs::read_to_string(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(CatalogError::ReceiptNotFound)
            }
            Err(err) => Err(CatalogError::message(format!(
                "read {}: {err}",
                path.display()
            ))),
            Ok(text) => serde_json::from_str(&text).map_err(|err| {
                CatalogError::message(format!(
                    "malformed install receipt {}: {err}",
                    path.display()
                ))
            }),
        }
    }

    /// Atomically write receipt (unique same-dir staging + replace).
    ///
    /// Does not follow final/backup/temp leaf symlinks: staging uses
    /// `create_new`, and replace uses Unix `rename` / Windows `MoveFileExW` so
    /// only the directory entry is replaced. Cleanup removes only the staging
    /// file this call created.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn store(&self, plugin_root: &Path) -> Result<()> {
        fs::create_dir_all(plugin_root)?;
        let root = plugin_root.canonicalize().map_err(|err| {
            CatalogError::message(format!(
                "could not canonicalize plugin root {}: {err}",
                plugin_root.display()
            ))
        })?;
        let final_path = root.join(RECEIPT_FILE);
        if !final_path.starts_with(&root) {
            return Err(CatalogError::message("receipt path escaped plugin root"));
        }
        let text = serde_json::to_string_pretty(self)?;
        maybe_backup_receipt(&root, &final_path)?;
        write_receipt_atomic(&root, &final_path, text.as_bytes())?;
        Ok(())
    }
}

/// Unique same-directory staging path for an atomic receipt replace.
fn staging_receipt_path(dir: &Path, dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(RECEIPT_FILE);
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    dir.join(format!(
        ".{name}.tmp-{}-{nonce:016x}-{n}",
        std::process::id()
    ))
}

/// Replace `to` with staging file `from` without following a leaf symlink at `to`.
fn replace_receipt_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        replace_receipt_windows(from, to)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // MoveFileExW FFI — same boundary as bookclerk-config.
/// Atomically replaces `to` with `from` via `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`.
///
/// # Errors
///
/// Returns an I/O error when `MoveFileExW` fails.
fn replace_receipt_windows(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let src = wide(from);
    let dst = wide(to);
    // SAFETY: `src` and `dst` are NUL-terminated wide paths that outlive the call.
    let ok = unsafe {
        MoveFileExW(
            src.as_ptr(),
            dst.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Write `bytes` to `dest` via same-dir temp + atomic replace.
fn write_receipt_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> Result<()> {
    if !dest.starts_with(dir) {
        return Err(CatalogError::message(
            "receipt destination escapes plugin root",
        ));
    }
    let staging = staging_receipt_path(dir, dest);
    if !staging.starts_with(dir) {
        return Err(CatalogError::message(
            "receipt staging path escapes plugin root",
        ));
    }
    // Exclusive create: do not truncate/follow an unexpected existing entry.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .map_err(|err| {
            CatalogError::message(format!(
                "create receipt staging {}: {err}",
                staging.display()
            ))
        })?;
    if let Err(err) = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })() {
        let _ = fs::remove_file(&staging);
        return Err(CatalogError::message(format!(
            "write receipt staging {}: {err}",
            staging.display()
        )));
    }
    match replace_receipt_entry(&staging, dest) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&staging);
            Err(CatalogError::message(format!(
                "replace receipt {}: {err}",
                dest.display()
            )))
        }
    }
}

/// Copy an existing regular-file receipt to `receipt.json.bak` without following
/// leaf symlinks at the backup path.
fn maybe_backup_receipt(root: &Path, final_path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(final_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(CatalogError::message(format!(
                "stat {}: {err}",
                final_path.display()
            )));
        }
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        // Do not follow a leaf symlink for backup content.
        return Ok(());
    }
    let backup = root.join(RECEIPT_BACKUP);
    if !backup.starts_with(root) {
        return Err(CatalogError::message(
            "receipt backup path escaped plugin root",
        ));
    }
    if let Ok(bmeta) = fs::symlink_metadata(&backup) {
        if bmeta.file_type().is_symlink() {
            // Unlink the directory entry only — never a canonicalized outside target.
            fs::remove_file(&backup).map_err(|err| {
                CatalogError::message(format!(
                    "unlink receipt backup symlink {}: {err}",
                    backup.display()
                ))
            })?;
        }
    }
    let bytes = fs::read(final_path).map_err(|err| {
        CatalogError::message(format!("read {} for backup: {err}", final_path.display()))
    })?;
    write_receipt_atomic(root, &backup, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinate::RegistrySource;
    use crate::kind::PluginKind;
    use crate::manifest::PROTOCOL_WORKERS_RPC;

    fn sample_receipt() -> InstallReceipt {
        let key = PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite").unwrap();
        InstallReceipt {
            schema_version: InstallReceipt::SCHEMA_VERSION,
            plugin_key: key.canonical().to_string(),
            provenance: PluginProvenance::PlatformBundled,
            coordinate: PackageCoordinate {
                source: RegistrySource::LocalArchive,
                name: "/tmp/x.tar.gz".into(),
                version: "1.0.0".into(),
            },
            version: "1.0.0".into(),
            registry_url: None,
            artifact_url: "file:///tmp/x.tar.gz".into(),
            target: "linux-x64-gnu".into(),
            archive_sha256: "aa".repeat(32),
            manifest_sha256: "bb".repeat(32),
            payload_root_sha256: "cc".repeat(32),
            executable_sha256: None,
            protocol: PROTOCOL_WORKERS_RPC.into(),
            api_version: 3,
            runtime: RuntimeIdentity::new(PluginKind::Database, "sqlite"),
            requested_sandbox: SandboxRequest {
                network: "deny".into(),
            },
            approved_network: "deny".into(),
            installed_at: Utc::now(),
            update_constraint: None,
        }
    }

    #[test]
    fn receipt_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = sample_receipt();
        let key = receipt.plugin_key().unwrap();
        receipt.store(dir.path()).unwrap();
        let loaded = InstallReceipt::load(dir.path()).unwrap();
        assert_eq!(loaded.runtime.id, "sqlite");
        assert_eq!(loaded.plugin_key().unwrap(), key);
        assert_eq!(loaded.provenance, PluginProvenance::PlatformBundled);
    }

    #[test]
    fn load_missing_is_receipt_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = InstallReceipt::load(dir.path()).unwrap_err();
        assert!(err.is_receipt_not_found(), "{err}");
    }

    #[test]
    fn store_allows_parent_dir_spelling_in_plugin_root() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("plug");
        fs::create_dir_all(&nested).unwrap();
        let with_dotdot = nested.join("..").join("plug");
        sample_receipt().store(&with_dotdot).unwrap();
        let loaded = InstallReceipt::load(&with_dotdot).unwrap();
        assert_eq!(loaded.runtime.id, "sqlite");
    }

    #[cfg(unix)]
    #[test]
    fn load_refuses_escaping_receipt_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("secret.json");
        fs::write(&victim, b"{\"stolen\":true}").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(RECEIPT_FILE)).unwrap();
        let err = InstallReceipt::load(dir.path()).unwrap_err();
        assert!(!err.is_receipt_not_found(), "{err}");
        assert_eq!(fs::read(&victim).unwrap(), b"{\"stolen\":true}");
    }

    #[cfg(unix)]
    #[test]
    fn load_refuses_dangling_receipt_symlink() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("missing.json"),
            dir.path().join(RECEIPT_FILE),
        )
        .unwrap();
        let err = InstallReceipt::load(dir.path()).unwrap_err();
        assert!(!err.is_receipt_not_found(), "{err}");
        assert!(
            err.to_string().contains("symlink")
                || err.to_string().contains("dangling")
                || err.to_string().contains("refusing")
                || err.to_string().contains("canonicalize"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_replaces_receipt_symlink_without_touching_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim.json");
        fs::write(&victim, b"keep-me").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(RECEIPT_FILE)).unwrap();
        sample_receipt().store(dir.path()).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep-me");
        let loaded = InstallReceipt::load(dir.path()).unwrap();
        assert_eq!(loaded.runtime.id, "sqlite");
    }

    #[cfg(unix)]
    #[test]
    fn store_does_not_follow_backup_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("bak-victim");
        fs::write(&victim, b"bak-keep").unwrap();
        sample_receipt().store(dir.path()).unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(RECEIPT_BACKUP)).unwrap();
        let mut second = sample_receipt();
        second.version = "2.0.0".into();
        second.store(dir.path()).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"bak-keep");
        let loaded = InstallReceipt::load(dir.path()).unwrap();
        assert_eq!(loaded.version, "2.0.0");
    }
}
