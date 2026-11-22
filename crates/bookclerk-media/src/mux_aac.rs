//! Minimal progressive single-track M4B muxer: AAC-LC from the encoder, or
//! any audio whose MP4 sample entry the caller supplies (Opus, FLAC, ALAC,
//! E-AC-3, AC-4 copied without re-encoding).
//!
//! The finished file is faststart (`ftyp`, `moov`, `mdat`), so a player or
//! the web app can start a book after reading its first bytes instead of
//! seeking to the end for the sample table.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{MediaError, Result};

/// Buffer for the access-unit copy. An AU is a few hundred bytes and an
/// audiobook has a couple of million of them, so an unbuffered handle would
/// spend the run in `write`/`read` syscalls.
pub(crate) const IO_BUFFER_BYTES: usize = 1 << 20;

/// Sample timing for a muxed track.
#[derive(Debug, Clone, Copy)]
enum AuTiming<'a> {
    /// Every access unit has the same duration (typical fdk-aac encode).
    Uniform(u32),
    /// Per-AU durations (lossless remux of existing AAC).
    Variable(&'a [u32]),
}

/// Writes a single-track AAC-LC M4B as its access units arrive.
///
/// `mdat` has to declare its length before any of the media, and the sample
/// table cannot be finished until the last unit is written — only the encoder
/// knows how big each one comes out. Reserving the 64-bit size field costs eight
/// bytes and settles both: the units stream straight to the output and the
/// length is patched in at the end. The alternative is parking the whole encode
/// somewhere first, which for a book is a second full-size write and a full-size
/// read back.
///
/// [`Self::finish`] then moves `mdat` forward in place by the size of
/// `moov` and writes `moov` into the gap, so the book is faststart. That is
/// one more pass over the media, inside the same file, so the disk never
/// holds two copies.
pub struct AacM4bWriter {
    /// Buffered output file (opened for reading too, for the move in
    /// [`Self::finish`]); `mdat` is streamed into it.
    out: BufWriter<File>,
    /// Where the `mdat` largesize field sits, to be patched on [`Self::finish`].
    size_field: u64,
    /// File offset of the first AAC access unit inside `mdat`.
    first_sample: u64,
    /// Media timescale in ticks per second (the audio's sample rate, or
    /// 48 000 for Opus).
    timescale: u32,
    /// The whole sample entry box written into `stsd` (`mp4a` with its
    /// `esds`, or another codec's entry).
    entry: Vec<u8>,
    /// Ticks of decoded audio at the start that are not played (Opus
    /// pre-skip), written as an edit list; 0 writes none.
    skip: u32,
    /// Per-access-unit payload sizes in bytes, in encode order.
    sizes: Vec<u32>,
    /// The duration the first unit claimed, and the one the rest are compared to.
    uniform: Option<u32>,
    /// Stays empty while every unit matches `uniform`, which is the whole of an
    /// encoder's output. A book runs to a couple of million units, so the table
    /// nobody needs is worth not keeping.
    durations: Vec<u32>,
    /// Total AAC payload bytes written into `mdat` (excludes the 16-byte header).
    payload: u64,
}

impl AacM4bWriter {
    /// Opens `output` for AAC with an `mp4a` sample entry, writes `ftyp`,
    /// and reserves a 64-bit `mdat` size field.
    pub fn create(output: &Path, sample_rate: u32, channels: u16, asc: &[u8]) -> Result<Self> {
        if channels == 0 {
            return Err(MediaError::Mp4("channel count must be non-zero".into()));
        }
        if asc.is_empty() {
            return Err(MediaError::Mp4("AudioSpecificConfig is empty".into()));
        }
        let entry = encode_mp4a_entry(sample_rate, channels, asc)?;
        Self::create_with_entry(output, sample_rate, entry)
    }

    /// Opens `output` for audio described by `entry`, a whole MP4 sample
    /// entry box, with sample durations in `timescale` ticks per second.
    ///
    /// # Errors
    ///
    /// [`MediaError::Mp4`] for a zero timescale or an entry that is not a
    /// box of its stated size; an I/O error when the file cannot be opened.
    pub fn create_with_entry(output: &Path, timescale: u32, entry: Vec<u8>) -> Result<Self> {
        if timescale == 0 {
            return Err(MediaError::Mp4("sample rate must be non-zero".into()));
        }
        let stated = entry
            .get(..4)
            .and_then(|size| <[u8; 4]>::try_from(size).ok())
            .map(u32::from_be_bytes)
            .and_then(|size| usize::try_from(size).ok());
        if entry.len() < 8 || stated != Some(entry.len()) {
            return Err(MediaError::Mp4("sample entry is not one whole box".into()));
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let ftyp = build_m4b_ftyp();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(output)?;
        let mut out = BufWriter::with_capacity(IO_BUFFER_BYTES, file);
        out.write_all(&ftyp)?;
        // The extended-size form, always: the length is not known yet, and this
        // is the only header that can be corrected without moving the media.
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;

        let mdat = ftyp.len() as u64;
        Ok(Self {
            out,
            size_field: mdat + 8,
            first_sample: mdat + MDAT_HEADER_BYTES,
            timescale,
            entry,
            skip: 0,
            sizes: Vec::new(),
            uniform: None,
            durations: Vec::new(),
            payload: 0,
        })
    }

    /// Appends one AAC access unit and records its size and duration in timescale ticks.
    pub fn push(&mut self, au: &[u8], duration: u32) -> Result<()> {
        if au.is_empty() {
            return Err(MediaError::Mp4("AAC access unit is empty".into()));
        }
        if duration == 0 {
            return Err(MediaError::Mp4("sample durations must be non-zero".into()));
        }
        match self.uniform {
            None => self.uniform = Some(duration),
            Some(first) if first != duration && self.durations.is_empty() => {
                self.durations = vec![first; self.sizes.len()];
                self.durations.push(duration);
            }
            Some(_) if !self.durations.is_empty() => self.durations.push(duration),
            Some(_) => {}
        }
        self.out.write_all(au)?;
        self.sizes.push(au.len() as u32);
        self.payload += au.len() as u64;
        Ok(())
    }

    /// Marks the first `skip` ticks of decoded audio as not played (Opus
    /// pre-skip): the file gets an edit list that starts the track there.
    pub fn set_skip(&mut self, skip: u32) {
        self.skip = skip;
    }

    /// Number of AAC access units written so far.
    pub fn samples(&self) -> usize {
        self.sizes.len()
    }

    /// Patches the `mdat` length, moves `mdat` forward to make room for
    /// `moov` right after `ftyp`, writes `moov` there, and syncs the file;
    /// empty input fails.
    pub fn finish(self) -> Result<()> {
        if self.sizes.is_empty() {
            return Err(MediaError::Mp4("no AAC samples to mux".into()));
        }
        let timing = match self.uniform {
            Some(duration) if self.durations.is_empty() => AuTiming::Uniform(duration),
            _ => AuTiming::Variable(&self.durations),
        };
        let media_duration = media_duration(timing, self.sizes.len())?;
        // All samples in one chunk keeps stco/co64 to a single entry however
        // long the book runs. The chunk sits `moov` bytes later than it was
        // written, and `moov`'s size depends on whether that offset needs
        // co64, so build until the size settles (at most twice: the offset
        // only grows, and only once from stco to co64).
        let mut moov_len = 0u64;
        let moov = loop {
            let mut moov = build_moov(
                self.timescale,
                &self.entry,
                self.skip,
                &self.sizes,
                timing,
                &[self.first_sample + moov_len],
                media_duration,
            )?;
            // Tags, a cover and the chapter track are written into `moov`
            // after the media is on disk. The reserved slack takes them in
            // place; without it every edit rebuilds the whole file beside
            // the original and renames it over, which a confined worker on
            // Windows may not be allowed to do.
            let reserved = moov.len() + bookclerk_mp4::RESERVED_MOOV_SLACK;
            bookclerk_mp4::pad_moov_to(&mut moov, reserved)?;
            let len = moov.len() as u64;
            if len == moov_len {
                break moov;
            }
            moov_len = len;
        };

        let mut file = self
            .out
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        file.seek(SeekFrom::Start(self.size_field))?;
        file.write_all(&(MDAT_HEADER_BYTES + self.payload).to_be_bytes())?;
        let mdat_start = self.size_field - 8;
        let mdat_len = MDAT_HEADER_BYTES + self.payload;
        shift_forward(&mut file, mdat_start, mdat_len, moov_len)?;
        file.seek(SeekFrom::Start(mdat_start))?;
        file.write_all(&moov)?;
        file.sync_all()?;
        Ok(())
    }
}

/// Moves the `len` bytes at `start` forward by `by` bytes within `file`,
/// copying blocks from the end backwards so no byte is overwritten before
/// it has been read. The bytes left at `start..start + by` are stale.
fn shift_forward(file: &mut File, start: u64, len: u64, by: u64) -> Result<()> {
    use std::io::Read;
    let mut buf = vec![0u8; IO_BUFFER_BYTES];
    let mut remaining = len;
    while remaining > 0 {
        let block = remaining.min(buf.len() as u64);
        let from = start + remaining - block;
        let chunk = &mut buf[..usize::try_from(block).unwrap_or(IO_BUFFER_BYTES)];
        file.seek(SeekFrom::Start(from))?;
        file.read_exact(chunk)?;
        file.seek(SeekFrom::Start(from + by))?;
        file.write_all(chunk)?;
        remaining -= block;
    }
    Ok(())
}

/// A `size == 1` header: the four-byte marker, the type, and the real length.
const MDAT_HEADER_BYTES: u64 = 16;

/// Sums access-unit durations in timescale ticks; zero durations or count mismatches fail.
fn media_duration(timing: AuTiming<'_>, samples: usize) -> Result<u64> {
    match timing {
        AuTiming::Uniform(duration) => {
            if duration == 0 {
                return Err(MediaError::Mp4("sample duration must be non-zero".into()));
            }
            Ok(u64::from(duration).saturating_mul(samples as u64))
        }
        AuTiming::Variable(durations) => {
            if durations.len() != samples {
                return Err(MediaError::Mp4(format!(
                    "size/duration count mismatch: {samples} vs {}",
                    durations.len()
                )));
            }
            if durations.contains(&0) {
                return Err(MediaError::Mp4("sample durations must be non-zero".into()));
            }
            Ok(durations.iter().map(|d| u64::from(*d)).sum())
        }
    }
}

/// Builds an `ftyp` box with major brand `M4B ` and compatible `mp42`/`isom`.
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

/// Builds a single-track `moov` (`mvhd`/`trak`/`mdia`/`stbl`) for the streamed `mdat`.
fn build_moov(
    timescale: u32,
    entry: &[u8],
    skip: u32,
    sample_sizes: &[u32],
    timing: AuTiming<'_>,
    chunk_offsets: &[u64],
    media_duration: u64,
) -> Result<Vec<u8>> {
    let skip = u64::from(skip).min(media_duration);
    // The movie plays what follows the skipped start.
    let movie_duration = media_duration - skip;

    let stsd = encode_stsd(entry);
    let stts = match timing {
        AuTiming::Uniform(duration) => encode_stts_uniform(sample_sizes.len() as u32, duration),
        AuTiming::Variable(durations) => encode_stts_variable(durations),
    };
    let stsc = encode_stsc_all_in_one_chunk(sample_sizes.len() as u32);
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
    let trak = if skip > 0 {
        let edts = wrap_box(b"edts", &encode_elst(movie_duration, skip));
        wrap_box(b"trak", &[tkhd, edts, mdia].concat())
    } else {
        wrap_box(b"trak", &[tkhd, mdia].concat())
    };
    let mvhd = encode_mvhd(timescale, movie_duration);
    Ok(wrap_box(b"moov", &[mvhd, trak].concat()))
}

/// Encodes an `mp4a` sample entry with an `esds` AudioSpecificConfig.
fn encode_mp4a_entry(sample_rate: u32, channels: u16, asc: &[u8]) -> Result<Vec<u8>> {
    let esds = encode_esds(asc)?;
    Ok(audio_sample_entry(*b"mp4a", channels, sample_rate, &esds))
}

/// An AudioSampleEntry box of type `kind` (ISO/IEC 14496-12): the
/// reserved fields, `channels`, 16-bit samples, `sample_rate` in 16.16
/// fixed point (0 when it does not fit 16 bits), then `children` (the
/// codec's configuration box).
pub(crate) fn audio_sample_entry(
    kind: [u8; 4],
    channels: u16,
    sample_rate: u32,
    children: &[u8],
) -> Vec<u8> {
    let rate = if sample_rate <= u32::from(u16::MAX) {
        sample_rate << 16
    } else {
        0
    };
    let mut body = Vec::with_capacity(28 + children.len());
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    body.extend_from_slice(&[0u8; 8]); // reserved
    body.extend_from_slice(&channels.to_be_bytes());
    body.extend_from_slice(&16u16.to_be_bytes()); // sample_size
    body.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    body.extend_from_slice(&0u16.to_be_bytes()); // reserved
    body.extend_from_slice(&rate.to_be_bytes()); // sample_rate 16.16
    body.extend_from_slice(children);
    wrap_box(&kind, &body)
}

/// Encodes `stsd` with the one sample entry `entry`.
fn encode_stsd(entry: &[u8]) -> Vec<u8> {
    let mut stsd = Vec::new();
    let size = 8 + 4 + 4 + entry.len();
    stsd.extend_from_slice(&(size as u32).to_be_bytes());
    stsd.extend_from_slice(b"stsd");
    stsd.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    stsd.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    stsd.extend_from_slice(entry);
    stsd
}

/// Encodes an `elst` with one edit that plays `duration` ticks from media
/// time `skip` at normal rate (version 1 when either needs 64 bits).
fn encode_elst(duration: u64, skip: u64) -> Vec<u8> {
    let mut b = Vec::new();
    if duration > u64::from(u32::MAX) || skip > u64::from(u32::MAX >> 1) {
        b.extend_from_slice(&0x0100_0000u32.to_be_bytes()); // version 1
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&duration.to_be_bytes());
        b.extend_from_slice(&skip.to_be_bytes());
    } else {
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&(duration as u32).to_be_bytes());
        b.extend_from_slice(&(skip as u32).to_be_bytes());
    }
    b.extend_from_slice(&1u16.to_be_bytes()); // media_rate_integer
    b.extend_from_slice(&0u16.to_be_bytes()); // media_rate_fraction
    wrap_box(b"elst", &b)
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

/// Encodes an ISO 14496-1 expandable length (1–4 bytes); lengths above 28 bits fail.
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
        Err(MediaError::Mp4("descriptor length too large".into()))
    }
}

/// Encodes `stts` as a single run when every access unit has the same duration.
fn encode_stts_uniform(sample_count: u32, duration: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    let size = 8 + 4 + 4 + 8;
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stts");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes()); // one run
    buf.extend_from_slice(&sample_count.to_be_bytes());
    buf.extend_from_slice(&duration.to_be_bytes());
    buf
}

/// Encodes `stts` as run-length pairs when access-unit durations vary.
fn encode_stts_variable(durations: &[u32]) -> Vec<u8> {
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

/// Encodes `stsc` with one chunk that holds every sample.
fn encode_stsc_all_in_one_chunk(samples_per_chunk: u32) -> Vec<u8> {
    let size = 8 + 4 + 4 + 12;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stsc");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    buf.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
    buf.extend_from_slice(&samples_per_chunk.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
    buf
}

/// Encodes `stsz` with a per-sample size table (`sample_size` field left at 0).
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

/// Encodes 32-bit `stco` chunk offsets when every offset fits in `u32`.
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

/// Encodes 64-bit `co64` chunk offsets when any offset exceeds `u32::MAX`.
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

/// Encodes a zero-balance `smhd` sound-media header.
fn encode_smhd() -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&16u32.to_be_bytes());
    b.extend_from_slice(b"smhd");
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b
}

/// Encodes `dinf`/`dref` with a self-contained `url ` entry (media is in this file).
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

/// Encodes an `hdlr` box with handler type `soun`.
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

/// Encodes `mdhd` (version 1 when duration exceeds `u32::MAX`) with language `und`.
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

/// Encodes `tkhd` with the track enabled in the movie and preview.
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

/// Encodes `mvhd` with `next_track_ID` 2 (single audio track).
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

/// Prefixes `content` with a 32-bit size and a fourcc to form an ISO-BMFF box.
pub(crate) fn wrap_box(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
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

    /// Top-level box types of `bytes`, in order.
    fn top_level_boxes(bytes: &[u8]) -> Vec<[u8; 4]> {
        let mut at = 0usize;
        let mut kinds = Vec::new();
        while at + 8 <= bytes.len() {
            let mut size = u64::from(u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()));
            let kind: [u8; 4] = bytes[at + 4..at + 8].try_into().unwrap();
            if size == 1 {
                size = u64::from_be_bytes(bytes[at + 8..at + 16].try_into().unwrap());
            }
            kinds.push(kind);
            at += usize::try_from(size).unwrap().max(8);
        }
        assert_eq!(at, bytes.len(), "boxes do not tile the file");
        kinds
    }

    #[test]
    fn finished_files_are_faststart_and_play_the_same_units() {
        let dir = tempfile::tempdir().unwrap();
        // Enough units to need several shift blocks.
        let units: Vec<Vec<u8>> = (0..3_000u32)
            .map(|i| vec![u8::try_from(i % 251).unwrap(); 500 + (i % 7) as usize])
            .collect();
        let path = dir.path().join("fast.m4b");
        let mut writer = AacM4bWriter::create(&path, 44_100, 2, &[0x12, 0x10]).unwrap();
        for unit in &units {
            writer.push(unit, 1024).unwrap();
        }
        writer.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(top_level_boxes(&bytes), [*b"ftyp", *b"moov", *b"mdat"]);
        let (at, _) = bookclerk_mp4::read_moov(&path).unwrap();
        assert!(
            at.len > u64::try_from(bookclerk_mp4::RESERVED_MOOV_SLACK).unwrap(),
            "moov reserves slack for later edits"
        );
        let mp4 = bookclerk_mp4::parse_mp4(&path).unwrap();
        assert_eq!(mp4.audio.samples.len(), units.len());
        let mut reader = bookclerk_mp4::SampleReader::open(&path).unwrap();
        let mut buf = Vec::new();
        for (sample, unit) in mp4.audio.samples.iter().zip(&units) {
            reader
                .read_sample(sample.offset, sample.size as usize, &mut buf)
                .unwrap();
            assert_eq!(&buf, unit);
        }
    }

    #[test]
    fn shifting_moves_every_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shift.bin");
        let data: Vec<u8> = (0..(IO_BUFFER_BYTES * 2 + 17))
            .map(|i| u8::try_from(i % 253).unwrap())
            .collect();
        std::fs::write(&path, &data).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        shift_forward(&mut file, 5, data.len() as u64 - 5, 1_000).unwrap();
        drop(file);
        let moved = std::fs::read(&path).unwrap();
        assert_eq!(moved.len(), data.len() + 1_000);
        assert_eq!(&moved[..5], &data[..5]);
        assert_eq!(&moved[1_005..], &data[5..]);
    }
}
