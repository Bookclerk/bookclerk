//! Native Rust Adrm / CENC decrypt entry points (no aaxclean-cli).

use std::fs::File;
use std::path::Path;

use bookclerk_mp4::{parse_mp4, remux_progressive, RemuxOptions, SampleEntryKind, TrimRange};

use super::crypto::parse_aes128_hex;
use super::decrypt::{Decryptor, SampleCipher};
use super::error::{DrmError, Result};
use super::mp4::{decrypt_dash_cenc, looks_like_dash};
use super::mp4::{
    find_stbl_in_trak, parse_tenc_from_enca_entry, progressive_sample_ivs,
    sample_entry_end_from_type_offset,
};
use super::DecryptOutcome;

/// Decrypt into a DRM-free faststart M4B, trimming the requested window as the
/// samples stream past.
fn decrypt_to_m4b(
    input: &Path,
    output: &Path,
    cipher: SampleCipher,
    trim: Option<TrimRange>,
) -> Result<()> {
    let mut decryptor = Decryptor::new(cipher);
    remux_progressive(input, output, &RemuxOptions { trim }, &mut decryptor)?;
    Ok(())
}

/// Native Adrm aaxc decrypt (+ optional brand trim) to a DRM-free M4B.
pub fn decrypt_adrm_native(
    input: &Path,
    output: &Path,
    audible_key_hex: &str,
    audible_iv_hex: &str,
    trim: Option<TrimRange>,
) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DrmError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let key = parse_aes128_hex(audible_key_hex)?;
    let iv = parse_aes128_hex(audible_iv_hex)?;

    // Validate the input looks like an Adrm / aaxc file when possible.
    match parse_mp4(input) {
        Ok(mp4) => match mp4.audio.sample_entry_kind {
            SampleEntryKind::Aavd | SampleEntryKind::Mp4a => {}
            SampleEntryKind::Enca => {
                return Err(DrmError::Native(
                    "file looks like CENC (enca); use decrypt_cenc_native / decrypt_cenc".into(),
                ));
            }
            SampleEntryKind::Other(kind) => {
                tracing::warn!(
                    sample_entry = %kind,
                    "unexpected audio sample entry; attempting Adrm decrypt anyway"
                );
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "MP4 pre-parse failed; attempting Adrm decrypt anyway");
        }
    }

    tracing::info!(
        input = %input.display(),
        output = %output.display(),
        trim = trim.is_some(),
        "native Adrm aaxc decrypt"
    );

    decrypt_to_m4b(input, output, SampleCipher::Adrm { key, iv }, trim)?;

    if !output.exists() {
        return Err(DrmError::OutputMissing(output.to_path_buf()));
    }
    Ok(DecryptOutcome {
        output: output.to_path_buf(),
    })
}

/// Native CENC decrypt for Audible Widevine media (and compatible CENC files).
///
/// Audible’s acquire path downloads a DASH **fragmented** CENC MP4 (`moof`/`senc`).
/// Progressive `enca` (constant_IV / `saiz`+`saio`) is supported as a general CENC
/// fallback when the input is not fragmented DASH.
pub fn decrypt_cenc_native(
    input: &Path,
    output: &Path,
    kid_hex: &str,
    key_hex: &str,
    trim: Option<TrimRange>,
) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DrmError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if looks_like_dash(input).unwrap_or(false) {
        return decrypt_dash_cenc(input, output, kid_hex, key_hex, trim);
    }

    let mp4 = parse_mp4(input).map_err(|err| {
        DrmError::Native(format!(
            "native CENC requires a progressive MP4 or DASH fragment ({err})"
        ))
    })?;

    if !matches!(
        mp4.audio.sample_entry_kind,
        SampleEntryKind::Enca | SampleEntryKind::Mp4a
    ) {
        return Err(DrmError::Native(format!(
            "unsupported CENC sample entry {:?}",
            mp4.audio.sample_entry_kind
        )));
    }

    if matches!(mp4.audio.sample_entry_kind, SampleEntryKind::Mp4a) {
        // Nothing to decrypt, but the key still has to parse: a bad key here
        // means the caller's voucher is wrong, whatever this file turned out
        // to be.
        let _key = parse_aes128_hex(key_hex)?;
        decrypt_to_m4b(input, output, SampleCipher::Clear, trim)?;
        return finish_cenc_output(output);
    }

    let key = parse_aes128_hex(key_hex)?;
    let want_kid = parse_aes128_hex(kid_hex)?;

    let mut file = File::open(input)?;
    let entry_end =
        sample_entry_end_from_type_offset(&mut file, mp4.audio.sample_entry_type_offset)?;
    let tenc =
        parse_tenc_from_enca_entry(&mut file, mp4.audio.sample_entry_type_offset, entry_end)?;
    if tenc.kid != want_kid {
        return Err(DrmError::Native(format!(
            "progressive enca tenc KID {} does not match requested {}",
            hex::encode(tenc.kid),
            kid_hex.to_ascii_lowercase()
        )));
    }

    let stbl = find_stbl_in_trak(&mut file, &mp4.audio.trak)?;
    let ivs = progressive_sample_ivs(&mut file, &stbl, &tenc, mp4.audio.samples.len())?;
    drop(file);

    tracing::info!(
        input = %input.display(),
        output = %output.display(),
        samples = mp4.audio.samples.len(),
        per_sample_iv_size = tenc.per_sample_iv_size,
        trim = trim.is_some(),
        "native progressive enca CENC decrypt"
    );

    let cipher = if tenc.per_sample_iv_size == 0 {
        let iv = ivs.first().copied().ok_or_else(|| {
            DrmError::Mp4("progressive enca constant_IV produced no sample IVs".into())
        })?;
        SampleCipher::CencConstantIv { key, iv }
    } else {
        SampleCipher::CencSampleIvs { key, ivs }
    };
    decrypt_to_m4b(input, output, cipher, trim)?;

    finish_cenc_output(output)
}

fn finish_cenc_output(output: &Path) -> Result<DecryptOutcome> {
    if !output.exists() {
        return Err(DrmError::OutputMissing(output.to_path_buf()));
    }
    Ok(DecryptOutcome {
        output: output.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use bookclerk_mp4::fixture::ProgressiveFixture;

    use super::*;

    const KEY: [u8; 16] = [0x3cu8; 16];
    const IV: [u8; 16] = [0x9au8; 16];

    /// Encrypt like an aaxc: AES-128-CBC over whole blocks, tail left clear.
    fn encrypt_adrm(plain: &[u8]) -> Vec<u8> {
        use cbc::cipher::{block_padding::NoPadding, BlockModeEncrypt, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

        let mut out = plain.to_vec();
        let whole = out.len() & !0x0f;
        Aes128CbcEnc::new(&KEY.into(), &IV.into())
            .encrypt_padded::<NoPadding>(&mut out[..whole], whole)
            .expect("whole blocks");
        out
    }

    fn payloads(path: &Path) -> Vec<Vec<u8>> {
        let mp4 = parse_mp4(path).expect("parse output");
        let mut file = File::open(path).expect("open output");
        mp4.audio
            .samples
            .iter()
            .map(|sample| {
                let mut buf = vec![0u8; sample.size as usize];
                file.seek(SeekFrom::Start(sample.offset)).expect("seek");
                file.read_exact(&mut buf).expect("read");
                buf
            })
            .collect()
    }

    /// An `aavd` file whose payloads are the encrypted form of `plain`.
    fn encrypted_fixture(plain: &[Vec<u8>]) -> ProgressiveFixture {
        ProgressiveFixture {
            timescale: 1000,
            sample_duration: 100,
            ..ProgressiveFixture::default()
        }
        .with_sample_entry(b"aavd")
        .with_samples(plain.iter().map(|s| encrypt_adrm(s)).collect())
    }

    fn plain_samples(count: usize) -> Vec<Vec<u8>> {
        ProgressiveFixture::with_generated_samples(count).samples
    }

    #[test]
    fn an_aaxc_file_decrypts_to_a_clear_m4b() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.aaxc");
        let output = dir.path().join("book.m4b");

        let plain = plain_samples(12);
        encrypted_fixture(&plain).write(&input).unwrap();
        assert_ne!(
            payloads(&input),
            plain,
            "the fixture must actually be encrypted"
        );

        let outcome =
            decrypt_adrm_native(&input, &output, &hex::encode(KEY), &hex::encode(IV), None)
                .expect("decrypt");
        assert_eq!(outcome.output, output);

        assert_eq!(payloads(&output), plain);
        assert_eq!(
            parse_mp4(&output).unwrap().audio.sample_entry_kind,
            SampleEntryKind::Mp4a,
            "output must no longer claim to be Adrm"
        );
    }

    #[test]
    fn a_brand_trim_is_applied_during_the_decrypt_pass() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.aaxc");
        let output = dir.path().join("book.m4b");

        // 100 ms per sample: drop the first 300 ms and everything past 900 ms.
        let plain = plain_samples(12);
        encrypted_fixture(&plain).write(&input).unwrap();

        decrypt_adrm_native(
            &input,
            &output,
            &hex::encode(KEY),
            &hex::encode(IV),
            Some(TrimRange {
                start_ms: 300,
                end_ms: Some(900),
            }),
        )
        .expect("decrypt");

        assert_eq!(payloads(&output), plain[3..9]);
    }

    #[test]
    fn a_cenc_file_is_not_decrypted_as_adrm() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.mp4");
        ProgressiveFixture::with_generated_samples(4)
            .with_sample_entry(b"enca")
            .write(&input)
            .unwrap();

        let err = decrypt_adrm_native(
            &input,
            &dir.path().join("out.m4b"),
            &hex::encode(KEY),
            &hex::encode(IV),
            None,
        )
        .expect_err("enca must be refused");
        assert!(matches!(err, DrmError::Native(_)), "{err}");
    }
}
