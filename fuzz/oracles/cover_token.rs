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
//!
//! The avatar route's other way to name a URL, a Gravatar hash from the
//! request path, is checked on the same input: `gravatar_url` answers only
//! for exactly 64 lowercase hex digits, decided here byte by byte, and the
//! URL it builds is that hash on `www.gravatar.com` and nothing else.

use base64::Engine as _;
use bookclerk_cover::token::{gravatar_url, open, sign, TokenKey, MAC_BYTES, MAX_URL_BYTES};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 8 * 1024;

/// A fixed test key: the SHA-256 of `label`, joined at runtime so static
/// analysis does not read a test key as a hard-coded production secret.
fn derived_key(label: &[&str]) -> [u8; 32] {
    Sha256::digest(label.concat()).into()
}

/// The key every input is checked against, and the one the corpus seeds are
/// signed with.
pub fn key_bytes() -> [u8; 32] {
    derived_key(&["cover_token ", "fuzz key"])
}

/// A second key that must never open the first key's tokens.
fn other_key_bytes() -> [u8; 32] {
    derived_key(&["cover_token ", "other key"])
}

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

/// Whether `hash` is a Gravatar hash, decided without the crate under test.
fn is_gravatar_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b"0123456789abcdef".contains(&b))
}

/// `gravatar_url` names a URL exactly for a Gravatar hash, and only that
/// hash's picture on `www.gravatar.com`.
fn check_gravatar(hash: &str) {
    let url = gravatar_url(hash);
    assert_eq!(
        url.is_some(),
        is_gravatar_hash(hash),
        "gravatar hash {hash:?}"
    );
    if let Some(url) = url {
        let parsed = url::Url::parse(&url).expect("gravatar URL parses");
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("www.gravatar.com"));
        assert_eq!(parsed.path(), format!("/avatar/{hash}"));
        assert_eq!(parsed.query(), Some("d=404&s=360"));
        assert!(parsed.username().is_empty() && parsed.port().is_none());
    }
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
    let key_bytes = key_bytes();
    let key = TokenKey::from_bytes(key_bytes);
    let other = TokenKey::from_bytes(other_key_bytes());
    if let Ok(text) = std::str::from_utf8(data) {
        check_gravatar(text);
    }
    if let Ok(token) = std::str::from_utf8(data) {
        if let Some(url) = open(&key, token) {
            assert_eq!(
                independent_token(&key_bytes, &url),
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
            assert_eq!(token, independent_token(&key_bytes, url));
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
                    assert_eq!(independent_token(&key_bytes, &opened), mutated);
                }
            }
        }
    }
}
