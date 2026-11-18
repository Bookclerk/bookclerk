//! MP4 files: `ilst` items, the QuickTime chapter track and Nero `chpl`,
//! read from `moov` without touching the audio samples.
//!
//! The walk reads box headers, the small boxes it needs (`mvhd`, `tkhd`,
//! `tref`, `mdhd`, `hdlr`), the whole `udta` and `meta` boxes, and the
//! sample tables of the chapter text track only. Of the audio track it reads
//! the sample description (`stsd`), for the encoding; its sample tables,
//! which are megabytes long in a long book, are skipped by size.

use std::cell::Cell;

use crate::cache::{Miss, Source, Step};
use crate::text::{latin1, utf16be, utf8, RecordBuilder};
use crate::{
    AudioCodec, ByteSpan, ChapterSource, Container, Duration, DurationPrecision, TagError,
    TagField, TagRecord, MAX_CHAPTERS,
};

/// Container name in errors.
const NAME: &str = "MP4";

/// Most boxes one file's walk visits.
const MAX_BOXES: u32 = 16_384;

/// Largest chapter title sample read, in bytes.
const MAX_SAMPLE_BYTES: u64 = 16 * 1024;

/// Largest small box (`mvhd`, `tkhd`, `mdhd`, `hdlr`, `tref`) read whole.
const MAX_SMALL_BOX: u64 = 64 * 1024;

/// Largest chapter-track sample table read, in bytes.
const MAX_TEXT_STBL: u64 = 4 * 1024 * 1024;

/// A malformed-MP4 failure.
fn malformed(detail: &'static str) -> Miss {
    Miss::Fail(TagError::Malformed {
        container: NAME,
        detail,
    })
}

/// A box's position and type.
#[derive(Debug, Clone, Copy)]
struct BoxHeader {
    /// The four-character type.
    kind: [u8; 4],
    /// Offset of the box's first byte.
    offset: u64,
    /// Header length (8, or 16 with a 64-bit size).
    header_len: u64,
    /// Whole box length, header included.
    size: u64,
}

impl BoxHeader {
    /// Offset of the body.
    fn body(&self) -> u64 {
        self.offset + self.header_len
    }

    /// Length of the body.
    fn body_len(&self) -> u64 {
        self.size - self.header_len
    }

    /// The whole box as a span.
    fn span(&self) -> ByteSpan {
        ByteSpan {
            offset: self.offset,
            len: self.size,
        }
    }
}

/// Walks boxes and counts them against [`MAX_BOXES`].
struct Walker<'s, 'a> {
    /// The bytes.
    src: &'s Source<'a>,
    /// Boxes visited so far.
    visited: Cell<u32>,
}

impl<'a> Walker<'_, 'a> {
    /// The header of the box at `offset`, which must end by `end`.
    fn header(&self, offset: u64, end: u64) -> Step<BoxHeader> {
        let count = self.visited.get() + 1;
        if count > MAX_BOXES {
            return Err(malformed("too many boxes"));
        }
        self.visited.set(count);
        let head = self.src.bytes(offset, 8, NAME)?;
        let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let kind = [head[4], head[5], head[6], head[7]];
        let (header_len, size) = match size32 {
            0 => (8, end - offset),
            1 => {
                let large = self.src.bytes(offset + 8, 8, NAME)?;
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(large);
                (16, u64::from_be_bytes(bytes))
            }
            n => (8, u64::from(n)),
        };
        if size < header_len || offset.checked_add(size).is_none_or(|box_end| box_end > end) {
            return Err(malformed("box runs past its parent"));
        }
        Ok(BoxHeader {
            kind,
            offset,
            header_len,
            size,
        })
    }

    /// The headers of the boxes in `[start, end)`. A tail too short for a
    /// header is ignored.
    fn children(&self, start: u64, end: u64) -> Step<Vec<BoxHeader>> {
        let mut out = Vec::new();
        let mut at = start;
        while end.saturating_sub(at) >= 8 {
            let header = self.header(at, end)?;
            at = header.offset + header.size;
            out.push(header);
        }
        Ok(out)
    }

    /// The body of a box, read whole.
    fn body(&self, header: &BoxHeader, cap: u64) -> Step<&'a [u8]> {
        if header.body_len() > cap {
            return Err(malformed("box is too large"));
        }
        self.src.bytes(header.body(), header.body_len(), NAME)
    }
}

/// What a `trak` holds that the reader needs.
#[derive(Debug, Default)]
struct Track {
    /// `tkhd` track id.
    id: u32,
    /// `hdlr` handler type.
    handler: [u8; 4],
    /// `mdhd` timescale.
    timescale: u32,
    /// Track ids named by `tref/chap`.
    chapter_refs: Vec<u32>,
    /// The `stbl` box, read only for text tracks.
    stbl: Option<BoxHeader>,
}

/// Reads an MP4 file.
pub(crate) fn read(src: &Source<'_>) -> Step<TagRecord> {
    let walker = Walker {
        src,
        visited: Cell::new(0),
    };
    let mut at = 0;
    while src.size.saturating_sub(at) >= 8 {
        let header = walker.header(at, src.size)?;
        if &header.kind == b"moov" {
            return read_moov(&walker, &header);
        }
        at = header.offset + header.size;
    }
    Err(malformed("no moov box"))
}

/// Reads everything the record needs from `moov`.
fn read_moov(walker: &Walker<'_, '_>, moov: &BoxHeader) -> Step<TagRecord> {
    let src = walker.src;
    let mut builder = RecordBuilder::new(Container::Mp4);
    let mut tracks = Vec::new();
    let mut nero = Vec::new();
    for child in walker.children(moov.body(), moov.offset + moov.size)? {
        match &child.kind {
            b"mvhd" => builder.duration = mvhd_duration(walker.body(&child, MAX_SMALL_BOX)?),
            b"trak" => tracks.push(read_trak(walker, &child)?),
            b"udta" => {
                let body = src.bytes(child.body(), child.body_len(), NAME)?;
                builder.region(child.span());
                for (kind, at, item) in slice_children(body) {
                    match &kind {
                        b"chpl" => nero = nero_chapters(item),
                        b"meta" => read_meta(item, offset(child.body(), at), &mut builder),
                        _ => {}
                    }
                }
            }
            b"meta" => {
                let body = src.bytes(child.body(), child.body_len(), NAME)?;
                builder.region(child.span());
                read_meta(body, child.body(), &mut builder);
            }
            _ => {}
        }
    }

    builder.codec = audio_codec(walker, &tracks)?;
    read_chapter_track(walker, &tracks, moov, &mut builder)?;
    if !builder.has_chapters() {
        for (start_ms, title) in &nero {
            builder.chapter(*start_ms, title);
        }
        builder.chapter_source(ChapterSource::Mp4Nero);
    }
    builder.finish(src)
}

/// Reads the parts of a `trak` that say what it is.
fn read_trak(walker: &Walker<'_, '_>, trak: &BoxHeader) -> Step<Track> {
    let mut track = Track::default();
    for child in walker.children(trak.body(), trak.offset + trak.size)? {
        match &child.kind {
            b"tkhd" => {
                let body = walker.body(&child, MAX_SMALL_BOX)?;
                let at = if body.first() == Some(&1) { 20 } else { 12 };
                track.id = be32(body, at).unwrap_or_default();
            }
            b"tref" => {
                let body = walker.body(&child, MAX_SMALL_BOX)?;
                for (kind, _, refs) in slice_children(body) {
                    if &kind == b"chap" {
                        track
                            .chapter_refs
                            .extend(refs.as_chunks::<4>().0.iter().filter_map(|id| be32(id, 0)));
                    }
                }
            }
            b"mdia" => {
                for part in walker.children(child.body(), child.offset + child.size)? {
                    match &part.kind {
                        b"mdhd" => {
                            let body = walker.body(&part, MAX_SMALL_BOX)?;
                            let at = if body.first() == Some(&1) { 20 } else { 12 };
                            track.timescale = be32(body, at).unwrap_or_default();
                        }
                        b"hdlr" => {
                            let body = walker.body(&part, MAX_SMALL_BOX)?;
                            if let Some(handler) = body.get(8..12) {
                                track.handler.copy_from_slice(handler);
                            }
                        }
                        b"minf" => {
                            for minf in walker.children(part.body(), part.offset + part.size)? {
                                if &minf.kind == b"stbl" {
                                    track.stbl = Some(minf);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(track)
}

/// The encoding of the first audio track: the type of the first sample
/// entry in its `stsd`.
///
/// # Returns
///
/// `None` when there is no audio track, its `stsd` is missing, empty or
/// larger than a sample description can honestly be, or the entry is of a
/// type the reader does not know.
fn audio_codec(walker: &Walker<'_, '_>, tracks: &[Track]) -> Step<Option<AudioCodec>> {
    let Some(stbl) = tracks
        .iter()
        .find(|track| &track.handler == b"soun")
        .and_then(|track| track.stbl.as_ref())
    else {
        return Ok(None);
    };
    // Walked one header at a time and stopped at `stsd`, which comes first:
    // the tables after it are megabytes apart in a long book, and reading
    // their headers would cost a fetch each.
    let end = stbl.offset + stbl.size;
    let mut at = stbl.body();
    while end.saturating_sub(at) >= 8 {
        let child = walker.header(at, end)?;
        if &child.kind != b"stsd" {
            at = child.offset + child.size;
            continue;
        }
        if child.body_len() > MAX_SMALL_BOX {
            return Ok(None);
        }
        // Version and flags, the entry count, then the first entry: its
        // size and its four-character type.
        let body = walker.body(&child, MAX_SMALL_BOX)?;
        let entries = be32(body, 4).unwrap_or_default();
        let format = body
            .get(12..16)
            .and_then(|kind| <[u8; 4]>::try_from(kind).ok());
        return Ok(format
            .filter(|_| entries > 0)
            .and_then(AudioCodec::from_sample_entry));
    }
    Ok(None)
}

/// Reads the chapter titles from the text track the audio track's
/// `tref/chap` names.
///
/// A track whose samples point into `moov` itself is broken (its media was
/// cut off when `moov` was rewritten), so it is ignored and the Nero list,
/// if any, is used instead.
fn read_chapter_track(
    walker: &Walker<'_, '_>,
    tracks: &[Track],
    moov: &BoxHeader,
    builder: &mut RecordBuilder,
) -> Step<()> {
    let src = walker.src;
    let Some(text) = tracks
        .iter()
        .flat_map(|track| track.chapter_refs.iter())
        .find_map(|id| {
            tracks
                .iter()
                .find(|track| track.id == *id && track.id != 0 && &track.handler == b"text")
        })
    else {
        return Ok(());
    };
    let Some(stbl) = text.stbl else {
        return Ok(());
    };
    if text.timescale == 0 {
        return Ok(());
    }
    let tables = walker.body(&stbl, MAX_TEXT_STBL)?;
    let Some(samples) = SampleTable::parse(tables) else {
        builder.mark_truncated();
        return Ok(());
    };
    let spans: Vec<ByteSpan> = samples
        .samples
        .iter()
        .map(|&(offset, len, _)| ByteSpan { offset, len })
        .collect();
    if spans
        .iter()
        .any(|span| span.end().is_none_or(|end| end > src.size))
    {
        return Err(malformed("chapter sample runs past the end"));
    }
    let moov_end = moov.offset + moov.size;
    if spans
        .iter()
        .any(|span| span.offset < moov_end && span.end().is_some_and(|end| end > moov.offset))
    {
        return Ok(());
    }
    let bodies = src.many(&spans, NAME)?;
    builder.region(stbl.span());
    for ((span, body), &(_, _, start)) in spans.iter().zip(&bodies).zip(&samples.samples) {
        builder.region(*span);
        let start_ms = u128::from(start) * 1000 / u128::from(text.timescale);
        let start_ms = u64::try_from(start_ms).unwrap_or(u64::MAX);
        builder.chapter(start_ms, &sample_text(body));
    }
    if samples.truncated {
        builder.mark_truncated();
    }
    builder.chapter_source(ChapterSource::Mp4Track);
    Ok(())
}

/// A chapter track's samples: `(file offset, length, start in track ticks)`.
struct SampleTable {
    /// The samples, at most [`MAX_CHAPTERS`].
    samples: Vec<(u64, u64, u64)>,
    /// Whether samples past [`MAX_CHAPTERS`] were dropped.
    truncated: bool,
}

impl SampleTable {
    /// Joins `stts`, `stsz`, `stsc` and `stco`/`co64` from an `stbl` body.
    ///
    /// # Returns
    ///
    /// `None` when a table is missing or inconsistent, or a sample is
    /// larger than a title can be.
    fn parse(stbl: &[u8]) -> Option<Self> {
        let mut stts = None;
        let mut stsz = None;
        let mut stsc = None;
        let mut chunks: Option<Vec<u64>> = None;
        for (kind, _, body) in slice_children(stbl) {
            match &kind {
                b"stts" => stts = Some(body),
                b"stsz" => stsz = Some(body),
                b"stsc" => stsc = Some(body),
                b"stco" => {
                    let count = usize::try_from(be32(body, 4)?).ok()?;
                    let table = body.get(8..8 + count.checked_mul(4)?)?;
                    chunks = Some(
                        table
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .filter_map(|c| be32(c, 0).map(u64::from))
                            .collect(),
                    );
                }
                b"co64" => {
                    let count = usize::try_from(be32(body, 4)?).ok()?;
                    let table = body.get(8..8 + count.checked_mul(8)?)?;
                    chunks = Some(
                        table
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .filter_map(|c| be64(c, 0))
                            .collect(),
                    );
                }
                _ => {}
            }
        }
        let (stts, stsz, stsc, chunks) = (stts?, stsz?, stsc?, chunks?);

        let uniform = be32(stsz, 4)?;
        let total = usize::try_from(be32(stsz, 8)?).ok()?;
        let truncated = total > MAX_CHAPTERS;
        let count = total.min(MAX_CHAPTERS);
        let sizes: Vec<u64> = if uniform == 0 {
            let table = stsz.get(12..12 + count.checked_mul(4)?)?;
            table
                .as_chunks::<4>()
                .0
                .iter()
                .filter_map(|s| be32(s, 0).map(u64::from))
                .collect()
        } else {
            vec![u64::from(uniform); count]
        };
        if sizes.iter().any(|&size| size > MAX_SAMPLE_BYTES) {
            return None;
        }

        // stsc: runs of (first chunk, samples per chunk), 1-based chunks.
        let runs_len = usize::try_from(be32(stsc, 4)?).ok()?;
        let runs: Vec<(u64, u64)> = stsc
            .get(8..8 + runs_len.checked_mul(12)?)?
            .as_chunks::<12>()
            .0
            .iter()
            .filter_map(|r| Some((u64::from(be32(r, 0)?), u64::from(be32(r, 4)?))))
            .collect();
        // Runs must start at chunk 1 and rise, as the format requires; that
        // keeps this walk linear in chunks plus runs.
        if runs.first().is_some_and(|(first, _)| *first != 1)
            || runs.windows(2).any(|pair| pair[0].0 >= pair[1].0)
        {
            return None;
        }
        let mut offsets = Vec::with_capacity(count);
        let mut run = 0usize;
        'chunks: for (index, &chunk_offset) in chunks.iter().enumerate() {
            if offsets.len() == count {
                break;
            }
            let chunk = u64::try_from(index).ok()? + 1;
            while runs.get(run + 1).is_some_and(|(first, _)| *first <= chunk) {
                run += 1;
            }
            let per_chunk = runs.get(run)?.1;
            let mut at = chunk_offset;
            for _ in 0..per_chunk {
                if offsets.len() == count {
                    break 'chunks;
                }
                let size = *sizes.get(offsets.len())?;
                offsets.push((at, size));
                at = at.checked_add(size)?;
            }
        }
        if offsets.len() < count {
            return None;
        }

        // stts: runs of (sample count, delta).
        let entries = usize::try_from(be32(stts, 4)?).ok()?;
        let mut starts = Vec::with_capacity(count);
        let mut tick = 0u64;
        for run in stts
            .get(8..8 + entries.checked_mul(8)?)?
            .as_chunks::<8>()
            .0
            .iter()
        {
            let (run_count, delta) = (be32(run, 0)?, u64::from(be32(run, 4)?));
            for _ in 0..run_count {
                if starts.len() == count {
                    break;
                }
                starts.push(tick);
                tick = tick.saturating_add(delta);
            }
        }
        if starts.len() < count {
            return None;
        }
        Some(Self {
            samples: offsets
                .into_iter()
                .zip(starts)
                .map(|((offset, len), start)| (offset, len, start))
                .collect(),
            truncated,
        })
    }
}

/// A QuickTime text sample's string: a `u16` length, then UTF-8, or UTF-16
/// with a byte order mark.
fn sample_text(sample: &[u8]) -> String {
    let Some(len) = sample
        .get(..2)
        .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
    else {
        return String::new();
    };
    let text = sample
        .get(2..2 + len)
        .unwrap_or_else(|| sample.get(2..).unwrap_or_default());
    match text {
        [0xFE, 0xFF, rest @ ..] => utf16be(rest),
        [0xFF, 0xFE, rest @ ..] => {
            let swapped: Vec<u8> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|p| [p[1], p[0]])
                .collect();
            utf16be(&swapped)
        }
        _ => utf8(text),
    }
}

/// The movie's running time from an `mvhd` body.
fn mvhd_duration(body: &[u8]) -> Option<Duration> {
    let (timescale, duration) = if body.first() == Some(&1) {
        (be32(body, 20)?, be64(body, 24)?)
    } else {
        let duration = be32(body, 16)?;
        if duration == u32::MAX {
            return None;
        }
        (be32(body, 12)?, u64::from(duration))
    };
    if timescale == 0 || duration == u64::MAX {
        return None;
    }
    let ms = u128::from(duration) * 1000 / u128::from(timescale);
    Some(Duration {
        ms: u64::try_from(ms).ok()?,
        precision: DurationPrecision::Exact,
    })
}

/// Nero `chpl` chapters: `(start ms, title)`.
fn nero_chapters(body: &[u8]) -> Vec<(u64, String)> {
    // Version 1 adds four reserved bytes before the count.
    let count_at = if body.first() == Some(&1) { 8 } else { 4 };
    let Some(&count) = body.get(count_at) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut at = count_at + 1;
    for _ in 0..count {
        let Some(start) = be64(body, at) else { break };
        let Some(&len) = body.get(at + 8) else { break };
        let from = at + 9;
        let Some(title) = body.get(from..from + usize::from(len)) else {
            break;
        };
        // 100 ns units.
        out.push((start / 10_000, utf8(title)));
        at = from + usize::from(len);
    }
    out
}

/// Reads the `ilst` items of a `meta` body that starts at file offset `at`.
fn read_meta(meta: &[u8], at: u64, builder: &mut RecordBuilder) {
    // ISO `meta` is a full box (four bytes of version and flags); QuickTime
    // `meta` is not. A child type at bytes 4..8 means no version.
    let skip = if meta.get(4..8).is_some_and(|kind| kind == b"hdlr") {
        0
    } else {
        4
    };
    let Some(children) = meta.get(skip..) else {
        return;
    };
    for (kind, child_at, ilst) in slice_children(children) {
        if &kind != b"ilst" {
            continue;
        }
        let ilst_at = offset(at, skip + child_at);
        for (item, item_at, body) in slice_children(ilst) {
            read_item(item, body, offset(ilst_at, item_at), builder);
        }
    }
}

/// Reads one `ilst` item whose body starts at file offset `at`.
fn read_item(kind: [u8; 4], body: &[u8], at: u64, builder: &mut RecordBuilder) {
    let mut mean = None;
    let mut name = None;
    for (part, part_at, value) in slice_children(body) {
        match &part {
            // `mean` and `name` are full boxes: four bytes of version and flags.
            b"mean" => mean = value.get(4..).map(utf8),
            b"name" => name = value.get(4..).map(utf8),
            b"data" => {
                let Some(type_code) = be32(value, 0) else {
                    continue;
                };
                let Some(payload) = value.get(8..) else {
                    continue;
                };
                let payload_at = offset(at, part_at + 8);
                if &kind == b"covr" {
                    let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
                    builder.cover(
                        payload,
                        Some(ByteSpan {
                            offset: payload_at,
                            len,
                        }),
                    );
                    continue;
                }
                let field = match &kind {
                    b"----" => match &name {
                        Some(name) => TagField::Freeform {
                            namespace: mean.clone(),
                            name: name.clone(),
                        },
                        None => continue,
                    },
                    other => item_field(other),
                };
                if let Some(text) = data_text(&kind, type_code & 0x00FF_FFFF, payload) {
                    builder.entry(field, &text);
                }
            }
            _ => {}
        }
    }
}

/// The text of a `data` payload of well-known type `type_code`.
fn data_text(kind: &[u8; 4], type_code: u32, payload: &[u8]) -> Option<String> {
    match type_code {
        1 => Some(utf8(payload)),
        2 => Some(utf16be(payload)),
        21 | 22 => integer(payload, type_code == 21).map(|n| n.to_string()),
        0 if kind == b"trkn" || kind == b"disk" => {
            let number = u16::from_be_bytes(payload.get(2..4)?.try_into().ok()?);
            let total = payload
                .get(4..6)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .unwrap_or_default();
            Some(if total > 0 {
                format!("{number}/{total}")
            } else {
                number.to_string()
            })
        }
        0 if kind == b"gnre" => {
            Some(u16::from_be_bytes(payload.get(..2)?.try_into().ok()?).to_string())
        }
        _ => None,
    }
}

/// A big-endian integer of 1, 2, 3, 4 or 8 bytes.
fn integer(bytes: &[u8], signed: bool) -> Option<i128> {
    if !matches!(bytes.len(), 1..=4 | 8) {
        return None;
    }
    let mut value: u64 = 0;
    for &b in bytes {
        value = (value << 8) | u64::from(b);
    }
    let bits = u32::try_from(bytes.len()).ok()? * 8;
    if signed && bits < 64 && value >> (bits - 1) == 1 {
        Some(i128::from(value) - (1i128 << bits))
    } else if signed && bits == 64 {
        Some(i128::from(i64::from_be_bytes(bytes.try_into().ok()?)))
    } else {
        Some(i128::from(value))
    }
}

/// The field an `ilst` item fills.
fn item_field(kind: &[u8; 4]) -> TagField {
    match kind {
        b"\xA9nam" => TagField::Title,
        b"\xA9alb" => TagField::Album,
        b"\xA9ART" => TagField::Artist,
        b"aART" => TagField::AlbumArtist,
        b"\xA9wrt" => TagField::Composer,
        b"\xA9gen" => TagField::Genre,
        b"\xA9day" => TagField::Date,
        b"\xA9cmt" => TagField::Comment,
        b"desc" => TagField::Description,
        b"ldes" => TagField::LongDescription,
        b"\xA9pub" => TagField::Publisher,
        b"cprt" => TagField::Copyright,
        b"\xA9grp" => TagField::Grouping,
        b"trkn" => TagField::Track,
        b"disk" => TagField::Disc,
        other => TagField::Native { id: latin1(other) },
    }
}

/// The boxes in an in-memory buffer: `(type, body offset in buf, body)`.
/// Stops at the first malformed header.
fn slice_children(buf: &[u8]) -> Vec<([u8; 4], usize, &[u8])> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while buf.len().saturating_sub(at) >= 8 {
        let Some(size32) = be32(buf, at) else { break };
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&buf[at + 4..at + 8]);
        let (header, size) = match size32 {
            0 => (8, buf.len() - at),
            1 => match be64(buf, at + 8).and_then(|s| usize::try_from(s).ok()) {
                Some(size) => (16, size),
                None => break,
            },
            n => match usize::try_from(n) {
                Ok(size) => (8, size),
                Err(_) => break,
            },
        };
        let Some(end) = at
            .checked_add(size)
            .filter(|&end| end <= buf.len() && size >= header)
        else {
            break;
        };
        out.push((kind, at + header, &buf[at + header..end]));
        at = end;
    }
    out
}

/// `base` plus an in-buffer position, saturating.
fn offset(base: u64, at: usize) -> u64 {
    base.saturating_add(u64::try_from(at).unwrap_or(u64::MAX))
}

/// A big-endian `u32` at `at`.
fn be32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        buf.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// A big-endian `u64` at `at`.
fn be64(buf: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        buf.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fixtures::{mp4_box, tagged_m4b};
    use crate::reader::tests::drive;
    use crate::{read_all, PictureFormat};

    #[test]
    fn reads_items_chapter_track_and_cover() {
        let file = tagged_m4b();
        let record = drive(&file).expect("read");
        assert_eq!(record, read_all(&file).expect("read all"));
        assert_eq!(record.container, Container::Mp4);
        assert_eq!(record.entries[0].field, TagField::Title);
        assert_eq!(record.entries[0].value, "The Book");
        assert!(record
            .entries
            .iter()
            .any(|e| e.field == TagField::Track && e.value == "3/9"));
        assert!(record.entries.iter().any(|e| e.field
            == TagField::Freeform {
                namespace: Some("com.pilabor.tone".into()),
                name: "AUDIBLE_ASIN".into()
            }
            && e.value == "B000000001"));
        assert!(record
            .entries
            .iter()
            .any(|e| e.field == TagField::Native { id: "tmpo".into() } && e.value == "-2"));
        assert_eq!(record.chapter_source, Some(ChapterSource::Mp4Track));
        assert_eq!(
            record
                .chapters
                .iter()
                .map(|c| (c.start_ms, c.title.as_str()))
                .collect::<Vec<_>>(),
            [(0, "Opening"), (60_000, "Chapter One")]
        );
        assert_eq!(record.duration.map(|d| d.ms), Some(90_000));
        assert_eq!(record.codec, Some(AudioCodec::Aac));
        let cover = record.cover.expect("cover");
        assert_eq!(cover.format, PictureFormat::Jpeg);
        let at = usize::try_from(cover.span.expect("span").offset).expect("offset");
        assert!(file[at..].starts_with(&[0xFF, 0xD8, 0xFF, 0xDB]));
    }

    #[test]
    fn names_the_audio_tracks_encoding() {
        // The fixture's sample entry is `mp4a`; swapping its type swaps the
        // codec, and a type the reader does not know reads as none.
        let file = tagged_m4b();
        let at = file.windows(4).position(|w| w == b"mp4a").expect("mp4a");
        for (kind, codec) in [
            (*b"alac", Some(AudioCodec::Alac)),
            (*b"fLaC", Some(AudioCodec::Flac)),
            (*b"Opus", Some(AudioCodec::Opus)),
            (*b"ec-3", Some(AudioCodec::Eac3)),
            (*b"zzzz", None),
        ] {
            let mut other = file.clone();
            other[at..at + 4].copy_from_slice(&kind);
            let record = crate::read_all(&other).expect("record");
            assert_eq!(record.codec, codec);
            assert_eq!(
                codec.map(AudioCodec::is_lossless),
                codec.map(|c| matches!(c, AudioCodec::Alac | AudioCodec::Flac))
            );
            record
                .validate(u64::try_from(other.len()).expect("len"))
                .expect("valid");
        }
        // No entries: the type bytes that follow are not a sample entry.
        let mut empty = file.clone();
        empty[at - 8..at - 4].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(crate::read_all(&empty).expect("record").codec, None);
    }

    #[test]
    fn a_record_whose_codec_the_container_cannot_hold_is_refused() {
        let size = |file: &[u8]| u64::try_from(file.len()).expect("len");
        for file in [
            crate::fixtures::tagged_mp3(),
            crate::fixtures::tagged_flac(),
        ] {
            let record = crate::read_all(&file).expect("record");
            let own = match record.container {
                Container::Mp3 => AudioCodec::Mp3,
                Container::Flac => AudioCodec::Flac,
                Container::Mp4 => unreachable!("not an MP4 fixture"),
            };
            assert_eq!(record.codec, Some(own));
            assert!(!record.container.content_type().is_empty());
            record.validate(size(&file)).expect("valid");
            for wrong in [None, Some(AudioCodec::Aac)] {
                let mut forged = record.clone();
                forged.codec = wrong;
                assert_eq!(forged.validate(size(&file)), Err(crate::RecordError::Codec));
            }
        }
    }

    #[test]
    fn skips_audio_sample_tables() {
        let file = tagged_m4b();
        let mut reader = crate::TagReader::new(u64::try_from(file.len()).expect("len"));
        let mut fetched = 0u64;
        loop {
            match reader.poll() {
                crate::Poll::Done(result) => {
                    result.expect("read");
                    break;
                }
                crate::Poll::Need(spans) => {
                    for span in spans {
                        fetched += span.len;
                        let from = usize::try_from(span.offset).expect("offset");
                        let to = usize::try_from(span.offset + span.len).expect("end");
                        reader
                            .supply(span.offset, file[from..to].to_vec())
                            .expect("supply");
                    }
                }
            }
        }
        // The 200 KB audio stbl is never fetched.
        assert!(fetched < 150_000, "fetched {fetched} bytes");
    }

    #[test]
    fn a_track_pointing_into_moov_falls_back_to_nero() {
        let mut file = tagged_m4b();
        // Point the chapter track's only chunk at the moov box itself.
        let moov_at = file.windows(4).position(|w| w == b"moov").expect("moov") - 4;
        let stco = file.windows(4).position(|w| w == b"stco").expect("stco");
        let at = stco + 4 + 4 + 4;
        file[at..at + 4].copy_from_slice(&u32::try_from(moov_at + 8).expect("fits").to_be_bytes());
        let record = drive(&file).expect("read");
        assert_eq!(record.chapter_source, Some(ChapterSource::Mp4Nero));
        assert_eq!(record.chapters[0].title, "Nero");
    }

    #[test]
    fn nero_chapters_are_the_fallback() {
        let body = {
            let mut b = vec![0, 0, 0, 0, 2];
            for (start, title) in [(0u64, "A"), (50_000_000, "B")] {
                b.extend_from_slice(&start.to_be_bytes());
                b.push(1);
                b.extend_from_slice(title.as_bytes());
            }
            b
        };
        assert_eq!(
            nero_chapters(&body),
            [(0, "A".to_string()), (5_000, "B".to_string())]
        );
    }

    #[test]
    fn integers_decode_by_width() {
        assert_eq!(integer(&[0xFF], true), Some(-1));
        assert_eq!(integer(&[0xFF], false), Some(255));
        assert_eq!(
            integer(&[0x80, 0, 0, 0, 0, 0, 0, 0], true),
            Some(i128::from(i64::MIN))
        );
        assert_eq!(integer(&[0; 5], false), None);
    }

    #[test]
    fn no_moov_is_malformed() {
        let file = mp4_box(b"ftyp", b"M4B \0\0\0\0");
        assert!(matches!(drive(&file), Err(TagError::Malformed { .. })));
    }
}
