//! Auth-file encryption passphrase resolution for headless deployments.
//!
//! Audible OAuth material is stored as audible-rs envelopes under `Accounts/`.
//! Prefer encrypting those envelopes with a passphrase from:
//!
//! 1. `BOOKCLERK_AUTH_PASSWORD` (env)
//! 2. `BOOKCLERK_AUTH_PASSWORD_FILE` (env path — Docker/systemd secret)
//! 3. Explicit `password_file` argument (CLI / call site)
//! 4. Process default from [`configure_auth_secrets`] (`[auth].password_file`)
//!
//! When a password **file path** is configured but the file does not exist yet,
//! Bookclerk creates it with a strong CSPRNG secret. Point that path at a
//! dedicated secrets volume (not `Accounts/`) so the key is not co-located with
//! the ciphertext.
//!
//! With no passphrase configured, callers may opt into plaintext `.auth` files
//! via `auth.allow_plaintext` / [`configure_auth_secrets`].
//!
//! OS keychains are a poor fit for non-interactive VPS/Docker.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use secrecy::{ExposeSecret, SecretString};

use crate::error::{AudibleError, Result};

/// Environment variable holding the auth-file passphrase directly.
pub const AUTH_PASSWORD_ENV: &str = "BOOKCLERK_AUTH_PASSWORD";

/// Environment variable pointing at a file that contains the passphrase.
pub const AUTH_PASSWORD_FILE_ENV: &str = "BOOKCLERK_AUTH_PASSWORD_FILE";

#[derive(Debug, Clone, Default)]
struct AuthSecretDefaults {
    password_file: Option<PathBuf>,
    allow_plaintext: bool,
}

fn auth_defaults() -> &'static Mutex<AuthSecretDefaults> {
    static CELL: OnceLock<Mutex<AuthSecretDefaults>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(AuthSecretDefaults::default()))
}

/// Apply `[auth]` settings from the loaded config so scan/acquire/migrate pick
/// up `password_file` / `allow_plaintext` without threading them through every API.
pub fn configure_auth_secrets(password_file: Option<PathBuf>, allow_plaintext: bool) {
    if let Ok(mut guard) = auth_defaults().lock() {
        guard.password_file = password_file;
        guard.allow_plaintext = allow_plaintext;
    }
}

/// Whether plaintext auth files are allowed when no passphrase is configured.
#[must_use]
pub fn default_allow_plaintext() -> bool {
    auth_defaults()
        .lock()
        .map(|g| g.allow_plaintext)
        .unwrap_or(false)
}

fn default_password_file() -> Option<PathBuf> {
    auth_defaults()
        .lock()
        .ok()
        .and_then(|g| g.password_file.clone())
}

/// Resolve the auth-file encryption passphrase, if any.
///
/// `configured_password_file` comes from an explicit call-site override; when
/// `None`, the process default from [`configure_auth_secrets`] is used.
/// Missing password-file paths are created with a strong random secret.
pub fn resolve_auth_password(
    configured_password_file: Option<&Path>,
) -> Result<Option<SecretString>> {
    if let Ok(value) = std::env::var(AUTH_PASSWORD_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            bookclerk_config::register_secret(trimmed);
            return Ok(Some(SecretString::from(trimmed.to_string())));
        }
    }

    if let Ok(path) = std::env::var(AUTH_PASSWORD_FILE_ENV) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            let secret = read_or_create_password_file(Path::new(trimmed))?;
            bookclerk_config::register_secret(secret.expose_secret());
            return Ok(Some(secret));
        }
    }

    if let Some(path) = configured_password_file {
        let secret = read_or_create_password_file(path)?;
        bookclerk_config::register_secret(secret.expose_secret());
        return Ok(Some(secret));
    }

    if let Some(path) = default_password_file() {
        let secret = read_or_create_password_file(&path)?;
        bookclerk_config::register_secret(secret.expose_secret());
        return Ok(Some(secret));
    }

    Ok(None)
}

/// Require a passphrase for writing encrypted auth files.
pub fn require_auth_password(configured_password_file: Option<&Path>) -> Result<SecretString> {
    resolve_auth_password(configured_password_file)?.ok_or_else(|| {
        AudibleError::Auth(format!(
            "auth-file encryption requires a passphrase — set {AUTH_PASSWORD_ENV}, \
             {AUTH_PASSWORD_FILE_ENV} (auto-creates the file with a strong random secret \
             if missing), or [auth].password_file; or set auth.allow_plaintext = true \
             to store unprotected tokens"
        ))
    })
}

/// Read a passphrase file, or create it with a strong random secret if absent.
///
/// The path is chosen by the operator (Docker secrets volume, systemd
/// `LoadCredential`, etc.) — never under `Accounts/` beside the ciphertext.
pub fn read_or_create_password_file(path: &Path) -> Result<SecretString> {
    if path.is_file() {
        return read_password_file(path);
    }

    let secret = generate_strong_password()?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| {
                AudibleError::Auth(format!(
                    "failed to create auth password directory {}: {err}",
                    parent.display()
                ))
            })?;
            let _ = harden_secret_path(parent);
        }
    }
    write_password_file(path, secret.expose_secret())?;
    tracing::info!(
        path = %path.display(),
        "generated auth encryption passphrase at configured password file \
         (keep this path off the Accounts/ volume when possible)"
    );
    Ok(secret)
}

fn generate_strong_password() -> Result<SecretString> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| {
        AudibleError::Auth(format!("failed to generate auth encryption key: {err}"))
    })?;
    Ok(SecretString::from(encode_hex(&bytes)))
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

fn read_password_file(path: &Path) -> Result<SecretString> {
    let raw = std::fs::read_to_string(path).map_err(|err| {
        AudibleError::Auth(format!(
            "failed to read auth password file {}: {err}",
            path.display()
        ))
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AudibleError::Auth(format!(
            "auth password file {} is empty",
            path.display()
        )));
    }
    Ok(SecretString::from(trimmed.to_string()))
}

fn write_password_file(path: &Path, secret: &str) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|err| {
        AudibleError::Auth(format!(
            "failed to create auth password file {}: {err}",
            path.display()
        ))
    })?;
    file.write_all(secret.as_bytes()).map_err(|err| {
        AudibleError::Auth(format!(
            "failed to write auth password file {}: {err}",
            path.display()
        ))
    })?;
    file.write_all(b"\n").ok();
    harden_secret_path(path)?;
    Ok(())
}

/// Best-effort restrictive permissions for a secret-bearing path (Unix).
pub fn harden_secret_path(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.is_dir() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        } else if path.is_file() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_defaults() {
        configure_auth_secrets(None, false);
    }

    #[test]
    fn resolve_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_defaults();
        std::env::set_var(AUTH_PASSWORD_ENV, "unit-test-secret");
        std::env::remove_var(AUTH_PASSWORD_FILE_ENV);
        let got = resolve_auth_password(None).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "unit-test-secret");
        std::env::remove_var(AUTH_PASSWORD_ENV);
    }

    #[test]
    fn resolve_from_existing_password_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_defaults();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        std::env::remove_var(AUTH_PASSWORD_FILE_ENV);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pw");
        std::fs::write(&path, "file-secret\n").unwrap();
        let got = resolve_auth_password(Some(&path)).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "file-secret");
    }

    #[test]
    fn auto_creates_missing_password_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_defaults();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        std::env::remove_var(AUTH_PASSWORD_FILE_ENV);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets").join("bookclerk_auth");
        let first = resolve_auth_password(Some(&path)).unwrap().unwrap();
        let second = resolve_auth_password(Some(&path)).unwrap().unwrap();
        assert_eq!(first.expose_secret(), second.expose_secret());
        assert_eq!(first.expose_secret().len(), 64);
        assert!(path.is_file());
    }

    #[test]
    fn uses_configured_process_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        std::env::remove_var(AUTH_PASSWORD_FILE_ENV);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("from-config");
        std::fs::write(&path, "config-secret\n").unwrap();
        configure_auth_secrets(Some(path), false);
        let got = resolve_auth_password(None).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "config-secret");
        clear_defaults();
    }

    #[test]
    fn none_when_unconfigured() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_defaults();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        std::env::remove_var(AUTH_PASSWORD_FILE_ENV);
        assert!(resolve_auth_password(None).unwrap().is_none());
    }
}
