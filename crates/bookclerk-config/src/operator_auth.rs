//! Operator API token for the daemon HTTP control plane / GUI.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{ConfigError, Result};
use crate::redact::register_secret;
use crate::Config;

/// How an operator token was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOperatorToken {
    /// Value from `BOOKCLERK_OPERATOR_TOKEN`.
    Env,
    /// Value read from (or written to) the configured token file.
    File,
}

/// Absolute path to the operator token file.
#[must_use]
pub fn operator_token_path(config: &Config) -> PathBuf {
    let raw = config.daemon.auth.token_file.trim();
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config.paths().files_dir.join(path)
    }
}

/// Read the operator token from env or file without creating a missing file.
///
/// Returns `Ok(None)` when neither env nor an existing file provides a token.
pub fn read_operator_token(config: &Config) -> Result<Option<(String, ResolveOperatorToken)>> {
    if let Ok(v) = std::env::var("BOOKCLERK_OPERATOR_TOKEN") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            let token = validate_operator_token(trimmed, "BOOKCLERK_OPERATOR_TOKEN")?;
            register_secret(&token);
            return Ok(Some((token, ResolveOperatorToken::Env)));
        }
    }
    let path = operator_token_path(config);
    if !path.is_file() {
        return Ok(None);
    }
    let token = read_token_file(&path)?;
    register_secret(&token);
    Ok(Some((token, ResolveOperatorToken::File)))
}

/// Read the operator token, creating the token file with a strong secret if needed.
///
/// The second return value is `true` when a new file was minted.
pub fn read_or_create_operator_token(config: &Config) -> Result<(String, bool)> {
    if let Ok(v) = std::env::var("BOOKCLERK_OPERATOR_TOKEN") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            let token = validate_operator_token(trimmed, "BOOKCLERK_OPERATOR_TOKEN")?;
            register_secret(&token);
            return Ok((token, false));
        }
    }

    let path = operator_token_path(config);
    if path.is_file() {
        let token = read_token_file(&path)?;
        register_secret(&token);
        return Ok((token, false));
    }

    let token = generate_token()?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| {
                ConfigError::Invalid(format!(
                    "failed to create operator token directory {}: {err}",
                    parent.display()
                ))
            })?;
            harden_secret_dir(parent);
        }
    }
    write_token_file(&path, &token)?;
    register_secret(&token);
    eprintln!(
        "bookclerkd: generated operator API token at {} (use for GUI login / Bearer auth; \
         keep this file private)",
        path.display()
    );
    tracing::info!(
        path = %path.display(),
        "generated operator API token file"
    );
    Ok((token, true))
}

fn read_token_file(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path).map_err(|err| {
        ConfigError::Invalid(format!(
            "failed to read operator token file {}: {err}",
            path.display()
        ))
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "operator token file {} is empty",
            path.display()
        )));
    }
    validate_operator_token(trimmed, &format!("operator token file {}", path.display()))
}

/// Reject tokens that can break URL fragments, HTTP headers, or shell pastes.
///
/// Generated tokens are hex. Env overrides must stay printable single-line and
/// free of whitespace / control characters so they cannot inject into
/// `#token=…` fragments or `Authorization` headers.
fn validate_operator_token(token: &str, source: &str) -> Result<String> {
    if token.is_empty() {
        return Err(ConfigError::Invalid(format!("{source} is empty")));
    }
    if token.len() > 512 {
        return Err(ConfigError::Invalid(format!(
            "{source} is too long (max 512 bytes)"
        )));
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_graphic() && c != '"' && c != '\'' && c != '`' && c != '\\')
    {
        return Err(ConfigError::Invalid(format!(
            "{source} contains whitespace, quotes, or non-printable characters; \
             use a single-line URL-safe secret"
        )));
    }
    Ok(token.to_string())
}

fn write_token_file(path: &Path, token: &str) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|err| {
        ConfigError::Invalid(format!(
            "failed to create operator token file {}: {err}",
            path.display()
        ))
    })?;
    file.write_all(token.as_bytes()).map_err(|err| {
        ConfigError::Invalid(format!(
            "failed to write operator token file {}: {err}",
            path.display()
        ))
    })?;
    file.write_all(b"\n").ok();
    harden_secret_file(path);
    Ok(())
}

fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|err| ConfigError::Invalid(format!("failed to generate operator token: {err}")))?;
    Ok(encode_hex(&bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn harden_secret_file(path: &Path) {
    set_mode(path, 0o600);
}

fn harden_secret_dir(path: &Path) {
    set_mode(path, 0o700);
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(mode);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Paths;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn creates_and_reads_token_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("BOOKCLERK_OPERATOR_TOKEN");
        std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN");

        let dir = tempdir().unwrap();
        let mut cfg = Config {
            paths: Some(Paths::from_files_dir(dir.path().to_path_buf())),
            ..Config::default()
        };
        cfg.daemon.auth.token_file = "operator.token".into();

        let (token, created) = read_or_create_operator_token(&cfg).unwrap();
        assert!(created);
        assert_eq!(token.len(), 64);

        let (again, created2) = read_or_create_operator_token(&cfg).unwrap();
        assert!(!created2);
        assert_eq!(again, token);

        let read = read_operator_token(&cfg).unwrap().unwrap();
        assert_eq!(read.0, token);
        assert_eq!(read.1, ResolveOperatorToken::File);

        match prev {
            Some(v) => std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", v),
            None => std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN"),
        }
    }

    #[test]
    fn rejects_env_token_with_injection_chars() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let cfg = Config {
            paths: Some(Paths::from_files_dir(dir.path().to_path_buf())),
            ..Config::default()
        };
        let prev = std::env::var_os("BOOKCLERK_OPERATOR_TOKEN");
        std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", "bad token with spaces");
        let err = read_operator_token(&cfg).unwrap_err().to_string();
        assert!(err.contains("whitespace") || err.contains("non-printable"));
        match prev {
            Some(v) => std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", v),
            None => std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN"),
        }
    }
}
