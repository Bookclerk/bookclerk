//! Account file layout under `BOOKCLERK_FILES_DIR`.
//!
//! | Path | Contents |
//! | --- | --- |
//! | `Accounts/<name>.auth` | Audible OAuth envelope (encrypted or plaintext) |
//! | `Accounts/<name>.wvd` | Widevine L3 CDM |

use std::path::{Path, PathBuf};

/// Directory for per-account auth + CDM artifacts (`{files_dir}/Accounts/`).
#[must_use]
pub fn accounts_dir(files_dir: &Path) -> PathBuf {
    files_dir.join("Accounts")
}

/// Canonical path for one account's auth file (`{files_dir}/Accounts/{name}.auth`).
#[must_use]
pub fn auth_file_for(files_dir: &Path, account_name: &str) -> PathBuf {
    accounts_dir(files_dir).join(format!("{}.auth", sanitize_name(account_name)))
}

/// Path for one account's Widevine L3 CDM (`{files_dir}/Accounts/{name}.wvd`).
#[must_use]
pub fn widevine_cdm_file_for(files_dir: &Path, account_name: &str) -> PathBuf {
    accounts_dir(files_dir).join(format!("{}.wvd", sanitize_name(account_name)))
}

/// Ensure `Accounts/` exists with restrictive permissions.
pub fn ensure_accounts_dir(files_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = accounts_dir(files_dir);
    std::fs::create_dir_all(&dir)?;
    let _ = crate::secret::harden_secret_path(&dir);
    Ok(dir)
}

/// List Audible `*.auth` files under `Accounts/`.
///
/// Skips other sources' envelopes that also end in `.auth`
/// (`.libro.auth`, `.ga.auth`, `.chirp.auth`).
pub fn list_auth_files(files_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let dir = accounts_dir(files_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // Multi-suffix envelopes share Accounts/ but belong to other sources.
        if name.ends_with(".libro.auth")
            || name.ends_with(".ga.auth")
            || name.ends_with(".chirp.auth")
        {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("auth") {
            continue;
        }
        out.push(path);
    }
    out.sort();
    Ok(out)
}

/// Sanitize an account name for use as a filename stem.
#[must_use]
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => out.push('_'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_matches('.');
    if trimmed.is_empty() {
        "account".into()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_name_and_paths() {
        assert_eq!(sanitize_name("Main US"), "Main_US");
        assert_eq!(
            auth_file_for(Path::new("/data"), "us"),
            PathBuf::from("/data/Accounts/us.auth")
        );
        assert_eq!(
            widevine_cdm_file_for(Path::new("/data"), "us"),
            PathBuf::from("/data/Accounts/us.wvd")
        );
    }

    #[test]
    fn list_only_accounts_auth_files() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts_dir(dir.path());
        std::fs::create_dir_all(&accounts).unwrap();
        std::fs::write(accounts.join("alice.auth"), b"{}").unwrap();
        // Other sources share Accounts/ but must not be treated as Audible.
        std::fs::write(accounts.join("alice.libro.auth"), b"{}").unwrap();
        std::fs::write(accounts.join("alice.ga.auth"), b"{}").unwrap();
        std::fs::write(accounts.join("alice.chirp.auth"), b"{}").unwrap();
        // Stray non-auth files must be ignored.
        std::fs::write(accounts.join("notes.txt"), b"x").unwrap();
        // Stray legacy dir must be ignored.
        let legacy = dir.path().join("auth");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("bob.auth"), b"{}").unwrap();
        let list = list_auth_files(dir.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].ends_with("Accounts/alice.auth"));
    }
}
