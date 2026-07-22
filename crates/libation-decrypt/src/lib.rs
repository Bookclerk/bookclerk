//! Decrypt pipeline: native Rust Adrm + DASH/CENC remux, metadata fix-up, and MP3 encode.

mod brand;
mod crypto;
mod error;
mod metadata;
mod mp3;
mod mp4;
mod native;
mod package_m4b;

pub use brand::{
    brand_durations_from_chapter_info, brand_trim_range, rebase_chapters_after_brand_trim,
    runtime_length_ms_from_chapter_info, BrandDurations,
};
pub use error::{DecryptError, Result};
pub use metadata::{fixup_audiobook, libation_tool_tag, FixupRequest, LIBATION_TOOL_NAME};
pub use mp4::{
    decrypt_and_remux, decrypt_dash_cenc, parse_mp4, track_duration_ms, DecryptMode, RemuxOptions,
    SampleEntryKind, TrimRange,
};
pub use native::{decrypt_adrm_native, decrypt_cenc_native, remux_trimmed};
pub use package_m4b::{package_m4b_from_mp3, package_m4b_from_pcm, PackageM4bRequest};

use std::path::{Path, PathBuf};

/// Input for an Adrm aaxc decrypt job.
#[derive(Debug, Clone)]
pub struct DecryptRequest {
    /// Encrypted AAX/AAXC file.
    pub input: PathBuf,
    /// Destination m4b/m4a path.
    pub output: PathBuf,
    /// Adrm aaxc content key (hex) — preferred modern path.
    pub audible_key: Option<String>,
    /// Adrm aaxc IV (hex).
    pub audible_iv: Option<String>,
    /// Legacy AAX activation bytes (unsupported by the native key/iv path).
    pub activation_bytes: Option<String>,
    /// Optional brand / chapter trim window (milliseconds).
    pub trim: Option<TrimRange>,
}

/// Input for a Widevine CENC decrypt job.
#[derive(Debug, Clone)]
pub struct CencDecryptRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    /// 32-hex key id.
    pub kid: String,
    /// 32-hex content key.
    pub key: String,
    pub trim: Option<TrimRange>,
}

/// Outcome of a successful decrypt / encode.
#[derive(Debug, Clone)]
pub struct DecryptOutcome {
    pub output: PathBuf,
}

/// Decrypt Adrm aaxc using the native Rust remuxer (no external binary).
pub async fn decrypt_with_aaxclean(req: DecryptRequest) -> Result<DecryptOutcome> {
    // Kept name for call-site compatibility; implementation is native.
    decrypt_adrm(req).await
}

/// Decrypt Adrm aaxc natively (AES-128-CBC sample remux + optional trim).
pub async fn decrypt_adrm(req: DecryptRequest) -> Result<DecryptOutcome> {
    if !req.input.exists() {
        return Err(DecryptError::InputMissing(req.input));
    }
    let (Some(key), Some(iv)) = (&req.audible_key, &req.audible_iv) else {
        if req.activation_bytes.is_some() {
            return Err(DecryptError::UnsupportedActivationBytes);
        }
        return Err(DecryptError::MissingCredentials);
    };
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let input = req.input.clone();
    let output = req.output.clone();
    let key = key.clone();
    let iv = iv.clone();
    let trim = req.trim;
    tokio::task::spawn_blocking(move || decrypt_adrm_native(&input, &output, &key, &iv, trim))
        .await
        .map_err(|err| DecryptError::Native(format!("decrypt task join error: {err}")))?
}

/// Decrypt Widevine CENC natively (fragmented DASH or progressive `enca`).
pub async fn decrypt_cenc(req: CencDecryptRequest) -> Result<DecryptOutcome> {
    if !req.input.exists() {
        return Err(DecryptError::InputMissing(req.input.clone()));
    }
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let input = req.input.clone();
    let output = req.output.clone();
    let kid = req.kid.clone();
    let key = req.key.clone();
    let trim = req.trim;
    tokio::task::spawn_blocking(move || decrypt_cenc_native(&input, &output, &kid, &key, trim))
        .await
        .map_err(|err| DecryptError::Native(format!("decrypt task join error: {err}")))?
}

/// Re-encode audio to MP3 via Symphonia + LAME (classic Libation `DecryptToLossy`).
pub async fn encode_to_mp3(
    input: &Path,
    output: &Path,
    lame: &libation_config::LameConfig,
    max_sample_rate: Option<u32>,
) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DecryptError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let input = input.to_path_buf();
    let output = output.to_path_buf();
    let lame = lame.clone();
    tokio::task::spawn_blocking(move || {
        mp3::encode_to_mp3_native(&input, &output, &lame, max_sample_rate)
    })
    .await
    .map_err(|err| DecryptError::Native(format!("mp3 encode task join error: {err}")))?
}

/// Copy/trim a progressive M4B/M4A into a new file (chapter split helper).
pub async fn remux_trimmed_async(
    input: &Path,
    output: &Path,
    trim: TrimRange,
) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DecryptError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let input = input.to_path_buf();
    let output = output.to_path_buf();
    tokio::task::spawn_blocking(move || remux_trimmed(&input, &output, trim))
        .await
        .map_err(|err| DecryptError::Native(format!("remux task join error: {err}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{decrypt_aavd_sample_in_place, parse_aes128_hex};
    use cbc::cipher::{block_padding::NoPadding, BlockModeEncrypt, KeyIvInit};
    use std::io::Write;

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    #[tokio::test]
    async fn activation_bytes_alone_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.aax");
        std::fs::write(&input, b"not-a-real-aax").unwrap();
        let req = DecryptRequest {
            input,
            output: dir.path().join("out.m4b"),
            audible_key: None,
            audible_iv: None,
            activation_bytes: Some("deadbeef".into()),
            trim: None,
        };
        let err = decrypt_adrm(req).await.unwrap_err();
        assert!(matches!(err, DecryptError::UnsupportedActivationBytes));
    }

    /// Build a minimal progressive MP4 with one encrypted AAC-like sample and remux it.
    #[test]
    fn native_adrm_roundtrip_synthetic() {
        let key = parse_aes128_hex("00112233445566778899aabbccddeeff").unwrap();
        let iv = parse_aes128_hex("ffeeddccbbaa99887766554433221100").unwrap();

        let mut plain = vec![0u8; 32];
        for (i, b) in plain.iter_mut().enumerate() {
            *b = (i * 7) as u8;
        }
        let mut enc = plain.clone();
        {
            let len = enc.len();
            Aes128CbcEnc::new(&key.into(), &iv.into())
                .encrypt_padded::<NoPadding>(&mut enc, len)
                .unwrap();
        }
        assert_ne!(enc, plain);

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.aaxc");
        let output = dir.path().join("book.m4b");
        write_synthetic_aavd_mp4(&input, &enc).unwrap();

        decrypt_adrm_native(
            &input,
            &output,
            "00112233445566778899aabbccddeeff",
            "ffeeddccbbaa99887766554433221100",
            None,
        )
        .unwrap();

        assert!(output.exists());
        let out_mp4 = crate::mp4::parse_mp4(&output).unwrap();
        assert_eq!(out_mp4.audio.samples.len(), 1);
        let sample = &out_mp4.audio.samples[0];
        let mut got = vec![0u8; sample.size as usize];
        let mut f = std::fs::File::open(&output).unwrap();
        use std::io::{Read, Seek, SeekFrom};
        f.seek(SeekFrom::Start(sample.offset)).unwrap();
        f.read_exact(&mut got).unwrap();
        assert_eq!(got, plain);

        // Trim that keeps the only sample should still succeed.
        decrypt_adrm_native(
            &input,
            &output,
            "00112233445566778899aabbccddeeff",
            "ffeeddccbbaa99887766554433221100",
            Some(TrimRange {
                start_ms: 0,
                end_ms: Some(10_000),
            }),
        )
        .unwrap();
    }

    /// Progressive (non-DASH) `enca` with a `tenc` constant_IV.
    #[test]
    fn native_progressive_enca_constant_iv_roundtrip() {
        use crate::crypto::{decrypt_cenc_sample_in_place, expand_cenc_iv};

        let key = [0x42u8; 16];
        let kid = [0x11u8; 16];
        let plain = b"PROGRESSIVE_ENCA_AAC_FRAME!".to_vec();
        let mut enc = plain.clone();
        let iv8 = [0xAAu8; 8];
        let iv16 = expand_cenc_iv(&iv8);
        decrypt_cenc_sample_in_place(&key, &iv16, &mut enc);

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.enca.m4a");
        let output = dir.path().join("book.m4b");
        write_synthetic_progressive_enca_mp4(&input, &enc, &kid, &iv8, true).unwrap();

        decrypt_cenc_native(&input, &output, &hex::encode(kid), &hex::encode(key), None).unwrap();

        let out_mp4 = crate::mp4::parse_mp4(&output).unwrap();
        assert_eq!(out_mp4.audio.sample_entry_kind, SampleEntryKind::Mp4a);
        assert_eq!(out_mp4.audio.samples.len(), 1);
        let sample = &out_mp4.audio.samples[0];
        let mut got = vec![0u8; sample.size as usize];
        let mut f = std::fs::File::open(&output).unwrap();
        use std::io::{Read, Seek, SeekFrom};
        f.seek(SeekFrom::Start(sample.offset)).unwrap();
        f.read_exact(&mut got).unwrap();
        assert_eq!(got, plain);
        let out_bytes = std::fs::read(&output).unwrap();
        assert!(!out_bytes.windows(4).any(|w| w == b"enca"));
        assert!(!out_bytes.windows(4).any(|w| w == b"sinf"));
    }

    /// Progressive `enca` with per-sample IVs via `saiz`/`saio`.
    #[test]
    fn native_progressive_enca_per_sample_iv_roundtrip() {
        use crate::crypto::{decrypt_cenc_sample_in_place, expand_cenc_iv};

        let key = [0x7Cu8; 16];
        let kid = [0x22u8; 16];
        let plain = b"PER_SAMPLE_IV_ENCA_FRAME!!".to_vec();
        let mut enc = plain.clone();
        let iv8 = [0x55u8; 8];
        let iv16 = expand_cenc_iv(&iv8);
        decrypt_cenc_sample_in_place(&key, &iv16, &mut enc);

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.ps.enca.m4a");
        let output = dir.path().join("book.m4b");
        write_synthetic_progressive_enca_mp4(&input, &enc, &kid, &iv8, false).unwrap();

        decrypt_cenc_native(&input, &output, &hex::encode(kid), &hex::encode(key), None).unwrap();

        let out_mp4 = crate::mp4::parse_mp4(&output).unwrap();
        assert_eq!(out_mp4.audio.sample_entry_kind, SampleEntryKind::Mp4a);
        let sample = &out_mp4.audio.samples[0];
        let mut got = vec![0u8; sample.size as usize];
        let mut f = std::fs::File::open(&output).unwrap();
        use std::io::{Read, Seek, SeekFrom};
        f.seek(SeekFrom::Start(sample.offset)).unwrap();
        f.read_exact(&mut got).unwrap();
        assert_eq!(got, plain);
        let out_bytes = std::fs::read(&output).unwrap();
        assert!(!out_bytes.windows(4).any(|w| w == b"saiz"));
        assert!(!out_bytes.windows(4).any(|w| w == b"saio"));
    }

    fn write_synthetic_progressive_enca_mp4(
        path: &Path,
        encrypted_sample: &[u8],
        kid: &[u8; 16],
        iv8: &[u8; 8],
        constant_iv: bool,
    ) -> std::io::Result<()> {
        let mut ftyp = Vec::new();
        let brands = [b"isom", b"mp42", b"iso2"];
        let ftyp_size = 8 + 8 + brands.len() * 4;
        ftyp.extend_from_slice(&(ftyp_size as u32).to_be_bytes());
        ftyp.extend_from_slice(b"ftyp");
        ftyp.extend_from_slice(b"isom");
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        for b in brands {
            ftyp.extend_from_slice(b);
        }

        // Layout: ftyp | mdat(sample) | [aux IVs] | moov
        // For per-sample IVs, saio points at the aux region after mdat payload.
        let mdat_file_offset = ftyp.len();
        let sample_offset = (mdat_file_offset + 8) as u32;
        let aux_offset = (mdat_file_offset + 8 + encrypted_sample.len()) as u32;

        let mut mdat = Vec::new();
        let mdat_payload_len = if constant_iv {
            encrypted_sample.len()
        } else {
            encrypted_sample.len() + iv8.len()
        };
        let mdat_size = 8 + mdat_payload_len;
        mdat.extend_from_slice(&(mdat_size as u32).to_be_bytes());
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(encrypted_sample);
        if !constant_iv {
            mdat.extend_from_slice(iv8);
        }

        let mut tenc_content = Vec::new();
        tenc_content.push(0); // reserved
        tenc_content.push(0); // reserved
        tenc_content.push(1); // isProtected
        if constant_iv {
            tenc_content.push(0); // Per_Sample_IV_Size = 0
            tenc_content.extend_from_slice(kid);
            tenc_content.push(8); // constant_IV_size
            tenc_content.extend_from_slice(iv8);
        } else {
            tenc_content.push(8); // Per_Sample_IV_Size
            tenc_content.extend_from_slice(kid);
        }
        let mut tenc = Vec::new();
        let tenc_size = 8 + 4 + tenc_content.len();
        tenc.extend_from_slice(&(tenc_size as u32).to_be_bytes());
        tenc.extend_from_slice(b"tenc");
        tenc.extend_from_slice(&0u32.to_be_bytes());
        tenc.extend_from_slice(&tenc_content);

        let schi = wrap_box(b"schi", &tenc);
        let mut schm = Vec::new();
        let schm_size = 8 + 4 + 4 + 4;
        schm.extend_from_slice(&(schm_size as u32).to_be_bytes());
        schm.extend_from_slice(b"schm");
        schm.extend_from_slice(&0u32.to_be_bytes());
        schm.extend_from_slice(b"cenc");
        schm.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        let frma = wrap_box(b"frma", b"mp4a");
        let sinf = wrap_box(b"sinf", &[frma, schm, schi].concat());

        let mut enca = Vec::new();
        enca.extend_from_slice(&0u32.to_be_bytes());
        enca.extend_from_slice(b"enca");
        enca.extend_from_slice(&[0u8; 6]);
        enca.extend_from_slice(&1u16.to_be_bytes());
        enca.extend_from_slice(&[0u8; 8]);
        enca.extend_from_slice(&2u16.to_be_bytes());
        enca.extend_from_slice(&16u16.to_be_bytes());
        enca.extend_from_slice(&0u16.to_be_bytes());
        enca.extend_from_slice(&0u16.to_be_bytes());
        enca.extend_from_slice(&(44100u32 << 16).to_be_bytes());
        enca.extend_from_slice(&sinf);
        let enca_size = enca.len() as u32;
        enca[0..4].copy_from_slice(&enca_size.to_be_bytes());

        let mut stsd = Vec::new();
        let stsd_size = 8 + 4 + 4 + enca.len();
        stsd.extend_from_slice(&(stsd_size as u32).to_be_bytes());
        stsd.extend_from_slice(b"stsd");
        stsd.extend_from_slice(&0u32.to_be_bytes());
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&enca);

        let stts = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 8;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stts");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1024u32.to_be_bytes());
            b
        };
        let stsc = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 12;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stsc");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        let stsz = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 4 + 4;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stsz");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&(encrypted_sample.len() as u32).to_be_bytes());
            b
        };
        let stco = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 4;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stco");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&sample_offset.to_be_bytes());
            b
        };

        let mut stbl_parts = vec![stsd, stts, stsc, stsz, stco];
        if !constant_iv {
            let mut saiz = Vec::new();
            // default_sample_info_size=8, sample_count=1
            let saiz_size = 8 + 4 + 1 + 4;
            saiz.extend_from_slice(&(saiz_size as u32).to_be_bytes());
            saiz.extend_from_slice(b"saiz");
            saiz.extend_from_slice(&0u32.to_be_bytes());
            saiz.push(8);
            saiz.extend_from_slice(&1u32.to_be_bytes());

            let mut saio = Vec::new();
            let saio_size = 8 + 4 + 4 + 4;
            saio.extend_from_slice(&(saio_size as u32).to_be_bytes());
            saio.extend_from_slice(b"saio");
            saio.extend_from_slice(&0u32.to_be_bytes());
            saio.extend_from_slice(&1u32.to_be_bytes());
            saio.extend_from_slice(&aux_offset.to_be_bytes());

            stbl_parts.push(saiz);
            stbl_parts.push(saio);
        }
        let stbl = wrap_box(b"stbl", &stbl_parts.concat());

        let smhd = {
            let mut b = Vec::new();
            let size = 16;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"smhd");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b
        };
        let dinf = {
            let mut dref_entry = Vec::new();
            dref_entry.extend_from_slice(&12u32.to_be_bytes());
            dref_entry.extend_from_slice(b"url ");
            dref_entry.extend_from_slice(&1u32.to_be_bytes());
            let mut dref = Vec::new();
            let dref_size = 8 + 4 + 4 + dref_entry.len();
            dref.extend_from_slice(&(dref_size as u32).to_be_bytes());
            dref.extend_from_slice(b"dref");
            dref.extend_from_slice(&0u32.to_be_bytes());
            dref.extend_from_slice(&1u32.to_be_bytes());
            dref.extend_from_slice(&dref_entry);
            wrap_box(b"dinf", &dref)
        };
        let minf = wrap_box(b"minf", &[smhd, dinf, stbl].concat());
        let hdlr = {
            let mut b = Vec::new();
            let name = b"SoundHandler\0";
            let size = 8 + 4 + 4 + 4 + 12 + name.len();
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"hdlr");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(b"soun");
            b.extend_from_slice(&[0u8; 12]);
            b.extend_from_slice(name);
            b
        };
        let mdhd = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 4 + 4 + 4 + 2 + 2;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"mdhd");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&44100u32.to_be_bytes());
            b.extend_from_slice(&1024u32.to_be_bytes());
            b.extend_from_slice(&0x55c4u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b
        };
        let mdia = wrap_box(b"mdia", &[mdhd, hdlr, minf].concat());
        let tkhd = {
            let mut b = Vec::new();
            let size = 92;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"tkhd");
            b.extend_from_slice(&0x000003u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1024u32.to_be_bytes());
            b.extend_from_slice(&[0u8; 8]);
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&0x0100u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            let matrix = [0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
            for v in matrix {
                b.extend_from_slice(&v.to_be_bytes());
            }
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            assert_eq!(b.len(), size);
            b
        };
        let trak = wrap_box(b"trak", &[tkhd, mdia].concat());
        let mvhd = {
            let mut b = Vec::new();
            let size = 108;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"mvhd");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&44100u32.to_be_bytes());
            b.extend_from_slice(&1024u32.to_be_bytes());
            b.extend_from_slice(&0x00010000u32.to_be_bytes());
            b.extend_from_slice(&0x0100u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&[0u8; 8]);
            let matrix = [0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
            for v in matrix {
                b.extend_from_slice(&v.to_be_bytes());
            }
            b.extend_from_slice(&[0u8; 24]);
            b.extend_from_slice(&2u32.to_be_bytes());
            assert_eq!(b.len(), size);
            b
        };
        let moov = wrap_box(b"moov", &[mvhd, trak].concat());

        let mut file = std::fs::File::create(path)?;
        file.write_all(&ftyp)?;
        file.write_all(&mdat)?;
        file.write_all(&moov)?;
        Ok(())
    }

    fn write_synthetic_aavd_mp4(path: &Path, encrypted_sample: &[u8]) -> std::io::Result<()> {
        let mut ftyp = Vec::new();
        let brands = [b"aaxc", b"aax ", b"isom"];
        let ftyp_size = 8 + 8 + brands.len() * 4;
        ftyp.extend_from_slice(&(ftyp_size as u32).to_be_bytes());
        ftyp.extend_from_slice(b"ftyp");
        ftyp.extend_from_slice(b"aaxc");
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        for b in brands {
            ftyp.extend_from_slice(b);
        }

        let mut mdat = Vec::new();
        let mdat_size = 8 + encrypted_sample.len();
        mdat.extend_from_slice(&(mdat_size as u32).to_be_bytes());
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(encrypted_sample);

        let mdat_file_offset = ftyp.len();
        let sample_offset = (mdat_file_offset + 8) as u32;

        let stsd = {
            let mut e = Vec::new();
            e.extend_from_slice(&0u32.to_be_bytes());
            e.extend_from_slice(b"aavd");
            e.extend_from_slice(&[0u8; 6]);
            e.extend_from_slice(&1u16.to_be_bytes());
            e.extend_from_slice(&[0u8; 8]);
            e.extend_from_slice(&2u16.to_be_bytes());
            e.extend_from_slice(&16u16.to_be_bytes());
            e.extend_from_slice(&0u16.to_be_bytes());
            e.extend_from_slice(&0u16.to_be_bytes());
            e.extend_from_slice(&(44100u32 << 16).to_be_bytes());
            let esize = e.len() as u32;
            e[0..4].copy_from_slice(&esize.to_be_bytes());

            let mut b = Vec::new();
            let size = 8 + 4 + 4 + e.len();
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stsd");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&e);
            b
        };
        let stts = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 8;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stts");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1024u32.to_be_bytes());
            b
        };
        let stsc = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 12;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stsc");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        let stsz = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 4 + 4;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stsz");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&(encrypted_sample.len() as u32).to_be_bytes());
            b
        };
        let stco = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 4;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"stco");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&sample_offset.to_be_bytes());
            b
        };

        let stbl = wrap_box(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
        let smhd = {
            let mut b = Vec::new();
            let size = 16;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"smhd");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b
        };
        let dinf = {
            let mut dref_entry = Vec::new();
            dref_entry.extend_from_slice(&12u32.to_be_bytes());
            dref_entry.extend_from_slice(b"url ");
            dref_entry.extend_from_slice(&1u32.to_be_bytes());
            let mut dref = Vec::new();
            let dref_size = 8 + 4 + 4 + dref_entry.len();
            dref.extend_from_slice(&(dref_size as u32).to_be_bytes());
            dref.extend_from_slice(b"dref");
            dref.extend_from_slice(&0u32.to_be_bytes());
            dref.extend_from_slice(&1u32.to_be_bytes());
            dref.extend_from_slice(&dref_entry);
            wrap_box(b"dinf", &dref)
        };
        let minf = wrap_box(b"minf", &[smhd, dinf, stbl].concat());
        let hdlr = {
            let mut b = Vec::new();
            let name = b"SoundHandler\0";
            let size = 8 + 4 + 4 + 4 + 12 + name.len();
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"hdlr");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(b"soun");
            b.extend_from_slice(&[0u8; 12]);
            b.extend_from_slice(name);
            b
        };
        let mdhd = {
            let mut b = Vec::new();
            let size = 8 + 4 + 4 + 4 + 4 + 4 + 2 + 2;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"mdhd");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&44100u32.to_be_bytes());
            b.extend_from_slice(&1024u32.to_be_bytes());
            b.extend_from_slice(&0x55c4u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b
        };
        let mdia = wrap_box(b"mdia", &[mdhd, hdlr, minf].concat());
        let tkhd = {
            let mut b = Vec::new();
            let size = 92;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"tkhd");
            b.extend_from_slice(&0x000003u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&1024u32.to_be_bytes());
            b.extend_from_slice(&[0u8; 8]);
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&0x0100u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            let matrix = [0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
            for v in matrix {
                b.extend_from_slice(&v.to_be_bytes());
            }
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            assert_eq!(b.len(), size);
            b
        };
        let trak = wrap_box(b"trak", &[tkhd, mdia].concat());
        let mvhd = {
            let mut b = Vec::new();
            let size = 108;
            b.extend_from_slice(&(size as u32).to_be_bytes());
            b.extend_from_slice(b"mvhd");
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&44100u32.to_be_bytes());
            b.extend_from_slice(&1024u32.to_be_bytes());
            b.extend_from_slice(&0x00010000u32.to_be_bytes());
            b.extend_from_slice(&0x0100u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&[0u8; 8]);
            let matrix = [0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
            for v in matrix {
                b.extend_from_slice(&v.to_be_bytes());
            }
            b.extend_from_slice(&[0u8; 24]);
            b.extend_from_slice(&2u32.to_be_bytes());
            assert_eq!(b.len(), size);
            b
        };
        let moov = wrap_box(b"moov", &[mvhd, trak].concat());

        let mut file = std::fs::File::create(path)?;
        file.write_all(&ftyp)?;
        file.write_all(&mdat)?;
        file.write_all(&moov)?;
        Ok(())
    }

    fn wrap_box(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let size = 8 + content.len();
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(&(size as u32).to_be_bytes());
        b.extend_from_slice(kind);
        b.extend_from_slice(content);
        b
    }

    #[test]
    fn decrypt_helper_matches_fixture_crypto() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let mut data = vec![9u8; 16];
        let original = data.clone();
        {
            let len = data.len();
            Aes128CbcEnc::new(&key.into(), &iv.into())
                .encrypt_padded::<NoPadding>(&mut data, len)
                .unwrap();
        }
        decrypt_aavd_sample_in_place(&key, &iv, &mut data);
        assert_eq!(data, original);
    }
}
