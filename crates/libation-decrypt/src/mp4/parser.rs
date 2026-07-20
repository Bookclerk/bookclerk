//! Progressive MP4 structure parsing for Audible Adrm / CENC files.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::boxutil::{
    find_child, read_box_header, read_fourcc, read_full_box_version_flags, read_u32, read_u64,
    walk_children, BoxHeader, FourCC, AAVD, CO64, ENCA, FTYP, HDLR, MDAT, MDHD, MDIA, MINF, MOOV,
    MP4A, STBL, STCO, STSC, STSD, STSZ, STTS, STZ2, TRAK,
};
use super::samples::{build_samples, ChunkMapEntry, SampleInfo};
use crate::error::{DecryptError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEntryKind {
    /// Audible Adrm encrypted AAC (`aavd`).
    Aavd,
    /// Clear AAC (`mp4a`).
    Mp4a,
    /// Common Encryption audio (`enca`).
    Enca,
    Other(FourCC),
}

/// Parsed progressive (non-fragmented) MP4 relevant to decrypt/remux.
#[derive(Debug)]
#[allow(dead_code)] // structural fields retained for diagnostics / future CENC work
pub struct Mp4File {
    pub path: std::path::PathBuf,
    pub file_size: u64,
    pub ftyp: BoxHeader,
    pub moov: BoxHeader,
    pub mdat: BoxHeader,
    pub major_brand: FourCC,
    pub compatible_brands: Vec<FourCC>,
    pub mvhd_timescale: u32,
    pub mvhd_duration: u64,
    pub audio: AudioTrack,
    /// Raw moov bytes (including header) — used when rewriting with patched stsd.
    pub moov_bytes: Vec<u8>,
    pub ftyp_bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct AudioTrack {
    pub trak: BoxHeader,
    pub timescale: u32,
    pub duration: u64,
    pub sample_entry_kind: SampleEntryKind,
    /// Absolute file offset of the 4-byte sample-entry type inside stsd.
    pub sample_entry_type_offset: u64,
    pub samples: Vec<SampleInfo>,
}

pub fn parse_mp4(path: &Path) -> Result<Mp4File> {
    let mut file = File::open(path)?;
    let file_size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;

    let mut ftyp = None;
    let mut moov = None;
    let mut mdat = None;
    let mut pos = 0u64;
    while pos + 8 <= file_size {
        file.seek(SeekFrom::Start(pos))?;
        let header = read_box_header(&mut file)?;
        match header.kind {
            FTYP if ftyp.is_none() => ftyp = Some(header.clone()),
            MOOV if moov.is_none() => moov = Some(header.clone()),
            MDAT if mdat.is_none() => mdat = Some(header.clone()),
            _ => {}
        }
        if header.size == 0 {
            break;
        }
        pos = header.end();
    }

    let ftyp = ftyp.ok_or_else(|| DecryptError::Mp4("missing ftyp box".into()))?;
    let moov = moov.ok_or_else(|| DecryptError::Mp4("missing moov box".into()))?;
    let mdat = mdat.ok_or_else(|| DecryptError::Mp4("missing mdat box".into()))?;

    let (major_brand, compatible_brands) = parse_ftyp(&mut file, &ftyp)?;
    let (mvhd_timescale, mvhd_duration) = parse_mvhd(&mut file, &moov)?;
    let audio = parse_audio_track(&mut file, &moov)?;

    let mut ftyp_bytes = vec![
        0u8;
        usize::try_from(ftyp.size).map_err(|_| {
            DecryptError::Mp4(format!("ftyp too large: {}", ftyp.size))
        })?
    ];
    file.seek(SeekFrom::Start(ftyp.start))?;
    file.read_exact(&mut ftyp_bytes)?;

    let mut moov_bytes = vec![
        0u8;
        usize::try_from(moov.size).map_err(|_| {
            DecryptError::Mp4(format!("moov too large: {}", moov.size))
        })?
    ];
    file.seek(SeekFrom::Start(moov.start))?;
    file.read_exact(&mut moov_bytes)?;

    Ok(Mp4File {
        path: path.to_path_buf(),
        file_size,
        ftyp,
        moov,
        mdat,
        major_brand,
        compatible_brands,
        mvhd_timescale,
        mvhd_duration,
        audio,
        moov_bytes,
        ftyp_bytes,
    })
}

fn parse_ftyp(file: &mut File, ftyp: &BoxHeader) -> Result<(FourCC, Vec<FourCC>)> {
    file.seek(SeekFrom::Start(ftyp.content_start()))?;
    let major = read_fourcc(file)?;
    let _minor = read_u32(file)?;
    let mut brands = Vec::new();
    let end = ftyp.end();
    while file.stream_position()? + 4 <= end {
        brands.push(read_fourcc(file)?);
    }
    Ok((major, brands))
}

fn parse_mvhd(file: &mut File, moov: &BoxHeader) -> Result<(u32, u64)> {
    let mvhd = find_child(file, moov.content_start(), moov.end(), super::boxutil::MVHD)?
        .ok_or_else(|| DecryptError::Mp4("missing mvhd".into()))?;
    file.seek(SeekFrom::Start(mvhd.content_start()))?;
    let (version, _) = read_full_box_version_flags(file)?;
    let (timescale, duration) = if version == 1 {
        let _ctime = read_u64(file)?;
        let _mtime = read_u64(file)?;
        let timescale = read_u32(file)?;
        let duration = read_u64(file)?;
        (timescale, duration)
    } else {
        let _ctime = read_u32(file)?;
        let _mtime = read_u32(file)?;
        let timescale = read_u32(file)?;
        let duration = u64::from(read_u32(file)?);
        (timescale, duration)
    };
    Ok((timescale, duration))
}

fn parse_audio_track(file: &mut File, moov: &BoxHeader) -> Result<AudioTrack> {
    let mut audio = None;
    walk_children(file, moov.content_start(), moov.end(), |file, header| {
        if header.kind != TRAK || audio.is_some() {
            return Ok(());
        }
        if let Some(track) = try_parse_audio_trak(file, header)? {
            audio = Some(track);
        }
        Ok(())
    })?;
    audio.ok_or_else(|| DecryptError::Mp4("no audio track found in moov".into()))
}

fn try_parse_audio_trak(file: &mut File, trak: &BoxHeader) -> Result<Option<AudioTrack>> {
    let mdia = match find_child(file, trak.content_start(), trak.end(), MDIA)? {
        Some(b) => b,
        None => return Ok(None),
    };
    let hdlr = match find_child(file, mdia.content_start(), mdia.end(), HDLR)? {
        Some(b) => b,
        None => return Ok(None),
    };
    file.seek(SeekFrom::Start(hdlr.content_start()))?;
    let (_version, _) = read_full_box_version_flags(file)?;
    let _pre = read_u32(file)?;
    let handler = read_fourcc(file)?;
    if handler.0 != *b"soun" {
        return Ok(None);
    }

    let mdhd = find_child(file, mdia.content_start(), mdia.end(), MDHD)?
        .ok_or_else(|| DecryptError::Mp4("audio track missing mdhd".into()))?;
    file.seek(SeekFrom::Start(mdhd.content_start()))?;
    let (version, _) = read_full_box_version_flags(file)?;
    let (timescale, duration) = if version == 1 {
        let _c = read_u64(file)?;
        let _m = read_u64(file)?;
        (read_u32(file)?, read_u64(file)?)
    } else {
        let _c = read_u32(file)?;
        let _m = read_u32(file)?;
        (read_u32(file)?, u64::from(read_u32(file)?))
    };

    let minf = find_child(file, mdia.content_start(), mdia.end(), MINF)?
        .ok_or_else(|| DecryptError::Mp4("audio track missing minf".into()))?;
    let stbl = find_child(file, minf.content_start(), minf.end(), STBL)?
        .ok_or_else(|| DecryptError::Mp4("audio track missing stbl".into()))?;

    let (sample_entry_kind, sample_entry_type_offset) = parse_stsd(file, &stbl)?;
    let stts = parse_stts(file, &stbl)?;
    let stsc = parse_stsc(file, &stbl)?;
    let sample_sizes = parse_stsz(file, &stbl)?;
    let chunk_offsets = parse_chunk_offsets(file, &stbl)?;

    let samples = build_samples(&stts, &stsc, &sample_sizes, &chunk_offsets)?;

    Ok(Some(AudioTrack {
        trak: trak.clone(),
        timescale,
        duration,
        sample_entry_kind,
        sample_entry_type_offset,
        samples,
    }))
}

fn parse_stsd(file: &mut File, stbl: &BoxHeader) -> Result<(SampleEntryKind, u64)> {
    let stsd = find_child(file, stbl.content_start(), stbl.end(), STSD)?
        .ok_or_else(|| DecryptError::Mp4("missing stsd".into()))?;
    file.seek(SeekFrom::Start(stsd.content_start()))?;
    let (_version, _) = read_full_box_version_flags(file)?;
    let entry_count = read_u32(file)?;
    if entry_count == 0 {
        return Err(DecryptError::Mp4("stsd has no sample entries".into()));
    }
    // First sample entry header.
    let entry_start = file.stream_position()?;
    let _entry_size = read_u32(file)?;
    let type_offset = file.stream_position()?;
    let entry_type = read_fourcc(file)?;
    let kind = match entry_type {
        AAVD => SampleEntryKind::Aavd,
        MP4A => SampleEntryKind::Mp4a,
        ENCA => SampleEntryKind::Enca,
        other => SampleEntryKind::Other(other),
    };
    let _ = entry_start;
    Ok((kind, type_offset))
}

fn parse_stts(file: &mut File, stbl: &BoxHeader) -> Result<Vec<(u32, u32)>> {
    let stts = find_child(file, stbl.content_start(), stbl.end(), STTS)?
        .ok_or_else(|| DecryptError::Mp4("missing stts".into()))?;
    file.seek(SeekFrom::Start(stts.content_start()))?;
    let (_version, _) = read_full_box_version_flags(file)?;
    let entry_count = read_u32(file)?;
    let mut out = Vec::with_capacity(entry_count as usize);
    for _ in 0..entry_count {
        let sample_count = read_u32(file)?;
        let sample_delta = read_u32(file)?;
        out.push((sample_count, sample_delta));
    }
    Ok(out)
}

fn parse_stsc(file: &mut File, stbl: &BoxHeader) -> Result<Vec<ChunkMapEntry>> {
    let stsc = find_child(file, stbl.content_start(), stbl.end(), STSC)?
        .ok_or_else(|| DecryptError::Mp4("missing stsc".into()))?;
    file.seek(SeekFrom::Start(stsc.content_start()))?;
    let (_version, _) = read_full_box_version_flags(file)?;
    let entry_count = read_u32(file)?;
    let mut out = Vec::with_capacity(entry_count as usize);
    for _ in 0..entry_count {
        out.push(ChunkMapEntry {
            first_chunk: read_u32(file)?,
            samples_per_chunk: read_u32(file)?,
            sample_description_index: read_u32(file)?,
        });
    }
    Ok(out)
}

fn parse_stsz(file: &mut File, stbl: &BoxHeader) -> Result<Vec<u32>> {
    if let Some(stsz) = find_child(file, stbl.content_start(), stbl.end(), STSZ)? {
        file.seek(SeekFrom::Start(stsz.content_start()))?;
        let (_version, _) = read_full_box_version_flags(file)?;
        let sample_size = read_u32(file)?;
        let sample_count = read_u32(file)?;
        if sample_size != 0 {
            return Ok(vec![sample_size; sample_count as usize]);
        }
        let mut sizes = Vec::with_capacity(sample_count as usize);
        for _ in 0..sample_count {
            sizes.push(read_u32(file)?);
        }
        return Ok(sizes);
    }
    if find_child(file, stbl.content_start(), stbl.end(), STZ2)?.is_some() {
        return Err(DecryptError::Mp4(
            "compact sample size (stz2) is not supported yet".into(),
        ));
    }
    Err(DecryptError::Mp4("missing stsz".into()))
}

fn parse_chunk_offsets(file: &mut File, stbl: &BoxHeader) -> Result<Vec<u64>> {
    if let Some(stco) = find_child(file, stbl.content_start(), stbl.end(), STCO)? {
        file.seek(SeekFrom::Start(stco.content_start()))?;
        let (_version, _) = read_full_box_version_flags(file)?;
        let entry_count = read_u32(file)?;
        let mut out = Vec::with_capacity(entry_count as usize);
        for _ in 0..entry_count {
            out.push(u64::from(read_u32(file)?));
        }
        return Ok(out);
    }
    if let Some(co64) = find_child(file, stbl.content_start(), stbl.end(), CO64)? {
        file.seek(SeekFrom::Start(co64.content_start()))?;
        let (_version, _) = read_full_box_version_flags(file)?;
        let entry_count = read_u32(file)?;
        let mut out = Vec::with_capacity(entry_count as usize);
        for _ in 0..entry_count {
            out.push(read_u64(file)?);
        }
        return Ok(out);
    }
    Err(DecryptError::Mp4("missing stco/co64".into()))
}

/// Absolute duration of the audio track in milliseconds.
#[must_use]
pub fn track_duration_ms(track: &AudioTrack) -> u64 {
    if track.timescale == 0 {
        return 0;
    }
    track
        .samples
        .last()
        .map(|s| ((s.start_cts + u64::from(s.duration)) * 1000) / u64::from(track.timescale))
        .unwrap_or(0)
}
