//! Apply a title's cipher to sample payloads while the shared remuxer copies them.
//!
//! [`bookclerk_mp4`] moves the bytes and rebuilds the tables; the key and the
//! cipher stay here, in the plugin process. Nothing in this file is reachable
//! from the host binaries.

use bookclerk_mp4::{Mp4Error, SampleTransform};

use super::crypto::{decrypt_aavd_sample_in_place, decrypt_cenc_sample_in_place};

/// The cipher a downloaded title's samples are under.
#[derive(Debug, Clone)]
pub enum SampleCipher {
    /// Audible Adrm AES-128-CBC, one IV for the whole track (from the voucher).
    Adrm { key: [u8; 16], iv: [u8; 16] },
    /// CENC AES-CTR, one IV for the whole track (`tenc` constant_IV).
    CencConstantIv { key: [u8; 16], iv: [u8; 16] },
    /// CENC AES-CTR, one IV per sample in full track order (`saiz`/`saio`).
    CencSampleIvs { key: [u8; 16], ivs: Vec<[u8; 16]> },
    /// Already clear — a plain `mp4a` track that only needs remuxing.
    Clear,
}

/// Decrypts payloads in place as [`bookclerk_mp4::remux_progressive`] streams them.
#[derive(Debug)]
pub struct Decryptor {
    cipher: SampleCipher,
}

impl Decryptor {
    #[must_use]
    pub fn new(cipher: SampleCipher) -> Self {
        Self { cipher }
    }
}

impl SampleTransform for Decryptor {
    /// Narrow a per-sample IV table to the samples that survived the trim, so
    /// that afterwards an IV can be found by output position.
    fn retain(&mut self, kept: &[usize]) -> bookclerk_mp4::Result<()> {
        let SampleCipher::CencSampleIvs { ivs, .. } = &mut self.cipher else {
            return Ok(());
        };
        if let Some(&missing) = kept.iter().find(|&&index| index >= ivs.len()) {
            return Err(Mp4Error::transform(format!(
                "CENC IV table has {} entries but the track has a sample {missing}",
                ivs.len()
            )));
        }
        *ivs = kept.iter().map(|&index| ivs[index]).collect();
        Ok(())
    }

    fn sample(&mut self, index: usize, payload: &mut [u8]) -> bookclerk_mp4::Result<()> {
        match &self.cipher {
            SampleCipher::Adrm { key, iv } => decrypt_aavd_sample_in_place(key, iv, payload),
            SampleCipher::CencConstantIv { key, iv } => {
                decrypt_cenc_sample_in_place(key, iv, payload);
            }
            SampleCipher::CencSampleIvs { key, ivs } => {
                let iv = ivs
                    .get(index)
                    .ok_or_else(|| Mp4Error::transform(format!("no CENC IV for sample {index}")))?;
                decrypt_cenc_sample_in_place(key, iv, payload);
            }
            SampleCipher::Clear => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivs(count: usize) -> Vec<[u8; 16]> {
        (0..count).map(|i| [i as u8; 16]).collect()
    }

    #[test]
    fn a_trim_leaves_each_sample_with_its_own_iv() {
        let mut decryptor = Decryptor::new(SampleCipher::CencSampleIvs {
            key: [7u8; 16],
            ivs: ivs(10),
        });
        decryptor.retain(&[3, 4, 5]).unwrap();
        let SampleCipher::CencSampleIvs { ivs, .. } = &decryptor.cipher else {
            unreachable!()
        };
        assert_eq!(ivs, &[[3u8; 16], [4u8; 16], [5u8; 16]]);
    }

    #[test]
    fn a_short_iv_table_is_refused_before_any_payload_is_written() {
        let mut decryptor = Decryptor::new(SampleCipher::CencSampleIvs {
            key: [7u8; 16],
            ivs: ivs(2),
        });
        let err = decryptor.retain(&[0, 1, 2]).unwrap_err();
        assert!(matches!(err, Mp4Error::Transform(_)), "{err}");
    }

    #[test]
    fn a_clear_track_is_copied_untouched() {
        let mut decryptor = Decryptor::new(SampleCipher::Clear);
        let mut payload = b"already plain".to_vec();
        decryptor.sample(0, &mut payload).unwrap();
        assert_eq!(payload, b"already plain");
    }

    #[test]
    fn an_adrm_sample_round_trips_through_the_transform() {
        use cbc::cipher::{block_padding::NoPadding, BlockModeEncrypt, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let plain: Vec<u8> = (0..48u8).collect();
        let mut cipher_text = plain.clone();
        Aes128CbcEnc::new(&key.into(), &iv.into())
            .encrypt_padded::<NoPadding>(&mut cipher_text, plain.len())
            .unwrap();

        let mut decryptor = Decryptor::new(SampleCipher::Adrm { key, iv });
        decryptor.sample(0, &mut cipher_text).unwrap();
        assert_eq!(cipher_text, plain);
    }
}
