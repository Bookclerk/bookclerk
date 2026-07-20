//! Native audiobook metadata fix-up (M4B via mp4ameta, MP3 via id3).

use std::path::{Path, PathBuf};
use std::time::Duration;

use id3::frame::{Picture, PictureType};
use id3::{Tag as Id3Tag, TagLike, Version as Id3Version};
use mp4ameta::{Chapter, Img, ImgFmt, MediaType, Tag as Mp4Tag};

use crate::error::{DecryptError, Result};
use crate::DecryptOutcome;

/// Request to fix up audiobook metadata after decrypt.
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
}

/// Apply metadata tags, optional cover embed, and optional chapters.
pub async fn fixup_audiobook(req: FixupRequest) -> Result<DecryptOutcome> {
    if !req.input.exists() {
        return Err(DecryptError::InputMissing(req.input));
    }
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let output = req.output.clone();
    tokio::task::spawn_blocking(move || fixup_audiobook_sync(&req))
        .await
        .map_err(|err| DecryptError::Native(format!("fixup task join error: {err}")))??;

    if !output.exists() {
        return Err(DecryptError::OutputMissing(output));
    }
    Ok(DecryptOutcome { output })
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

fn fixup_m4b(req: &FixupRequest) -> Result<()> {
    let mut tag = Mp4Tag::read_from_path(&req.output)
        .map_err(|err| DecryptError::Native(format!("mp4ameta read failed: {err}")))?;

    tag.set_title(&req.title);
    tag.set_album(&req.title);
    tag.set_media_type(MediaType::AudioBook);
    if let Some(author) = req.author.as_deref().filter(|s| !s.is_empty()) {
        tag.set_artist(author);
        tag.set_album_artist(author);
    }
    if let Some(narrator) = req.narrator.as_deref().filter(|s| !s.is_empty()) {
        tag.set_composer(narrator);
    }

    if let Some(cover) = &req.cover {
        if cover.exists() {
            let bytes = std::fs::read(cover)?;
            tag.set_artwork(Img::new(guess_img_fmt(cover, &bytes), bytes));
        }
    }

    if !req.chapters.is_empty() {
        tag.chapter_track_mut().clear();
        tag.chapter_list_mut().clear();
        for (title, start_ms) in &req.chapters {
            tag.chapter_list_mut().push(Chapter::new(
                Duration::from_millis(*start_ms),
                title.clone(),
            ));
        }
    }

    tag.write_to_path(&req.output)
        .map_err(|err| DecryptError::Native(format!("mp4ameta write failed: {err}")))?;
    Ok(())
}

fn fixup_mp3(req: &FixupRequest) -> Result<()> {
    let mut tag = Id3Tag::read_from_path(&req.output).unwrap_or_else(|_| Id3Tag::new());
    tag.set_title(&req.title);
    tag.set_album(&req.title);
    if let Some(author) = req.author.as_deref().filter(|s| !s.is_empty()) {
        tag.set_artist(author);
        tag.set_album_artist(author);
    }
    if let Some(narrator) = req.narrator.as_deref().filter(|s| !s.is_empty()) {
        tag.set_text("TCOM", narrator);
    }
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
        .map_err(|err| DecryptError::Native(format!("id3 write failed: {err}")))?;
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
