//! Signed cover tokens: the remote URL of a cover plus a truncated
//! HMAC-SHA256 under a per-node key, so the cover route only fetches URLs
//! the API itself handed out.
//!
//! A token is `<base64url(url)>.<base64url(mac)>`, both unpadded, where `mac`
//! is the first [`MAC_BYTES`] bytes of HMAC-SHA256(key, url). [`open`] is the
//! parser for tokens arriving in request paths; it is a function over a
//! string so the `cover_token` fuzz target drives exactly what the daemon
//! runs.

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Longest cover URL that gets a token, in bytes.
pub const MAX_URL_BYTES: usize = 2048;

/// Bytes of the HMAC-SHA256 tag a token carries (128 bits).
pub const MAC_BYTES: usize = 16;

/// HMAC-SHA256, the token MAC.
type TokenMac = Hmac<Sha256>;

/// Base64url without padding; decoding rejects non-canonical trailing bits,
/// so a token has one spelling.
const ENGINE: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A node's secret for signing cover tokens; never printed.
#[derive(Clone)]
pub struct TokenKey([u8; 32]);

impl TokenKey {
    /// A key from its 32 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The key's bytes, for storing it.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The truncated tag for `url`.
    fn tag(&self, url: &str) -> [u8; MAC_BYTES] {
        let mut mac = self.mac();
        mac.update(url.as_bytes());
        let tag = mac.finalize().into_bytes();
        let mut out = [0; MAC_BYTES];
        out.copy_from_slice(&tag[..MAC_BYTES]);
        out
    }

    /// A fresh MAC under this key.
    fn mac(&self) -> TokenMac {
        #[allow(clippy::expect_used)] // HMAC accepts a key of any length.
        TokenMac::new_from_slice(&self.0).expect("hmac key")
    }
}

impl std::fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenKey(..)")
    }
}

/// Whether `url` may get a token: an absolute `https` URL with a host, at
/// most [`MAX_URL_BYTES`] long.
#[must_use]
pub fn is_coverable(url: &str) -> bool {
    url.len() <= MAX_URL_BYTES
        && url::Url::parse(url)
            .is_ok_and(|parsed| parsed.scheme() == "https" && parsed.host_str().is_some())
}

/// Hex digits in a Gravatar hash: the SHA-256 of an email address.
pub const GRAVATAR_HASH_LEN: usize = 64;

/// The Gravatar address the avatar route may fetch for a hash taken from a
/// request path.
///
/// `d=404` makes Gravatar answer 404 for an email with no picture, and
/// `s=360` asks for enough pixels for the 128 px copy.
///
/// # Returns
///
/// `None` unless `hash` is exactly [`GRAVATAR_HASH_LEN`] lowercase hex
/// digits, so the URL is always on `www.gravatar.com` with nothing else in
/// it.
#[must_use]
pub fn gravatar_url(hash: &str) -> Option<String> {
    let valid = hash.len() == GRAVATAR_HASH_LEN
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid.then(|| format!("https://www.gravatar.com/avatar/{hash}?d=404&s=360"))
}

/// The token for `url`.
///
/// # Returns
///
/// `None` when `url` is not [coverable](is_coverable).
#[must_use]
pub fn sign(key: &TokenKey, url: &str) -> Option<String> {
    if !is_coverable(url) {
        return None;
    }
    Some(format!(
        "{}.{}",
        ENGINE.encode(url),
        ENGINE.encode(key.tag(url))
    ))
}

/// The URL a token names, when `key` signed it.
///
/// # Returns
///
/// `None` for a malformed token, a wrong or shortened tag, or a URL that is
/// not [coverable](is_coverable).
#[must_use]
pub fn open(key: &TokenKey, token: &str) -> Option<String> {
    let (encoded, tag) = token.split_once('.')?;
    if encoded.len() > MAX_URL_BYTES.div_ceil(3) * 4 {
        return None;
    }
    let url = String::from_utf8(ENGINE.decode(encoded).ok()?).ok()?;
    let tag = ENGINE.decode(tag).ok()?;
    if tag.len() != MAC_BYTES {
        return None;
    }
    let mut expected = key.mac();
    expected.update(url.as_bytes());
    // Constant-time comparison of the leftmost MAC_BYTES bytes.
    expected.verify_truncated_left(&tag).ok()?;
    is_coverable(&url).then_some(url)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A distinct test key per `n`, derived at runtime so static analysis
    /// does not read it as a hard-coded secret.
    fn key_bytes(n: u8) -> [u8; 32] {
        use sha2::Digest as _;
        Sha256::digest(format!("token test key {n}")).into()
    }

    fn key(n: u8) -> TokenKey {
        TokenKey::from_bytes(key_bytes(n))
    }

    #[test]
    fn tokens_round_trip_and_refuse_tampering() {
        let url = "https://img.example/a.jpg?x=1";
        let token = sign(&key(1), url).unwrap();
        assert_eq!(open(&key(1), &token).as_deref(), Some(url));
        assert_eq!(open(&key(2), &token), None);
        let (encoded, tag) = token.split_once('.').unwrap();
        assert_eq!(open(&key(1), encoded), None);
        assert_eq!(open(&key(1), &format!("{encoded}.{}", &tag[..4])), None);
        let other = ENGINE.encode("https://evil.example/a.jpg");
        assert_eq!(open(&key(1), &format!("{other}.{tag}")), None);
        assert_eq!(open(&key(1), &format!("{encoded}.{tag}A")), None);
    }

    #[test]
    fn only_https_urls_with_a_host_are_signed() {
        assert!(sign(&key(1), "http://img.example/a.jpg").is_none());
        assert!(sign(&key(1), "file:///etc/passwd").is_none());
        let long = format!("https://img.example/{}", "a".repeat(MAX_URL_BYTES));
        assert!(sign(&key(1), &long).is_none());
        // A validly tagged URL that is not coverable is still refused.
        let url = "http://img.example/a.jpg";
        let forged = format!("{}.{}", ENGINE.encode(url), ENGINE.encode(key(1).tag(url)));
        assert_eq!(open(&key(1), &forged), None);
        assert_eq!(format!("{:?}", key(1)), "TokenKey(..)");
        assert_eq!(key(3).clone().as_bytes(), &key_bytes(3));
    }
}
