//! Shared HTTP download helpers for GraphicAudio clients.

use std::path::Path;

use reqwest::Response;
use tokio::io::AsyncWriteExt;

use crate::error::{GraphicAudioError, Result};

/// Stream a successful HTTP response body to `path`.
pub async fn response_to_path(mut resp: Response, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(GraphicAudioError::download(format!(
            "download failed ({status}) for {}",
            path.display()
        )));
    }
    let mut file = tokio::fs::File::create(path).await?;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

/// Guess an audio file extension from a URL path (query stripped).
#[must_use]
pub fn extension_from_url(url: &str) -> &'static str {
    bookclerk_source::audio_extension(url, None, None, ".m4a")
}
