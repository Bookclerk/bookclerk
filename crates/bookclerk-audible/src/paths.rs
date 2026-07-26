//! Account file layout under `BOOKCLERK_FILES_DIR`.
//!
//! | Path | Contents |
//! | --- | --- |
//! | `Accounts/<name>.audible.auth` | Audible OAuth envelope (encrypted or plaintext) |
//! | `Accounts/<name>.wvd` | Widevine L3 CDM |
//!
//! Classic Libation migration does **not** read bare `Accounts/*.auth`; it converts
//! `AccountsSettings.json` IdentityTokens into `*.audible.auth` via
//! [`crate::accounts::import_auth_file`] / migrate. Import of an external
//! audible-rs file (any path/name) still re-saves under the canonical suffix.

use std::path::{Path, PathBuf};

/// On-disk auth suffix for Audible accounts (aligned with `.libro.auth`, etc.).
pub const AUTH_SUFFIX: &str = ".audible.auth";

/// Directory for per-account auth + CDM artifacts (`{files_dir}/Accounts/`).
#[must_use]
pub fn accounts_dir(files_dir: &Path) -> PathBuf {
    bookclerk_source::accounts_dir(files_dir)
}

/// Canonical path for one account's auth file
/// (`{files_dir}/Accounts/{name}.audible.auth`).
#[must_use]
pub fn auth_file_for(files_dir: &Path, account_name: &str) -> PathBuf {
    bookclerk_source::auth_file_for(files_dir, account_name, AUTH_SUFFIX)
}

/// Path for one account's Widevine L3 CDM (`{files_dir}/Accounts/{name}.wvd`).
#[must_use]
pub fn widevine_cdm_file_for(files_dir: &Path, account_name: &str) -> PathBuf {
    accounts_dir(files_dir).join(format!("{}.wvd", sanitize_name(account_name)))
}

/// Ensure `Accounts/` exists with restrictive permissions.
pub fn ensure_accounts_dir(files_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = bookclerk_source::ensure_accounts_dir(files_dir)?;
    let _ = crate::secret::harden_secret_path(&dir);
    Ok(dir)
}

/// Filename stem from an Audible auth path (`alice.audible.auth` → `alice`).
#[must_use]
pub fn auth_stem_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    auth_stem_from_name(name).map(str::to_string)
}

/// Filename stem from an Audible auth filename.
///
/// Also accepts a bare `*.auth` basename when importing an external audible-rs
/// file whose name is not yet Bookclerk-qualified (destination is always
/// [`AUTH_SUFFIX`]).
#[must_use]
pub fn auth_stem_from_name(name: &str) -> Option<&str> {
    if let Some(stem) = name.strip_suffix(AUTH_SUFFIX) {
        return Some(stem);
    }
    // Other sources / destinations also end in `.auth`.
    if name.ends_with(".libro.auth")
        || name.ends_with(".ga.auth")
        || name.ends_with(".chirp.auth")
        || name.ends_with(".s3.auth")
    {
        return None;
    }
    name.strip_suffix(".auth")
}

/// List Audible `*.audible.auth` files under `Accounts/`.
pub fn list_auth_files(files_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    bookclerk_source::list_auth_files(files_dir, AUTH_SUFFIX)
}

/// Sanitize an account name for use as a filename stem.
#[must_use]
pub fn sanitize_name(name: &str) -> String {
    bookclerk_source::sanitize_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_name_and_paths() {
        assert_eq!(sanitize_name("Main US"), "Main_US");
        assert_eq!(
            auth_file_for(Path::new("/data"), "us"),
            PathBuf::from("/data/Accounts/us.audible.auth")
        );
        assert_eq!(
            widevine_cdm_file_for(Path::new("/data"), "us"),
            PathBuf::from("/data/Accounts/us.wvd")
        );
    }

    #[test]
    fn stem_from_canonical_and_import_basenames() {
        assert_eq!(auth_stem_from_name("alice.audible.auth"), Some("alice"));
        // External audible-rs import may still be named `alice.auth`.
        assert_eq!(auth_stem_from_name("alice.auth"), Some("alice"));
        assert_eq!(auth_stem_from_name("alice.libro.auth"), None);
        assert_eq!(auth_stem_from_name("alice.s3.auth"), None);
    }

    #[test]
    fn list_only_audible_auth_files() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts_dir(dir.path());
        std::fs::create_dir_all(&accounts).unwrap();
        std::fs::write(accounts.join("alice.audible.auth"), b"{}").unwrap();
        // Bare legacy Bookclerk names are ignored (not Libation; not migrated).
        std::fs::write(accounts.join("legacy.auth"), b"{}").unwrap();
        // Other sources share Accounts/ but must not be treated as Audible.
        std::fs::write(accounts.join("alice.libro.auth"), b"{}").unwrap();
        std::fs::write(accounts.join("alice.ga.auth"), b"{}").unwrap();
        std::fs::write(accounts.join("alice.chirp.auth"), b"{}").unwrap();
        std::fs::write(accounts.join("default.s3.auth"), b"{}").unwrap();
        std::fs::write(accounts.join("notes.txt"), b"x").unwrap();

        let list = list_auth_files(dir.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].ends_with("Accounts/alice.audible.auth"));
    }
}
