//! Sealed boxes to a node's X25519 box key, and the secret bundles they
//! carry (H-4 design, section 5).
//!
//! A sealed box is anonymous public-key encryption: the owner encrypts to
//! the host's enrolled box key with a fresh ephemeral key, and only the
//! holder of the host's box secret can open it. The construction uses only
//! primitives already in the tree:
//!
//! 1. `E = X25519(e, 9)` for a fresh 32-byte ephemeral secret `e`;
//! 2. `shared = X25519(e, R)` for the recipient's public key `R` (an
//!    all-zero result is refused);
//! 3. `key ‖ nonce = HKDF-SHA256(salt = E ‖ R, ikm = shared,
//!    info = SEALED_BOX_INFO)`, 32 + 24 bytes;
//! 4. `box = E ‖ XChaCha20-Poly1305(key, nonce, plaintext, aad)`.
//!
//! The key and nonce are unique per ephemeral key, so no random nonce is
//! needed, and the caller supplies `e` so this crate stays free of
//! randomness. The associated data binds a box to its context: a secret
//! bundle is bound to the node, the job, and the lease generation it was
//! handed under ([`secrets_aad`]), so a box cannot be replayed into another
//! job or another claim.
//!
//! The other direction works the same way: the owner sends a fresh box key
//! for each connection in `welcome`, and a host seals a refreshed token it
//! writes back to that key ([`seal_secret_update`]), bound to the node,
//! job, fence and secret ref. So no secret is ever frame text, and a
//! WebSocket library that traces frames only ever sees ciphertext.
//!
//! Secret values are [`SecretValue`]s: zeroized on drop and redacted in
//! `Debug`, so a frame or bundle that is logged never shows them.

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use curve25519_dalek::constants::X25519_BASEPOINT;
use curve25519_dalek::montgomery::MontgomeryPoint;
use hkdf::Hkdf;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

use crate::keys::{validate_box_public_key, PUBLIC_KEY_LEN};

/// HKDF info string of a sealed box.
pub const SEALED_BOX_INFO: &[u8] = b"bookclerk-node.v1 sealed-box";

/// Domain tag that starts the associated data of a secret bundle.
pub const SECRETS_AAD_DOMAIN: &[u8] = b"bookclerk-node.v1 job-secrets";

/// Domain tag that starts the associated data of a token write-back.
pub const SECRET_UPDATE_AAD_DOMAIN: &[u8] = b"bookclerk-node.v1 secret-update";

/// Bytes a sealed box adds to its plaintext (ephemeral key and tag).
pub const SEALED_BOX_OVERHEAD: usize = PUBLIC_KEY_LEN + 16;

/// Length of the derived XChaCha20-Poly1305 key.
const KEY_LEN: usize = 32;

/// Length of the derived XChaCha20-Poly1305 nonce.
const NONCE_LEN: usize = 24;

/// Why a sealed box could not be made or opened.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SealError {
    /// The recipient key is not a valid box key.
    #[error("invalid recipient box key")]
    Key,
    /// Key agreement produced the all-zero point.
    #[error("degenerate key agreement")]
    Degenerate,
    /// The box is too short, not base64url, or does not authenticate.
    #[error("sealed box does not open")]
    Open,
    /// The opened bundle is not a JSON list of secrets.
    #[error("malformed secret bundle")]
    Bundle,
}

/// Derives the AEAD key and nonce for one box.
fn derive(
    shared: &MontgomeryPoint,
    ephemeral: &[u8; PUBLIC_KEY_LEN],
    recipient: &[u8; PUBLIC_KEY_LEN],
) -> Result<Zeroizing<[u8; KEY_LEN + NONCE_LEN]>, SealError> {
    let shared = Zeroizing::new(shared.to_bytes());
    if shared.iter().all(|byte| *byte == 0) {
        return Err(SealError::Degenerate);
    }
    let mut salt = [0u8; 2 * PUBLIC_KEY_LEN];
    salt[..PUBLIC_KEY_LEN].copy_from_slice(ephemeral);
    salt[PUBLIC_KEY_LEN..].copy_from_slice(recipient);
    let mut okm = Zeroizing::new([0u8; KEY_LEN + NONCE_LEN]);
    Hkdf::<Sha256>::new(Some(&salt), shared.as_slice())
        .expand(SEALED_BOX_INFO, okm.as_mut_slice())
        .map_err(|_| SealError::Degenerate)?;
    Ok(okm)
}

/// The AEAD cipher and nonce for derived key material.
fn cipher(okm: &[u8; KEY_LEN + NONCE_LEN]) -> Result<(XChaCha20Poly1305, XNonce), SealError> {
    let cipher = XChaCha20Poly1305::new_from_slice(&okm[..KEY_LEN]).map_err(|_| SealError::Key)?;
    let nonce = XNonce::try_from(&okm[KEY_LEN..]).map_err(|_| SealError::Key)?;
    Ok((cipher, nonce))
}

/// Seals `plaintext` to `recipient` (an X25519 public key) with the
/// ephemeral secret `ephemeral_secret`, binding `aad`.
///
/// `ephemeral_secret` must be fresh random bytes for every box; reusing it
/// for two plaintexts to one recipient reuses the key and nonce.
///
/// # Errors
///
/// Returns [`SealError::Key`] for an invalid recipient key and
/// [`SealError::Degenerate`] when key agreement fails.
pub fn seal(
    recipient: &[u8],
    ephemeral_secret: &[u8; PUBLIC_KEY_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, SealError> {
    validate_box_public_key(recipient).map_err(|_| SealError::Key)?;
    let recipient: [u8; PUBLIC_KEY_LEN] = recipient.try_into().map_err(|_| SealError::Key)?;
    let ephemeral = X25519_BASEPOINT.mul_clamped(*ephemeral_secret).to_bytes();
    let shared = MontgomeryPoint(recipient).mul_clamped(*ephemeral_secret);
    let okm = derive(&shared, &ephemeral, &recipient)?;
    let (cipher, nonce) = cipher(&okm)?;
    let sealed = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| SealError::Degenerate)?;
    let mut out = Vec::with_capacity(PUBLIC_KEY_LEN + sealed.len());
    out.extend_from_slice(&ephemeral);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Opens a box sealed to the public key of `recipient_secret`.
///
/// # Errors
///
/// Returns [`SealError::Open`] when the box is short, was sealed to another
/// key or other associated data, or was altered.
pub fn open(
    recipient_secret: &[u8; PUBLIC_KEY_LEN],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, SealError> {
    if sealed.len() < SEALED_BOX_OVERHEAD {
        return Err(SealError::Open);
    }
    let (ephemeral, body) = sealed.split_at(PUBLIC_KEY_LEN);
    let ephemeral: [u8; PUBLIC_KEY_LEN] = ephemeral.try_into().map_err(|_| SealError::Open)?;
    let recipient = X25519_BASEPOINT.mul_clamped(*recipient_secret).to_bytes();
    let shared = MontgomeryPoint(ephemeral).mul_clamped(*recipient_secret);
    let okm = derive(&shared, &ephemeral, &recipient).map_err(|_| SealError::Open)?;
    let (cipher, nonce) = cipher(&okm)?;
    cipher
        .decrypt(&nonce, Payload { msg: body, aad })
        .map(Zeroizing::new)
        .map_err(|_| SealError::Open)
}

/// Names one `encrypted_secrets` row of a store account. It carries no
/// secret material.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    /// Secret kind (`source_auth`, `widevine`, ...).
    pub kind: String,
    /// Plugin key that owns the secret.
    pub provider: String,
    /// Store account id.
    pub account_id: String,
    /// Logical secret name within that account.
    pub name: String,
}

/// Secret bytes: zeroized on drop, redacted in `Debug`, and base64url on
/// the wire.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(Zeroizing<Vec<u8>>);

impl SecretValue {
    /// Wraps secret bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The secret bytes.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretValue(<redacted {} bytes>)", self.0.len())
    }
}

impl Serialize for SecretValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let encoded =
            Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&*self.0));
        serializer.serialize_str(&encoded)
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut encoded = String::deserialize(deserializer)?;
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded.as_bytes());
        encoded.zeroize();
        decoded
            .map(Self::new)
            .map_err(|_| serde::de::Error::custom("secret value is not base64url"))
    }
}

/// One secret in a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretEntry {
    /// Which row the value belongs to.
    #[serde(rename = "ref")]
    pub secret_ref: SecretRef,
    /// The unsealed value.
    pub value: SecretValue,
}

/// Associated data of the secret bundle handed to `node_id` for `job_id`
/// under lease generation `fence`: the domain tag, then each field as a
/// 4-byte big-endian length and its bytes.
#[must_use]
pub fn secrets_aad(node_id: &str, job_id: &str, fence: i64) -> Vec<u8> {
    let fence = fence.to_be_bytes();
    let fields: [&[u8]; 3] = [node_id.as_bytes(), job_id.as_bytes(), &fence];
    let mut out = SECRETS_AAD_DOMAIN.to_vec();
    for field in fields {
        out.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// Seals `entries` for `node_id`'s box key `recipient`, bound to `job_id`
/// and `fence`, as base64url text for a `work` frame.
///
/// # Errors
///
/// Returns [`SealError`] when the recipient key is invalid.
pub fn seal_secrets(
    recipient: &[u8],
    ephemeral_secret: &[u8; PUBLIC_KEY_LEN],
    node_id: &str,
    job_id: &str,
    fence: i64,
    entries: &[SecretEntry],
) -> Result<String, SealError> {
    let plaintext = Zeroizing::new(serde_json::to_vec(entries).map_err(|_| SealError::Bundle)?);
    let sealed = seal(
        recipient,
        ephemeral_secret,
        &secrets_aad(node_id, job_id, fence),
        &plaintext,
    )?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed))
}

/// Opens a bundle made by [`seal_secrets`] with the node's box secret.
///
/// # Errors
///
/// Returns [`SealError::Open`] when the box does not open for this node,
/// job and fence, and [`SealError::Bundle`] when its content is malformed.
pub fn open_secrets(
    recipient_secret: &[u8; PUBLIC_KEY_LEN],
    node_id: &str,
    job_id: &str,
    fence: i64,
    sealed: &str,
) -> Result<Vec<SecretEntry>, SealError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sealed.as_bytes())
        .map_err(|_| SealError::Open)?;
    let plaintext = open(recipient_secret, &secrets_aad(node_id, job_id, fence), &raw)?;
    serde_json::from_slice(&plaintext).map_err(|_| SealError::Bundle)
}

/// Associated data of a token write-back from `node_id` for `secret_ref`,
/// handed with `job_id` under `fence`.
#[must_use]
pub fn secret_update_aad(
    node_id: &str,
    job_id: &str,
    fence: i64,
    secret_ref: &SecretRef,
) -> Vec<u8> {
    let fence = fence.to_be_bytes();
    let fields: [&[u8]; 7] = [
        node_id.as_bytes(),
        job_id.as_bytes(),
        &fence,
        secret_ref.kind.as_bytes(),
        secret_ref.provider.as_bytes(),
        secret_ref.account_id.as_bytes(),
        secret_ref.name.as_bytes(),
    ];
    let mut out = SECRET_UPDATE_AAD_DOMAIN.to_vec();
    for field in fields {
        out.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// Seals a refreshed token to the owner's per-connection box key
/// `owner_box_key` (from `welcome`), bound to the node, job, fence and
/// ref, as base64url text for a `secret.update` frame.
///
/// # Errors
///
/// Returns [`SealError`] when the owner key is invalid.
pub fn seal_secret_update(
    owner_box_key: &[u8],
    ephemeral_secret: &[u8; PUBLIC_KEY_LEN],
    node_id: &str,
    job_id: &str,
    fence: i64,
    secret_ref: &SecretRef,
    value: &[u8],
) -> Result<String, SealError> {
    let sealed = seal(
        owner_box_key,
        ephemeral_secret,
        &secret_update_aad(node_id, job_id, fence, secret_ref),
        value,
    )?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed))
}

/// Opens a write-back made by [`seal_secret_update`] with the owner's
/// per-connection box secret.
///
/// # Errors
///
/// Returns [`SealError::Open`] when it was not sealed to this key for this
/// node, job, fence and ref.
pub fn open_secret_update(
    owner_box_secret: &[u8; PUBLIC_KEY_LEN],
    node_id: &str,
    job_id: &str,
    fence: i64,
    secret_ref: &SecretRef,
    sealed: &str,
) -> Result<SecretValue, SealError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sealed.as_bytes())
        .map_err(|_| SealError::Open)?;
    let mut plaintext = open(
        owner_box_secret,
        &secret_update_aad(node_id, job_id, fence, secret_ref),
        &raw,
    )?;
    Ok(SecretValue::new(std::mem::take(&mut *plaintext)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::box_public_key;

    fn entry(value: &[u8]) -> SecretEntry {
        SecretEntry {
            secret_ref: SecretRef {
                kind: "source_auth".into(),
                provider: "store".into(),
                account_id: "acct".into(),
                name: "acct.auth".into(),
            },
            value: SecretValue::new(value.to_vec()),
        }
    }

    #[test]
    fn a_box_opens_only_for_its_recipient_and_context() {
        let secret = [5u8; 32];
        let recipient = box_public_key(&secret);
        let sealed = seal(&recipient, &[6; 32], b"ctx", b"hello").unwrap();
        assert_eq!(sealed.len(), 5 + SEALED_BOX_OVERHEAD);
        assert_eq!(open(&secret, b"ctx", &sealed).unwrap().as_slice(), b"hello");
        assert_eq!(open(&[7; 32], b"ctx", &sealed), Err(SealError::Open));
        assert_eq!(open(&secret, b"other", &sealed), Err(SealError::Open));
        let mut altered = sealed.clone();
        *altered.last_mut().unwrap() ^= 1;
        assert_eq!(open(&secret, b"ctx", &altered), Err(SealError::Open));
        assert_eq!(open(&secret, b"ctx", &sealed[..10]), Err(SealError::Open));
        // A fresh ephemeral key gives a different box for the same input.
        assert_ne!(
            seal(&recipient, &[8; 32], b"ctx", b"hello").unwrap(),
            sealed
        );
        // Weak recipient keys are refused before any key agreement.
        assert_eq!(seal(&[0; 32], &[6; 32], b"", b"x"), Err(SealError::Key));
    }

    #[test]
    fn secret_bundles_bind_node_job_and_fence() {
        let secret = [5u8; 32];
        let recipient = box_public_key(&secret);
        let entries = vec![entry(b"token-123")];
        let sealed = seal_secrets(&recipient, &[9; 32], "node", "job", 4, &entries).unwrap();
        assert!(!sealed.contains("token-123"));
        assert_eq!(
            open_secrets(&secret, "node", "job", 4, &sealed).unwrap(),
            entries
        );
        for (node, job, fence) in [("other", "job", 4), ("node", "job2", 4), ("node", "job", 5)] {
            assert_eq!(
                open_secrets(&secret, node, job, fence, &sealed),
                Err(SealError::Open)
            );
        }
    }

    #[test]
    fn secret_values_never_show_in_debug_output() {
        let entry = entry(b"hunter2-token");
        let debug = format!("{entry:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("redacted 13 bytes"), "{debug}");
        let json = serde_json::to_string(&entry).unwrap();
        let back: SecretEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.value.expose(), b"hunter2-token");
    }
}
