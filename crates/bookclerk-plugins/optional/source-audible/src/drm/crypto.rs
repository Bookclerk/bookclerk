//! AES helpers for Audible Adrm (CBC) and CENC (CTR) sample decryption.

use aes::Aes128;
use cbc::cipher::{block_padding::NoPadding, BlockModeDecrypt, KeyIvInit};
use ctr::cipher::StreamCipher;

use super::error::{DrmError, Result};

/// AES-128-CBC decryptor for Adrm AAC samples (no padding; trailing bytes stay clear).
type Aes128CbcDec = cbc::Decryptor<Aes128>;
/// AES-128-CTR (big-endian counter) decryptor for CENC sample decryption.
type Aes128Ctr128BE = ctr::Ctr128BE<Aes128>;

/// Decode a hex key/IV string into a fixed 16-byte array.
pub fn parse_aes128_hex(hex_str: &str) -> Result<[u8; 16]> {
    let trimmed = hex_str.trim();
    let bytes = hex::decode(trimmed).map_err(|err| {
        DrmError::InvalidKey(format!("invalid hex ({err}): expected 32 hex chars"))
    })?;
    <[u8; 16]>::try_from(bytes).map_err(|bytes| {
        DrmError::InvalidKey(format!(
            "expected 16 bytes (32 hex chars), got {}",
            bytes.len()
        ))
    })
}

/// Decrypt an Adrm AAC sample in place (AES-128-CBC, no padding).
///
/// Only whole 16-byte blocks are encrypted; the trailing `len % 16` bytes stay clear.
/// The IV is constant for every sample (from the voucher).
pub fn decrypt_aavd_sample_in_place(key: &[u8; 16], iv: &[u8; 16], sample: &mut [u8]) {
    let encrypted_len = sample.len() & !0x0f;
    if encrypted_len == 0 {
        return;
    }
    Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded::<NoPadding>(&mut sample[..encrypted_len])
        .expect("NoPadding decrypt requires an exact multiple of the AES block size");
}

/// Decrypt a CENC sample in place with AES-128-CTR (big-endian counter).
///
/// `iv` must be the full 16-byte counter block (8-byte IVs should be expanded first).
pub fn decrypt_cenc_sample_in_place(key: &[u8; 16], iv: &[u8; 16], sample: &mut [u8]) {
    if sample.is_empty() {
        return;
    }
    // KeyIvInit is in scope via cbc::cipher re-export (same cipher crate as ctr).
    let mut cipher = Aes128Ctr128BE::new(key.into(), iv.into());
    cipher.apply_keystream(sample);
}

/// Expand an 8-byte CENC IV to 16 bytes (IV || 0x00 * 8), per ISO/IEC 23001-7.
#[must_use]
pub fn expand_cenc_iv(iv8: &[u8; 8]) -> [u8; 16] {
    // ISO/IEC 23001-7: 8-byte CENC IVs are left-aligned and zero-padded to 16.
    std::array::from_fn(|i| iv8.get(i).copied().unwrap_or_else(u8::default))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::{block_padding::NoPadding, BlockModeEncrypt, KeyIvInit};
    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    #[test]
    fn parse_rejects_bad_hex() {
        assert!(parse_aes128_hex("dead").is_err());
        assert!(parse_aes128_hex("gg").is_err());
    }

    fn test_block(tag: u8) -> [u8; 16] {
        std::array::from_fn(|i| tag.wrapping_add(i as u8))
    }

    #[test]
    fn aavd_roundtrip_partial_block() {
        let key = test_block(0x11);
        let iv = test_block(0x22);
        let mut plain = vec![0u8; 40];
        for (i, b) in plain.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut enc = plain.clone();
        {
            let encrypted_len = enc.len() & !0x0f;
            Aes128CbcEnc::new(&key.into(), &iv.into())
                .encrypt_padded::<NoPadding>(&mut enc[..encrypted_len], encrypted_len)
                .unwrap();
        }
        assert_ne!(&enc[..32], &plain[..32]);
        assert_eq!(&enc[32..], &plain[32..]);
        decrypt_aavd_sample_in_place(&key, &iv, &mut enc);
        assert_eq!(enc, plain);
    }

    #[test]
    fn cenc_roundtrip() {
        let key = test_block(0x33);
        let iv = test_block(0x44);
        let mut data = b"hello cenc audio!!".to_vec();
        let original = data.clone();
        decrypt_cenc_sample_in_place(&key, &iv, &mut data);
        assert_ne!(data, original);
        decrypt_cenc_sample_in_place(&key, &iv, &mut data);
        assert_eq!(data, original);
    }
}
