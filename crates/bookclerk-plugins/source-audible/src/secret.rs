//! Auth passphrase resolution for headless deployments.
//!
//! New writes use the process DEK via `bookclerk_library::master_key`.
//! This module keeps `resolve_auth_password` for reading legacy
//! `json-encrypted` rows and loading external audible-rs files that were
//! encrypted with a passphrase.

use secrecy::SecretString;

use crate::error::Result;

/// Environment variable holding the auth encryption passphrase.
///
/// For new installs this is optional — the process DEK (`master.key`) is
/// sufficient. Set it to wrap `master.key` at rest (strongly recommended for
/// production) and to read legacy `json-encrypted` DB rows.
pub const AUTH_PASSWORD_ENV: &str = "BOOKCLERK_AUTH_PASSWORD";

/// Resolve the auth encryption passphrase from `BOOKCLERK_AUTH_PASSWORD`, if set.
///
/// Used for reading legacy `json-encrypted` rows and loading external
/// audible-rs files that were previously encrypted with this passphrase.
pub fn resolve_auth_password() -> Result<Option<SecretString>> {
    if let Ok(value) = std::env::var(AUTH_PASSWORD_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            bookclerk_config::register_secret(trimmed);
            return Ok(Some(SecretString::from(trimmed.to_string())));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(AUTH_PASSWORD_ENV, "unit-test-secret");
        let got = resolve_auth_password().unwrap().unwrap();
        assert_eq!(got.expose_secret(), "unit-test-secret");
        std::env::remove_var(AUTH_PASSWORD_ENV);
    }

    #[test]
    fn none_when_unconfigured() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        assert!(resolve_auth_password().unwrap().is_none());
    }
}
