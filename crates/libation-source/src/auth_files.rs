//! Shared `Accounts/*.auth` path helpers for plain-token content sources.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Directory for per-account auth artifacts (`{files_dir}/Accounts/`).
#[must_use]
pub fn accounts_dir(files_dir: &Path) -> PathBuf {
    files_dir.join("Accounts")
}

/// Ensure `Accounts/` exists.
pub fn ensure_accounts_dir(files_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = accounts_dir(files_dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Sanitize a label or email stem for use as a filename.
#[must_use]
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' | '@' => out.push('_'),
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

/// Filename stem from an optional label or email (`user@host` → `user`).
#[must_use]
pub fn auth_stem(label: Option<&str>, email: &str) -> String {
    if let Some(label) = label.map(str::trim).filter(|s| !s.is_empty()) {
        return sanitize_name(label);
    }
    let stem = email.split('@').next().unwrap_or(email);
    sanitize_name(stem)
}

/// Path `{files_dir}/Accounts/{stem}{suffix}` (suffix includes the leading `.`, e.g. `.ga.auth`).
#[must_use]
pub fn auth_file_for(files_dir: &Path, stem: &str, suffix: &str) -> PathBuf {
    accounts_dir(files_dir).join(format!("{}{}", sanitize_name(stem), suffix))
}

/// Path for a label/email pair.
#[must_use]
pub fn auth_file_for_account(
    files_dir: &Path,
    label: Option<&str>,
    email: &str,
    suffix: &str,
) -> PathBuf {
    auth_file_for(files_dir, &auth_stem(label, email), suffix)
}

/// List auth files under `Accounts/` whose names end with `suffix`.
pub fn list_auth_files(files_dir: &Path, suffix: &str) -> std::io::Result<Vec<PathBuf>> {
    let dir = accounts_dir(files_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if name.ends_with(suffix) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Write pretty-printed JSON auth to disk.
pub fn save_json_auth<T: Serialize>(path: &Path, auth: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(auth)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
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
    fn auth_path_uses_suffix() {
        let path = auth_file_for(Path::new("/tmp/files"), "alice", ".ga.auth");
        assert_eq!(path, PathBuf::from("/tmp/files/Accounts/alice.ga.auth"));
    }
}
