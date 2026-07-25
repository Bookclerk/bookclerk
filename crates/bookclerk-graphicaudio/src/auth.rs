//! GraphicAudio auth file persistence under `Accounts/*.ga.auth`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{GraphicAudioError, Result};

/// On-disk auth suffix for GraphicAudio accounts.
pub const AUTH_SUFFIX: &str = ".ga.auth";

/// On-disk auth envelope for one GraphicAudio account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphicAudioAuthFile {
    /// Opaque device activation token from `POST activation/login`.
    /// Empty when the account was logged in with Magento-only access (`web`/`zip`).
    #[serde(default)]
    pub token: String,
    /// Device / client id sent at login (stable for this auth file; used by `device`).
    pub client_id: String,
    pub email: String,
    #[serde(default = "default_marketplace")]
    pub marketplace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn default_marketplace() -> String {
    String::from("us")
}

impl GraphicAudioAuthFile {
    /// Stable account id for library rows (email).
    #[must_use]
    pub fn account_id(&self) -> &str {
        self.email.as_str()
    }

    /// True when an Access App device token is present.
    #[must_use]
    pub fn has_device_token(&self) -> bool {
        !self.token.trim().is_empty()
    }
}

/// Path for a label/email pair.
#[must_use]
pub fn auth_file_for_account(files_dir: &Path, label: Option<&str>, email: &str) -> PathBuf {
    bookclerk_source::auth_file_for_account(files_dir, label, email, AUTH_SUFFIX)
}

/// Ensure `Accounts/` exists.
pub fn ensure_accounts_dir(files_dir: &Path) -> std::io::Result<PathBuf> {
    bookclerk_source::ensure_accounts_dir(files_dir)
}

/// Filename stem from an optional label or email (`user@host` → `user`).
#[must_use]
pub fn auth_stem(label: Option<&str>, email: &str) -> String {
    bookclerk_source::auth_stem(label, email)
}

/// Path `{files_dir}/Accounts/{stem}.ga.auth`.
#[must_use]
pub fn auth_file_for(files_dir: &Path, stem: &str) -> PathBuf {
    bookclerk_source::auth_file_for(files_dir, stem, AUTH_SUFFIX)
}

/// List `*.ga.auth` files under `Accounts/`.
pub fn list_auth_files(files_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    bookclerk_source::list_auth_files(files_dir, AUTH_SUFFIX)
}

/// Write auth JSON to disk (pretty-printed).
pub fn save_auth(path: &Path, auth: &GraphicAudioAuthFile) -> Result<()> {
    bookclerk_source::save_json_auth(path, auth).map_err(Into::into)
}

/// Load auth JSON from disk.
///
/// Magento-only (`web`/`zip`) accounts may have an empty `token`.
pub fn load_auth(path: &Path) -> Result<GraphicAudioAuthFile> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            GraphicAudioError::AccountNotFound(path.display().to_string())
        } else {
            GraphicAudioError::Io(e)
        }
    })?;
    let auth: GraphicAudioAuthFile = serde_json::from_str(&raw)?;
    if auth.email.is_empty() {
        return Err(GraphicAudioError::auth("auth file missing email"));
    }
    if auth.client_id.is_empty() {
        return Err(GraphicAudioError::auth("auth file missing client_id"));
    }
    Ok(auth)
}

/// Find an auth file by account id (email) or filename stem.
pub fn find_auth_file(files_dir: &Path, account_id: &str) -> Result<PathBuf> {
    let needle = account_id.trim();
    if needle.is_empty() {
        return Err(GraphicAudioError::AccountNotFound(
            "empty account id".into(),
        ));
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
                    "skipping unreadable GraphicAudio auth file during account lookup"
                );
                continue;
            }
        };
        if auth.email.eq_ignore_ascii_case(needle)
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

    Err(GraphicAudioError::AccountNotFound(needle.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_from_email_and_label() {
        assert_eq!(auth_stem(None, "alice@example.com"), "alice");
        assert_eq!(auth_stem(Some("My GA"), "alice@example.com"), "My_GA");
    }

    #[test]
    fn roundtrip_auth_file_allows_empty_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_file_for_account(dir.path(), None, "bob@ga.example");
        let auth = GraphicAudioAuthFile {
            token: String::new(),
            client_id: "bookclerk-1".into(),
            email: "bob@ga.example".into(),
            marketplace: "us".into(),
            label: None,
        };
        save_auth(&path, &auth).unwrap();
        let loaded = load_auth(&path).unwrap();
        assert!(!loaded.has_device_token());
        assert_eq!(loaded.email, "bob@ga.example");
    }

    #[test]
    fn roundtrip_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_file_for_account(dir.path(), None, "a@ex.com");
        let auth = GraphicAudioAuthFile {
            token: "tok".into(),
            client_id: "device-1".into(),
            email: "a@ex.com".into(),
            marketplace: "us".into(),
            label: Some("GA".into()),
        };
        save_auth(&path, &auth).unwrap();
        let loaded = load_auth(&path).unwrap();
        assert_eq!(loaded.token, "tok");
        assert_eq!(loaded.account_id(), "a@ex.com");
    }
}
