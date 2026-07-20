//! Native Rust Adrm / CENC decrypt entry points (no aaxclean-cli).

use std::fs::File;
use std::path::Path;

use crate::crypto::parse_aes128_hex;
use crate::error::{DecryptError, Result};
use crate::mp4::cenc::{
    find_stbl_in_trak, parse_tenc_from_enca_entry, progressive_sample_ivs,
    sample_entry_end_from_type_offset,
};
use crate::mp4::{
    decrypt_and_remux, decrypt_dash_cenc, looks_like_dash, parse_mp4, DecryptMode, RemuxOptions,
    SampleEntryKind, TrimRange,
};
use crate::DecryptOutcome;

/// Native Adrm aaxc decrypt (+ optional brand trim) to a DRM-free M4B.
pub fn decrypt_adrm_native(
    input: &Path,
    output: &Path,
    audible_key_hex: &str,
    audible_iv_hex: &str,
    trim: Option<TrimRange>,
) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DecryptError::InputMissing(input.to_path_buf()));
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
                return Err(DecryptError::Native(
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

    decrypt_and_remux(
        input,
        output,
        &RemuxOptions {
            decrypt: DecryptMode::Adrm { key: &key, iv: &iv },
            trim,
            rewrite_ftyp: true,
        },
    )?;

    if !output.exists() {
        return Err(DecryptError::OutputMissing(output.to_path_buf()));
    }
    Ok(DecryptOutcome {
        output: output.to_path_buf(),
    })
}

/// Native CENC decrypt for Audible Widevine media (and compatible CENC files).
///
/// Audible’s liberate path downloads a DASH **fragmented** CENC MP4 (`moof`/`senc`).
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
        return Err(DecryptError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if looks_like_dash(input).unwrap_or(false) {
        return decrypt_dash_cenc(input, output, kid_hex, key_hex, trim);
    }

    let mp4 = parse_mp4(input).map_err(|err| {
        DecryptError::Native(format!(
            "native CENC requires a progressive MP4 or DASH fragment ({err})"
        ))
    })?;

    if !matches!(
        mp4.audio.sample_entry_kind,
        SampleEntryKind::Enca | SampleEntryKind::Mp4a
    ) {
        return Err(DecryptError::Native(format!(
            "unsupported CENC sample entry {:?}",
            mp4.audio.sample_entry_kind
        )));
    }

    if matches!(mp4.audio.sample_entry_kind, SampleEntryKind::Mp4a) {
        let _key = parse_aes128_hex(key_hex)?;
        decrypt_and_remux(
            input,
            output,
            &RemuxOptions {
                decrypt: DecryptMode::None,
                trim,
                rewrite_ftyp: true,
            },
        )?;
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
        return Err(DecryptError::Native(format!(
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

    if tenc.per_sample_iv_size == 0 {
        let iv = ivs.first().copied().ok_or_else(|| {
            DecryptError::Mp4("progressive enca constant_IV produced no sample IVs".into())
        })?;
        decrypt_and_remux(
            input,
            output,
            &RemuxOptions {
                decrypt: DecryptMode::CencConstantIv { key: &key, iv: &iv },
                trim,
                rewrite_ftyp: true,
            },
        )?;
    } else {
        decrypt_and_remux(
            input,
            output,
            &RemuxOptions {
                decrypt: DecryptMode::CencSampleIvs {
                    key: &key,
                    ivs: &ivs,
                },
                trim,
                rewrite_ftyp: true,
            },
        )?;
    }

    finish_cenc_output(output)
}

fn finish_cenc_output(output: &Path) -> Result<DecryptOutcome> {
    if !output.exists() {
        return Err(DecryptError::OutputMissing(output.to_path_buf()));
    }
    Ok(DecryptOutcome {
        output: output.to_path_buf(),
    })
}

/// Remux a progressive clear M4B/M4A with an optional media-time trim (chapter split).
pub fn remux_trimmed(input: &Path, output: &Path, trim: TrimRange) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DecryptError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    decrypt_and_remux(
        input,
        output,
        &RemuxOptions {
            decrypt: DecryptMode::None,
            trim: Some(trim),
            rewrite_ftyp: true,
        },
    )?;
    if !output.exists() {
        return Err(DecryptError::OutputMissing(output.to_path_buf()));
    }
    Ok(DecryptOutcome {
        output: output.to_path_buf(),
    })
}
