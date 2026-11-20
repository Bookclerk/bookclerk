//! Native audiobook metadata fix-up (M4B via mp4ameta, MP3 via id3).
//!
//! Tag mapping targets [Audiobookshelf file metadata](https://audiobookshelf.org/docs/documentation/libraries/book-library/directory-structure/)
//! plus the `----:com.pilabor.tone:*` freeform atoms ABS embeds for series/ASIN.

use std::path::{Path, PathBuf};

use id3::frame::{
    Chapter as Id3Chapter, ExtendedText, Frame, Picture, PictureType, TableOfContents,
};
use id3::{Tag as Id3Tag, TagLike, Version as Id3Version};
use mp4ameta::{Data, FreeformIdent, Img, ImgFmt, MediaType, Tag as Mp4Tag, WriteConfig};

use crate::chapters_mp4::write_audiobook_chapters;
use crate::error::{MediaError, Result};
use crate::MediaOutcome;

/// Product / CLI name written into tool attribution tags.
pub const BOOKCLERK_TOOL_NAME: &str = "bookclerk";

/// Tone / ABS freeform namespace used for series + Audible ASIN on MPEG-4.
const TONE_MEAN: &str = "com.pilabor.tone";
/// Bookclerk freeform namespace for tool attribution.
const BOOKCLERK_MEAN: &str = "app.bookclerk";
/// Common iTunes freeform mean (ffprobe often surfaces these without the mean).
const ITUNES_MEAN: &str = "com.apple.iTunes";

/// Request to fix up audiobook metadata after decrypt / download.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FixupRequest {
    /// Absolute path of the clear media file to tag (copied/written to output).
    pub input: PathBuf,
    /// Absolute path where the tagged file should be written.
    pub output: PathBuf,
    /// Primary title embedded in the container tags.
    pub title: String,
    /// Author / artist tag when known.
    pub author: Option<String>,
    /// Narrator tag when known (ABS / iTunes freeform).
    pub narrator: Option<String>,
    /// Optional cover image path to embed (JPEG/PNG).
    pub cover: Option<PathBuf>,
    /// Chapter titles + start offsets in milliseconds, embedded as a
    /// QuickTime chapter track in an M4B and as ID3 `CHAP`/`CTOC` frames in
    /// an MP3.
    pub chapters: Vec<(String, u64)>,
    /// When true, replace any chapters the file already has (an M4B chapter
    /// list/track, or MP3 `CHAP`/`CTOC` frames) with [`Self::chapters`]
    /// (used when overlaying Audible chapter trees onto Libro packaged M4Bs).
    /// When false, preserve existing chapters if the file already has them.
    pub replace_chapters: bool,
    /// Subtitle / album subtitle when known.
    pub subtitle: Option<String>,
    /// Publisher imprint for the publisher tag.
    pub publisher: Option<String>,
    /// Publish year as a string (e.g. `"2011"`).
    pub year: Option<String>,
    /// Genre string; multiple genres should use `;` separators for ABS.
    pub genre: Option<String>,
    /// Series name written to Tone / ABS freeform atoms when set.
    pub series: Option<String>,
    /// Series sequence string (e.g. `"1"`) for ABS freeform atoms.
    pub series_index: Option<String>,
    /// Amazon ASIN written to freeform metadata when set.
    pub asin: Option<String>,
    /// ISBN written to freeform metadata when set.
    pub isbn: Option<String>,
    /// Long description / comment body when set.
    pub description: Option<String>,
    /// Content language tag when set.
    pub language: Option<String>,
    /// Tool attribution (`bookclerk 0.1.0`). Defaults to [`bookclerk_tool_tag`] when `None`.
    pub tool: Option<String>,
}

/// `bookclerk <version>` string embedded in encoder / freeform TOOL tags.
#[must_use]
pub fn bookclerk_tool_tag() -> String {
    format!("{BOOKCLERK_TOOL_NAME} {}", env!("CARGO_PKG_VERSION"))
}

/// Apply metadata tags, optional cover embed, and optional chapters.
///
/// Runs in a confined media worker; see the [crate] documentation.
///
/// # Errors
///
/// Returns [`MediaError::InputMissing`] when the input does not exist,
/// [`MediaError::OutputMissing`] when the worker reported success but wrote
/// nothing, and propagates tag-writing and worker failures otherwise.
pub async fn fixup_audiobook(req: FixupRequest) -> Result<MediaOutcome> {
    if !req.input.exists() {
        return Err(MediaError::InputMissing(req.input));
    }
    let output = req.output.clone();
    let pool = crate::pool();
    pool.run(crate::MediaJob::Fixup {
        request: Box::new(req),
    })
    .await?;

    if !output.exists() {
        return Err(MediaError::OutputMissing(output));
    }
    Ok(MediaOutcome { output })
}

/// Copies input to output when needed, then writes MP3 or M4B tags from the fixup request.
pub(crate) fn fixup_audiobook_sync(req: &FixupRequest) -> Result<()> {
    tracing::info!(
        input = %req.input.display(),
        output = %req.output.display(),
        cover = req.cover.is_some(),
        chapters = req.chapters.len(),
        "native metadata fixup"
    );

    if req.input != req.output {
        std::fs::copy(&req.input, &req.output)?;
    }

    // The writer follows what the file holds, never its name: an MP4 named
    // `.mp3` must not be handed to the ID3 writer, nor a FLAC file to the
    // MP4 writer.
    match read_tag_record(&req.output)?.map(|record| record.container) {
        Some(bookclerk_tags::Container::Mp4) => fixup_m4b(req),
        Some(bookclerk_tags::Container::Mp3) => fixup_mp3(req),
        Some(bookclerk_tags::Container::Flac | bookclerk_tags::Container::Ogg) => {
            crate::vorbis_tags::fixup_vorbis(req)
        }
        None => Err(MediaError::Native(format!(
            "cannot write tags into {}: not a container this build reads",
            req.output.display()
        ))),
    }
}

/// Encoder/tool tag: request override, or the Bookclerk binary name and version.
pub(crate) fn tool_string(req: &FixupRequest) -> String {
    req.tool
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(bookclerk_tool_tag)
}

/// ABS-style album string: `title: subtitle` when a subtitle is present.
pub(crate) fn album_title(req: &FixupRequest) -> String {
    // ABS `getFFMetadataObject` uses `title: subtitle` for album when subtitle exists.
    match req.subtitle.as_deref().filter(|s| !s.is_empty()) {
        Some(sub) => format!("{}: {sub}", req.title),
        None => req.title.clone(),
    }
}

/// Normalizes catalog genre lists to `; `-separated ABS tags, preserving comma-in-name genres.
pub(crate) fn genres_for_abs(raw: &str) -> String {
    // ABS accepts `/`, `//`, or `;` as genre separators.
    // Prefer `;` when present so catalog names that contain commas
    // (e.g. "Movie, TV & Video Game Tie-Ins") stay intact.
    let parts: Vec<&str> = if raw.contains(';') {
        raw.split(';').collect()
    } else if raw.contains('/') {
        raw.split('/').collect()
    } else {
        raw.split(',').collect()
    };
    parts
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// ABS grouping string: `series #index` when an index is present.
pub(crate) fn grouping_for_abs(series: &str, index: Option<&str>) -> String {
    match index.filter(|s| !s.is_empty()) {
        Some(idx) => format!("{series} #{idx}"),
        None => series.to_string(),
    }
}

/// Writes a non-empty iTunes/Tone freeform atom; no-ops on empty values.
fn set_freeform(tag: &mut Mp4Tag, mean: &'static str, name: &'static str, value: &str) {
    if value.is_empty() {
        return;
    }
    tag.set_data(
        FreeformIdent::new_static(mean, name),
        Data::Utf8(value.to_string()),
    );
}

/// Writes audiobook MP4 tags and optional QuickTime chapters onto an existing M4B.
fn fixup_m4b(req: &FixupRequest) -> Result<()> {
    let mut tag = Mp4Tag::read_from_path(&req.output)
        .map_err(|err| MediaError::Native(format!("mp4ameta read failed: {err}")))?;

    let tool = tool_string(req);
    tag.set_title(&req.title);
    tag.set_album(album_title(req));
    tag.set_media_type(MediaType::AudioBook);
    tag.set_encoder(&tool);

    if let Some(author) = req.author.as_deref().filter(|s| !s.is_empty()) {
        tag.set_artist(author);
        tag.set_album_artist(author);
    }
    if let Some(narrator) = req.narrator.as_deref().filter(|s| !s.is_empty()) {
        // ABS maps composer → narrator.
        tag.set_composer(narrator);
    }
    if let Some(year) = req.year.as_deref().filter(|s| !s.is_empty()) {
        tag.set_year(year);
    }
    if let Some(genre) = req.genre.as_deref().filter(|s| !s.is_empty()) {
        tag.set_genre(genres_for_abs(genre));
    }
    if let Some(publisher) = req.publisher.as_deref().filter(|s| !s.is_empty()) {
        tag.set_copyright(publisher);
        set_freeform(&mut tag, ITUNES_MEAN, "PUBLISHER", publisher);
    }
    if let Some(subtitle) = req.subtitle.as_deref().filter(|s| !s.is_empty()) {
        set_freeform(&mut tag, ITUNES_MEAN, "SUBTITLE", subtitle);
    }
    if let Some(desc) = req.description.as_deref().filter(|s| !s.is_empty()) {
        tag.set_description(desc);
        tag.set_comment(desc);
    }
    if let Some(lang) = req.language.as_deref().filter(|s| !s.is_empty()) {
        set_freeform(&mut tag, ITUNES_MEAN, "LANGUAGE", lang);
    }
    if let Some(series) = req.series.as_deref().filter(|s| !s.is_empty()) {
        // ©mvn / series-part style fields ABS also reads as series / series-part.
        tag.set_movement(series);
        tag.set_grouping(grouping_for_abs(series, req.series_index.as_deref()));
        set_freeform(&mut tag, TONE_MEAN, "SERIES", series);
        set_freeform(&mut tag, ITUNES_MEAN, "SERIES", series);
        if let Some(idx) = req.series_index.as_deref().filter(|s| !s.is_empty()) {
            set_freeform(&mut tag, TONE_MEAN, "PART", idx);
            set_freeform(&mut tag, ITUNES_MEAN, "SERIES-PART", idx);
            if let Ok(n) = idx.parse::<u16>() {
                tag.set_movement_index(n);
            }
        }
    }
    if let Some(asin) = req.asin.as_deref().filter(|s| !s.is_empty()) {
        set_freeform(&mut tag, TONE_MEAN, "AUDIBLE_ASIN", asin);
        set_freeform(&mut tag, ITUNES_MEAN, "ASIN", asin);
        set_freeform(&mut tag, ITUNES_MEAN, "AUDIBLE_ASIN", asin);
    }
    if let Some(isbn) = req.isbn.as_deref().filter(|s| !s.is_empty()) {
        set_freeform(&mut tag, ITUNES_MEAN, "ISBN", isbn);
    }

    // Identify Bookclerk as the metadata writer (binary name + version).
    set_freeform(&mut tag, BOOKCLERK_MEAN, "TOOL", &tool);

    if let Some(cover) = &req.cover {
        if cover.exists() {
            let bytes = std::fs::read(cover)?;
            tag.set_artwork(Img::new(guess_img_fmt(cover, &bytes), bytes));
        }
    }

    // Packaged Libro (and similar) M4Bs often already carry player-compatible
    // chapter tracks that are track-boundary placeholders, not literary chapters.
    // Preserve those by default; callers that supply a preferred chapter list
    // (e.g. Audible tree overlaid onto Libro) set `replace_chapters`.
    let has_existing_chapters = !tag.chapter_list().is_empty() || !tag.chapter_track().is_empty();
    let write_chapters =
        !req.chapters.is_empty() && (req.replace_chapters || !has_existing_chapters);
    if !req.chapters.is_empty() && has_existing_chapters && !req.replace_chapters {
        tracing::debug!(
            existing_list = tag.chapter_list().len(),
            existing_track = tag.chapter_track().len(),
            incoming = req.chapters.len(),
            "preserving existing M4B chapters; skipping manifest rewrite"
        );
    }

    // Tags only via mp4ameta. Its QuickTime chapter track is not AVFoundation-
    // conformant (wrong tkhd flags / timescale / sample entry), so chapters are
    // written once below by `write_audiobook_chapters`.
    tag.chapter_list_mut().clear();
    tag.chapter_track_mut().clear();
    let write_cfg = WriteConfig {
        write_chapter_list: false,
        write_chapter_track: false,
        ..WriteConfig::DEFAULT
    };
    write_tags(&req.output, &tag, &write_cfg)?;

    if write_chapters {
        write_audiobook_chapters(&req.output, &req.chapters)?;
    }
    Ok(())
}

/// Write `tag` into an M4B whose media is already on disk.
///
/// mp4ameta makes room for metadata by moving everything that follows it, and it
/// reads the moved bytes into memory to do so. In a faststart book that is the
/// entire audio track, which is more than a small VPS has to spare — so it is
/// handed a header-only stand-in instead: the same `moov` at the same offset,
/// with a stub where the media would be. The `moov` that comes back is patched
/// into the real file, where the reserved slack absorbs the growth.
///
/// When `moov` is already last there is nothing behind it to move, so mp4ameta
/// writes to the file directly.
fn write_tags(path: &Path, tag: &Mp4Tag, cfg: &WriteConfig) -> Result<()> {
    use std::io::{Read, Write};

    let (at, mut moov) = bookclerk_mp4::read_moov(path)?;
    if at.is_last {
        return tag
            .write_with_path(path, cfg)
            .map_err(|err| MediaError::Native(format!("mp4ameta write failed: {err}")));
    }
    // Anything appended to `moov` lands after its last child, so the slack has
    // to come off first and go back on at the end.
    bookclerk_mp4::strip_trailing_free(&mut moov)?;

    let sidecar = crate::package_m4b::scratch_beside(path)?;
    {
        let mut source = std::fs::File::open(path)?;
        let mut stand_in = sidecar.as_file();
        // Everything ahead of `moov` is the brands, so `moov` lands at the same
        // offset it has in the real file and the arithmetic matches.
        std::io::copy(&mut Read::take(&mut source, at.start), &mut stand_in)?;
        stand_in.write_all(&moov)?;
        // mp4ameta refuses a file with no media; it only reads the bounds.
        stand_in.write_all(&8u32.to_be_bytes())?;
        stand_in.write_all(b"mdat")?;
        stand_in.flush()?;
    }

    tag.write_with_path(sidecar.path(), cfg)
        .map_err(|err| MediaError::Native(format!("mp4ameta write failed: {err}")))?;

    let (_, mut edited) = bookclerk_mp4::read_moov(sidecar.path())?;
    // mp4ameta pushed the stub media along to make room and corrected the chunk
    // offsets to match. The real media is staying where it is, so undo that.
    let delta = edited.len() as i64 - moov.len() as i64;
    bookclerk_mp4::shift_chunk_offsets(&mut edited, at.start, -delta)?;
    crate::moov::write_moov(path, at, edited)
}

/// Adds a non-empty ID3 TXXX frame; no-ops on empty values.
fn set_txxx(tag: &mut Id3Tag, description: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    tag.add_frame(ExtendedText {
        description: description.to_string(),
        value: value.to_string(),
    });
}

/// Writes ID3v2.4 audiobook tags, cover and chapters onto an MP3.
fn fixup_mp3(req: &FixupRequest) -> Result<()> {
    let mut tag = Id3Tag::read_from_path(&req.output).unwrap_or_else(|_| Id3Tag::new());
    let tool = tool_string(req);
    tag.set_title(&req.title);
    tag.set_album(album_title(req));
    tag.set_text("TSSE", tool.as_str());
    if let Some(author) = req.author.as_deref().filter(|s| !s.is_empty()) {
        tag.set_artist(author);
        tag.set_album_artist(author);
    }
    if let Some(narrator) = req.narrator.as_deref().filter(|s| !s.is_empty()) {
        tag.set_text("TCOM", narrator);
    }
    if let Some(year) = req.year.as_deref().filter(|s| !s.is_empty()) {
        tag.set_text("TDRC", year);
        tag.set_text("TYER", year);
    }
    if let Some(genre) = req.genre.as_deref().filter(|s| !s.is_empty()) {
        tag.set_genre(genres_for_abs(genre));
    }
    if let Some(publisher) = req.publisher.as_deref().filter(|s| !s.is_empty()) {
        tag.set_text("TPUB", publisher);
        tag.set_text("TCOP", publisher);
    }
    if let Some(subtitle) = req.subtitle.as_deref().filter(|s| !s.is_empty()) {
        tag.set_text("TIT3", subtitle);
    }
    if let Some(desc) = req.description.as_deref().filter(|s| !s.is_empty()) {
        tag.set_text("COMM", desc);
        set_txxx(&mut tag, "description", desc);
    }
    if let Some(lang) = req.language.as_deref().filter(|s| !s.is_empty()) {
        tag.set_text("TLAN", lang);
    }
    if let Some(series) = req.series.as_deref().filter(|s| !s.is_empty()) {
        set_txxx(&mut tag, "SERIES", series);
        set_txxx(&mut tag, "series", series);
        set_txxx(&mut tag, "MVNM", series);
        if let Some(idx) = req.series_index.as_deref().filter(|s| !s.is_empty()) {
            set_txxx(&mut tag, "SERIES-PART", idx);
            set_txxx(&mut tag, "series-part", idx);
            set_txxx(&mut tag, "PART", idx);
            set_txxx(&mut tag, "MVIN", idx);
        }
    }
    if let Some(asin) = req.asin.as_deref().filter(|s| !s.is_empty()) {
        set_txxx(&mut tag, "ASIN", asin);
        set_txxx(&mut tag, "AUDIBLE_ASIN", asin);
        set_txxx(&mut tag, "asin", asin);
        set_txxx(&mut tag, "audible_asin", asin);
    }
    if let Some(isbn) = req.isbn.as_deref().filter(|s| !s.is_empty()) {
        set_txxx(&mut tag, "ISBN", isbn);
        set_txxx(&mut tag, "isbn", isbn);
    }
    set_txxx(&mut tag, "BOOKCLERK_TOOL", &tool);

    if let Some(cover) = &req.cover {
        if cover.exists() {
            let bytes = std::fs::read(cover)?;
            let mime = match guess_img_fmt(cover, &bytes) {
                ImgFmt::Png => "image/png",
                ImgFmt::Bmp => "image/bmp",
                ImgFmt::Jpeg => "image/jpeg",
            };
            tag.remove_all_pictures();
            tag.add_frame(Picture {
                mime_type: mime.to_string(),
                picture_type: PictureType::CoverFront,
                description: "Cover".to_string(),
                data: bytes,
            });
        }
    }
    if !req.chapters.is_empty() {
        write_mp3_chapters(&mut tag, req)?;
    }
    tag.write_to_path(&req.output, Id3Version::Id3v24)
        .map_err(|err| MediaError::Native(format!("id3 write failed: {err}")))?;
    Ok(())
}

/// Element id of the table of contents that lists the chapters Bookclerk writes.
const MP3_TOC_ID: &str = "toc";

/// Writes `req.chapters` as ID3 `CHAP` frames listed by a top-level, ordered
/// `CTOC` (with child tables past 255 chapters), unless the file already has chapters and `replace_chapters` is off.
///
/// Each chapter ends where the next one starts; the last ends at the file's
/// running time, read from its headers by the same reader the tag worker uses.
///
/// # Errors
///
/// Returns an error when the file cannot be read to find its running time.
fn write_mp3_chapters(tag: &mut Id3Tag, req: &FixupRequest) -> Result<()> {
    let has_existing = tag.chapters().next().is_some() || tag.tables_of_contents().next().is_some();
    if has_existing && !req.replace_chapters {
        tracing::info!(
            incoming = req.chapters.len(),
            "preserving existing MP3 chapters; skipping manifest rewrite"
        );
        return Ok(());
    }
    let Some(duration) = audio_duration(&req.output)? else {
        tracing::warn!(
            chapters = req.chapters.len(),
            "MP3 running time unknown; chapters not embedded"
        );
        return Ok(());
    };
    let spans = mp3_chapter_spans(&req.chapters, duration);
    if spans.is_empty() {
        tracing::warn!(
            chapters = req.chapters.len(),
            duration_ms = duration.ms,
            "no chapter starts inside the MP3's running time; chapters not embedded"
        );
        return Ok(());
    }
    tag.remove_all_chapters();
    tag.remove_all_tables_of_contents();
    let mut elements = Vec::with_capacity(spans.len());
    for (index, span) in spans.into_iter().enumerate() {
        let element_id = format!("ch{index}");
        tag.add_frame(Id3Chapter {
            element_id: element_id.clone(),
            start_time: span.start_ms,
            end_time: span.end_ms,
            // All ones: "no byte offset", so readers use the times.
            start_offset: u32::MAX,
            end_offset: u32::MAX,
            frames: vec![Frame::text("TIT2", span.title)],
        });
        elements.push(element_id);
    }
    for toc in mp3_tables_of_contents(elements) {
        tag.add_frame(toc);
    }
    Ok(())
}

/// Most entries one ID3 `CTOC` frame can list: its entry count is one byte.
const MP3_MAX_TOC_CHILDREN: usize = 255;

/// Most chapters Bookclerk writes into one MP3: a top-level `CTOC` listing
/// [`MP3_MAX_TOC_CHILDREN`] child tables of the same size each.
const MP3_MAX_CHAPTERS: usize = MP3_MAX_TOC_CHILDREN * MP3_MAX_TOC_CHILDREN;

/// Builds the ordered tables of contents that list `chapter_ids` in order.
///
/// Up to [`MP3_MAX_TOC_CHILDREN`] chapters fit in the one top-level table.
/// Beyond that, the top-level table lists child tables (`toc0`, `toc1`, …),
/// each listing up to that many chapters, so no table overflows its one-byte
/// entry count. The caller keeps `chapter_ids` within [`MP3_MAX_CHAPTERS`];
/// anything past that is not listed.
///
/// # Returns
///
/// The top-level table first, then the child tables in order.
fn mp3_tables_of_contents(chapter_ids: Vec<String>) -> Vec<TableOfContents> {
    let table = |element_id: String, top_level: bool, elements: Vec<String>| TableOfContents {
        element_id,
        top_level,
        ordered: true,
        elements,
        frames: Vec::new(),
    };
    if chapter_ids.len() <= MP3_MAX_TOC_CHILDREN {
        return vec![table(MP3_TOC_ID.to_string(), true, chapter_ids)];
    }
    let children: Vec<TableOfContents> = chapter_ids
        .chunks(MP3_MAX_TOC_CHILDREN)
        .take(MP3_MAX_TOC_CHILDREN)
        .enumerate()
        .map(|(index, ids)| table(format!("{MP3_TOC_ID}{index}"), false, ids.to_vec()))
        .collect();
    let top = table(
        MP3_TOC_ID.to_string(),
        true,
        children
            .iter()
            .map(|child| child.element_id.clone())
            .collect(),
    );
    std::iter::once(top).chain(children).collect()
}

/// One chapter as an ID3 `CHAP` frame states it: title and a half-open
/// window `[start_ms, end_ms)` in milliseconds from the start of the audio.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mp3ChapterSpan {
    /// Chapter title (the frame's `TIT2`).
    title: String,
    /// Start in milliseconds.
    start_ms: u32,
    /// End in milliseconds; never before `start_ms`.
    end_ms: u32,
}

/// Turns chapter starts into `CHAP` windows over a file `duration` long.
///
/// Chapters are ordered by start, and two chapters starting at the same
/// time keep the first. At most [`MP3_MAX_CHAPTERS`] are kept, the earliest.
/// Every start that does not fit the frame's 32-bit
/// millisecond field is dropped. When the running time is exact, starts at
/// or past it are dropped too, since a player could never reach them. An
/// estimated running time (a variable bit rate MP3 without a frame count)
/// can be far too short, so no start is dropped for it and the last
/// chapter ends at the later of the estimate and its own start.
///
/// # Returns
///
/// The windows in order: each ends where the next starts, the last at the
/// running time (capped to the field).
fn mp3_chapter_spans(
    chapters: &[(String, u64)],
    duration: bookclerk_tags::Duration,
) -> Vec<Mp3ChapterSpan> {
    let reachable = |start: u64| match duration.precision {
        bookclerk_tags::DurationPrecision::Exact => start < duration.ms,
        bookclerk_tags::DurationPrecision::Estimated => true,
    };
    let mut starts: Vec<(u32, &str)> = chapters
        .iter()
        .filter(|(_, start)| reachable(*start))
        .filter_map(|(title, start)| u32::try_from(*start).ok().map(|s| (s, title.as_str())))
        .collect();
    starts.sort_by_key(|(start, _)| *start);
    starts.dedup_by_key(|(start, _)| *start);
    // Cap before closing the windows, so the last kept chapter still ends
    // at the running time and not at the first dropped start.
    if starts.len() > MP3_MAX_CHAPTERS {
        tracing::warn!(
            chapters = starts.len(),
            kept = MP3_MAX_CHAPTERS,
            "more chapters than two levels of ID3 tables of contents can list; extra chapters dropped"
        );
        starts.truncate(MP3_MAX_CHAPTERS);
    }
    let end_of_file = u32::try_from(duration.ms).unwrap_or(u32::MAX);
    let next_starts = starts
        .iter()
        .skip(1)
        .map(|(start, _)| *start)
        .chain(std::iter::once(end_of_file));
    starts
        .iter()
        .zip(next_starts)
        .map(|((start_ms, title), end_ms)| Mp3ChapterSpan {
            title: (*title).to_string(),
            start_ms: *start_ms,
            end_ms: end_ms.max(*start_ms),
        })
        .collect()
}

/// Running time of the audio file at `path`, read from its headers
/// ([`read_tag_record`]), with whether it is exact or estimated.
///
/// # Returns
///
/// `None` when the reader cannot parse the file or it states no running time.
///
/// # Errors
///
/// Returns an error when the file cannot be opened or read.
fn audio_duration(path: &Path) -> Result<Option<bookclerk_tags::Duration>> {
    Ok(read_tag_record(path)?.and_then(|r| r.duration))
}

/// The headers of the audio file at `path`, read by
/// [`bookclerk_tags::TagReader`].
///
/// # Returns
///
/// `None` when the reader does not recognise or cannot parse the file.
///
/// # Errors
///
/// Returns an error when the file cannot be opened or read.
pub(crate) fn read_tag_record(path: &Path) -> Result<Option<bookclerk_tags::TagRecord>> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut reader = bookclerk_tags::TagReader::new(size);
    loop {
        match reader.poll() {
            bookclerk_tags::Poll::Done(record) => {
                return Ok(record.ok());
            }
            bookclerk_tags::Poll::Need(spans) => {
                for span in spans {
                    let len = usize::try_from(span.len).map_err(|_| {
                        MediaError::Native(format!("tag span of {} bytes", span.len))
                    })?;
                    let mut bytes = vec![0; len];
                    file.seek(SeekFrom::Start(span.offset))?;
                    file.read_exact(&mut bytes)?;
                    if reader.supply(span.offset, bytes).is_err() {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

/// Detects PNG/BMP/JPEG from magic bytes, then falls back to the file extension.
pub(crate) fn guess_img_fmt(path: &Path, bytes: &[u8]) -> ImgFmt {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return ImgFmt::Png;
    }
    if bytes.starts_with(b"BM") {
        return ImgFmt::Bmp;
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return ImgFmt::Jpeg;
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => ImgFmt::Png,
        Some("bmp") => ImgFmt::Bmp,
        _ => ImgFmt::Jpeg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An exact running time of `ms` milliseconds.
    fn exact(ms: u64) -> bookclerk_tags::Duration {
        bookclerk_tags::Duration {
            ms,
            precision: bookclerk_tags::DurationPrecision::Exact,
        }
    }

    #[test]
    fn chapter_spans_keep_starts_past_an_estimated_running_time() {
        let chapters = vec![("One".to_string(), 0), ("Ten seconds".to_string(), 10_000)];
        let estimated = bookclerk_tags::Duration {
            ms: 4_937,
            precision: bookclerk_tags::DurationPrecision::Estimated,
        };
        let spans = mp3_chapter_spans(&chapters, estimated);
        let windows: Vec<(u32, u32)> = spans.iter().map(|s| (s.start_ms, s.end_ms)).collect();
        assert_eq!(windows, vec![(0, 10_000), (10_000, 10_000)]);
        assert_eq!(mp3_chapter_spans(&chapters, exact(4_937)).len(), 1);
    }

    #[test]
    fn chapter_spans_past_the_cap_still_end_at_the_running_time() {
        let chapters: Vec<(String, u64)> = (0..=MP3_MAX_CHAPTERS as u64)
            .map(|index| (format!("Chapter {index}"), index * 1_000))
            .collect();
        let spans = mp3_chapter_spans(&chapters, exact(100_000_000));
        assert_eq!(spans.len(), MP3_MAX_CHAPTERS);
        let last = spans.last().expect("last chapter");
        assert_eq!(last.start_ms, 65_024_000);
        assert_eq!(last.end_ms, 100_000_000);
    }

    /// A fix-up of the MP3 at `path` in place, carrying `chapters`.
    fn mp3_request(path: &Path, chapters: &[(&str, u64)], replace: bool) -> FixupRequest {
        FixupRequest {
            input: path.to_path_buf(),
            output: path.to_path_buf(),
            title: "The Book".into(),
            author: None,
            narrator: None,
            cover: None,
            chapters: chapters
                .iter()
                .map(|(title, start)| ((*title).to_string(), *start))
                .collect(),
            replace_chapters: replace,
            subtitle: None,
            publisher: None,
            year: None,
            genre: None,
            series: None,
            series_index: None,
            asin: None,
            isbn: None,
            description: None,
            language: None,
            tool: None,
        }
    }

    /// Chapters the tag reader finds in the file at `path`, as `(title, start)`.
    fn read_chapters(path: &Path) -> Vec<(String, u64)> {
        let record = bookclerk_tags::read_all(&std::fs::read(path).expect("read")).expect("tags");
        record
            .chapters
            .into_iter()
            .map(|chapter| (chapter.title, chapter.start_ms))
            .collect()
    }

    /// The fixture MP3 written to a fresh temporary file.
    fn fixture_mp3(with_chapters: bool) -> tempfile::NamedTempFile {
        let mut bytes = bookclerk_tags::fixtures::tagged_mp3();
        if !with_chapters {
            let mut tag = Id3Tag::read_from2(std::io::Cursor::new(&bytes)).expect("tag");
            tag.remove_all_chapters();
            let audio = bytes.split_off(bytes.len() - 8 * 417);
            bytes.clear();
            tag.write_to(&mut bytes, Id3Version::Id3v24).expect("write");
            bytes.extend(audio);
        }
        let file = tempfile::Builder::new()
            .suffix(".mp3")
            .tempfile()
            .expect("tmp");
        std::fs::write(file.path(), bytes).expect("write");
        file
    }

    #[test]
    fn mp3_fixup_embeds_chapters_the_tag_reader_reads_back() {
        let file = fixture_mp3(false);
        let duration = audio_duration(file.path())
            .expect("read")
            .expect("duration")
            .ms;
        assert!(duration > 100, "fixture runs {duration} ms");
        let req = mp3_request(
            file.path(),
            &[("Two", 100), ("One", 0), ("Past the end", duration)],
            false,
        );
        fixup_audiobook_sync(&req).expect("fixup");
        assert_eq!(
            read_chapters(file.path()),
            vec![("One".to_string(), 0), ("Two".to_string(), 100)]
        );
        let tag = Id3Tag::read_from_path(file.path()).expect("tag");
        let ends: Vec<u32> = tag.chapters().map(|c| c.end_time).collect();
        assert_eq!(ends, vec![100, u32::try_from(duration).expect("fits")]);
        let toc = tag.tables_of_contents().next().expect("toc");
        assert!(toc.top_level && toc.ordered);
        assert_eq!(toc.elements, vec!["ch0", "ch1"]);
    }

    #[test]
    fn mp3_tables_of_contents_split_past_255_chapters_and_round_trip() {
        let small = mp3_tables_of_contents(vec!["ch0".into(), "ch1".into()]);
        assert_eq!(small.len(), 1);
        assert!(small[0].top_level && small[0].ordered);
        assert_eq!(small[0].elements, vec!["ch0", "ch1"]);

        let ids: Vec<String> = (0..600).map(|index| format!("ch{index}")).collect();
        let mut tag = Id3Tag::new();
        for toc in mp3_tables_of_contents(ids.clone()) {
            tag.add_frame(toc);
        }
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, Id3Version::Id3v24).expect("write");
        let read = Id3Tag::read_from2(std::io::Cursor::new(&bytes)).expect("read");
        let tables: Vec<&TableOfContents> = read.tables_of_contents().collect();
        let top: Vec<&&TableOfContents> = tables.iter().filter(|toc| toc.top_level).collect();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].elements, vec!["toc0", "toc1", "toc2"]);
        let mut listed = Vec::new();
        for child_id in &top[0].elements {
            let child = tables
                .iter()
                .find(|toc| &toc.element_id == child_id)
                .expect("child table");
            assert!(!child.top_level && child.ordered);
            assert!(child.elements.len() <= MP3_MAX_TOC_CHILDREN);
            listed.extend(child.elements.iter().cloned());
        }
        assert_eq!(listed, ids);

        let too_many: Vec<String> = (0..MP3_MAX_CHAPTERS + 1)
            .map(|i| format!("ch{i}"))
            .collect();
        let tables = mp3_tables_of_contents(too_many);
        assert_eq!(tables[0].elements.len(), MP3_MAX_TOC_CHILDREN);
        assert_eq!(tables.len(), MP3_MAX_TOC_CHILDREN + 1);
    }

    #[test]
    fn mp3_fixup_keeps_existing_chapters_unless_told_to_replace() {
        let file = fixture_mp3(true);
        let before = read_chapters(file.path());
        assert_eq!(before.len(), 2);
        fixup_audiobook_sync(&mp3_request(file.path(), &[("New", 0)], false)).expect("fixup");
        assert_eq!(read_chapters(file.path()), before);

        fixup_audiobook_sync(&mp3_request(file.path(), &[("New", 0)], true)).expect("fixup");
        assert_eq!(read_chapters(file.path()), vec![("New".to_string(), 0)]);
        let tag = Id3Tag::read_from_path(file.path()).expect("tag");
        assert_eq!(tag.chapters().count(), 1);
        assert_eq!(tag.tables_of_contents().count(), 1);
    }

    #[test]
    fn mp3_fixup_skips_chapters_that_all_start_past_the_end() {
        let file = fixture_mp3(false);
        fixup_audiobook_sync(&mp3_request(file.path(), &[("Late", 3_600_000)], false))
            .expect("fixup");
        assert!(read_chapters(file.path()).is_empty());
        assert_eq!(
            Id3Tag::read_from_path(file.path())
                .expect("tag")
                .chapters()
                .count(),
            0
        );
    }

    #[test]
    fn fixup_picks_the_writer_by_content_not_by_name() {
        let dir = tempfile::tempdir().expect("dir");
        // An MP4 named `.mp3` gets MP4 tags, not an ID3 tag glued to its front.
        let m4b = dir.path().join("tone.m4b");
        crate::package_m4b::package_m4b_from_pcm(&[0i16; 44_100], 44_100, 1, &m4b, &[])
            .expect("package");
        let misnamed = dir.path().join("book.mp3");
        std::fs::rename(&m4b, &misnamed).expect("rename");
        let mut req = mp3_request(&misnamed, &[], false);
        req.title = "Renamed".into();
        fixup_audiobook_sync(&req).expect("fixup");
        let record =
            bookclerk_tags::read_all(&std::fs::read(&misnamed).expect("read")).expect("tags");
        assert_eq!(record.container, bookclerk_tags::Container::Mp4);
        let title = Mp4Tag::read_from_path(&misnamed).expect("mp4 tag");
        assert_eq!(title.title(), Some("Renamed"));

        // An MP3 named `.m4b` gets ID3 tags.
        let mp3 = fixture_mp3(false);
        let renamed = dir.path().join("book.m4b");
        std::fs::copy(mp3.path(), &renamed).expect("copy");
        let mut req = mp3_request(&renamed, &[], false);
        req.title = "Also renamed".into();
        fixup_audiobook_sync(&req).expect("fixup");
        let tag = Id3Tag::read_from_path(&renamed).expect("id3");
        assert_eq!(tag.title(), Some("Also renamed"));
    }

    #[test]
    fn fixup_refuses_containers_it_cannot_write() {
        let dir = tempfile::tempdir().expect("dir");
        let flac = dir.path().join("book.m4b");
        std::fs::write(&flac, bookclerk_tags::fixtures::tagged_flac()).expect("write");
        let before = std::fs::read(&flac).expect("read");
        let err = fixup_audiobook_sync(&mp3_request(&flac, &[], false)).expect_err("flac");
        assert!(err.to_string().contains("FLAC"), "{err}");
        assert_eq!(
            std::fs::read(&flac).expect("read"),
            before,
            "file untouched"
        );

        let text = dir.path().join("notes.mp3");
        std::fs::write(&text, b"plain text, not audio").expect("write");
        let err = fixup_audiobook_sync(&mp3_request(&text, &[], false)).expect_err("text");
        assert!(err.to_string().contains("cannot write tags"), "{err}");
    }

    #[test]
    fn chapter_spans_are_ordered_deduplicated_and_closed_at_the_end() {
        let chapters = vec![
            ("b".to_string(), 2_000),
            ("a".to_string(), 0),
            ("a again".to_string(), 0),
            ("late".to_string(), 10_000),
            ("huge".to_string(), u64::MAX),
        ];
        let spans = mp3_chapter_spans(&chapters, exact(5_000));
        assert_eq!(
            spans,
            vec![
                Mp3ChapterSpan {
                    title: "a".into(),
                    start_ms: 0,
                    end_ms: 2_000
                },
                Mp3ChapterSpan {
                    title: "b".into(),
                    start_ms: 2_000,
                    end_ms: 5_000
                },
            ]
        );
        assert!(mp3_chapter_spans(&chapters, exact(0)).is_empty());
    }

    #[test]
    fn chapter_spans_cap_a_running_time_past_the_frame_field() {
        let long = u64::from(u32::MAX) + 10;
        let spans = mp3_chapter_spans(&[("only".to_string(), 5)], exact(long));
        assert_eq!(spans[0].end_ms, u32::MAX);
    }

    #[test]
    fn duration_of_a_file_the_reader_refuses_is_unknown() {
        let file = tempfile::NamedTempFile::new().expect("tmp");
        std::fs::write(file.path(), b"not audio at all").expect("write");
        assert_eq!(audio_duration(file.path()).expect("read"), None);
    }

    #[test]
    fn tool_tag_includes_binary_name_and_version() {
        let tag = bookclerk_tool_tag();
        assert!(tag.starts_with("bookclerk "), "{tag}");
        assert!(tag.contains(env!("CARGO_PKG_VERSION")), "{tag}");
    }

    #[test]
    fn album_includes_subtitle_like_abs() {
        let req = FixupRequest {
            input: PathBuf::from("in.m4b"),
            output: PathBuf::from("out.m4b"),
            title: "Title".into(),
            author: None,
            narrator: None,
            cover: None,
            chapters: vec![],
            replace_chapters: false,
            subtitle: Some("A Subtitle".into()),
            publisher: None,
            year: None,
            genre: None,
            series: None,
            series_index: None,
            asin: None,
            isbn: None,
            description: None,
            language: None,
            tool: None,
        };
        assert_eq!(album_title(&req), "Title: A Subtitle");
    }

    #[test]
    fn genres_normalized_for_abs() {
        assert_eq!(
            genres_for_abs("Classics, Fiction, Science Fiction"),
            "Classics; Fiction; Science Fiction"
        );
        assert_eq!(
            genres_for_abs("Literature & Fiction; Movie, TV & Video Game Tie-Ins"),
            "Literature & Fiction; Movie, TV & Video Game Tie-Ins"
        );
    }
}
