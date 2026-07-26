//! S3 destination credentials under `Accounts/*.s3.auth`.
//!
//! Mirrors content-source auth files (`*.audible.auth`, `*.libro.auth`, …):
//! non-secret destination settings live in `[output.s3]`, while access keys are
//! stored as pretty JSON beside other Accounts credentials.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, StorageError};

/// On-disk auth suffix for S3 destination credentials.
pub const AUTH_SUFFIX: &str = ".s3.auth";

/// Default stem when `[output.s3].credentials_file` is unset.
pub const DEFAULT_STEM: &str = "default";

/// On-disk envelope for one S3 destination identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct S3AuthFile {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl S3AuthFile {
    /// Register key material for log redaction.
    pub fn register_secrets(&self) {
        bookclerk_config::register_secret(&self.access_key_id);
        bookclerk_config::register_secret(&self.secret_access_key);
        if let Some(token) = &self.session_token {
            bookclerk_config::register_secret(token);
        }
    }
}

/// Canonical path `{files_dir}/Accounts/{stem}.s3.auth`.
#[must_use]
pub fn credentials_file_for(files_dir: &Path, stem: &str) -> PathBuf {
    bookclerk_source::auth_file_for(files_dir, stem, AUTH_SUFFIX)
}

/// Default credentials path (`Accounts/default.s3.auth`).
#[must_use]
pub fn default_credentials_file(files_dir: &Path) -> PathBuf {
    credentials_file_for(files_dir, DEFAULT_STEM)
}

/// Resolve which credentials file to load for `[output.s3]`.
///
/// - Explicit `credentials_file` → that path (must exist when loading).
/// - Otherwise → `Accounts/default.s3.auth` when present.
#[must_use]
pub fn resolve_credentials_path(files_dir: &Path, configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(path.to_path_buf());
    }
    let default = default_credentials_file(files_dir);
    if default.is_file() {
        Some(default)
    } else {
        None
    }
}

/// Load S3 credentials JSON from disk.
pub fn load_auth(path: &Path) -> Result<S3AuthFile> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StorageError::S3(format!("S3 credentials file not found: {}", path.display()))
        } else {
            StorageError::S3(format!("failed to read {}: {e}", path.display()))
        }
    })?;
    let auth: S3AuthFile = serde_json::from_str(&raw).map_err(|e| {
        StorageError::S3(format!(
            "invalid S3 credentials JSON {}: {e}",
            path.display()
        ))
    })?;
    if auth.access_key_id.trim().is_empty() {
        return Err(StorageError::S3(format!(
            "{} missing access_key_id",
            path.display()
        )));
    }
    if auth.secret_access_key.trim().is_empty() {
        return Err(StorageError::S3(format!(
            "{} missing secret_access_key",
            path.display()
        )));
    }
    auth.register_secrets();
    Ok(auth)
}

/// Write pretty-printed S3 credentials JSON (mode hardened when possible).
pub fn save_auth(path: &Path, auth: &S3AuthFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| StorageError::S3(format!("failed to create {}: {e}", parent.display())))?;
    }
    bookclerk_source::save_json_auth(path, auth)
        .map_err(|e| StorageError::S3(format!("failed to write {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    auth.register_secrets();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_path_uses_s3_suffix() {
        assert_eq!(
            default_credentials_file(Path::new("/data")),
            PathBuf::from("/data/Accounts/default.s3.auth")
        );
    }

    #[test]
    fn round_trip_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_file_for(dir.path(), "minio");
        let auth = S3AuthFile {
            access_key_id: "AKIAEXAMPLE".into(),
            secret_access_key: "secret".into(),
            session_token: Some("sess".into()),
            label: Some("minio".into()),
        };
        save_auth(&path, &auth).unwrap();
        let loaded = load_auth(&path).unwrap();
        assert_eq!(loaded, auth);
    }

    #[test]
    fn resolve_prefers_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("custom.s3.auth");
        let resolved = resolve_credentials_path(dir.path(), Some(&custom));
        assert_eq!(resolved, Some(custom));
    }

    #[test]
    fn resolve_falls_back_to_default_when_present() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_credentials_path(dir.path(), None), None);
        let default = default_credentials_file(dir.path());
        std::fs::create_dir_all(default.parent().unwrap()).unwrap();
        std::fs::write(&default, r#"{"access_key_id":"a","secret_access_key":"b"}"#).unwrap();
        assert_eq!(resolve_credentials_path(dir.path(), None), Some(default));
    }
}
