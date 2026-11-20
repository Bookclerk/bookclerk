//! Tags, cover and chapters in Vorbis comments: FLAC files and Ogg (Opus
//! or Vorbis) files, written through `lofty`.
//!
//! The field mapping follows the names Audiobookshelf and ffprobe read
//! from Vorbis comments: `ARTIST` and `ALBUMARTIST` for the author,
//! `COMPOSER` for the narrator, `SERIES` / `SERIES-PART`, `ASIN`, `ISBN`
//! and `DESCRIPTION`. Chapters are `CHAPTERnnn` / `CHAPTERnnnNAME` pairs
//! (the convention ffmpeg, Audiobookshelf and most players read), and the
//! cover is a front-cover picture (`METADATA_BLOCK_PICTURE` in Ogg, a
//! `PICTURE` block in FLAC).

use std::fs::File;
use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OggPictureStorage, OpusFile, VorbisFile};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::TagExt;

use crate::error::{MediaError, Result};
use crate::metadata::{
    album_title, genres_for_abs, grouping_for_abs, guess_img_fmt, read_tag_record, tool_string,
    FixupRequest,
};
use mp4ameta::ImgFmt;

/// Most chapters written: the three digits of `CHAPTERnnn` allow 999.
const MAX_CHAPTERS: usize = 999;

/// The comment keys Bookclerk owns: set from the request when it has a
/// value, otherwise left as the file had them.
const SINGLE_KEYS: &[&str] = &[
    "TITLE",
    "ALBUM",
    "ARTIST",
    "ALBUMARTIST",
    "COMPOSER",
    "DATE",
    "GENRE",
    "PUBLISHER",
    "SUBTITLE",
    "DESCRIPTION",
    "COMMENT",
    "LANGUAGE",
    "GROUPING",
    "SERIES",
    "SERIES-PART",
    "ASIN",
    "ISBN",
    "ENCODER",
];

/// Which file layout holds the comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// A FLAC file's Vorbis comment block.
    Flac,
    /// An Ogg Opus stream's `OpusTags` header.
    OggOpus,
    /// An Ogg Vorbis stream's comment header.
    OggVorbis,
}

/// Writes the request's tags, cover and chapters into a FLAC or Ogg file
/// in place at `req.output`.
///
/// Existing comments the request has no value for are kept; chapters are
/// kept unless the request carries some and `replace_chapters` is set or
/// the file has none.
///
/// # Errors
///
/// [`MediaError::Native`] when the file is not FLAC, Ogg Opus or Ogg
/// Vorbis, or `lofty` cannot read or write it; an I/O error when the cover
/// cannot be read.
pub(crate) fn fixup_vorbis(req: &FixupRequest) -> Result<()> {
    let layout = layout_of(&req.output)?;
    let mut tag = read_comments(&req.output, layout)?;
    let tool = tool_string(req);
    let mut set = |key: &str, value: Option<&str>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            tag.insert(key.to_string(), value.to_string());
        }
    };
    set("TITLE", Some(&req.title));
    set("ALBUM", Some(&album_title(req)));
    set("ARTIST", req.author.as_deref());
    set("ALBUMARTIST", req.author.as_deref());
    set("COMPOSER", req.narrator.as_deref());
    set("DATE", req.year.as_deref());
    set("GENRE", req.genre.as_deref().map(genres_for_abs).as_deref());
    set("PUBLISHER", req.publisher.as_deref());
    set("SUBTITLE", req.subtitle.as_deref());
    set("DESCRIPTION", req.description.as_deref());
    set("COMMENT", req.description.as_deref());
    set("LANGUAGE", req.language.as_deref());
    if let Some(series) = req.series.as_deref().filter(|s| !s.is_empty()) {
        set("SERIES", Some(series));
        set("SERIES-PART", req.series_index.as_deref());
        set(
            "GROUPING",
            Some(&grouping_for_abs(series, req.series_index.as_deref())),
        );
    }
    set("ASIN", req.asin.as_deref());
    set("ISBN", req.isbn.as_deref());
    set("ENCODER", Some(&tool));
    debug_assert!(SINGLE_KEYS.contains(&"ENCODER"));

    write_chapters(&mut tag, req);
    if let Some(cover) = req.cover.as_deref().filter(|c| c.exists()) {
        let bytes = std::fs::read(cover)?;
        let mime = match guess_img_fmt(cover, &bytes) {
            ImgFmt::Png => MimeType::Png,
            ImgFmt::Bmp => MimeType::Bmp,
            ImgFmt::Jpeg => MimeType::Jpeg,
        };
        let picture = Picture::unchecked(bytes)
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .description("Cover")
            .build();
        tag.remove_picture_type(PictureType::CoverFront);
        tag.insert_picture(picture, None)
            .map_err(|err| MediaError::Native(format!("cover refused: {err}")))?;
    }
    tag.save_to_path(&req.output, WriteOptions::default())
        .map_err(|err| MediaError::Native(format!("Vorbis comment write failed: {err}")))?;
    Ok(())
}

/// The layout of the file at `path`, from what the tag reader finds in it.
///
/// # Errors
///
/// [`MediaError::Native`] for any other container or codec.
fn layout_of(path: &Path) -> Result<Layout> {
    let record = read_tag_record(path)?;
    match record.as_ref().map(|r| (r.container, r.codec)) {
        Some((bookclerk_tags::Container::Flac, _)) => Ok(Layout::Flac),
        Some((bookclerk_tags::Container::Ogg, Some(bookclerk_tags::AudioCodec::Opus))) => {
            Ok(Layout::OggOpus)
        }
        Some((bookclerk_tags::Container::Ogg, Some(bookclerk_tags::AudioCodec::Vorbis))) => {
            Ok(Layout::OggVorbis)
        }
        _ => Err(MediaError::Native(format!(
            "cannot write Vorbis comments into {}",
            path.display()
        ))),
    }
}

/// The comments the file already holds (an empty set when it has none).
///
/// # Errors
///
/// [`MediaError::Native`] when `lofty` cannot read the file.
fn read_comments(path: &Path, layout: Layout) -> Result<VorbisComments> {
    let mut file = File::open(path)?;
    let options = ParseOptions::new().read_properties(false);
    let refused = |err: lofty::error::FileParseError| {
        MediaError::Native(format!("Vorbis comment read failed: {err}"))
    };
    let comments = match layout {
        Layout::Flac => {
            let mut flac = FlacFile::read_from(&mut file, options).map_err(refused)?;
            let mut comments = flac.vorbis_comments().cloned().unwrap_or_default();
            // FLAC keeps pictures in their own blocks; carry them over so
            // writing the comments does not drop them.
            for (picture, info) in flac.pictures().to_vec() {
                let _ = comments.insert_picture(picture, Some(info));
            }
            flac.remove_vorbis_comments();
            comments
        }
        Layout::OggOpus => OpusFile::read_from(&mut file, options)
            .map_err(refused)?
            .vorbis_comments()
            .clone(),
        Layout::OggVorbis => VorbisFile::read_from(&mut file, options)
            .map_err(refused)?
            .vorbis_comments()
            .clone(),
    };
    Ok(comments)
}

/// Writes `req.chapters` as `CHAPTERnnn` comments, replacing the file's
/// own unless it has some and the request does not ask to replace them.
///
/// Only a `CHAPTERnnn` start counts as an existing chapter: a name with no
/// start is not a chapter to players, so it does not stop the request's
/// chapters from being written (and is removed with the rest).
fn write_chapters(tag: &mut VorbisComments, req: &FixupRequest) {
    let has_existing = tag
        .items()
        .any(|(key, _)| chapter_key_kind(key) == Some(ChapterKey::Start));
    if req.chapters.is_empty() || (has_existing && !req.replace_chapters) {
        return;
    }
    let old: Vec<String> = tag
        .items()
        .filter(|(key, _)| chapter_key_kind(key).is_some())
        .map(|(key, _)| key.to_string())
        .collect();
    for key in old {
        let _ = tag.remove(&key).count();
    }
    let mut chapters = req.chapters.clone();
    chapters.sort_by_key(|(_, start)| *start);
    chapters.dedup_by_key(|(_, start)| *start);
    if chapters.len() > MAX_CHAPTERS {
        tracing::warn!(
            chapters = chapters.len(),
            "more chapters than CHAPTERnnn comments can number; extra chapters dropped"
        );
        chapters.truncate(MAX_CHAPTERS);
    }
    for (index, (title, start_ms)) in chapters.iter().enumerate() {
        let number = index + 1;
        tag.push(format!("CHAPTER{number:03}"), timestamp(*start_ms));
        tag.push(format!("CHAPTER{number:03}NAME"), title.clone());
    }
}

/// Which part of a chapter a comment key holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChapterKey {
    /// `CHAPTERnnn`: the chapter's start time.
    Start,
    /// `CHAPTERnnnNAME`: the chapter's title.
    Name,
}

/// The chapter part `key` names, or `None` when it is neither a
/// `CHAPTERnnn` nor a `CHAPTERnnnNAME` key (any case).
fn chapter_key_kind(key: &str) -> Option<ChapterKey> {
    let upper = key.to_ascii_uppercase();
    let rest = upper.strip_prefix("CHAPTER")?;
    let (digits, kind) = match rest.strip_suffix("NAME") {
        Some(digits) => (digits, ChapterKey::Name),
        None => (rest, ChapterKey::Start),
    };
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(kind)
}

/// `HH:MM:SS.mmm` for a start in milliseconds.
fn timestamp(ms: u64) -> String {
    let (hours, rest) = (ms / 3_600_000, ms % 3_600_000);
    let (minutes, rest) = (rest / 60_000, rest % 60_000);
    let (seconds, millis) = (rest / 1000, rest % 1000);
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::fixup_audiobook_sync;
    use bookclerk_tags::{TagField, TagRecord};

    /// A full fix-up of the file at `path` in place.
    fn request(path: &Path, cover: Option<&Path>, chapters: &[(&str, u64)]) -> FixupRequest {
        FixupRequest {
            input: path.to_path_buf(),
            output: path.to_path_buf(),
            title: "The Book".into(),
            author: Some("An Author".into()),
            narrator: Some("A Narrator".into()),
            cover: cover.map(Path::to_path_buf),
            chapters: chapters
                .iter()
                .map(|(title, start)| ((*title).to_string(), *start))
                .collect(),
            replace_chapters: false,
            subtitle: None,
            publisher: Some("A House".into()),
            year: Some("2011".into()),
            genre: None,
            series: Some("Cycle".into()),
            series_index: Some("2".into()),
            asin: Some("B000TEST01".into()),
            isbn: None,
            description: None,
            language: None,
            tool: None,
        }
    }

    /// The tag reader's record for the file at `path`.
    fn record(path: &Path) -> TagRecord {
        bookclerk_tags::read_all(&std::fs::read(path).expect("read")).expect("tags")
    }

    /// The first value of a freeform `name` comment.
    fn freeform<'a>(record: &'a TagRecord, name: &str) -> Option<&'a str> {
        record.entries.iter().find_map(|entry| match &entry.field {
            TagField::Freeform { name: key, .. } if key == name => Some(entry.value.as_str()),
            _ => None,
        })
    }

    /// Fixes up a copy of `name` from the decode corpus (or an Opus
    /// encode of it) and checks what the tag reader finds.
    fn check_fixup(path: &Path) {
        let dir = path.parent().expect("dir");
        let cover = dir.join("cover.png");
        std::fs::write(&cover, b"\x89PNG\r\n\x1a\nnot really a png").expect("cover");
        let req = request(path, Some(&cover), &[("Opening", 0), ("Middle", 120)]);
        fixup_audiobook_sync(&req).expect("fixup");
        let found = record(path);
        let value = |field: TagField| {
            found
                .entries
                .iter()
                .find(|entry| entry.field == field)
                .map(|entry| entry.value.clone())
        };
        assert_eq!(value(TagField::Title).as_deref(), Some("The Book"));
        assert_eq!(value(TagField::Artist).as_deref(), Some("An Author"));
        assert_eq!(value(TagField::Composer).as_deref(), Some("A Narrator"));
        assert_eq!(freeform(&found, "SERIES"), Some("Cycle"));
        assert_eq!(freeform(&found, "ASIN"), Some("B000TEST01"));
        let chapters: Vec<_> = found
            .chapters
            .iter()
            .map(|c| (c.title.as_str(), c.start_ms))
            .collect();
        assert_eq!(chapters, [("Opening", 0), ("Middle", 120)]);
        assert!(found.cover.is_some(), "cover");
        // A second pass keeps the file's chapters and does not duplicate tags.
        let again = request(path, None, &[("Other", 50)]);
        fixup_audiobook_sync(&again).expect("fixup again");
        let found = record(path);
        assert_eq!(found.chapters.len(), 2);
        let titles = found
            .entries
            .iter()
            .filter(|entry| entry.field == TagField::Title)
            .count();
        assert_eq!(titles, 1);
        assert!(found.cover.is_some(), "cover kept");
        // The audio still decodes.
        crate::decode::decode_pcm_bytes(
            std::fs::read(path).expect("read"),
            crate::OutChannels::SourceUpToStereo,
            |_, _| Ok(()),
        )
        .expect("decodes");
    }

    #[test]
    fn ogg_opus_files_get_tags_chapters_and_a_cover() {
        let dir = tempfile::tempdir().expect("dir");
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/media_decode");
        let opus = dir.path().join("book.opus");
        crate::encode_to_opus_native(
            &corpus.join("tone_2ch.flac"),
            &opus,
            crate::OpusEncodeOptions {
                bitrate_kbps: 24,
                mono: false,
            },
            None,
        )
        .expect("encode");
        check_fixup(&opus);
    }

    #[test]
    fn ogg_vorbis_and_flac_files_get_tags_chapters_and_a_cover() {
        let dir = tempfile::tempdir().expect("dir");
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/media_decode");
        for name in ["tone_2ch.ogg", "tone_2ch.flac"] {
            let path = dir.path().join(name);
            std::fs::copy(corpus.join(name), &path).expect("copy");
            check_fixup(&path);
        }
    }

    #[test]
    fn timestamps_and_chapter_keys() {
        assert_eq!(timestamp(0), "00:00:00.000");
        assert_eq!(timestamp(3_723_456), "01:02:03.456");
        assert_eq!(timestamp(100 * 3_600_000), "100:00:00.000");
        assert_eq!(chapter_key_kind("CHAPTER001"), Some(ChapterKey::Start));
        assert_eq!(chapter_key_kind("chapter012name"), Some(ChapterKey::Name));
        assert_eq!(chapter_key_kind("CHAPTERS"), None);
        assert_eq!(chapter_key_kind("CHAPTER"), None);
        assert_eq!(chapter_key_kind("CHAPTERNAME"), None);
    }

    #[test]
    fn a_chapter_name_with_no_start_does_not_keep_chapters_out() {
        let mut tag = VorbisComments::default();
        tag.push("CHAPTER007NAME".into(), "Stray".into());
        let req = request(
            Path::new("book.opus"),
            None,
            &[("Opening", 0), ("Middle", 120)],
        );
        assert!(!req.replace_chapters);
        write_chapters(&mut tag, &req);
        let keys: Vec<_> = tag
            .items()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        assert_eq!(
            keys,
            [
                ("CHAPTER001".to_string(), "00:00:00.000".to_string()),
                ("CHAPTER001NAME".to_string(), "Opening".to_string()),
                ("CHAPTER002".to_string(), "00:00:00.120".to_string()),
                ("CHAPTER002NAME".to_string(), "Middle".to_string()),
            ]
        );
        // A real start keeps the file's chapters.
        let before = tag.clone();
        write_chapters(
            &mut tag,
            &request(Path::new("book.opus"), None, &[("Other", 50)]),
        );
        assert_eq!(tag.items().count(), before.items().count());
    }
}
