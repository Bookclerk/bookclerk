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

/// Page flag: the page ends its logical stream.
const END_OF_STREAM: u8 = 0x04;

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

/// The first logical stream's header packets and where they lie.
struct Headers {
    /// The identification and comment packets.
    packets: [Vec<u8>; 2],
    /// The spans of the pages that carry the comment packet.
    comment_pages: Vec<ByteSpan>,
    /// The stream's serial.
    serial: u32,
    /// Offset of the page after the one that ends the comment packet.
    audio_at: u64,
}

/// Reads an Ogg file.
pub(crate) fn read(src: &Source<'_>) -> Step<TagRecord> {
    let Headers {
        packets,
        comment_pages,
        serial,
        audio_at,
    } = header_packets(src)?;
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
    let start = match codec {
        Codec::Opus { .. } => opus_initial_granule(src, audio_at, serial)?,
        Codec::Vorbis { .. } => 0,
    };
    builder.duration =
        last_granule(src, serial)?.and_then(|granule| duration(codec, granule.checked_sub(start)?));
    builder.finish(src)
}

/// The first two packets of the first logical stream and where they lie
/// ([`Headers`]).
///
/// # Errors
///
/// A malformed failure when the file does not start with a beginning-of-
/// stream page, a page runs past the end, or the packets take more than
/// [`MAX_HEADER_PAGES`] pages or [`crate::MAX_FETCH_BYTES`] bytes.
fn header_packets(src: &Source<'_>) -> Step<Headers> {
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
                    return Ok(Headers {
                        packets: [id, comment],
                        comment_pages,
                        serial,
                        audio_at: at,
                    });
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
///
/// A capture pattern counts as a page only when the bytes after it form a
/// whole page whose checksum matches, and the search resumes after a page
/// it accepts, so `OggS` inside a page body (a comment extension, say) is
/// never read as a header.
fn last_granule(src: &Source<'_>, serial: u32) -> Step<Option<u64>> {
    let start = src.size.saturating_sub(MAX_PAGE_BYTES);
    let tail = src.up_to(start, MAX_PAGE_BYTES, NAME)?;
    let mut best = None;
    let mut from = 0usize;
    while let Some(found) = memchr::memmem::find(&tail[from..], b"OggS") {
        let at = from + found;
        let Some((granule, page_serial, len)) = checked_page(&tail[at..]) else {
            from = at + 1;
            continue;
        };
        from = at + len;
        if page_serial == serial && granule != NO_GRANULE {
            best = Some(granule);
        }
    }
    Ok(best)
}

/// The granule position, serial and length of the page at the start of
/// `bytes`, or `None` when they do not start with a whole version 0 page
/// whose checksum matches.
fn checked_page(bytes: &[u8]) -> Option<(u64, u32, usize)> {
    let header = bytes.get(..27)?;
    let (_, granule, serial, segments) = page_fields(header)?;
    let table_end = 27 + usize::from(segments);
    let table = bytes.get(27..table_end)?;
    let body: usize = table.iter().map(|len| usize::from(*len)).sum();
    let page = bytes.get(..table_end + body)?;
    let stored = u32::from_le_bytes(page.get(22..26)?.try_into().ok()?);
    let crc = page_crc(page.get(..22)?, page.get(26..)?);
    (crc == stored).then_some((granule, serial, page.len()))
}

/// The Ogg page checksum of a page whose checksum field (four zero bytes
/// in the computation) lies between `before` and `after`: CRC-32 with
/// polynomial `0x04C11DB7`, no reflection, zero initial value and no final
/// XOR.
pub(crate) fn page_crc(before: &[u8], after: &[u8]) -> u32 {
    /// The table for one byte at a time, built at compile time.
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut index = 0;
        while index < 256 {
            // In range: `index` is below 256.
            #[allow(clippy::cast_possible_truncation)]
            let mut crc = (index as u32) << 24;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
                bit += 1;
            }
            table[index] = crc;
            index += 1;
        }
        table
    };
    let step =
        |crc: u32, byte: &u8| (crc << 8) ^ TABLE[usize::from((crc >> 24).to_le_bytes()[0] ^ byte)];
    let crc = before.iter().fold(0u32, step);
    let crc = [0u8; 4].iter().fold(crc, step);
    after.iter().fold(crc, step)
}

/// How many pages past the headers [`opus_initial_granule`] looks for the
/// first audio page.
const MAX_AUDIO_SEARCH_PAGES: usize = 64;

/// The granule position an Opus stream starts at: zero for a stream
/// encoded from the start, more for one cropped from a longer stream
/// (RFC 7845 section 4.5).
///
/// It is the first audio page's granule less the samples of the packets
/// that end on that page, worked out from each packet's TOC byte. Zero when
/// the first audio page cannot be read this way: not found within
/// [`MAX_AUDIO_SEARCH_PAGES`] pages, it continues a packet, it also ends the
/// stream, or its granule is below its packets' samples.
fn opus_initial_granule(src: &Source<'_>, mut at: u64, serial: u32) -> Step<u64> {
    for _ in 0..MAX_AUDIO_SEARCH_PAGES {
        if at >= src.size {
            return Ok(0);
        }
        let header = src.bytes(at, PAGE_HEADER_BYTES, NAME)?;
        let Some((flags, granule, page_serial, _)) = page_fields(header) else {
            return Ok(0);
        };
        let page = page(src, at)?;
        at = page.end();
        if page_serial != serial {
            continue;
        }
        // An end-of-stream page's granule may be end-trimmed (RFC 7845
        // section 4.4), so it says nothing about where the stream starts.
        if flags & (CONTINUED | END_OF_STREAM) != 0 || granule == NO_GRANULE {
            return Ok(0);
        }
        let body = src.bytes(page.body_at, page.body_len, NAME)?;
        let mut samples = 0u64;
        let mut start = 0usize;
        let mut offset = 0usize;
        for lace in page.lacing {
            offset += usize::from(*lace);
            if *lace == 255 {
                continue;
            }
            let Some(count) = body.get(start..offset).and_then(opus_packet_samples) else {
                return Ok(0);
            };
            samples += count;
            start = offset;
        }
        return Ok(granule.saturating_sub(samples));
    }
    Ok(0)
}

/// The 48 kHz samples one Opus packet decodes to, from its TOC byte and,
/// for a code 3 packet, its frame count byte (RFC 6716 section 3.1).
/// `None` for an empty or truncated packet.
fn opus_packet_samples(packet: &[u8]) -> Option<u64> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame: u64 = match config {
        // SILK: 10, 20, 40 or 60 ms.
        0..=11 => [480, 960, 1920, 2880][usize::from(config % 4)],
        // Hybrid: 10 or 20 ms.
        12..=15 => [480, 960][usize::from(config % 2)],
        // CELT: 2.5, 5, 10 or 20 ms.
        _ => [120, 240, 480, 960][usize::from(config % 4)],
    };
    let frames = match toc & 0x03 {
        0 => 1,
        1 | 2 => 2,
        _ => u64::from(*packet.get(1)? & 0x3F),
    };
    Some(frame * frames)
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

    /// An Opus file with the fixture's headers (pre-skip 312) and `audio`
    /// pages after them.
    fn opus_with(audio: &[Vec<u8>]) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.extend([1, 2]);
        head.extend(312u16.to_le_bytes());
        head.extend([0u8; 7]);
        let mut tags = b"OpusTags".to_vec();
        tags.extend(crate::fixtures::vorbis_comments(&["TITLE=Cropped"]));
        let mut file = ogg_page(BEGINNING_OF_STREAM, 0, 9, &[&head]);
        file.extend(ogg_page(0, 0, 9, &[&tags]));
        for page in audio {
            file.extend_from_slice(page);
        }
        file
    }

    #[test]
    fn a_cropped_opus_stream_counts_from_its_first_granule() {
        // Starts 480,000 samples in: the first page's one 20 ms packet
        // (960 samples) ends at 480,960.
        let file = opus_with(&[
            ogg_page(0, 480_960, 9, &[&[0xF8]]),
            ogg_page(END_OF_STREAM, 480_000 + 312 + 480_000, 9, &[&[0xF8]]),
        ]);
        let record = drive(&file).expect("read");
        assert_eq!(record.duration.map(|d| d.ms), Some(10_000));
        // Packet sizes from the TOC byte: code 0, 1 and 3 packets.
        assert_eq!(opus_packet_samples(&[0xF8]), Some(960));
        assert_eq!(opus_packet_samples(&[0x01]), Some(960));
        assert_eq!(opus_packet_samples(&[0x63, 0x03]), Some(3 * 480));
        assert_eq!(opus_packet_samples(&[0x83]), None);
        assert_eq!(opus_packet_samples(&[]), None);
    }

    #[test]
    fn a_page_inside_a_page_body_is_not_a_page() {
        // The last page's body carries a whole, valid page of the same
        // stream with another granule, as a comment extension could.
        let inner = ogg_page(0, 12_345, 9, &[b"x"]);
        let mut body = vec![0xF8];
        body.extend_from_slice(&inner);
        let file = opus_with(&[ogg_page(END_OF_STREAM, 312 + 480_000, 9, &[&body])]);
        let record = drive(&file).expect("read");
        assert_eq!(record.duration.map(|d| d.ms), Some(10_000));
        // A page whose checksum does not match is no page.
        assert!(checked_page(&inner).is_some());
        let mut bad = inner;
        bad[6] ^= 1;
        assert_eq!(checked_page(&bad), None);
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
