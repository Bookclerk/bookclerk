//! Ogg files holding Opus (RFC 7845) or Vorbis: the first logical stream's
//! identification and comment headers, and the last page's granule
//! position for the running time.
//!
//! The header packets come first in the stream, so only the pages that
//! carry them are read, plus the file's last page. Pages of other logical
//! streams (a multiplexed file) are skipped. Page checksums are not
//! verified: a damaged page gives wrong text, never a read outside the
//! file.

use crate::cache::{Miss, Source, Step};
use crate::flac;
use crate::text::RecordBuilder;
use crate::{AudioCodec, ByteSpan, Container, Duration, DurationPrecision, TagError, TagRecord};

/// Container name in errors.
const NAME: &str = "Ogg";

/// Bytes in a page header before its segment table.
const PAGE_HEADER_BYTES: u64 = 27;

/// Longest page: the header, 255 lacing values, and 255 segments of 255
/// bytes.
const MAX_PAGE_BYTES: u64 = PAGE_HEADER_BYTES + 255 + 255 * 255;

/// Most pages walked while gathering the two header packets. A comment
/// header holding a large cover spans a few hundred pages at most.
const MAX_HEADER_PAGES: usize = 1024;

/// Page header flag: this page continues a packet from the previous page.
const CONTINUED: u8 = 0x01;

/// Page header flag: the first page of a logical stream.
const BEGINNING_OF_STREAM: u8 = 0x02;

/// A granule position of all ones: no packet ends on the page.
const NO_GRANULE: u64 = u64::MAX;

/// The rate Opus granule positions count in, in Hz.
const OPUS_GRANULE_HZ: u64 = 48_000;

/// Front cover, in the ID3 picture type numbering FLAC pictures use.
const FRONT_COVER: u32 = 3;

/// A malformed-Ogg failure.
fn malformed(detail: &'static str) -> Miss {
    Miss::Fail(TagError::Malformed {
        container: NAME,
        detail,
    })
}

/// One page's header fields.
struct Page<'a> {
    /// Header type flags.
    flags: u8,
    /// The logical stream the page belongs to.
    serial: u32,
    /// The lacing values.
    lacing: &'a [u8],
    /// Offset of the first body byte.
    body_at: u64,
    /// Body length: the sum of the lacing values.
    body_len: u64,
}

impl Page<'_> {
    /// Offset one past the page's last byte.
    fn end(&self) -> u64 {
        self.body_at + self.body_len
    }
}

/// Reads the header of the page at `at`.
fn page<'a>(src: &Source<'a>, at: u64) -> Step<Page<'a>> {
    let header = src.bytes(at, PAGE_HEADER_BYTES, NAME)?;
    let Some(fields) = page_fields(header) else {
        return Err(malformed("page does not start with OggS"));
    };
    let lacing = src.bytes(at + PAGE_HEADER_BYTES, u64::from(fields.3), NAME)?;
    Ok(Page {
        flags: fields.0,
        serial: fields.2,
        lacing,
        body_at: at + PAGE_HEADER_BYTES + u64::from(fields.3),
        body_len: lacing.iter().map(|len| u64::from(*len)).sum(),
    })
}

/// A page header's flags, granule position, serial and segment count, or
/// `None` when the 27 bytes are not a version 0 page header.
fn page_fields(header: &[u8]) -> Option<(u8, u64, u32, u8)> {
    let [b'O', b'g', b'g', b'S', 0, flags, rest @ ..] = header else {
        return None;
    };
    let granule = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
    let serial = u32::from_le_bytes(rest.get(8..12)?.try_into().ok()?);
    let segments = *rest.get(20)?;
    Some((*flags, granule, serial, segments))
}

/// The stream's encoding and what its granule positions need.
#[derive(Clone, Copy)]
enum Codec {
    /// Opus: granules count 48 kHz samples, `pre_skip` of them not played.
    Opus {
        /// Samples at 48 kHz the decoder discards at the start.
        pre_skip: u16,
    },
    /// Vorbis: granules count samples at the stream's rate.
    Vorbis {
        /// The sample rate in Hz.
        rate: u32,
    },
}

/// Reads an Ogg file.
pub(crate) fn read(src: &Source<'_>) -> Step<TagRecord> {
    let (packets, comment_pages, serial) = header_packets(src)?;
    let [identification, comment] = packets;
    let codec = identify(&identification)?;
    let body = match codec {
        Codec::Opus { .. } => comment.strip_prefix(b"OpusTags"),
        Codec::Vorbis { .. } => comment.strip_prefix(b"\x03vorbis"),
    }
    .ok_or_else(|| malformed("second packet is not a comment header"))?;

    let mut builder = RecordBuilder::new(Container::Ogg);
    builder.codec = Some(match codec {
        Codec::Opus { .. } => AudioCodec::Opus,
        Codec::Vorbis { .. } => AudioCodec::Vorbis,
    });
    for span in comment_pages {
        builder.region(span);
    }
    let pictures = flac::read_comments(body, &mut builder)?;
    let cover = pictures
        .iter()
        .find(|(kind, _)| *kind == FRONT_COVER)
        .or_else(|| pictures.first());
    if let Some((_, data)) = cover {
        // Base64 inside a comment: the bytes are nowhere in the file as is.
        builder.cover(data, None);
    }
    builder.duration = last_granule(src, serial)?.and_then(|granule| duration(codec, granule));
    builder.finish(src)
}

/// The first two packets of the first logical stream, the spans of the
/// pages that carry the second, and the stream's serial.
///
/// # Errors
///
/// A malformed failure when the file does not start with a beginning-of-
/// stream page, a page runs past the end, or the packets take more than
/// [`MAX_HEADER_PAGES`] pages or [`crate::MAX_FETCH_BYTES`] bytes.
fn header_packets(src: &Source<'_>) -> Step<([Vec<u8>; 2], Vec<ByteSpan>, u32)> {
    let first = page(src, 0)?;
    if first.flags & BEGINNING_OF_STREAM == 0 {
        return Err(malformed("first page does not begin a stream"));
    }
    let serial = first.serial;
    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(2);
    let mut current: Vec<u8> = Vec::new();
    let mut comment_pages = Vec::new();
    let mut at = 0u64;
    let limit = usize::try_from(crate::MAX_FETCH_BYTES).unwrap_or(usize::MAX);
    for index in 0..MAX_HEADER_PAGES {
        let page = page(src, at)?;
        let page_span = ByteSpan {
            offset: at,
            len: page.end() - at,
        };
        at = page.end();
        if page.serial != serial {
            continue;
        }
        if index == 0 && page.flags & CONTINUED != 0 {
            return Err(malformed("first packet continues a missing page"));
        }
        if index > 0 && page.flags & BEGINNING_OF_STREAM != 0 {
            return Err(malformed("stream begins twice"));
        }
        // Whether bytes of the comment packet (the second) lie on this page.
        let mut holds_comment = packets.len() == 1;
        let body = src.bytes(page.body_at, page.body_len, NAME)?;
        let mut offset = 0usize;
        for lace in page.lacing {
            let len = usize::from(*lace);
            current.extend_from_slice(&body[offset..offset + len]);
            offset += len;
            if current.len() > limit {
                return Err(Miss::Fail(TagError::TooLarge));
            }
            if *lace == 255 {
                continue;
            }
            packets.push(std::mem::take(&mut current));
            match packets.len() {
                1 => holds_comment = true,
                _ => {
                    comment_pages.push(page_span);
                    let mut iter = packets.into_iter();
                    let (Some(id), Some(comment)) = (iter.next(), iter.next()) else {
                        return Err(malformed("header packets missing"));
                    };
                    return Ok(([id, comment], comment_pages, serial));
                }
            }
        }
        if holds_comment {
            comment_pages.push(page_span);
        }
        if at >= src.size {
            return Err(malformed("stream ends inside its headers"));
        }
    }
    Err(malformed("headers span too many pages"))
}

/// The encoding an identification packet names.
///
/// # Errors
///
/// [`TagError::Unsupported`] for a stream that is neither Opus nor Vorbis,
/// or a malformed failure for a short or zero-rate header.
fn identify(packet: &[u8]) -> Step<Codec> {
    if let Some(head) = packet.strip_prefix(b"OpusHead") {
        let &[_version, _channels, pre0, pre1, ..] = head else {
            return Err(malformed("OpusHead too short"));
        };
        return Ok(Codec::Opus {
            pre_skip: u16::from_le_bytes([pre0, pre1]),
        });
    }
    if let Some(head) = packet.strip_prefix(b"\x01vorbis") {
        // Version (4), channels (1), then the rate.
        let rate = head
            .get(5..9)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| malformed("Vorbis identification header too short"))?;
        if rate == 0 {
            return Err(malformed("Vorbis stream has no sample rate"));
        }
        return Ok(Codec::Vorbis { rate });
    }
    Err(Miss::Fail(TagError::Unsupported))
}

/// The granule position of the last page of stream `serial` that ends a
/// packet, found in the file's last [`MAX_PAGE_BYTES`] bytes.
fn last_granule(src: &Source<'_>, serial: u32) -> Step<Option<u64>> {
    let start = src.size.saturating_sub(MAX_PAGE_BYTES);
    let tail = src.up_to(start, MAX_PAGE_BYTES, NAME)?;
    let mut best = None;
    let mut from = 0usize;
    while let Some(found) = memchr::memmem::find(&tail[from..], b"OggS") {
        let at = from + found;
        from = at + 1;
        let Some((_, granule, page_serial, segments)) = tail
            .get(at..)
            .and_then(|rest| rest.get(..27))
            .and_then(page_fields)
        else {
            continue;
        };
        let table_at = at + 27;
        let Some(table) = tail.get(table_at..table_at + usize::from(segments)) else {
            continue;
        };
        let body: usize = table.iter().map(|len| usize::from(*len)).sum();
        // A page cut by the end of the file is not a page.
        if table_at + usize::from(segments) + body > tail.len() {
            continue;
        }
        if page_serial == serial && granule != NO_GRANULE {
            best = Some(granule);
        }
    }
    Ok(best)
}

/// The running time a final granule position gives, or `None` when it is
/// not past the pre-skip.
fn duration(codec: Codec, granule: u64) -> Option<Duration> {
    let (samples, rate) = match codec {
        Codec::Opus { pre_skip } => (granule.checked_sub(u64::from(pre_skip))?, OPUS_GRANULE_HZ),
        Codec::Vorbis { rate } => (granule, u64::from(rate)),
    };
    if samples == 0 {
        return None;
    }
    let ms = u128::from(samples) * 1000 / u128::from(rate);
    Some(Duration {
        ms: u64::try_from(ms).ok()?,
        precision: DurationPrecision::Exact,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{ogg_page, tagged_ogg_opus};
    use crate::reader::tests::drive;
    use crate::{read_all, PictureFormat, TagField};

    #[test]
    fn reads_comments_chapters_cover_and_duration() {
        let file = tagged_ogg_opus();
        let record = drive(&file).expect("read");
        assert_eq!(record, read_all(&file).expect("read all"));
        assert_eq!(record.container, Container::Ogg);
        assert_eq!(record.codec, Some(AudioCodec::Opus));
        assert_eq!(record.entries[0].field, TagField::Title);
        assert_eq!(record.entries[0].value, "The Book");
        assert!(record
            .entries
            .iter()
            .all(|entry| !matches!(&entry.field, TagField::Freeform { name, .. } if name.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE"))));
        assert_eq!(record.chapters.len(), 2);
        assert_eq!(record.chapters[1].title, "Later");
        // 48,000 * 10 samples past a 312-sample pre-skip.
        assert_eq!(record.duration.map(|d| d.ms), Some(10_000));
        record
            .validate(u64::try_from(file.len()).expect("len"))
            .expect("valid");
        let cover = record.cover.expect("cover");
        assert_eq!(cover.format, PictureFormat::Png);
        assert_eq!(cover.span, None);
    }

    #[test]
    fn files_from_a_real_muxer_read() {
        // Quarter-second tones muxed by ffmpeg (libopus, libvorbis).
        let corpus =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/tags_read");
        for (name, codec) in [
            ("ffmpeg.opus", AudioCodec::Opus),
            ("ffmpeg.ogg", AudioCodec::Vorbis),
        ] {
            let file = std::fs::read(corpus.join(name)).expect(name);
            let record = drive(&file).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(record.codec, Some(codec), "{name}");
            let ms = record.duration.expect("duration").ms;
            assert!((240..=270).contains(&ms), "{name}: {ms} ms");
        }
    }

    #[test]
    fn vorbis_streams_count_at_their_own_rate() {
        let mut id = b"\x01vorbis".to_vec();
        id.extend(0u32.to_le_bytes());
        id.push(2);
        id.extend(22_050u32.to_le_bytes());
        id.extend([0u8; 13]);
        let mut comment = b"\x03vorbis".to_vec();
        comment.extend(crate::fixtures::vorbis_comments(&["ARTIST=Someone"]));
        comment.push(1);
        let mut file = ogg_page(BEGINNING_OF_STREAM, 0, 7, &[&id]);
        file.extend(ogg_page(0, 0, 7, &[&comment]));
        file.extend(ogg_page(0x04, 44_100, 7, &[&[0u8; 10]]));
        let record = drive(&file).expect("read");
        assert_eq!(record.codec, Some(AudioCodec::Vorbis));
        assert_eq!(record.duration.map(|d| d.ms), Some(2_000));
    }

    #[test]
    fn a_comment_packet_across_pages_is_joined() {
        let mut head = b"OpusHead".to_vec();
        head.extend([1, 2]);
        head.extend(0u16.to_le_bytes());
        head.extend([0u8; 7]);
        let long = format!("DESCRIPTION={}", "x".repeat(65_100));
        let mut comment = b"OpusTags".to_vec();
        comment.extend(crate::fixtures::vorbis_comments(&[&long]));
        let mut file = ogg_page(BEGINNING_OF_STREAM, 0, 1, &[&head]);
        // The first page takes 255 full segments; the rest continues.
        let (first, rest) = comment.split_at(255 * 255);
        file.extend(ogg_page(0, NO_GRANULE, 1, &[first]));
        file.extend(ogg_page(CONTINUED, 0, 1, &[rest]));
        let record = drive(&file).expect("read");
        assert_eq!(record, read_all(&file).expect("read all"));
        assert_eq!(record.entries.len(), 1);
        assert_eq!(record.entries[0].value.len(), 65_100);
        assert_eq!(record.duration, None);
    }

    #[test]
    fn broken_streams_are_refused() {
        let file = tagged_ogg_opus();
        let mut cut = file.clone();
        cut.truncate(40);
        assert!(drive(&cut).is_err());
        // A stream that is neither Opus nor Vorbis.
        let mut flac_in_ogg = ogg_page(BEGINNING_OF_STREAM, 0, 1, &[b"\x7fFLAC\x01\x00"]);
        flac_in_ogg.extend(ogg_page(0, 0, 1, &[b"x"]));
        assert_eq!(drive(&flac_in_ogg), Err(TagError::Unsupported));
        // No beginning-of-stream flag.
        let mut no_bos = file;
        no_bos[5] = 0;
        assert!(drive(&no_bos).is_err());
    }
}
