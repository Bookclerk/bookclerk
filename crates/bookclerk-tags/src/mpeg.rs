//! MP3 files: the ID3v2 tag at the start, the ID3v1 tag at the end, and the
//! running time from the first audio frame.

use crate::cache::{Source, Step};
use crate::text::{latin1, RecordBuilder};
use crate::{
    id3v2, AudioFormat, ByteSpan, Container, Duration, DurationPrecision, TagField, TagRecord,
};

/// Container name in errors.
const NAME: &str = "MP3";

/// How far past the ID3v2 tag the first frame may start (padding, junk).
const SYNC_SEARCH_BYTES: u64 = 64 * 1024;

/// Bytes of an ID3v1 tag.
const ID3V1_LEN: u64 = 128;

/// Whether `bytes` starts with an MPEG audio frame header.
pub(crate) fn is_frame_sync(bytes: &[u8]) -> bool {
    FrameHeader::parse(bytes).is_some()
}

/// Reads an MP3 file.
pub(crate) fn read(src: &Source<'_>) -> Step<TagRecord> {
    let mut builder = RecordBuilder::new(Container::Mp3);
    let audio_start = id3v2::read(src, &mut builder)?;
    let has_v2 = audio_start > 0;

    let mut audio_end = src.size;
    if src.size >= audio_start.saturating_add(ID3V1_LEN) {
        let at = src.size - ID3V1_LEN;
        let tail = src.bytes(at, ID3V1_LEN, NAME)?;
        if tail.starts_with(b"TAG") {
            audio_end = at;
            builder.region(ByteSpan {
                offset: at,
                len: ID3V1_LEN,
            });
            if !has_v2 {
                read_id3v1(tail, &mut builder);
            }
        }
    }

    builder.duration = duration(src, audio_start, audio_end, &mut builder.format)?;
    builder.finish(src)
}

/// Adds the fields of an ID3v1 (or v1.1) tag.
fn read_id3v1(tag: &[u8], builder: &mut RecordBuilder) {
    let field = |from: usize, to: usize| -> String {
        let raw = tag.get(from..to).unwrap_or_default();
        let raw = raw.split(|&b| b == 0).next().unwrap_or_default();
        latin1(raw).trim_end().to_string()
    };
    builder.entry(TagField::Title, &field(3, 33));
    builder.entry(TagField::Artist, &field(33, 63));
    builder.entry(TagField::Album, &field(63, 93));
    builder.entry(TagField::Date, &field(93, 97));
    // ID3v1.1: a zero byte before the last comment byte makes that byte a
    // track number and shortens the comment to 28 bytes.
    let track = tag
        .get(126)
        .copied()
        .filter(|&t| t != 0 && tag.get(125) == Some(&0));
    builder.entry(
        TagField::Comment,
        &field(97, if track.is_some() { 125 } else { 127 }),
    );
    if let Some(track) = track {
        builder.entry(TagField::Track, &track.to_string());
    }
    if let Some(&genre) = tag.get(127).filter(|&&g| g != 0xFF) {
        builder.entry(
            TagField::Native {
                id: "ID3v1 genre".into(),
            },
            &genre.to_string(),
        );
    }
}

/// The running time from the first frame: its Xing/Info or VBRI frame
/// count when there is one, else the bit rate over the audio size.
///
/// The first frame's channel mode and sample rate go into `format`.
fn duration(
    src: &Source<'_>,
    audio_start: u64,
    audio_end: u64,
    format: &mut AudioFormat,
) -> Step<Option<Duration>> {
    if audio_start >= audio_end {
        return Ok(None);
    }
    let window = src.up_to(
        audio_start,
        SYNC_SEARCH_BYTES.min(audio_end - audio_start),
        NAME,
    )?;
    // Find a frame whose successor also syncs, so a stray 0xFF in padding
    // is not taken for audio.
    let Some((at, header)) = (0..window.len()).find_map(|i| {
        let header = FrameHeader::parse(window.get(i..)?)?;
        let next = window.get(i + header.frame_len..);
        match next {
            Some(next) if next.len() >= 4 => FrameHeader::parse(next).map(|_| (i, header)),
            // The window ends inside the next frame: accept the one we have.
            _ => Some((i, header)),
        }
    }) else {
        return Ok(None);
    };
    *format = AudioFormat {
        channels: Some(if header.mono { 1 } else { 2 }),
        sample_rate_hz: Some(header.sample_rate),
        aac_profile: None,
    };
    let frame = window.get(at..).unwrap_or_default();
    let frames = header.vbr_frame_count(frame);
    let ms = match frames {
        Some(frames) if header.sample_rate > 0 => {
            let samples = u128::from(frames) * u128::from(header.samples_per_frame);
            u64::try_from(samples * 1000 / u128::from(header.sample_rate)).ok()
        }
        _ => None,
    };
    if let Some(ms) = ms {
        return Ok(Some(Duration {
            ms,
            precision: DurationPrecision::Exact,
        }));
    }
    // No frame count: walk the frames in the window. When the window holds
    // all the audio the count is exact; otherwise the average frame length
    // in the window stands in for the rest (right for CBR, close for VBR).
    let Ok(at_offset) = u64::try_from(at) else {
        return Ok(None);
    };
    let audio_bytes = audio_end - audio_start - at_offset;
    let mut walked = 0usize;
    let mut frames = 0u64;
    while let Some(next) = window
        .get(at + walked..)
        .and_then(FrameHeader::parse)
        .filter(|next| next.sample_rate == header.sample_rate)
    {
        if at + walked + next.frame_len > window.len() {
            break;
        }
        walked += next.frame_len;
        frames += 1;
    }
    let Ok(walked_bytes) = u64::try_from(walked) else {
        return Ok(None);
    };
    if frames == 0 || walked_bytes == 0 || header.sample_rate == 0 {
        return Ok(None);
    }
    let whole = walked_bytes >= audio_bytes;
    let total_frames = if whole {
        u128::from(frames)
    } else {
        u128::from(audio_bytes) * u128::from(frames) / u128::from(walked_bytes)
    };
    let ms =
        total_frames * u128::from(header.samples_per_frame) * 1000 / u128::from(header.sample_rate);
    Ok(u64::try_from(ms).ok().map(|ms| Duration {
        ms,
        precision: if whole {
            DurationPrecision::Exact
        } else {
            DurationPrecision::Estimated
        },
    }))
}

/// The fields of an MPEG audio frame header the duration needs.
#[derive(Debug, Clone, Copy)]
struct FrameHeader {
    /// MPEG-1 (true) or MPEG-2/2.5.
    mpeg1: bool,
    /// Single channel.
    mono: bool,
    /// Sample rate in Hz.
    sample_rate: u32,
    /// Samples each frame decodes to.
    samples_per_frame: u32,
    /// Layer 3.
    layer3: bool,
    /// Frame length in bytes.
    frame_len: usize,
}

impl FrameHeader {
    /// Parses the four header bytes at the start of `bytes`.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let &[b0, b1, b2, b3, ..] = bytes else {
            return None;
        };
        if b0 != 0xFF || b1 & 0xE0 != 0xE0 {
            return None;
        }
        let version = (b1 >> 3) & 0b11; // 0: 2.5, 1: reserved, 2: 2, 3: 1
        let layer = (b1 >> 1) & 0b11; // 1: III, 2: II, 3: I
        if version == 1 || layer == 0 {
            return None;
        }
        let mpeg1 = version == 3;
        let bitrate_index = usize::from(b2 >> 4);
        let rate_index = usize::from((b2 >> 2) & 0b11);
        if bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
            return None;
        }
        const V1_L1: [u32; 15] = [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ];
        const V1_L2: [u32; 15] = [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ];
        const V1_L3: [u32; 15] = [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ];
        const V2_L1: [u32; 15] = [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ];
        const V2_L23: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
        let table = match (mpeg1, layer) {
            (true, 3) => &V1_L1,
            (true, 2) => &V1_L2,
            (true, _) => &V1_L3,
            (false, 3) => &V2_L1,
            (false, _) => &V2_L23,
        };
        let bitrate_kbps = *table.get(bitrate_index)?;
        let base_rate = [44_100u32, 48_000, 32_000][rate_index];
        let sample_rate = match version {
            3 => base_rate,
            2 => base_rate / 2,
            _ => base_rate / 4,
        };
        let samples_per_frame = match (layer, mpeg1) {
            (3, _) => 384,
            (2, _) | (1, true) => 1152,
            _ => 576,
        };
        let padding = u32::from((b2 >> 1) & 1);
        let frame_len = if layer == 3 {
            (12 * bitrate_kbps * 1000 / sample_rate + padding) * 4
        } else {
            samples_per_frame / 8 * bitrate_kbps * 1000 / sample_rate + padding
        };
        Some(Self {
            mpeg1,
            mono: (b3 >> 6) == 0b11,
            sample_rate,
            samples_per_frame,
            layer3: layer == 1,
            frame_len: usize::try_from(frame_len).ok().filter(|&len| len >= 4)?,
        })
    }

    /// The total frame count from a Xing/Info or VBRI header in `frame`
    /// (which starts with this header).
    fn vbr_frame_count(&self, frame: &[u8]) -> Option<u32> {
        let be32 = |at: usize| -> Option<u32> {
            Some(u32::from_be_bytes(frame.get(at..at + 4)?.try_into().ok()?))
        };
        if self.layer3 {
            let side_info = match (self.mpeg1, self.mono) {
                (true, false) => 32,
                (true, true) | (false, false) => 17,
                (false, true) => 9,
            };
            let at = 4 + side_info;
            let tag = frame.get(at..at + 4)?;
            if tag == b"Xing" || tag == b"Info" {
                let flags = be32(at + 4)?;
                if flags & 1 == 1 {
                    return be32(at + 8).filter(|&frames| frames > 0);
                }
                return None;
            }
        }
        if frame.get(36..40) == Some(b"VBRI") {
            return be32(36 + 14).filter(|&frames| frames > 0);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::tests::drive;

    /// A 128 kbit/s 44.1 kHz stereo MPEG-1 layer 3 frame header.
    const HEADER: [u8; 4] = [0xFF, 0xFB, 0x90, 0x00];

    fn frames(count: usize) -> Vec<u8> {
        let len = FrameHeader::parse(&HEADER).expect("header").frame_len;
        let mut out = Vec::new();
        for _ in 0..count {
            let mut frame = vec![0u8; len];
            frame[..4].copy_from_slice(&HEADER);
            out.extend(frame);
        }
        out
    }

    #[test]
    fn short_files_are_counted_exactly() {
        let file = frames(10);
        let duration = drive(&file).expect("read").duration.expect("duration");
        assert_eq!(duration.precision, DurationPrecision::Exact);
        assert_eq!(duration.ms, 261);
        assert_eq!(
            drive(&file).expect("read").format,
            crate::AudioFormat {
                channels: Some(2),
                sample_rate_hz: Some(44_100),
                aac_profile: None,
            }
        );
    }

    #[test]
    fn long_cbr_duration_is_estimated() {
        let file = frames(300);
        let record = drive(&file).expect("read");
        let duration = record.duration.expect("duration");
        assert_eq!(duration.precision, DurationPrecision::Estimated);
        // 300 frames of 1152 samples at 44.1 kHz is about 7837 ms.
        assert!((7800..7870).contains(&duration.ms), "{}", duration.ms);
        assert!(record.entries.is_empty());
    }

    #[test]
    fn xing_frame_count_is_exact() {
        let mut file = frames(3);
        let at = 4 + 32;
        file[at..at + 4].copy_from_slice(b"Xing");
        file[at + 4..at + 8].copy_from_slice(&1u32.to_be_bytes());
        file[at + 8..at + 12].copy_from_slice(&1000u32.to_be_bytes());
        let record = drive(&file).expect("read");
        assert_eq!(
            record.duration,
            Some(Duration {
                ms: 26_122,
                precision: DurationPrecision::Exact
            })
        );
    }

    #[test]
    fn id3v1_is_read_without_id3v2() {
        let mut file = frames(4);
        let mut tag = vec![0u8; 128];
        tag[..3].copy_from_slice(b"TAG");
        tag[3..8].copy_from_slice(b"Title");
        tag[33..39].copy_from_slice(b"Author");
        tag[126] = 7;
        tag[127] = 101;
        file.extend(tag);
        let record = drive(&file).expect("read");
        assert_eq!(record.entries[0].field, TagField::Title);
        assert_eq!(record.entries[0].value, "Title");
        assert!(record
            .entries
            .iter()
            .any(|e| e.field == TagField::Track && e.value == "7"));
        assert_eq!(record.region.len, 128);
    }
}
