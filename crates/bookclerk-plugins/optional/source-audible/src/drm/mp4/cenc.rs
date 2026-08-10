//! Shared CENC helpers: `tenc` parsing and progressive sample IVs (`saiz`/`saio`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use bookclerk_mp4::boxutil::{
    find_child, read_full_box_version_flags, read_u32, read_u64, read_u8, BoxHeader, FourCC, MDIA,
    MINF, SAIO, SAIZ, SCHI, SCHM, SINF, STBL, TENC,
};

use crate::drm::crypto::expand_cenc_iv;
use crate::drm::error::{DrmError, Result};

/// Parsed `tenc` (Track Encryption) defaults.
#[derive(Debug, Clone)]
pub struct TencInfo {
    pub kid: [u8; 16],
    /// 0 means constant IV; 8 or 16 means per-sample IVs.
    pub per_sample_iv_size: u8,
    pub constant_iv: Option<Vec<u8>>,
}

/// Expand 8- or 16-byte CENC IV to the 16-byte AES-CTR counter block.
pub fn normalize_cenc_iv(iv: &[u8]) -> Result<[u8; 16]> {
    match iv.len() {
        16 => {
            let mut out = [0u8; 16];
            out.copy_from_slice(iv);
            Ok(out)
        }
        8 => {
            let mut iv8 = [0u8; 8];
            iv8.copy_from_slice(iv);
            Ok(expand_cenc_iv(&iv8))
        }
        n => Err(DrmError::Mp4(format!("unexpected CENC IV length {n}"))),
    }
}

/// Parse `tenc` from an `enca` sample entry (absolute file offsets).
pub fn parse_tenc_from_enca_entry(
    file: &mut File,
    sample_entry_type_offset: u64,
    sample_entry_end: u64,
) -> Result<TencInfo> {
    // AudioSampleEntry fields follow the 4-byte type (28 bytes), then children.
    let children_start = sample_entry_type_offset + 4 + 28;
    let sinf = find_child(file, children_start, sample_entry_end, SINF)?
        .ok_or_else(|| DrmError::Mp4("enca missing sinf".into()))?;
    if let Some(schm) = find_child(file, sinf.content_start(), sinf.end(), SCHM)? {
        file.seek(SeekFrom::Start(schm.content_start()))?;
        let (_v, _) = read_full_box_version_flags(file)?;
        let mut scheme = [0u8; 4];
        file.read_exact(&mut scheme)?;
        if &scheme != b"cenc" {
            return Err(DrmError::Mp4(format!(
                "unsupported DRM scheme {}; only cenc is supported",
                FourCC(scheme)
            )));
        }
    }
    let schi = find_child(file, sinf.content_start(), sinf.end(), SCHI)?
        .ok_or_else(|| DrmError::Mp4("sinf missing schi".into()))?;
    let tenc = find_child(file, schi.content_start(), schi.end(), TENC)?
        .ok_or_else(|| DrmError::Mp4("schi missing tenc".into()))?;
    parse_tenc(file, &tenc)
}

pub fn parse_tenc(file: &mut (impl Read + Seek), tenc: &BoxHeader) -> Result<TencInfo> {
    file.seek(SeekFrom::Start(tenc.content_start()))?;
    let (version, _) = read_full_box_version_flags(file)?;
    let _reserved0 = read_u8(file)?;
    let _reserved1_or_pattern = read_u8(file)?;
    let is_protected = read_u8(file)?;
    let per_sample_iv_size = read_u8(file)?;
    let mut kid = [0u8; 16];
    file.read_exact(&mut kid)?;

    let constant_iv = if is_protected == 1 && per_sample_iv_size == 0 {
        let constant_iv_size = read_u8(file)? as usize;
        if !matches!(constant_iv_size, 8 | 16) {
            return Err(DrmError::Mp4(format!(
                "unsupported tenc constant_IV_size {constant_iv_size}"
            )));
        }
        let mut iv = vec![0u8; constant_iv_size];
        file.read_exact(&mut iv)?;
        Some(iv)
    } else {
        let _ = version;
        None
    };

    Ok(TencInfo {
        kid,
        per_sample_iv_size,
        constant_iv,
    })
}

/// Resolve one AES-CTR IV per sample for a progressive `enca` track.
pub fn progressive_sample_ivs(
    file: &mut File,
    stbl: &BoxHeader,
    tenc: &TencInfo,
    sample_count: usize,
) -> Result<Vec<[u8; 16]>> {
    if tenc.per_sample_iv_size == 0 {
        let raw = tenc.constant_iv.as_ref().ok_or_else(|| {
            DrmError::Mp4("tenc has Per_Sample_IV_Size=0 but no constant_IV".into())
        })?;
        let iv16 = normalize_cenc_iv(raw)?;
        return Ok(vec![iv16; sample_count]);
    }

    if !matches!(tenc.per_sample_iv_size, 8 | 16) {
        return Err(DrmError::Mp4(format!(
            "unsupported tenc Per_Sample_IV_Size {}",
            tenc.per_sample_iv_size
        )));
    }

    let ivs = read_saio_saiz_ivs(file, stbl, sample_count, tenc.per_sample_iv_size as usize)?;
    ivs.into_iter().map(|iv| normalize_cenc_iv(&iv)).collect()
}

fn read_saio_saiz_ivs(
    file: &mut File,
    stbl: &BoxHeader,
    sample_count: usize,
    iv_size: usize,
) -> Result<Vec<Vec<u8>>> {
    let saiz = find_child(file, stbl.content_start(), stbl.end(), SAIZ)?.ok_or_else(|| {
        DrmError::Mp4(
            "progressive enca needs saiz/saio for per-sample IVs (or a tenc constant_IV)".into(),
        )
    })?;
    let saio = find_child(file, stbl.content_start(), stbl.end(), SAIO)?
        .ok_or_else(|| DrmError::Mp4("progressive enca has saiz but missing saio".into()))?;

    let sizes = parse_saiz(file, &saiz, sample_count)?;
    for (i, &sz) in sizes.iter().enumerate() {
        if sz as usize != iv_size {
            return Err(DrmError::Mp4(format!(
                "saiz sample {i} info size {sz} != tenc IV size {iv_size}"
            )));
        }
    }

    let offsets = parse_saio(file, &saio)?;
    // Common case: one offset for the whole aux info contiguous region.
    if offsets.len() == 1 {
        let mut out = Vec::with_capacity(sample_count);
        file.seek(SeekFrom::Start(offsets[0]))?;
        for _ in 0..sample_count {
            let mut iv = vec![0u8; iv_size];
            file.read_exact(&mut iv)?;
            out.push(iv);
        }
        return Ok(out);
    }
    if offsets.len() != sample_count {
        return Err(DrmError::Mp4(format!(
            "saio entry_count {} neither 1 nor sample_count {sample_count}",
            offsets.len()
        )));
    }
    let mut out = Vec::with_capacity(sample_count);
    for &off in &offsets {
        file.seek(SeekFrom::Start(off))?;
        let mut iv = vec![0u8; iv_size];
        file.read_exact(&mut iv)?;
        out.push(iv);
    }
    Ok(out)
}

fn parse_saiz(file: &mut File, saiz: &BoxHeader, expect_count: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(saiz.content_start()))?;
    let (_version, flags) = read_full_box_version_flags(file)?;
    if flags & 1 != 0 {
        let _aux_info_type = read_u32(file)?;
        let _aux_info_type_parameter = read_u32(file)?;
    }
    let default_sample_info_size = read_u8(file)?;
    let sample_count = read_u32(file)? as usize;
    if sample_count != expect_count {
        return Err(DrmError::Mp4(format!(
            "saiz sample_count {sample_count} != track sample_count {expect_count}"
        )));
    }
    if default_sample_info_size != 0 {
        return Ok(vec![default_sample_info_size; sample_count]);
    }
    let mut sizes = vec![0u8; sample_count];
    file.read_exact(&mut sizes)?;
    Ok(sizes)
}

fn parse_saio(file: &mut File, saio: &BoxHeader) -> Result<Vec<u64>> {
    file.seek(SeekFrom::Start(saio.content_start()))?;
    let (version, flags) = read_full_box_version_flags(file)?;
    if flags & 1 != 0 {
        let _aux_info_type = read_u32(file)?;
        let _aux_info_type_parameter = read_u32(file)?;
    }
    let entry_count = read_u32(file)? as usize;
    let mut offsets = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        let off = if version == 0 {
            u64::from(read_u32(file)?)
        } else {
            read_u64(file)?
        };
        offsets.push(off);
    }
    Ok(offsets)
}

/// Locate the `stbl` box under a track's `minf`.
pub fn find_stbl_in_trak(file: &mut File, trak: &BoxHeader) -> Result<BoxHeader> {
    let mdia = find_child(file, trak.content_start(), trak.end(), MDIA)?
        .ok_or_else(|| DrmError::Mp4("trak missing mdia".into()))?;
    let minf = find_child(file, mdia.content_start(), mdia.end(), MINF)?
        .ok_or_else(|| DrmError::Mp4("mdia missing minf".into()))?;
    find_child(file, minf.content_start(), minf.end(), STBL)?
        .ok_or_else(|| DrmError::Mp4("minf missing stbl".into()))
}

/// Absolute end offset of the first sample entry in `stsd` (for `enca` child walk).
pub fn sample_entry_end_from_type_offset(file: &mut File, type_offset: u64) -> Result<u64> {
    // type is at offset 4 within the sample entry; size is at entry start.
    let entry_start = type_offset.saturating_sub(4);
    file.seek(SeekFrom::Start(entry_start))?;
    let size = u64::from(read_u32(file)?);
    Ok(entry_start + size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parse_tenc_constant_iv() {
        let mut tenc = Vec::new();
        tenc.extend_from_slice(&0u32.to_be_bytes()); // placeholder size
        tenc.extend_from_slice(b"tenc");
        tenc.extend_from_slice(&0u32.to_be_bytes()); // version+flags
        tenc.push(0); // reserved
        tenc.push(0); // reserved
        tenc.push(1); // isProtected
        tenc.push(0); // Per_Sample_IV_Size = 0 → constant IV
        let kid = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        tenc.extend_from_slice(&kid);
        tenc.push(8); // constant_IV_size
        tenc.extend_from_slice(&[0xAAu8; 8]);
        let size = tenc.len() as u32;
        tenc[0..4].copy_from_slice(&size.to_be_bytes());

        let header = BoxHeader {
            start: 0,
            size: u64::from(size),
            header_len: 8,
            kind: TENC,
        };
        let mut cur = Cursor::new(tenc);
        let info = parse_tenc(&mut cur, &header).unwrap();
        assert_eq!(info.kid, kid);
        assert_eq!(info.per_sample_iv_size, 0);
        assert_eq!(info.constant_iv.as_deref(), Some(&[0xAAu8; 8][..]));
    }
}
