//! The handshake signature.

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey};

use crate::keys::validate_sign_public_key;

/// Domain tag that starts every signed transcript.
pub const AUTH_DOMAIN: &[u8] = b"bookclerk-node-auth-v1";

/// Length of a challenge nonce.
pub const NONCE_LEN: usize = 32;

/// What a handshake signature covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthContext<'a> {
    /// Owner cluster id from the challenge.
    pub cluster_id: &'a str,
    /// Node id from the hello.
    pub node_id: &'a str,
    /// Incarnation from the hello.
    pub incarnation: &'a str,
    /// Nonce from the challenge.
    pub nonce: &'a [u8; NONCE_LEN],
    /// Owner origin as [`canonical_origin`] spells it. The host uses the
    /// origin it dialed; the owner uses its own configured origin, so a
    /// signature relayed through another server does not verify.
    pub owner_origin: &'a str,
}

/// Why a handshake signature was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    /// The stored public key is invalid.
    #[error("invalid node public key")]
    Key,
    /// The signature is not base64url of 64 bytes.
    #[error("malformed signature")]
    Malformed,
    /// The signature does not verify.
    #[error("signature does not verify")]
    Mismatch,
}

/// The bytes a host signs: the domain tag, then each field as a 4-byte
/// big-endian length and its bytes.
#[must_use]
pub fn auth_transcript(ctx: &AuthContext<'_>) -> Vec<u8> {
    let fields: [&[u8]; 5] = [
        ctx.cluster_id.as_bytes(),
        ctx.node_id.as_bytes(),
        ctx.incarnation.as_bytes(),
        ctx.nonce,
        ctx.owner_origin.as_bytes(),
    ];
    let mut out = Vec::with_capacity(
        AUTH_DOMAIN.len() + fields.iter().map(|field| 4 + field.len()).sum::<usize>(),
    );
    out.extend_from_slice(AUTH_DOMAIN);
    for field in fields {
        // Fields are ids, a 32-byte nonce, and an origin: far below 4 GiB.
        let len = u32::try_from(field.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// Signs a handshake; the result is base64url without padding.
#[must_use]
pub fn sign_auth(key: &SigningKey, ctx: &AuthContext<'_>) -> String {
    let sig = key.sign(&auth_transcript(ctx));
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes())
}

/// Verifies a handshake signature with the node's enrolled public key.
/// Uses strict verification (no malleable or small-order signatures).
///
/// # Errors
///
/// Returns [`AuthError`] when the key is invalid, the signature is
/// malformed, or it does not verify.
pub fn verify_auth(public_key: &[u8], ctx: &AuthContext<'_>, sig: &str) -> Result<(), AuthError> {
    let key = validate_sign_public_key(public_key).map_err(|_| AuthError::Key)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig.trim())
        .map_err(|_| AuthError::Malformed)?;
    let bytes: [u8; 64] = raw.try_into().map_err(|_| AuthError::Malformed)?;
    key.verify_strict(&auth_transcript(ctx), &Signature::from_bytes(&bytes))
        .map_err(|_| AuthError::Mismatch)
}

/// Encodes a nonce as base64url without padding.
#[must_use]
pub fn encode_nonce(nonce: &[u8; NONCE_LEN]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce)
}

/// Decodes a challenge nonce.
#[must_use]
pub fn decode_nonce(raw: &str) -> Option<[u8; NONCE_LEN]> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.trim())
        .ok()?
        .try_into()
        .ok()
}

/// The ASCII origin (`scheme://host[:port]`) of an http(s) URL, with the
/// default port and any path dropped and the host lowercased.
#[must_use]
pub fn canonical_origin(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(nonce: &'a [u8; NONCE_LEN], origin: &'a str) -> AuthContext<'a> {
        AuthContext {
            cluster_id: "cluster",
            node_id: "node",
            incarnation: "inc",
            nonce,
            owner_origin: origin,
        }
    }

    #[test]
    fn signatures_bind_every_field() {
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let public = key.verifying_key().to_bytes();
        let nonce = [1u8; NONCE_LEN];
        let sig = sign_auth(&key, &ctx(&nonce, "https://owner.example"));
        assert_eq!(
            verify_auth(&public, &ctx(&nonce, "https://owner.example"), &sig),
            Ok(())
        );
        let other_nonce = [2u8; NONCE_LEN];
        assert_eq!(
            verify_auth(&public, &ctx(&other_nonce, "https://owner.example"), &sig),
            Err(AuthError::Mismatch)
        );
        assert_eq!(
            verify_auth(&public, &ctx(&nonce, "https://evil.example"), &sig),
            Err(AuthError::Mismatch)
        );
        let mut swapped = ctx(&nonce, "https://owner.example");
        swapped.node_id = "nodeinc";
        swapped.incarnation = "";
        assert_eq!(
            verify_auth(&public, &swapped, &sig),
            Err(AuthError::Mismatch),
            "length prefixes keep fields from sliding"
        );
        let other = SigningKey::from_bytes(&[4u8; 32])
            .verifying_key()
            .to_bytes();
        assert_eq!(
            verify_auth(&other, &ctx(&nonce, "https://owner.example"), &sig),
            Err(AuthError::Mismatch)
        );
        assert_eq!(
            verify_auth(&public, &ctx(&nonce, "https://owner.example"), "!!"),
            Err(AuthError::Malformed)
        );
        assert_eq!(
            verify_auth(&[0; 32], &ctx(&nonce, "https://owner.example"), &sig),
            Err(AuthError::Key)
        );
    }

    #[test]
    fn origins_and_nonces_normalize() {
        assert_eq!(
            canonical_origin("HTTPS://Owner.Example:443/enroll#x").as_deref(),
            Some("https://owner.example")
        );
        assert_eq!(
            canonical_origin("http://localhost:8787/").as_deref(),
            Some("http://localhost:8787")
        );
        assert_eq!(canonical_origin("ftp://owner.example"), None);
        let nonce = [5u8; NONCE_LEN];
        assert_eq!(decode_nonce(&encode_nonce(&nonce)), Some(nonce));
        assert_eq!(decode_nonce("AAAA"), None);
    }
}
