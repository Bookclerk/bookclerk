//! Minimal progressive AAC-LC → M4B muxer (single audio track).

use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::error::{DecryptError, Result};

/// One AAC access unit ready for `mdat`.
#[derive(Debug, Clone)]
pub struct AacAccessUnit {
    pub data: Vec<u8>,
    /// Sample duration in media timescale ticks (usually = frame_length).
    pub duration: u32,
}

/// Parameters for writing a single-track AAC-LC M4B.
#[derive(Debug, Clone)]
pub struct MuxAacRequest<'a> {
    pub sample_rate: u32,
    pub channels: u16,
    /// AudioSpecificConfig bytes from the encoder (`confBuf`).
    pub asc: &'a [u8],
    pub samples: &'a [AacAccessUnit],
}

/// Write `ftyp` + `mdat` + `moov` progressive M4B.
pub fn write_aac_m4b(output: &Path, req: &MuxAacRequest<'_>) -> Result<()> {
    if req.samples.is_empty() {
        return Err(DecryptError::Mp4("no AAC samples to mux".into()));
    }
    if req.sample_rate == 0 {
        return Err(DecryptError::Mp4("sample rate must be non-zero".into()));
    }
    if req.channels == 0 {
        return Err(DecryptError::Mp4("channel count must be non-zero".into()));
    }
    if req.asc.is_empty() {
        return Err(DecryptError::Mp4("AudioSpecificConfig is empty".into()));
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let ftyp = build_m4b_ftyp();
    let sample_sizes: Vec<u32> = req.samples.iter().map(|s| s.data.len() as u32).collect();
    let durations: Vec<u32> = req.samples.iter().map(|s| s.duration).collect();
    let media_duration: u64 = durations.iter().map(|d| u64::from(*d)).sum();
    let payload_total: u64 = sample_sizes.iter().map(|s| u64::from(*s)).sum();

    // Layout: ftyp | mdat(header+payload) | moov
    // Use 32-bit mdat size when it fits; otherwise 64-bit extended size.
    let mdat_header_len: u64 = if payload_total + 8 > u64::from(u32::MAX) {
        16
    } else {
        8
    };
    let mdat_file_offset = ftyp.len() as u64;
    let first_sample_offset = mdat_file_offset + mdat_header_len;

    let mut chunk_offsets = Vec::with_capacity(sample_sizes.len());
    let mut offset = first_sample_offset;
    for &size in &sample_sizes {
        chunk_offsets.push(offset);
        offset = offset.saturating_add(u64::from(size));
    }

    let moov = build_moov(
        req.sample_rate,
        req.channels,
        req.asc,
        &sample_sizes,
        &durations,
        &chunk_offsets,
        media_duration,
    )?;

    let mut out = File::create(output)?;
    out.write_all(&ftyp)?;

    let mdat_size = mdat_header_len + payload_total;
    if mdat_header_len == 16 {
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&mdat_size.to_be_bytes())?;
    } else {
        out.write_all(&(mdat_size as u32).to_be_bytes())?;
        out.write_all(b"mdat")?;
    }
    for sample in req.samples {
        out.write_all(&sample.data)?;
    }
    out.write_all(&moov)?;
    out.sync_all()?;
    Ok(())
}

fn build_m4b_ftyp() -> Vec<u8> {
    let brands: &[&[u8; 4]] = &[b"M4B ", b"mp42", b"isom"];
    let size = 8 + 8 + brands.len() * 4;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(b"M4B ");
    buf.extend_from_slice(&0u32.to_be_bytes());
    for b in brands {
        buf.extend_from_slice(*b);
    }
    buf
}

fn build_moov(
    sample_rate: u32,
    channels: u16,
    asc: &[u8],
    sample_sizes: &[u32],
    durations: &[u32],
    chunk_offsets: &[u64],
    media_duration: u64,
) -> Result<Vec<u8>> {
    let timescale = sample_rate;
    let movie_duration = media_duration;

    let stsd = encode_stsd_mp4a(sample_rate, channels, asc)?;
    let stts = encode_stts(durations);
    let stsc = encode_stsc_one_per_chunk();
    let stsz = encode_stsz(sample_sizes);
    let need_co64 = chunk_offsets.iter().any(|&o| o > u64::from(u32::MAX));
    let stco = if need_co64 {
        encode_co64(chunk_offsets)
    } else {
        encode_stco(&chunk_offsets.iter().map(|&o| o as u32).collect::<Vec<_>>())
    };
    let stbl = wrap_box(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
    let smhd = encode_smhd();
    let dinf = encode_dinf();
    let minf = wrap_box(b"minf", &[smhd, dinf, stbl].concat());
    let mdhd = encode_mdhd(timescale, media_duration);
    let hdlr = encode_hdlr();
    let mdia = wrap_box(b"mdia", &[mdhd, hdlr, minf].concat());
    let tkhd = encode_tkhd(1, movie_duration);
    let trak = wrap_box(b"trak", &[tkhd, mdia].concat());
    let mvhd = encode_mvhd(timescale, movie_duration);
    Ok(wrap_box(b"moov", &[mvhd, trak].concat()))
}

fn encode_stsd_mp4a(sample_rate: u32, channels: u16, asc: &[u8]) -> Result<Vec<u8>> {
    let esds = encode_esds(asc)?;
    // AudioSampleEntry (`mp4a`) body after size+type.
    let mut entry = Vec::new();
    entry.extend_from_slice(&0u32.to_be_bytes()); // size placeholder
    entry.extend_from_slice(b"mp4a");
    entry.extend_from_slice(&[0u8; 6]); // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    entry.extend_from_slice(&[0u8; 8]); // reserved
    entry.extend_from_slice(&channels.to_be_bytes());
    entry.extend_from_slice(&16u16.to_be_bytes()); // sample_size
    entry.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    entry.extend_from_slice(&0u16.to_be_bytes()); // reserved
    entry.extend_from_slice(&(sample_rate << 16).to_be_bytes()); // sample_rate 16.16
    entry.extend_from_slice(&esds);
    let entry_size = entry.len() as u32;
    entry[0..4].copy_from_slice(&entry_size.to_be_bytes());

    let mut stsd = Vec::new();
    let size = 8 + 4 + 4 + entry.len();
    stsd.extend_from_slice(&(size as u32).to_be_bytes());
    stsd.extend_from_slice(b"stsd");
    stsd.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    stsd.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    stsd.extend_from_slice(&entry);
    Ok(stsd)
}

/// Minimal `esds` with ES_Descriptor → DecoderConfigDescriptor → DecoderSpecificInfo.
fn encode_esds(asc: &[u8]) -> Result<Vec<u8>> {
    // DecoderSpecificInfo (tag 0x05)
    let mut dsi = Vec::new();
    dsi.push(0x05);
    dsi.extend_from_slice(&encode_expandable_length(asc.len())?);
    dsi.extend_from_slice(asc);

    // DecoderConfigDescriptor (tag 0x04)
    // objectTypeIndication=0x40 (Audio ISO/IEC 14496-3), streamType=0x05<<2 | 1 = 0x15
    let mut dcd = Vec::new();
    dcd.push(0x04);
    let dcd_payload_len = 13 + dsi.len();
    dcd.extend_from_slice(&encode_expandable_length(dcd_payload_len)?);
    dcd.push(0x40); // Audio
    dcd.push(0x15); // streamType=audio, upstream=0, reserved=1
    dcd.extend_from_slice(&[0u8; 3]); // bufferSizeDB
    dcd.extend_from_slice(&0u32.to_be_bytes()); // maxBitrate
    dcd.extend_from_slice(&0u32.to_be_bytes()); // avgBitrate
    dcd.extend_from_slice(&dsi);

    // SLConfigDescriptor (tag 0x06), predefined=2 (MP4)
    let sl = [0x06u8, 0x01, 0x02];

    // ES_Descriptor (tag 0x03): ES_ID=0, flags=0
    let mut es = Vec::new();
    es.push(0x03);
    let es_payload_len = 3 + dcd.len() + sl.len();
    es.extend_from_slice(&encode_expandable_length(es_payload_len)?);
    es.extend_from_slice(&0u16.to_be_bytes()); // ES_ID
    es.push(0x00); // flags
    es.extend_from_slice(&dcd);
    es.extend_from_slice(&sl);

    let mut esds = Vec::new();
    let size = 8 + 4 + es.len();
    esds.extend_from_slice(&(size as u32).to_be_bytes());
    esds.extend_from_slice(b"esds");
    esds.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    esds.extend_from_slice(&es);
    Ok(esds)
}

fn encode_expandable_length(len: usize) -> Result<Vec<u8>> {
    // ISO 14496-1 expandable length: 1–4 bytes with continuation bits.
    if len < 0x80 {
        Ok(vec![len as u8])
    } else if len < 0x4000 {
        Ok(vec![0x80 | ((len >> 7) as u8), (len & 0x7F) as u8])
    } else if len < 0x20_0000 {
        Ok(vec![
            0x80 | ((len >> 14) as u8),
            0x80 | (((len >> 7) & 0x7F) as u8),
            (len & 0x7F) as u8,
        ])
    } else if len <= 0x0FFF_FFFF {
        Ok(vec![
            0x80 | ((len >> 21) as u8),
            0x80 | (((len >> 14) & 0x7F) as u8),
            0x80 | (((len >> 7) & 0x7F) as u8),
            (len & 0x7F) as u8,
        ])
    } else {
        Err(DecryptError::Mp4("descriptor length too large".into()))
    }
}

fn encode_stts(durations: &[u32]) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in durations {
        if let Some(last) = runs.last_mut() {
            if last.1 == d {
                last.0 += 1;
                continue;
            }
        }
        runs.push((1, d));
    }
    let mut buf = Vec::new();
    let size = 8 + 4 + 4 + runs.len() * 8;
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stts");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (count, delta) in runs {
        buf.extend_from_slice(&count.to_be_bytes());
        buf.extend_from_slice(&delta.to_be_bytes());
    }
    buf
}

fn encode_stsc_one_per_chunk() -> Vec<u8> {
    let size = 8 + 4 + 4 + 12;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stsc");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf
}

fn encode_stsz(sizes: &[u32]) -> Vec<u8> {
    let size = 8 + 4 + 4 + 4 + sizes.len() * 4;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stsz");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    for s in sizes {
        buf.extend_from_slice(&s.to_be_bytes());
    }
    buf
}

fn encode_stco(offsets: &[u32]) -> Vec<u8> {
    let size = 8 + 4 + 4 + offsets.len() * 4;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stco");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
    for o in offsets {
        buf.extend_from_slice(&o.to_be_bytes());
    }
    buf
}

fn encode_co64(offsets: &[u64]) -> Vec<u8> {
    let size = 8 + 4 + 4 + offsets.len() * 8;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"co64");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
    for o in offsets {
        buf.extend_from_slice(&o.to_be_bytes());
    }
    buf
}

fn encode_smhd() -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&16u32.to_be_bytes());
    b.extend_from_slice(b"smhd");
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b
}

fn encode_dinf() -> Vec<u8> {
    let mut dref_entry = Vec::new();
    dref_entry.extend_from_slice(&12u32.to_be_bytes());
    dref_entry.extend_from_slice(b"url ");
    dref_entry.extend_from_slice(&1u32.to_be_bytes()); // self-contained
    let mut dref = Vec::new();
    let dref_size = 8 + 4 + 4 + dref_entry.len();
    dref.extend_from_slice(&(dref_size as u32).to_be_bytes());
    dref.extend_from_slice(b"dref");
    dref.extend_from_slice(&0u32.to_be_bytes());
    dref.extend_from_slice(&1u32.to_be_bytes());
    dref.extend_from_slice(&dref_entry);
    wrap_box(b"dinf", &dref)
}

fn encode_hdlr() -> Vec<u8> {
    let name = b"SoundHandler\0";
    let size = 8 + 4 + 4 + 4 + 12 + name.len();
    let mut b = Vec::with_capacity(size);
    b.extend_from_slice(&(size as u32).to_be_bytes());
    b.extend_from_slice(b"hdlr");
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(b"soun");
    b.extend_from_slice(&[0u8; 12]);
    b.extend_from_slice(name);
    b
}

fn encode_mdhd(timescale: u32, duration: u64) -> Vec<u8> {
    if duration > u64::from(u32::MAX) {
        // version 1
        let size = 8 + 4 + 8 + 8 + 4 + 8 + 2 + 2;
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(&(size as u32).to_be_bytes());
        b.extend_from_slice(b"mdhd");
        b.extend_from_slice(&0x0100_0000u32.to_be_bytes()); // version=1
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&timescale.to_be_bytes());
        b.extend_from_slice(&duration.to_be_bytes());
        b.extend_from_slice(&0x55c4u16.to_be_bytes()); // und
        b.extend_from_slice(&0u16.to_be_bytes());
        b
    } else {
        let size = 8 + 4 + 4 + 4 + 4 + 4 + 2 + 2;
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(&(size as u32).to_be_bytes());
        b.extend_from_slice(b"mdhd");
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&timescale.to_be_bytes());
        b.extend_from_slice(&(duration as u32).to_be_bytes());
        b.extend_from_slice(&0x55c4u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b
    }
}

fn encode_tkhd(track_id: u32, duration: u64) -> Vec<u8> {
    // version 0, flags=TrackEnabled|TrackInMovie|TrackInPreview
    let size = 92;
    let mut b = Vec::with_capacity(size);
    b.extend_from_slice(&(size as u32).to_be_bytes());
    b.extend_from_slice(b"tkhd");
    b.extend_from_slice(&0x0000_0003u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&track_id.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&(duration.min(u64::from(u32::MAX)) as u32).to_be_bytes());
    b.extend_from_slice(&[0u8; 8]);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0x0100u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    let matrix = [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
    for v in matrix {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    debug_assert_eq!(b.len(), size);
    b
}

fn encode_mvhd(timescale: u32, duration: u64) -> Vec<u8> {
    let size = 108;
    let mut b = Vec::with_capacity(size);
    b.extend_from_slice(&(size as u32).to_be_bytes());
    b.extend_from_slice(b"mvhd");
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&(duration.min(u64::from(u32::MAX)) as u32).to_be_bytes());
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    b.extend_from_slice(&0x0100u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&[0u8; 8]);
    let matrix = [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
    for v in matrix {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&[0u8; 24]);
    b.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID
    debug_assert_eq!(b.len(), size);
    b
}

fn wrap_box(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let size = 8 + content.len();
    let mut b = Vec::with_capacity(size);
    b.extend_from_slice(&(size as u32).to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(content);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expandable_length_small() {
        assert_eq!(encode_expandable_length(0).unwrap(), vec![0]);
        assert_eq!(encode_expandable_length(5).unwrap(), vec![5]);
        assert_eq!(encode_expandable_length(127).unwrap(), vec![127]);
    }
}
