//! Companion artifacts: PDF, cover art, chapter metadata, catalog JSON, clips.

use std::path::{Path, PathBuf};

use audible_rs::api::client::Client;
use audible_rs::api::locale;
use audible_rs::downloader::{self, Quality};
use bookclerk_config::AudioQuality;

use crate::error::{AudibleError, Result};

/// Fetch curated chapter metadata (`chapter_info` object).
pub async fn fetch_chapter_info(
    client: &Client,
    marketplace: &str,
    asin: &str,
    quality: AudioQuality,
    chapter_layout: &str,
) -> Result<serde_json::Value> {
    let q = match quality {
        AudioQuality::High => Quality::High,
        AudioQuality::Normal => Quality::Normal,
    };
    let layout = match chapter_layout.to_ascii_lowercase().as_str() {
        "flat" => "Flat",
        _ => "Tree",
    };
    downloader::request_chapters(client, marketplace, asin, q.api_value(), layout)
        .await
        .map_err(AudibleError::from)
}

/// Fetch full catalog metadata for a title (classic `metadata.json` sidecar).
pub async fn fetch_product_metadata(
    client: &Client,
    marketplace: &str,
    asin: &str,
    quality: AudioQuality,
) -> Result<serde_json::Value> {
    let q = match quality {
        AudioQuality::High => Quality::High,
        AudioQuality::Normal => Quality::Normal,
    };
    let layout = "Tree";
    downloader::request_content_metadata(client, marketplace, asin, "Adrm", q.api_value(), layout)
        .await
        .map_err(AudibleError::from)
}

/// Fetch clips/bookmarks/notes sidecar (classic `DownloadClipsBookmarks`).
pub async fn fetch_clips_bookmarks(
    client: &Client,
    asin: &str,
    acr: Option<&str>,
    version: Option<&str>,
    content_format: Option<&str>,
) -> Result<Option<serde_json::Value>> {
    downloader::request_annotations(client, asin, acr, version, content_format)
        .await
        .map_err(AudibleError::from)
}

/// Download companion PDF via the authenticated companion-file endpoint.
pub async fn download_companion_pdf(
    client: &Client,
    marketplace: &str,
    asin: &str,
    dest: &Path,
) -> Result<Option<PathBuf>> {
    let loc = locale::require(marketplace).map_err(|err| AudibleError::Auth(err.to_string()))?;
    let url = format!("https://www.audible.{}/companion-file/{asin}", loc.domain);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match downloader::download_to_file(
        client,
        &url,
        dest,
        None,
        true,
        None,
        &["application/octet-stream", "application/pdf"],
        &[],
        None,
    )
    .await
    {
        Ok((_outcome, path)) => Ok(Some(path)),
        Err(err) => {
            let msg = err.to_string();
            // Titles without a companion PDF return HTML — treat as absent.
            if msg.contains("unexpected content type") || msg.contains("text/html") {
                Ok(None)
            } else {
                Err(AudibleError::Download(msg))
            }
        }
    }
}

/// Download a JPEG cover at `size` (e.g. `500`, `1215`, or `native`).
pub async fn download_cover_jpeg(
    client: &Client,
    marketplace: &str,
    asin: &str,
    size: &str,
    dest: &Path,
) -> Result<Option<PathBuf>> {
    let images = downloader::request_cover_images(client, marketplace, asin, "500,1215")
        .await
        .map_err(AudibleError::from)?;
    let Some(images) = images else {
        return Ok(None);
    };
    let Some(url) = pick_cover_url(&images, size) else {
        return Ok(None);
    };
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let (_outcome, path) = downloader::download_to_file(
        client,
        &url,
        dest,
        None,
        true,
        None,
        &["image/jpeg"],
        &[],
        None,
    )
    .await
    .map_err(|err| AudibleError::Download(err.to_string()))?;
    Ok(Some(path))
}

fn pick_cover_url(
    images: &serde_json::Map<String, serde_json::Value>,
    size: &str,
) -> Option<String> {
    if let Some(url) = images.get(size).and_then(|v| v.as_str()) {
        return Some(url.to_string());
    }
    if size.eq_ignore_ascii_case("native") {
        return images
            .iter()
            .filter_map(|(k, v)| {
                let px = k.parse::<u32>().ok()?;
                Some((px, v.as_str()?))
            })
            .max_by_key(|(px, _)| *px)
            .map(|(_, url)| url.to_string());
    }
    // Derive from anchor sizes (simplified audible-rs logic).
    if let Some(url) = images
        .get("500")
        .or_else(|| images.get("1215"))
        .and_then(|v| v.as_str())
    {
        return rewrite_cover_size(url, size);
    }
    images
        .values()
        .filter_map(|v| v.as_str())
        .next()
        .map(str::to_string)
}

fn rewrite_cover_size(url: &str, size: &str) -> Option<String> {
    let (stem, extension) = url.rsplit_once('.')?;
    let base = match stem.rsplit_once('.') {
        Some((base, block)) if block.starts_with('_') && block.ends_with('_') => base,
        _ => stem,
    };
    Some(format!("{base}._SL{size}_.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_exact_cover_size() {
        let mut map = serde_json::Map::new();
        map.insert("500".into(), serde_json::json!("https://x/500.jpg"));
        map.insert("1215".into(), serde_json::json!("https://x/1215.jpg"));
        assert_eq!(
            pick_cover_url(&map, "500").as_deref(),
            Some("https://x/500.jpg")
        );
    }

    #[test]
    fn rewrites_cover_size_marker() {
        let url = "https://m.media-amazon.com/images/I/abc._SL500_.jpg";
        assert_eq!(
            rewrite_cover_size(url, "1215").as_deref(),
            Some("https://m.media-amazon.com/images/I/abc._SL1215_.jpg")
        );
    }
}
