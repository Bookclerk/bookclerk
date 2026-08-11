//! Operator API token helpers for the daemon HTTP control plane / GUI.
//!
//! Durable storage lives in `encrypted_secrets` (see `bookclerk-library::operator_token`).
//! This module owns validation, generation, and reading the optional
//! `BOOKCLERK_OPERATOR_TOKEN` env override.

use crate::error::{ConfigError, Result};
use crate::redact::register_secret;

/// How an operator token was resolved from env (config-layer only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOperatorTokenEnv {
    /// Value from `BOOKCLERK_OPERATOR_TOKEN`.
    Env,
}

/// Read `BOOKCLERK_OPERATOR_TOKEN` when set and non-empty.
pub fn read_operator_token_env() -> Result<Option<(String, ResolveOperatorTokenEnv)>> {
    if let Ok(v) = std::env::var("BOOKCLERK_OPERATOR_TOKEN") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            let token = validate_operator_token(trimmed, "BOOKCLERK_OPERATOR_TOKEN")?;
            register_secret(&token);
            return Ok(Some((token, ResolveOperatorTokenEnv::Env)));
        }
    }
    Ok(None)
}

/// Reject tokens that can break URL fragments, HTTP headers, or shell pastes.
///
/// Generated tokens are hex. Env overrides must stay printable single-line and
/// free of whitespace / control characters so they cannot inject into
/// `#token=…` fragments or `Authorization` headers.
pub fn validate_operator_token(token: &str, source: &str) -> Result<String> {
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

/// Generate a 32-byte hex operator token.
pub fn generate_operator_token() -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn rejects_env_token_with_injection_chars() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("BOOKCLERK_OPERATOR_TOKEN");
        std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", "bad token with spaces");
        let err = read_operator_token_env().unwrap_err().to_string();
        assert!(err.contains("whitespace") || err.contains("non-printable"));
        match prev {
            Some(v) => std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", v),
            None => std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN"),
        }
    }

    #[test]
    fn generate_is_64_hex_chars() {
        let token = generate_operator_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
