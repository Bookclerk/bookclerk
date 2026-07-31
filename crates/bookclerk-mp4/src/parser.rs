//! Progressive (non-fragmented) MP4 structure parsing.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::boxutil::{
    find_child, read_box_header, read_fourcc, read_full_box_version_flags, read_u32, read_u64,
    walk_children, BoxHeader, FourCC, AAVD, CO64, ENCA, FTYP, HDLR, MDAT, MDHD, MDIA, MINF, MOOV,
    MP4A, MVHD, STBL, STCO, STSC, STSD, STSZ, STTS, STZ2, TRAK,
};
use crate::edit::find_child_in_range;
use crate::error::{Mp4Error, Result};
use crate::samples::{build_samples, ChunkMapEntry, SampleInfo};

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

/// Parsed progressive (non-fragmented) MP4 relevant to remux.
#[derive(Debug)]
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

    let ftyp = ftyp.ok_or_else(|| Mp4Error::container("missing ftyp box"))?;
    let moov = moov.ok_or_else(|| Mp4Error::container("missing moov box"))?;
    let mdat = mdat.ok_or_else(|| Mp4Error::container("missing mdat box"))?;

    let (major_brand, compatible_brands) = parse_ftyp(&mut file, &ftyp)?;
    let (mvhd_timescale, mvhd_duration) = parse_mvhd(&mut file, &moov)?;
    let audio = parse_audio_track(&mut file, &moov)?;

    let mut ftyp_bytes = vec![
        0u8;
        usize::try_from(ftyp.size).map_err(|_| {
            Mp4Error::container(format!("ftyp too large: {}", ftyp.size))
        })?
    ];
    file.seek(SeekFrom::Start(ftyp.start))?;
    file.read_exact(&mut ftyp_bytes)?;

    let mut moov_bytes = vec![
        0u8;
        usize::try_from(moov.size).map_err(|_| {
            Mp4Error::container(format!("moov too large: {}", moov.size))
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
    let mvhd = find_child(file, moov.content_start(), moov.end(), MVHD)?
        .ok_or_else(|| Mp4Error::container("missing mvhd"))?;
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
    walk_children(
        file,
        moov.content_start(),
        moov.end(),
        |file, header| -> Result<()> {
            if header.kind != TRAK || audio.is_some() {
                return Ok(());
            }
            if let Some(track) = try_parse_audio_trak(file, header)? {
                audio = Some(track);
            }
            Ok(())
        },
    )?;
    audio.ok_or_else(|| Mp4Error::container("no audio track found in moov"))
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
        .ok_or_else(|| Mp4Error::container("audio track missing mdhd"))?;
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
        .ok_or_else(|| Mp4Error::container("audio track missing minf"))?;
    let stbl = find_child(file, minf.content_start(), minf.end(), STBL)?
        .ok_or_else(|| Mp4Error::container("audio track missing stbl"))?;

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
        .ok_or_else(|| Mp4Error::container("missing stsd"))?;
    file.seek(SeekFrom::Start(stsd.content_start()))?;
    let (_version, _) = read_full_box_version_flags(file)?;
    let entry_count = read_u32(file)?;
    if entry_count == 0 {
        return Err(Mp4Error::container("stsd has no sample entries"));
    }
    // First sample entry header.
    let _entry_size = read_u32(file)?;
    let type_offset = file.stream_position()?;
    let entry_type = read_fourcc(file)?;
    let kind = match entry_type {
        AAVD => SampleEntryKind::Aavd,
        MP4A => SampleEntryKind::Mp4a,
        ENCA => SampleEntryKind::Enca,
        other => SampleEntryKind::Other(other),
    };
    Ok((kind, type_offset))
}

fn parse_stts(file: &mut File, stbl: &BoxHeader) -> Result<Vec<(u32, u32)>> {
    let stts = find_child(file, stbl.content_start(), stbl.end(), STTS)?
        .ok_or_else(|| Mp4Error::container("missing stts"))?;
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
        .ok_or_else(|| Mp4Error::container("missing stsc"))?;
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
        return Err(Mp4Error::container(
            "compact sample size (stz2) is not supported yet",
        ));
    }
    Err(Mp4Error::container("missing stsz"))
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
    Err(Mp4Error::container("missing stco/co64"))
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

/// Clear AAC (`mp4a`) decoder config extracted from a progressive MP4/M4A/M4B.
#[derive(Debug, Clone)]
pub struct Mp4aConfig {
    pub sample_rate: u32,
    pub channels: u16,
    /// AudioSpecificConfig from `esds` DecoderSpecificInfo.
    pub asc: Vec<u8>,
}

/// Read sample-rate / channel count / ASC from a clear `mp4a` sample entry.
pub fn extract_mp4a_config(mp4: &Mp4File) -> Result<Mp4aConfig> {
    if mp4.audio.sample_entry_kind != SampleEntryKind::Mp4a {
        return Err(Mp4Error::container(format!(
            "expected clear mp4a sample entry, found {:?}",
            mp4.audio.sample_entry_kind
        )));
    }
    let moov = &mp4.moov_bytes;
    let type_rel = usize::try_from(
        mp4.audio
            .sample_entry_type_offset
            .checked_sub(mp4.moov.start)
            .ok_or_else(|| Mp4Error::container("sample entry outside moov"))?,
    )
    .map_err(|_| Mp4Error::container("sample entry offset overflow"))?;
    if type_rel < 4 || type_rel + 4 > moov.len() {
        return Err(Mp4Error::container("mp4a type offset invalid"));
    }
    let entry_pos = type_rel - 4;
    let entry_size =
        u32::from_be_bytes(moov[entry_pos..entry_pos + 4].try_into().unwrap()) as usize;
    let entry_end = entry_pos + entry_size;
    if entry_end > moov.len() || entry_size < 36 {
        return Err(Mp4Error::container("mp4a sample entry truncated"));
    }
    // AudioSampleEntry: after size(4)+type(4)+reserved(6)+data_ref(2)+reserved(8)
    // → channelcount(2) + samplesize(2) + pre(2) + reserved(2) + samplerate(4)
    let channels = u16::from_be_bytes(
        moov[entry_pos + 16 + 8..entry_pos + 18 + 8]
            .try_into()
            .unwrap(),
    );
    let rate_fixed = u32::from_be_bytes(
        moov[entry_pos + 24 + 8..entry_pos + 28 + 8]
            .try_into()
            .unwrap(),
    );
    let entry_rate = rate_fixed >> 16;
    let sample_rate = if mp4.audio.timescale > 0 {
        mp4.audio.timescale
    } else {
        entry_rate
    };

    let children_start = entry_pos + 36;
    let esds = find_child_in_range(moov, children_start, entry_end, b"esds")?
        .ok_or_else(|| Mp4Error::container("mp4a missing esds"))?;
    let asc = parse_asc_from_esds(&moov[esds.0..esds.1])?;
    let channels = channels_from_asc(&asc).unwrap_or(channels.max(1));
    if sample_rate == 0 || channels == 0 {
        return Err(Mp4Error::container(
            "mp4a config missing sample rate or channels",
        ));
    }
    Ok(Mp4aConfig {
        sample_rate,
        channels,
        asc,
    })
}

fn parse_asc_from_esds(esds_box: &[u8]) -> Result<Vec<u8>> {
    if esds_box.len() < 12 {
        return Err(Mp4Error::container("esds too small"));
    }
    // size(4)+type(4)+version/flags(4) + descriptors
    let mut i = 12;
    while i < esds_box.len() {
        let tag = esds_box[i];
        i += 1;
        let (len, next) = read_expandable_len(esds_box, i)?;
        i = next;
        let end = i
            .checked_add(len)
            .filter(|e| *e <= esds_box.len())
            .ok_or_else(|| Mp4Error::container("esds descriptor truncated"))?;
        if tag == 0x05 {
            return Ok(esds_box[i..end].to_vec());
        }
        if tag == 0x03 {
            // ES_Descriptor: ES_ID(2) + flags(1) [+ optional] then nested descriptors.
            if end - i < 3 {
                return Err(Mp4Error::container("ES_Descriptor truncated"));
            }
            let flags = esds_box[i + 2];
            let mut nest = i + 3;
            if flags & 0x80 != 0 {
                nest += 2; // dependsOn ES_ID
            }
            if flags & 0x40 != 0 {
                if nest >= end {
                    return Err(Mp4Error::container("ES_Descriptor URL truncated"));
                }
                let url_len = esds_box[nest] as usize;
                nest = nest
                    .checked_add(1 + url_len)
                    .ok_or_else(|| Mp4Error::container("ES_Descriptor URL overflow"))?;
            }
            if flags & 0x20 != 0 {
                nest += 2; // OCR ES_ID
            }
            if let Some(asc) = find_desc_tag(&esds_box[nest..end], 0x05)? {
                return Ok(asc);
            }
        } else if tag == 0x04 {
            // DecoderConfigDescriptor fixed header is 13 bytes, then nested.
            if end - i >= 13 {
                if let Some(asc) = find_desc_tag(&esds_box[i + 13..end], 0x05)? {
                    return Ok(asc);
                }
            }
        }
        i = end;
    }
    Err(Mp4Error::container(
        "esds missing DecoderSpecificInfo (tag 0x05)",
    ))
}

fn find_desc_tag(buf: &[u8], want: u8) -> Result<Option<Vec<u8>>> {
    let mut i = 0usize;
    while i < buf.len() {
        let tag = buf[i];
        i += 1;
        let (len, next) = read_expandable_len(buf, i)?;
        i = next;
        let end = i
            .checked_add(len)
            .filter(|e| *e <= buf.len())
            .ok_or_else(|| Mp4Error::container("descriptor truncated"))?;
        if tag == want {
            return Ok(Some(buf[i..end].to_vec()));
        }
        if tag == 0x04 && end - i >= 13 {
            if let Some(asc) = find_desc_tag(&buf[i + 13..end], want)? {
                return Ok(Some(asc));
            }
        }
        i = end;
    }
    Ok(None)
}

fn read_expandable_len(buf: &[u8], mut i: usize) -> Result<(usize, usize)> {
    let mut length = 0usize;
    for _ in 0..4 {
        if i >= buf.len() {
            return Err(Mp4Error::container("expandable length truncated"));
        }
        let b = buf[i];
        i += 1;
        length = (length << 7) | usize::from(b & 0x7f);
        if b & 0x80 == 0 {
            return Ok((length, i));
        }
    }
    Err(Mp4Error::container("expandable length too long"))
}

fn channels_from_asc(asc: &[u8]) -> Option<u16> {
    // Minimal ASC: AOT(5) + samplingFrequencyIndex(4) + channelConfiguration(4)
    if asc.len() < 2 {
        return None;
    }
    let freq_idx = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
    let chan = if freq_idx == 0x0f {
        // Explicit frequency: 24-bit rate follows; channel config after that.
        // AOT(5) + idx(4) + rate(24) = 33 bits → channel bits touch asc[4] and asc[5].
        if asc.len() < 6 {
            return None;
        }
        ((asc[4] >> 7) & 0x01) << 3 | ((asc[5] >> 4) & 0x07)
    } else {
        (asc[1] >> 3) & 0x0f
    };
    if chan == 0 {
        None
    } else {
        Some(u16::from(chan))
    }
}

#[cfg(test)]
mod channels_from_asc_tests {
    use super::channels_from_asc;

    #[test]
    fn explicit_freq_needs_six_bytes() {
        // AOT=2 (AAC LC), freq_idx=0x0f → would formerly panic on 5-byte ASC.
        let short = [0x17, 0x80, 0x00, 0x00, 0x00];
        assert_eq!(channels_from_asc(&short), None);
        // 6 bytes: channelConfiguration = 2 (stereo) after explicit 24-bit rate.
        let ok = [0x17, 0x80, 0x00, 0x00, 0x00, 0x20];
        assert_eq!(channels_from_asc(&ok), Some(2));
    }

    #[test]
    fn indexed_freq_stereo() {
        // AOT=2, freq_idx=3 (48 kHz), channels=2
        let asc = [0x11, 0x90];
        assert_eq!(channels_from_asc(&asc), Some(2));
    }
}
