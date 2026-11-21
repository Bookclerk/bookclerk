//! Shared HTTP download helpers for GraphicAudio clients.

use std::path::Path;

use bookclerk_plugin_sdk::download::Download;
use bookclerk_plugin_sdk::http::RequestBuilder;

use crate::error::{GraphicAudioError, Result};

/// Streams a media URL to `path`, resuming a dropped transfer.
///
/// `request` builds the `GET` (URL, cookies, user agent) for each attempt;
/// the SDK's resumable download (`bookclerk_plugin_sdk::download`) adds
/// the range headers, so a dropped connection on a large Hi/ZIP file asks
/// only for the missing bytes.
///
/// # Errors
///
/// Returns [`GraphicAudioError::Download`] when the store refuses the file
/// or the transfer keeps failing.
pub async fn download_resumable<F>(path: &Path, request: F) -> Result<()>
where
    F: FnMut() -> RequestBuilder,
{
    Download::new()
        .to_file(path, request)
        .await
        .map_err(|err| GraphicAudioError::download(format!("{err} ({})", path.display())))?;
    Ok(())
}

/// Guess an audio file extension from a URL path (query stripped).
#[must_use]
pub fn extension_from_url(url: &str) -> &'static str {
    bookclerk_storefront::audio_extension(url, None, None, ".m4a")
}
