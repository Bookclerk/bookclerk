//! FLAC files: the metadata blocks before the first audio frame.

use std::collections::BTreeMap;

use crate::cache::{Miss, Source, Step};
use crate::text::{utf8, RecordBuilder};
use crate::{
    AudioFormat, ByteSpan, ChapterSource, Container, Duration, DurationPrecision, TagError,
    TagField, TagRecord,
};

/// Container name in errors.
const NAME: &str = "FLAC";

/// Most metadata blocks walked before giving up.
const MAX_BLOCKS: usize = 1024;

/// Block types.
const STREAMINFO: u8 = 0;
/// Vorbis comment block.
const VORBIS_COMMENT: u8 = 4;
/// Cue sheet block.
const CUESHEET: u8 = 5;
/// Picture block.
const PICTURE: u8 = 6;

/// Front cover, in the ID3 picture type numbering FLAC uses.
const FRONT_COVER: u32 = 3;

/// A malformed-FLAC failure.
fn malformed(detail: &'static str) -> Miss {
    Miss::Fail(TagError::Malformed {
        container: NAME,
        detail,
    })
}

/// Reads a FLAC file.
pub(crate) fn read(src: &Source<'_>) -> Step<TagRecord> {
    let mut builder = RecordBuilder::new(Container::Flac);
    let mut offset = 4u64;
    let mut pictures: Vec<(u32, &[u8], u64)> = Vec::new();
    // Pictures carried as `METADATA_BLOCK_PICTURE` comments, used only
    // when the file has no `PICTURE` block.
    let mut comment_pictures: Vec<(u32, Vec<u8>)> = Vec::new();
    for _ in 0..MAX_BLOCKS {
        let &[flags, a, b, c] = src.bytes(offset, 4, NAME)? else {
            return Err(malformed("block header runs past the end"));
        };
        let last = flags & 0x80 != 0;
        let kind = flags & 0x7F;
        let len = u64::from(u32::from_be_bytes([0, a, b, c]));
        let body_at = offset + 4;
        match kind {
            STREAMINFO => {
                let body = src.bytes(body_at, len, NAME)?;
                builder.duration = streaminfo_duration(body);
                builder.format = streaminfo_format(body);
            }
            VORBIS_COMMENT | PICTURE | CUESHEET => {
                let body = src.bytes(body_at, len, NAME)?;
                builder.region(ByteSpan {
                    offset,
                    len: len + 4,
                });
                if kind == VORBIS_COMMENT {
                    for (kind, data) in read_comments(body, &mut builder)? {
                        comment_pictures.push((kind, data));
                    }
                } else if kind == PICTURE {
                    if let Some((kind, data, at)) = picture(body) {
                        pictures.push((kind, data, body_at + at));
                    }
                }
            }
            0x7F => return Err(malformed("invalid block type")),
            _ => {}
        }
        offset = body_at + len;
        if last {
            let cover = pictures
                .iter()
                .find(|(kind, ..)| *kind == FRONT_COVER)
                .or_else(|| pictures.first());
            if let Some(&(_, data, at)) = cover {
                let span = u64::try_from(data.len())
                    .ok()
                    .map(|len| ByteSpan { offset: at, len });
                builder.cover(data, span);
            } else if let Some((_, data)) = comment_pictures
                .iter()
                .find(|(kind, _)| *kind == FRONT_COVER)
                .or_else(|| comment_pictures.first())
            {
                builder.cover(data, None);
            }
            return builder.finish(src);
        }
    }
    Err(malformed("too many metadata blocks"))
}

/// The channel count and sample rate a `STREAMINFO` body states.
///
/// After the block sizes and frame sizes come 20 bits of rate, then the
/// channel count less one in 3 bits.
pub(crate) fn streaminfo_format(body: &[u8]) -> AudioFormat {
    let Some(bits) = body
        .get(10..18)
        .and_then(|bits| <[u8; 8]>::try_from(bits).ok())
    else {
        return AudioFormat::default();
    };
    let packed = u64::from_be_bytes(bits);
    AudioFormat {
        channels: u8::try_from(((packed >> 41) & 0b111) + 1).ok(),
        sample_rate_hz: u32::try_from(packed >> 44).ok().filter(|&rate| rate > 0),
        aac_profile: None,
    }
}

/// The running time from a `STREAMINFO` body: total samples over the rate.
fn streaminfo_duration(body: &[u8]) -> Option<Duration> {
    let bits: [u8; 8] = body.get(10..18)?.try_into().ok()?;
    let packed = u64::from_be_bytes(bits);
    let sample_rate = packed >> 44;
    let total_samples = packed & 0xF_FFFF_FFFF;
    if sample_rate == 0 || total_samples == 0 {
        return None;
    }
    let ms = u128::from(total_samples) * 1000 / u128::from(sample_rate);
    Some(Duration {
        ms: u64::try_from(ms).ok()?,
        precision: DurationPrecision::Exact,
    })
}

/// A little-endian length-prefixed cursor over a Vorbis comment block.
struct LeReader<'a>(&'a [u8]);

impl<'a> LeReader<'a> {
    /// The next `u32`.
    fn u32(&mut self) -> Option<u32> {
        let bytes: [u8; 4] = self.0.get(..4)?.try_into().ok()?;
        self.0 = &self.0[4..];
        Some(u32::from_le_bytes(bytes))
    }

    /// The next length-prefixed string's bytes.
    fn string(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.u32()?).ok()?;
        let bytes = self.0.get(..len)?;
        self.0 = &self.0[len..];
        Some(bytes)
    }
}

/// The comment key that carries a base64 FLAC `PICTURE` block (the Ogg
/// way to embed a cover).
const PICTURE_KEY: &str = "METADATA_BLOCK_PICTURE";

/// Adds the comments of a Vorbis comment block (FLAC, Ogg Opus, Ogg
/// Vorbis), and its chapters.
///
/// # Returns
///
/// The pictures carried as [`PICTURE_KEY`] comments, as picture type and
/// image bytes; those comments are not entries. A value that is not
/// base64 of a picture block is dropped.
///
/// # Errors
///
/// A malformed failure when a length runs past the block.
pub(crate) fn read_comments(body: &[u8], builder: &mut RecordBuilder) -> Step<Vec<(u32, Vec<u8>)>> {
    let mut pictures = Vec::new();
    let mut reader = LeReader(body);
    reader
        .string()
        .ok_or_else(|| malformed("vendor string runs past the block"))?;
    let count = reader
        .u32()
        .ok_or_else(|| malformed("comment count missing"))?;
    // chapter number -> (start, name)
    let mut chapters: BTreeMap<u32, (Option<u64>, Option<String>)> = BTreeMap::new();
    for _ in 0..count {
        let Some(comment) = reader.string() else {
            return Err(malformed("comment runs past the block"));
        };
        let comment = utf8(comment);
        let Some((key, value)) = comment.split_once('=') else {
            continue;
        };
        let upper = key.to_ascii_uppercase();
        if upper == PICTURE_KEY {
            if let Some((kind, data, _)) = base64(value).as_deref().and_then(picture) {
                pictures.push((kind, data.to_vec()));
            }
            continue;
        }
        if let Some((number, is_name)) = chapter_key(&upper) {
            let slot = chapters.entry(number).or_default();
            if is_name {
                slot.1 = Some(value.to_string());
            } else {
                slot.0 = parse_timestamp(value);
            }
            continue;
        }
        builder.entry(comment_field(&upper, key), value);
    }
    for (start, name) in chapters.into_values() {
        if let Some(start) = start {
            builder.chapter(start, name.as_deref().unwrap_or_default());
        }
    }
    builder.chapter_source(ChapterSource::VorbisComment);
    Ok(pictures)
}

/// Decodes standard base64 (RFC 4648 section 4) with optional padding;
/// `None` for any other character or a dangling bit group.
fn base64(text: &str) -> Option<Vec<u8>> {
    let digits = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(digits.len() / 4 * 3 + 2);
    let mut bits = 0u32;
    let mut held = 0u8;
    for &byte in digits {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(u8::try_from((bits >> held) & 0xFF).ok()?);
        }
    }
    // Six leftover bits cannot end a whole byte.
    (held < 6).then_some(out)
}

/// `CHAPTER001` → `(1, false)`, `CHAPTER001NAME` → `(1, true)`.
fn chapter_key(upper: &str) -> Option<(u32, bool)> {
    let rest = upper.strip_prefix("CHAPTER")?;
    let (digits, is_name) = match rest.strip_suffix("NAME") {
        Some(digits) => (digits, true),
        None => (rest, false),
    };
    if digits.is_empty() || digits.len() > 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, is_name))
}

/// `HH:MM:SS.mmm` (hours unbounded, fraction optional) in milliseconds.
fn parse_timestamp(value: &str) -> Option<u64> {
    let (clock, fraction) = match value.trim().split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (value.trim(), None),
    };
    let mut parts = clock.split(':');
    let hours: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || minutes >= 60 || seconds >= 60 {
        return None;
    }
    let millis = match fraction {
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            let padded: String = digits.chars().chain("000".chars()).take(3).collect();
            padded.parse::<u64>().ok()?
        }
        Some(_) => return None,
        None => 0,
    };
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000 + seconds * 1000 + millis)
}

/// The field a Vorbis comment key fills.
fn comment_field(upper: &str, key: &str) -> TagField {
    match upper {
        "TITLE" => TagField::Title,
        "SUBTITLE" => TagField::Subtitle,
        "ALBUM" => TagField::Album,
        "ARTIST" => TagField::Artist,
        "ALBUMARTIST" | "ALBUM ARTIST" => TagField::AlbumArtist,
        "COMPOSER" => TagField::Composer,
        "GENRE" => TagField::Genre,
        "DATE" | "YEAR" => TagField::Date,
        "COMMENT" => TagField::Comment,
        "DESCRIPTION" => TagField::Description,
        "PUBLISHER" | "ORGANIZATION" | "LABEL" => TagField::Publisher,
        "COPYRIGHT" => TagField::Copyright,
        "LANGUAGE" => TagField::Language,
        "TRACKNUMBER" => TagField::Track,
        "DISCNUMBER" => TagField::Disc,
        "GROUPING" => TagField::Grouping,
        _ => TagField::Freeform {
            namespace: None,
            name: key.to_string(),
        },
    }
}

/// A `PICTURE` body's type, data and the data's offset within the body.
fn picture(body: &[u8]) -> Option<(u32, &[u8], u64)> {
    let be32 = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(body.get(at..at + 4)?.try_into().ok()?))
    };
    let kind = be32(0)?;
    let mime_len = usize::try_from(be32(4)?).ok()?;
    let desc_at = 8usize.checked_add(mime_len)?;
    let desc_len = usize::try_from(be32(desc_at)?).ok()?;
    // Width, height, depth and colours: four u32 after the description.
    let data_len_at = desc_at
        .checked_add(4)?
        .checked_add(desc_len)?
        .checked_add(16)?;
    let data_len = usize::try_from(be32(data_len_at)?).ok()?;
    let data_at = data_len_at.checked_add(4)?;
    let data = body.get(data_at..data_at.checked_add(data_len)?)?;
    Some((kind, data, u64::try_from(data_at).ok()?))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fixtures::tagged_flac;
    use crate::reader::tests::drive;
    use crate::{read_all, PictureFormat};

    #[test]
    fn reads_comments_chapters_cover_and_duration() {
        let file = tagged_flac();
        let record = drive(&file).expect("read");
        assert_eq!(record, read_all(&file).expect("read all"));
        assert_eq!(
            record.format,
            crate::AudioFormat {
                channels: Some(2),
                sample_rate_hz: Some(44_100),
                aac_profile: None,
            }
        );
        assert_eq!(record.entries.len(), 3);
        assert_eq!(record.entries[1].field, TagField::Artist);
        assert_eq!(
            record.entries[2].field,
            TagField::Freeform {
                namespace: None,
                name: "NARRATOR".into()
            }
        );
        assert_eq!(record.chapters.len(), 2);
        assert_eq!(record.chapters[0].title, "Start");
        assert_eq!(record.chapters[1].start_ms, 60_500);
        assert_eq!(record.duration.map(|d| d.ms), Some(10_000));
        let cover = record.cover.expect("cover");
        assert_eq!(cover.format, PictureFormat::Png);
        let at = usize::try_from(cover.span.expect("span").offset).expect("offset");
        assert!(file[at..].starts_with(b"\x89PNG"));
    }

    #[test]
    fn timestamps_parse_strictly() {
        assert_eq!(parse_timestamp("01:02:03.4"), Some(3_723_400));
        assert_eq!(parse_timestamp("00:00:05"), Some(5000));
        assert_eq!(parse_timestamp("00:61:00"), None);
        assert_eq!(parse_timestamp("00:00:00.x"), None);
        assert_eq!(parse_timestamp("1:2"), None);
        assert_eq!(chapter_key("CHAPTER12NAME"), Some((12, true)));
        assert_eq!(chapter_key("CHAPTERNAME"), None);
    }

    #[test]
    fn base64_decodes_strictly() {
        assert_eq!(base64("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64("aGVsbG8").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64("").as_deref(), Some(&b""[..]));
        assert_eq!(base64("a"), None);
        assert_eq!(base64("aGV*"), None);
    }

    #[test]
    fn truncated_blocks_are_malformed() {
        let mut file = tagged_flac();
        file.truncate(60);
        assert!(drive(&file).is_err());
    }
}
