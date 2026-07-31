//! Native audiobook metadata fix-up (M4B via mp4ameta, MP3 via id3).
//!
//! Tag mapping targets [Audiobookshelf file metadata](https://audiobookshelf.org/docs/documentation/libraries/book-library/directory-structure/)
//! plus the `----:com.pilabor.tone:*` freeform atoms ABS embeds for series/ASIN.

use std::path::{Path, PathBuf};

use id3::frame::{ExtendedText, Picture, PictureType};
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
const BOOKCLERK_MEAN: &str = "org.bookclerk";
/// Common iTunes freeform mean (ffprobe often surfaces these without the mean).
const ITUNES_MEAN: &str = "com.apple.iTunes";

/// Request to fix up audiobook metadata after decrypt / download.
#[derive(Debug, Clone)]
pub struct FixupRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    pub title: String,
    pub author: Option<String>,
    pub narrator: Option<String>,
    pub cover: Option<PathBuf>,
    /// Chapter titles + start offsets in milliseconds (embedded for M4B).
    pub chapters: Vec<(String, u64)>,
    /// When true, replace any existing M4B chapter list/track with [`Self::chapters`]
    /// (used when overlaying Audible chapter trees onto Libro packaged M4Bs).
    /// When false, preserve existing chapters if the file already has them.
    pub replace_chapters: bool,
    pub subtitle: Option<String>,
    pub publisher: Option<String>,
    /// Publish year as a string (e.g. `"2011"`).
    pub year: Option<String>,
    /// Genre string; multiple genres should use `;` separators for ABS.
    pub genre: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub description: Option<String>,
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
pub async fn fixup_audiobook(req: FixupRequest) -> Result<MediaOutcome> {
    if !req.input.exists() {
        return Err(MediaError::InputMissing(req.input));
    }
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let output = req.output.clone();
    tokio::task::spawn_blocking(move || fixup_audiobook_sync(&req))
        .await
        .map_err(|err| MediaError::Native(format!("fixup task join error: {err}")))??;

    if !output.exists() {
        return Err(MediaError::OutputMissing(output));
    }
    Ok(MediaOutcome { output })
}

fn fixup_audiobook_sync(req: &FixupRequest) -> Result<()> {
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

    let ext = req
        .output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "mp3" => fixup_mp3(req),
        _ => fixup_m4b(req),
    }
}

fn tool_string(req: &FixupRequest) -> String {
    req.tool
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(bookclerk_tool_tag)
}

fn album_title(req: &FixupRequest) -> String {
    // ABS `getFFMetadataObject` uses `title: subtitle` for album when subtitle exists.
    match req.subtitle.as_deref().filter(|s| !s.is_empty()) {
        Some(sub) => format!("{}: {sub}", req.title),
        None => req.title.clone(),
    }
}

fn genres_for_abs(raw: &str) -> String {
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

fn grouping_for_abs(series: &str, index: Option<&str>) -> String {
    match index.filter(|s| !s.is_empty()) {
        Some(idx) => format!("{series} #{idx}"),
        None => series.to_string(),
    }
}

fn set_freeform(tag: &mut Mp4Tag, mean: &'static str, name: &'static str, value: &str) {
    if value.is_empty() {
        return;
    }
    tag.set_data(
        FreeformIdent::new_static(mean, name),
        Data::Utf8(value.to_string()),
    );
}

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
    tag.write_with_path(&req.output, &write_cfg)
        .map_err(|err| MediaError::Native(format!("mp4ameta write failed: {err}")))?;

    if write_chapters {
        write_audiobook_chapters(&req.output, &req.chapters)?;
    }
    Ok(())
}

fn set_txxx(tag: &mut Id3Tag, description: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    tag.add_frame(ExtendedText {
        description: description.to_string(),
        value: value.to_string(),
    });
}

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
        tracing::debug!(
            chapters = req.chapters.len(),
            "MP3 fixup skips embedded chapters; use cue/json sidecars"
        );
    }
    tag.write_to_path(&req.output, Id3Version::Id3v24)
        .map_err(|err| MediaError::Native(format!("id3 write failed: {err}")))?;
    Ok(())
}

fn guess_img_fmt(path: &Path, bytes: &[u8]) -> ImgFmt {
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
