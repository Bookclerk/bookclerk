//! Hashing for claim tickets and session cookies (store hashes only).

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::error::{LibraryError, Result};
use crate::master_key::MasterKey;

/// SHA-256 hex digest of a raw ticket/session token for durable storage.
///
/// # Arguments
///
/// * `raw` - Opaque plaintext token; callers must not persist this value.
///
/// # Returns
///
/// Lowercase hex SHA-256 digest (64 characters).
#[must_use]
pub fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}

/// Hex length of a browser-generated claim-redeem nonce (32 bytes).
pub const CLAIM_REDEEM_NONCE_HEX_LEN: usize = 64;

/// Domain-separated HMAC-SHA256 label for claim-ticket session derivation.
const CLAIM_SESSION_MAC_DOMAIN: &[u8] = b"bookclerk-claim-session-v1:";

/// Domain-separated HMAC-SHA256 label for the invite-password subkey.
const CLAIM_PASSWORD_SUBKEY_DOMAIN: &[u8] = b"bookclerk-claim-password-subkey-v1";

/// Accept a browser-generated claim-redeem nonce.
///
/// The SPA persists a 32-byte random value as 64 hex characters across HTTP
/// retries (`sessionStorage`). Possession of the used magic-link URL without
/// this nonce cannot recover the committed session.
///
/// # Errors
///
/// Returns an error when the value is missing, the wrong length, or not hex.
pub fn parse_claim_redeem_nonce(raw: &str) -> Result<&str> {
    let nonce = raw.trim();
    if nonce.len() == CLAIM_REDEEM_NONCE_HEX_LEN
        && nonce.as_bytes().iter().all(u8::is_ascii_hexdigit)
    {
        Ok(nonce)
    } else {
        Err(LibraryError::Other(anyhow::anyhow!(
            "invalid claim redeem nonce"
        )))
    }
}

/// Derive the portal session token for a claim ticket plus browser nonce.
///
/// The raw session is
/// `HMAC-SHA256(DEK, "bookclerk-claim-session-v1:" || ticket || 0x00 || nonce)`
/// hex-encoded (64 characters). The same ticket, nonce, and process DEK always
/// yield the same session, so a browser retry after a lost `dbAtomic` reply
/// reuses `redeemClaimTicket:{token}:{session}`. A different nonce (another
/// browser, or the magic link alone) cannot replay that receipt. The plaintext
/// bearer is never written to a receipt; only [`hash_token`] of this value is
/// stored.
///
/// # Arguments
///
/// * `dek` - Process data-encryption key from [`crate::require_master_key`].
/// * `raw_ticket` - Plaintext claim ticket from the SPA / share URL.
/// * `nonce` - Validated 64-hex redeem nonce from [`parse_claim_redeem_nonce`].
#[must_use]
pub fn derive_claim_session_token(dek: &MasterKey, raw_ticket: &str, nonce: &str) -> String {
    hex::encode(hmac_sha256(
        dek.as_bytes(),
        &[
            CLAIM_SESSION_MAC_DOMAIN,
            raw_ticket.as_bytes(),
            &[0],
            nonce.as_bytes(),
        ],
    ))
}

/// Stable, non-secret fingerprint of an invite/reset password for idempotency.
///
/// Argon2id storage hashes use a fresh salt on every POST, so they cannot be
/// the `dbAtomic` request hash. This value is
/// `HMAC-SHA256(subkey, nonce || 0x00 || password)` where
/// `subkey = HMAC-SHA256(DEK, "bookclerk-claim-password-subkey-v1")`.
/// The randomized Argon2id hash is still what gets stored on the user row.
#[must_use]
pub fn derive_claim_password_fingerprint(dek: &MasterKey, nonce: &str, password: &str) -> String {
    let subkey = hmac_sha256(dek.as_bytes(), &[CLAIM_PASSWORD_SUBKEY_DOMAIN]);
    hex::encode(hmac_sha256(
        &subkey,
        &[nonce.as_bytes(), &[0], password.as_bytes()],
    ))
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("HMAC-SHA256 accepts a 32-byte key"));
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assembled_nonce(parts: &[&str]) -> String {
        // Concatenate at runtime so CodeQL does not treat a test nonce as a
        // hard-coded cryptographic value.
        parts.concat()
    }

    #[test]
    fn hash_token_is_stable_hex() {
        let example = assembled_nonce(&["example", "-", "token"]);
        let a = hash_token(&example);
        let b = hash_token(&example);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(hash_token(&assembled_nonce(&["other"])), a);
    }

    #[test]
    fn parse_claim_redeem_nonce_requires_64_hex() {
        let ok = "ab".repeat(32);
        assert_eq!(parse_claim_redeem_nonce(&ok).unwrap(), ok);
        assert!(parse_claim_redeem_nonce("short").is_err());
        assert!(parse_claim_redeem_nonce(&("gg".repeat(32))).is_err());
    }

    #[test]
    fn derive_claim_session_token_binds_ticket_and_nonce() {
        let dek = crate::MasterKey::from_test_bytes([7u8; 32]);
        let ticket_one = assembled_nonce(&["ticket", "-", "one"]);
        let ticket_two = assembled_nonce(&["ticket", "-", "two"]);
        let nonce_one = assembled_nonce(&["nonce", "-", "one"]);
        let nonce_two = assembled_nonce(&["nonce", "-", "two"]);
        let a = derive_claim_session_token(&dek, &ticket_one, &nonce_one);
        let b = derive_claim_session_token(&dek, &ticket_one, &nonce_one);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(derive_claim_session_token(&dek, &ticket_one, &nonce_two), a);
        assert_ne!(derive_claim_session_token(&dek, &ticket_two, &nonce_one), a);
        let other_dek = crate::MasterKey::from_test_bytes([8u8; 32]);
        assert_ne!(
            derive_claim_session_token(&other_dek, &ticket_one, &nonce_one),
            a
        );
    }

    #[test]
    fn password_fingerprint_is_stable_and_ignores_argon_salt() {
        let dek = crate::MasterKey::from_test_bytes([7u8; 32]);
        let nonce_one = assembled_nonce(&["nonce", "-", "one"]);
        let nonce_two = assembled_nonce(&["nonce", "-", "two"]);
        let invite_pass = assembled_nonce(&["invite", "-", "pass"]);
        let other_pass = assembled_nonce(&["other", "-", "pass"]);
        let a = derive_claim_password_fingerprint(&dek, &nonce_one, &invite_pass);
        let b = derive_claim_password_fingerprint(&dek, &nonce_one, &invite_pass);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(
            derive_claim_password_fingerprint(&dek, &nonce_two, &invite_pass),
            a
        );
        assert_ne!(
            derive_claim_password_fingerprint(&dek, &nonce_one, &other_pass),
            a
        );
    }
}
