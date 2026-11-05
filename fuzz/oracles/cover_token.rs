//! Shared oracle for the `cover_token` fuzz target and the `bookclerk-cover`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The cover route opens a token from the request path into the remote URL
//! it may fetch. The invariant: a token opens only when it is exactly what
//! the node would have issued for that URL. Each opened URL is re-signed with
//! an independent HMAC-SHA256 (straight from the `hmac` crate, not
//! `bookclerk_cover::token`) and must reproduce the token byte for byte, so
//! no other spelling, shortened tag or foreign key authorizes a URL; the URL
//! is an `https` URL with a host; and a token signed under one key never
//! opens under another. The input is read both as a token and, from its
//! second byte on, as a URL to round-trip.

use base64::Engine as _;
use bookclerk_cover::token::{open, sign, TokenKey, MAC_BYTES, MAX_URL_BYTES};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 8 * 1024;

/// The key every input is checked against.
const KEY: [u8; 32] = [0x5a; 32];

/// A second key that must never open the first key's tokens.
const OTHER_KEY: [u8; 32] = [0xa5; 32];

/// The token for `url` under `key`, computed without the crate under test.
fn independent_token(key: &[u8; 32], url: &str) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("hmac key");
    mac.update(url.as_bytes());
    let tag = mac.finalize().into_bytes();
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}",
        engine.encode(url),
        engine.encode(&tag[..MAC_BYTES])
    )
}

/// Runs the oracle on one input.
///
/// # Panics
///
/// When a token opens that the key would not have issued, or a signed URL
/// does not round-trip.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let key = TokenKey::from_bytes(KEY);
    let other = TokenKey::from_bytes(OTHER_KEY);
    if let Ok(token) = std::str::from_utf8(data) {
        if let Some(url) = open(&key, token) {
            assert_eq!(
                independent_token(&KEY, &url),
                token,
                "non-canonical token opened"
            );
            assert!(url.len() <= MAX_URL_BYTES);
            let parsed = url::Url::parse(&url).expect("opened URL parses");
            assert_eq!(parsed.scheme(), "https");
            assert!(parsed.host_str().is_some());
        }
    }
    if let Some(url) = data
        .get(1..)
        .and_then(|rest| std::str::from_utf8(rest).ok())
    {
        if let Some(token) = sign(&key, url) {
            assert_eq!(token, independent_token(&KEY, url));
            assert_eq!(open(&key, &token).as_deref(), Some(url));
            assert_eq!(
                open(&other, &token),
                None,
                "a token opened under another key"
            );
            // Any one-byte change to an issued token is refused or names
            // the same URL under its own valid token.
            let mut bytes = token.into_bytes();
            let at = usize::from(data[0]) % bytes.len();
            bytes[at] ^= 0x01;
            if let Ok(mutated) = String::from_utf8(bytes) {
                if let Some(opened) = open(&key, &mutated) {
                    assert_eq!(independent_token(&KEY, &opened), mutated);
                }
            }
        }
    }
}
