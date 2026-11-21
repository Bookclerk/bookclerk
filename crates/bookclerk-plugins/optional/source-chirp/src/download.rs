//! Download Chirp title materials (plain MP3 tracks) into a cache dir.

use std::path::Path;

use bookclerk_storefront::{PlainAudioPart, PlainFetch};

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
        let download = title_dir.join(format!("{:04}.download", idx + 1));
        client.download_to_file(url, &download).await?;
        let ext = audio_extension(url, &read_head(&download)?);
        let path = title_dir.join(format!("{:04}{ext}", idx + 1));
        std::fs::rename(&download, &path)?;
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
        let path = title_dir.join("cover.jpg");
        match client.download_to_file(cover_url, &path).await {
            Ok(_) => Some(path),
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
        chapter_kind: bookclerk_storefront::ChapterKind::Tracks,
        pdf_url: None,
    })
}

/// The first bytes of a downloaded file, enough to sniff its type.
fn read_head(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut head = Vec::with_capacity(8192);
    std::fs::File::open(path)?
        .take(8192)
        .read_to_end(&mut head)?;
    Ok(head)
}

/// Sniffs a Chirp part URL and bytes for an audio extension, defaulting to `.bin`.
fn audio_extension(url: &str, bytes: &[u8]) -> &'static str {
    bookclerk_storefront::audio_extension(url, Some(bytes), None, ".bin")
}
