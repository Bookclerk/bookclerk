//! Libro.fm auth file persistence under `Accounts/*.libro.auth`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{LibroError, Result};

/// On-disk auth suffix for Libro.fm accounts.
pub const AUTH_SUFFIX: &str = ".libro.auth";

/// On-disk auth envelope for one Libro.fm account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibroAuthFile {
    pub access_token: String,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Absolute expiry when known (RFC3339 when serialized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Marketplace / locale hint (not part of the mobile API; stored for Libation).
    #[serde(default = "default_marketplace")]
    pub marketplace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn default_token_type() -> String {
    String::from("Bearer")
}

fn default_marketplace() -> String {
    String::from("us")
}

impl LibroAuthFile {
    /// Stable account id for library rows: prefer user id, else email.
    #[must_use]
    pub fn account_id(&self) -> &str {
        self.user_id.as_deref().unwrap_or(self.email.as_str())
    }
}

/// Path for a label/email pair.
#[must_use]
pub fn auth_file_for_account(files_dir: &Path, label: Option<&str>, email: &str) -> PathBuf {
    libation_source::auth_file_for_account(files_dir, label, email, AUTH_SUFFIX)
}

/// Ensure `Accounts/` exists.
pub fn ensure_accounts_dir(files_dir: &Path) -> std::io::Result<PathBuf> {
    libation_source::ensure_accounts_dir(files_dir)
}

/// Filename stem from an optional label or email (`user@host` → `user`).
#[must_use]
pub fn auth_stem(label: Option<&str>, email: &str) -> String {
    libation_source::auth_stem(label, email)
}

/// Path `{files_dir}/Accounts/{stem}.libro.auth`.
#[must_use]
pub fn auth_file_for(files_dir: &Path, stem: &str) -> PathBuf {
    libation_source::auth_file_for(files_dir, stem, AUTH_SUFFIX)
}

/// List `*.libro.auth` files under `Accounts/`.
pub fn list_auth_files(files_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    libation_source::list_auth_files(files_dir, AUTH_SUFFIX)
}

/// Write auth JSON to disk (pretty-printed).
pub fn save_auth(path: &Path, auth: &LibroAuthFile) -> Result<()> {
    libation_source::save_json_auth(path, auth).map_err(Into::into)
}

/// Load auth JSON from disk.
pub fn load_auth(path: &Path) -> Result<LibroAuthFile> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            LibroError::AccountNotFound(path.display().to_string())
        } else {
            LibroError::Io(e)
        }
    })?;
    let auth: LibroAuthFile = serde_json::from_str(&raw)?;
    if auth.access_token.is_empty() {
        return Err(LibroError::auth("auth file missing access_token"));
    }
    if auth.email.is_empty() {
        return Err(LibroError::auth("auth file missing email"));
    }
    Ok(auth)
}

/// Find an auth file by account id (email or user_id) or filename stem.
pub fn find_auth_file(files_dir: &Path, account_id: &str) -> Result<PathBuf> {
    let needle = account_id.trim();
    if needle.is_empty() {
        return Err(LibroError::AccountNotFound("empty account id".into()));
    }

    let by_stem = auth_file_for(files_dir, needle);
    if by_stem.is_file() {
        return Ok(by_stem);
    }

    for path in list_auth_files(files_dir)? {
        let auth = match load_auth(&path) {
            Ok(auth) => auth,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "skipping unreadable Libro.fm auth file during account lookup"
                );
                continue;
            }
        };
        if auth.email.eq_ignore_ascii_case(needle)
            || auth.account_id().eq_ignore_ascii_case(needle)
            || auth
                .label
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(needle))
        {
            return Ok(path);
        }
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(AUTH_SUFFIX))
            .unwrap_or("");
        if stem.eq_ignore_ascii_case(needle) {
            return Ok(path);
        }
    }

    Err(LibroError::AccountNotFound(needle.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_from_email_and_label() {
        assert_eq!(auth_stem(None, "alice@example.com"), "alice");
        assert_eq!(auth_stem(Some("My Libro"), "alice@example.com"), "My_Libro");
    }

    #[test]
    fn roundtrip_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_file_for_account(dir.path(), None, "bob@libro.fm");
        let auth = LibroAuthFile {
            access_token: "tok".into(),
            token_type: "Bearer".into(),
            expires_at: None,
            email: "bob@libro.fm".into(),
            user_id: Some("42".into()),
            marketplace: "us".into(),
            label: None,
        };
        save_auth(&path, &auth).unwrap();
        let loaded = load_auth(&path).unwrap();
        assert_eq!(loaded.account_id(), "42");
        assert_eq!(list_auth_files(dir.path()).unwrap().len(), 1);
    }
}
