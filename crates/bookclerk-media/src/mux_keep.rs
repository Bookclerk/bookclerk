//! Packaging audio into an M4B without re-encoding it, for codecs MP4 can
//! hold beside AAC: Opus (`Opus` + `dOps`), FLAC (`fLaC` + `dfLa`), ALAC,
//! E-AC-3 and AC-4.
//!
//! Only an operator's `output.keep_codec_in_m4b` asks for this: Apple Books
//! and some other players open none of these codecs in MP4. Parts that
//! cannot be kept together (different codecs or stream settings, or a
//! codec MP4 cannot hold here) are left to the AAC transcode.

use std::fs::File;
use std::path::Path;

use bookclerk_mp4::boxutil::FourCC;
use bookclerk_mp4::{parse_mp4, sample_entry_bytes, SampleEntryKind, SampleReader};
use symphonia::core::codecs::audio::well_known::{CODEC_ID_FLAC, CODEC_ID_OPUS};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::error::{MediaError, Result};
use crate::mux_aac::{audio_sample_entry, wrap_box, AacM4bWriter};

/// Sample entry types copied from another MP4 as they are.
const COPIED_ENTRIES: [&[u8; 4]; 5] = [b"alac", b"fLaC", b"Opus", b"ec-3", b"ac-4"];

/// Opus is timed at 48 kHz in MP4, whatever rate it was encoded from.
const OPUS_TIMESCALE: u32 = 48_000;

/// How one part's audio is read and described in the M4B.
enum KeptPart {
    /// An MP4 whose sample entry is copied and whose samples are read by
    /// its own sample table.
    Mp4 {
        /// The whole sample entry box.
        entry: Vec<u8>,
        /// Media timescale of the track.
        timescale: u32,
    },
    /// An Ogg Opus or native FLAC stream, demuxed packet by packet.
    Demuxed {
        /// The sample entry built from the stream's header.
        entry: Vec<u8>,
        /// Ticks per second of the packet durations.
        timescale: u32,
        /// Ticks at the start that are not played (Opus pre-skip).
        skip: u32,
    },
}

impl KeptPart {
    /// The sample entry and timescale, which every part must share.
    fn shape(&self) -> (&[u8], u32) {
        match self {
            Self::Mp4 { entry, timescale }
            | Self::Demuxed {
                entry, timescale, ..
            } => (entry, *timescale),
        }
    }
}

/// Packages `parts` into one M4B at `output` keeping their audio, with a
/// chapter at the start of each part titled from `titles`.
///
/// # Returns
///
/// The chapters `(title, start ms)`, or `None` when the parts cannot be
/// kept together and should be transcoded instead.
///
/// # Errors
///
/// A read or write error once packaging has started.
pub(crate) fn package_keeping_codec(
    parts: &[std::path::PathBuf],
    output: &Path,
    titles: &[String],
) -> Result<Option<Vec<(String, u64)>>> {
    let mut kept = Vec::with_capacity(parts.len());
    for part in parts {
        match classify(part) {
            Some(found) => kept.push(found),
            None => return Ok(None),
        }
    }
    let Some(first) = kept.first() else {
        return Ok(None);
    };
    let (entry, timescale) = first.shape();
    if kept.iter().any(|part| part.shape() != (entry, timescale)) {
        tracing::info!("parts differ in codec or stream settings; transcoding to AAC instead");
        return Ok(None);
    }
    let mut writer = AacM4bWriter::create_with_entry(output, timescale, entry.to_vec())?;
    if let Some(KeptPart::Demuxed { skip, .. }) = kept.first() {
        writer.set_skip(*skip);
    }
    let mut starts = Vec::with_capacity(parts.len());
    let mut ticks = 0u64;
    for (part, how) in parts.iter().zip(&kept) {
        starts.push(ticks.saturating_mul(1000) / u64::from(timescale));
        ticks = ticks.saturating_add(match how {
            KeptPart::Mp4 { .. } => copy_mp4_samples(part, &mut writer)?,
            KeptPart::Demuxed { .. } => copy_packets(part, &mut writer)?,
        });
    }
    writer.finish()?;
    let chapters = starts
        .into_iter()
        .enumerate()
        .map(|(i, start)| {
            let title = titles
                .get(i)
                .filter(|title| !title.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Chapter {}", i + 1));
            (title, start)
        })
        .collect();
    Ok(Some(chapters))
}

/// How `part` would be kept, or `None` when it cannot be.
fn classify(part: &Path) -> Option<KeptPart> {
    if let Ok(mp4) = parse_mp4(part) {
        let SampleEntryKind::Other(FourCC(kind)) = mp4.audio.sample_entry_kind else {
            return None;
        };
        if !COPIED_ENTRIES.contains(&&kind) || mp4.audio.samples.is_empty() {
            return None;
        }
        let entry = sample_entry_bytes(&mp4).ok()?;
        return Some(KeptPart::Mp4 {
            entry,
            timescale: mp4.audio.timescale.max(1),
        });
    }
    let format = open_demuxer(part).ok()?;
    let track = format.default_track(TrackType::Audio)?;
    let Some(CodecParameters::Audio(params)) = track.codec_params.as_ref() else {
        return None;
    };
    let extra = params.extra_data.as_deref()?;
    if params.codec == CODEC_ID_OPUS {
        let (entry, skip) = opus_entry(extra)?;
        return Some(KeptPart::Demuxed {
            entry,
            timescale: OPUS_TIMESCALE,
            skip,
        });
    }
    if params.codec == CODEC_ID_FLAC {
        let (entry, rate) = flac_entry(extra)?;
        return Some(KeptPart::Demuxed {
            entry,
            timescale: rate,
            skip: 0,
        });
    }
    None
}

/// Opens `path` with Symphonia's demuxers, recognised by its bytes.
fn open_demuxer(path: &Path) -> Result<Box<dyn FormatReader>> {
    let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
    symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|err| MediaError::Native(format!("probe failed: {err}")))
}

/// The `Opus` sample entry for a stream whose identification header is
/// `head` (RFC 7845 `OpusHead`), and its pre-skip in 48 kHz ticks; `None`
/// when `head` is not a version 0 header.
///
/// The `dOps` box carries the same fields big-endian (Opus in ISO BMFF,
/// section 4.3.2), with the channel mapping table for families other than 0.
pub(crate) fn opus_entry(head: &[u8]) -> Option<(Vec<u8>, u32)> {
    let [b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', version, channels, p0, p1, r0, r1, r2, r3, g0, g1, family, rest @ ..] =
        head
    else {
        return None;
    };
    if version >> 4 != 0 || *channels == 0 {
        return None;
    }
    // Version 0 is an MP4 `dOps` body behind the magic, already
    // big-endian; an Ogg `OpusHead` (version 1) is little-endian.
    let (pre_skip, rate, gain) = if *version == 0 {
        (
            u16::from_be_bytes([*p0, *p1]),
            u32::from_be_bytes([*r0, *r1, *r2, *r3]),
            i16::from_be_bytes([*g0, *g1]),
        )
    } else {
        (
            u16::from_le_bytes([*p0, *p1]),
            u32::from_le_bytes([*r0, *r1, *r2, *r3]),
            i16::from_le_bytes([*g0, *g1]),
        )
    };
    let mut dops = vec![0u8, *channels];
    dops.extend_from_slice(&pre_skip.to_be_bytes());
    dops.extend_from_slice(&rate.to_be_bytes());
    dops.extend_from_slice(&gain.to_be_bytes());
    dops.push(*family);
    if *family != 0 {
        let table = rest.get(..2 + usize::from(*channels))?;
        dops.extend_from_slice(table);
    }
    let dops = wrap_box(b"dOps", &dops);
    Some((
        audio_sample_entry(*b"Opus", u16::from(*channels), OPUS_TIMESCALE, &dops),
        u32::from(pre_skip),
    ))
}

/// The `fLaC` sample entry for a stream whose `STREAMINFO` block body is
/// `info` (34 bytes), and its sample rate; `None` when `info` is not one.
///
/// The `dfLa` box (FLAC in ISO BMFF) holds the metadata block with its
/// header, marked as the last block.
pub(crate) fn flac_entry(info: &[u8]) -> Option<(Vec<u8>, u32)> {
    let info: &[u8; 34] = info.try_into().ok()?;
    // Sample rate: 20 bits from byte 10; channels - 1: 3 bits after it.
    let rate =
        (u32::from(info[10]) << 12) | (u32::from(info[11]) << 4) | (u32::from(info[12]) >> 4);
    let channels = u16::from((info[12] >> 1) & 0x07) + 1;
    if rate == 0 {
        return None;
    }
    let mut dfla = vec![0u8; 4]; // version 0, flags 0
    dfla.push(0x80); // last metadata block, type 0 (STREAMINFO)
    dfla.extend_from_slice(&[0, 0, 34]);
    dfla.extend_from_slice(info);
    let dfla = wrap_box(b"dfLa", &dfla);
    Some((audio_sample_entry(*b"fLaC", channels, rate, &dfla), rate))
}

/// Copies every audio sample of the MP4 at `part` into `writer`.
///
/// # Returns
///
/// The ticks copied.
fn copy_mp4_samples(part: &Path, writer: &mut AacM4bWriter) -> Result<u64> {
    let mp4 = parse_mp4(part)?;
    let mut input = SampleReader::open(part)?;
    let mut buf = Vec::new();
    let mut ticks = 0u64;
    for sample in &mp4.audio.samples {
        let size = usize::try_from(sample.size)
            .map_err(|_| MediaError::Mp4("sample size does not fit memory".into()))?;
        input.read_sample(sample.offset, size, &mut buf)?;
        let duration = sample.duration.max(1);
        writer.push(&buf, duration)?;
        ticks = ticks.saturating_add(u64::from(duration));
    }
    Ok(ticks)
}

/// Copies every packet of the audio track of `part` into `writer`, each as
/// one sample lasting the frames it decodes to.
///
/// # Returns
///
/// The ticks copied.
fn copy_packets(part: &Path, writer: &mut AacM4bWriter) -> Result<u64> {
    let mut format = open_demuxer(part)?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| MediaError::Native("no audio track".into()))?
        .id;
    let mut ticks = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(err) => return Err(MediaError::Native(format!("demux failed: {err}"))),
        };
        if packet.track_id != track || packet.data.is_empty() {
            continue;
        }
        let frames = packet
            .dur
            .get()
            .saturating_add(packet.trim_start.get())
            .saturating_add(packet.trim_end.get());
        let duration = u32::try_from(frames.max(1))
            .map_err(|_| MediaError::Mp4("packet lasts too long for one sample".into()))?;
        writer.push(&packet.data, duration)?;
        ticks = ticks.saturating_add(u64::from(duration));
    }
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stereo, 48 kHz OpusHead with 312 frames of pre-skip.
    fn opus_head(family: u8, rest: &[u8]) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2]);
        head.extend_from_slice(&312u16.to_le_bytes());
        head.extend_from_slice(&44_100u32.to_le_bytes());
        head.extend_from_slice(&(-256i16).to_le_bytes());
        head.push(family);
        head.extend_from_slice(rest);
        head
    }

    #[test]
    fn opus_head_becomes_a_big_endian_dops() {
        let (entry, skip) = opus_entry(&opus_head(0, &[])).unwrap();
        assert_eq!(skip, 312);
        assert_eq!(&entry[4..8], b"Opus");
        let dops = entry.windows(4).position(|w| w == b"dOps").unwrap();
        assert_eq!(
            &entry[dops + 4..],
            &[0, 2, 0x01, 0x38, 0, 0, 0xAC, 0x44, 0xFF, 0x00, 0],
            "version, channels, pre-skip, input rate, gain, family"
        );
        let (entry, _) = opus_entry(&opus_head(1, &[1, 1, 0, 1])).unwrap();
        assert!(
            entry.ends_with(&[1, 1, 1, 0, 1]),
            "the mapping table follows"
        );
        assert!(opus_entry(&opus_head(1, &[1])).is_none(), "a short table");
        assert!(opus_entry(b"OpusTags").is_none());
    }

    #[test]
    fn streaminfo_becomes_a_last_block_dfla() {
        let mut info = [0u8; 34];
        // 44 100 Hz, 2 channels, 16 bits.
        info[10] = 0x0A;
        info[11] = 0xC4;
        info[12] = 0x42;
        info[13] = 0xF0;
        let (entry, rate) = flac_entry(&info).unwrap();
        assert_eq!(rate, 44_100);
        assert_eq!(&entry[4..8], b"fLaC");
        assert_eq!(u16::from_be_bytes([entry[24], entry[25]]), 2, "channels");
        let dfla = entry.windows(4).position(|w| w == b"dfLa").unwrap();
        assert_eq!(&entry[dfla + 8..dfla + 12], &[0x80, 0, 0, 34]);
        assert!(flac_entry(&info[..30]).is_none());
        assert!(flac_entry(&[0u8; 34]).is_none(), "no sample rate");
    }
}
