//! Checks on the public keys a host enrolls.
//!
//! Both keys must be canonical encodings of points in the prime-order
//! subgroup of Curve25519. That rejects small-order and mixed-order points
//! (which make signatures or key agreement degenerate) and non-canonical
//! encodings (which let one key have two spellings).

use curve25519_dalek::edwards::CompressedEdwardsY;
use curve25519_dalek::montgomery::MontgomeryPoint;
use ed25519_dalek::VerifyingKey;

/// Length of either public key.
pub const PUBLIC_KEY_LEN: usize = 32;

/// Why a public key was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    /// Not 32 bytes.
    #[error("public key must be {PUBLIC_KEY_LEN} bytes")]
    Length,
    /// Not a point on the curve.
    #[error("public key is not a curve point")]
    NotOnCurve,
    /// A non-canonical encoding of a point.
    #[error("public key is not canonically encoded")]
    NonCanonical,
    /// A small-order or mixed-order point.
    #[error("public key is not in the prime-order subgroup")]
    WeakPoint,
}

/// Validates an Ed25519 public signing key.
///
/// # Errors
///
/// Returns [`KeyError`] when the key is the wrong length, not a canonical
/// curve point, or not in the prime-order subgroup.
pub fn validate_sign_public_key(raw: &[u8]) -> Result<VerifyingKey, KeyError> {
    let bytes: [u8; PUBLIC_KEY_LEN] = raw.try_into().map_err(|_| KeyError::Length)?;
    let point = CompressedEdwardsY(bytes)
        .decompress()
        .ok_or(KeyError::NotOnCurve)?;
    if point.compress().to_bytes() != bytes {
        return Err(KeyError::NonCanonical);
    }
    if point.is_small_order() || !point.is_torsion_free() {
        return Err(KeyError::WeakPoint);
    }
    VerifyingKey::from_bytes(&bytes).map_err(|_| KeyError::NotOnCurve)
}

/// Validates an X25519 public box key (a Montgomery u-coordinate).
///
/// # Errors
///
/// Returns [`KeyError`] when the key is the wrong length, on the twist,
/// not canonical, or not in the prime-order subgroup.
pub fn validate_box_public_key(raw: &[u8]) -> Result<(), KeyError> {
    let bytes: [u8; PUBLIC_KEY_LEN] = raw.try_into().map_err(|_| KeyError::Length)?;
    let point = MontgomeryPoint(bytes)
        .to_edwards(0)
        .ok_or(KeyError::NotOnCurve)?;
    if point.to_montgomery().to_bytes() != bytes {
        return Err(KeyError::NonCanonical);
    }
    if point.is_small_order() || !point.is_torsion_free() {
        return Err(KeyError::WeakPoint);
    }
    Ok(())
}

/// The Ed25519 public key of a 32-byte signing seed.
#[must_use]
pub fn sign_public_key(seed: &[u8; PUBLIC_KEY_LEN]) -> [u8; PUBLIC_KEY_LEN] {
    ed25519_dalek::SigningKey::from_bytes(seed)
        .verifying_key()
        .to_bytes()
}

/// The X25519 public key of a 32-byte secret (clamped per RFC 7748).
#[must_use]
pub fn box_public_key(secret: &[u8; PUBLIC_KEY_LEN]) -> [u8; PUBLIC_KEY_LEN] {
    curve25519_dalek::constants::X25519_BASEPOINT
        .mul_clamped(*secret)
        .to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use curve25519_dalek::constants::{ED25519_BASEPOINT_POINT, EIGHT_TORSION, X25519_BASEPOINT};
    use curve25519_dalek::scalar::Scalar;
    use ed25519_dalek::SigningKey;

    #[test]
    fn honest_keys_pass() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let public = signing.verifying_key().to_bytes();
        assert_eq!(
            validate_sign_public_key(&public).unwrap().to_bytes(),
            public
        );
        assert_eq!(sign_public_key(&[7u8; 32]), public);
        // A clamped X25519 public key is a prime-order point.
        let box_pk = X25519_BASEPOINT.mul_clamped([9u8; 32]);
        assert_eq!(box_public_key(&[9u8; 32]), box_pk.to_bytes());
        assert_eq!(validate_box_public_key(box_pk.as_bytes()), Ok(()));
    }

    #[test]
    fn weak_and_malformed_keys_fail() {
        assert_eq!(validate_sign_public_key(&[1; 31]), Err(KeyError::Length));
        assert_eq!(validate_box_public_key(&[1; 33]), Err(KeyError::Length));
        // The identity and every other small-order point.
        for torsion in EIGHT_TORSION {
            let bytes = torsion.compress().to_bytes();
            assert_eq!(validate_sign_public_key(&bytes), Err(KeyError::WeakPoint));
        }
        // A prime-order point plus a torsion component.
        let mixed = ED25519_BASEPOINT_POINT * Scalar::from(5u8) + EIGHT_TORSION[1];
        assert_eq!(
            validate_sign_public_key(&mixed.compress().to_bytes()),
            Err(KeyError::WeakPoint)
        );
        // y = p (non-canonical spelling of y = 0).
        let mut non_canonical = [0xffu8; 32];
        non_canonical[0] = 0xed;
        non_canonical[31] = 0x7f;
        assert!(validate_sign_public_key(&non_canonical).is_err());
        // u = 0 is small order; u = 2 is on the twist.
        assert!(validate_box_public_key(&[0; 32]).is_err());
        let mut two = [0u8; 32];
        two[0] = 2;
        assert_eq!(validate_box_public_key(&two), Err(KeyError::NotOnCurve));
    }
}
