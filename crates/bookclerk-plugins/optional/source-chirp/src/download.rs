//! Download Chirp title materials (plain MP3 tracks) into a cache dir.

use std::path::Path;

use bookclerk_source::{PlainAudioPart, PlainFetch};

use crate::client::ChirpClient;
use crate::error::{ChirpError, Result};

/// Fetch one audiobook id into `cache_dir` via AndroidSingleAudiobook track URLs.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn fetch_title_materials(
    client: &ChirpClient,
    audiobook_id: &str,
    cache_dir: &Path,
) -> Result<PlainFetch> {
    std::fs::create_dir_all(cache_dir)?;
    let title_dir = cache_dir.join(audiobook_id);
    std::fs::create_dir_all(&title_dir)?;

    let book = client.audiobook(audiobook_id).await?;
    if book.tracks.is_empty() {
        return Err(ChirpError::download(format!(
            "audiobook {audiobook_id} has no tracks"
        )));
    }

    let mut parts = Vec::new();
    let mut chapters = Vec::new();
    for (idx, track) in book.tracks.iter().enumerate() {
        let url = track.media_url.as_deref().ok_or_else(|| {
            ChirpError::download(format!(
                "track {} missing mediaUrl (audiobook {audiobook_id})",
                track.id
            ))
        })?;
        let bytes = client.download_bytes(url).await?;
        let ext = audio_extension(url, &bytes);
        let name = format!("{:04}{ext}", idx + 1);
        let path = title_dir.join(&name);
        std::fs::write(&path, &bytes)?;
        let title = track
            .display_name
            .clone()
            .or_else(|| track.chapter_number.map(|n| format!("Chapter {n}")));
        parts.push(PlainAudioPart {
            path,
            title: title.clone(),
            duration_ms: track.duration_ms,
        });
        let start_ms = track.offset_from_book_start_ms.unwrap_or(0);
        chapters.push((
            title.unwrap_or_else(|| format!("Track {}", idx + 1)),
            start_ms,
        ));
    }

    let cover_path = if let Some(cover_url) = book.cover_url.as_deref() {
        match client.download_bytes(cover_url).await {
            Ok(bytes) => {
                let path = title_dir.join("cover.jpg");
                std::fs::write(&path, &bytes)?;
                Some(path)
            }
            Err(err) => {
                tracing::debug!(error = %err, "cover download skipped");
                None
            }
        }
    } else {
        None
    };

    Ok(PlainFetch {
        parts,
        m4b_path: None,
        cover_path,
        chapters,
        pdf_url: None,
    })
}

/// Sniffs a Chirp part URL and bytes for an audio extension, defaulting to `.bin`.
fn audio_extension(url: &str, bytes: &[u8]) -> &'static str {
    bookclerk_source::audio_extension(url, Some(bytes), None, ".bin")
}
