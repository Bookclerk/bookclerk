//! Rate-limited HTTP download helper.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use audible_rs::api::client::Client;
use audible_rs::downloader::{self, DownloadOutcome};
use reqwest::header::CONTENT_TYPE;
use tokio::io::AsyncWriteExt;

use crate::error::{AudibleError, Result};

/// Download a URL to `dest`, optionally throttling to `limit_kbps` (0 = unlimited).
///
/// When throttling is active this path mirrors `audible_rs::downloader::download_to_file`
/// for content-type checks, extension overrides, and optional `version_tag` markers.
#[allow(clippy::too_many_arguments)]
pub async fn download_to_file_limited(
    client: &Client,
    url: &str,
    dest: &Path,
    expected_size: Option<u64>,
    force: bool,
    limit_kbps: u32,
    expected_content_type: &[&str],
    ext_overrides: &[(&str, &str)],
    version_tag: Option<&str>,
) -> Result<(DownloadOutcome, std::path::PathBuf)> {
    if limit_kbps == 0 {
        return downloader::download_to_file(
            client,
            url,
            dest,
            expected_size,
            force,
            None,
            expected_content_type,
            ext_overrides,
            version_tag,
        )
        .await
        .map_err(|e| AudibleError::Download(e.to_string()));
    }

    // Same already-complete probe as audible-rs: planned path plus every
    // extension-corrected candidate.
    if !force {
        if let Some(size) = expected_size {
            let candidates = std::iter::once(dest.to_path_buf()).chain(
                ext_overrides
                    .iter()
                    .map(|(_, ext)| dest.with_extension(ext)),
            );
            for candidate in candidates {
                if let Ok(meta) = tokio::fs::metadata(&candidate).await {
                    if meta.len() == size {
                        return Ok((DownloadOutcome::AlreadyComplete, candidate));
                    }
                }
            }
        }
    }

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let part = part_path(dest);
    let marker = version_marker_path(&part);
    if force {
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&marker).await;
    }

    let request = client
        .authed_get(url)
        .await
        .map_err(|e| AudibleError::Download(e.to_string()))?;
    let response = request
        .send()
        .await
        .map_err(|e| AudibleError::Download(e.to_string()))?;
    let response = response
        .error_for_status()
        .map_err(|e| AudibleError::Download(e.to_string()))?;

    let got_content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    if !expected_content_type.is_empty() {
        if let Some(got) = &got_content_type {
            if !content_type_matches(got, expected_content_type) {
                return Err(content_type_error(response, got, expected_content_type).await);
            }
        }
    }

    let final_dest = got_content_type
        .as_deref()
        .and_then(|ct| extension_override(ct, ext_overrides))
        .map(|ext| dest.with_extension(ext))
        .unwrap_or_else(|| dest.to_path_buf());

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&part)
        .await?;

    if let Some(tag) = version_tag {
        let _ = tokio::fs::write(&marker, tag).await;
    }

    let limit_bps = u64::from(limit_kbps) * 1024;
    let mut downloaded = 0u64;
    let mut window_bytes = 0u64;
    let mut window_start = Instant::now();
    let mut stream = response.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AudibleError::Download(e.to_string()))?;
        file.write_all(&chunk).await?;
        let n = chunk.len() as u64;
        downloaded += n;
        window_bytes += n;
        let elapsed = window_start.elapsed();
        let expected = Duration::from_secs_f64(window_bytes as f64 / limit_bps as f64);
        if expected > elapsed {
            tokio::time::sleep(expected - elapsed).await;
        }
        if window_bytes >= limit_bps {
            window_start = Instant::now();
            window_bytes = 0;
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    if let Some(size) = expected_size {
        if downloaded != size {
            if downloaded > size {
                let _ = tokio::fs::remove_file(&part).await;
                let _ = tokio::fs::remove_file(&marker).await;
            }
            return Err(AudibleError::Download(format!(
                "size mismatch: wrote {downloaded} bytes, expected {size}"
            )));
        }
    }

    tokio::fs::rename(&part, &final_dest).await?;
    let _ = tokio::fs::remove_file(&marker).await;
    Ok((DownloadOutcome::Downloaded, final_dest))
}

/// Sibling `.part` path used while a download is still incomplete.
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// Sibling `.ver` marker that records the remote version for resume checks.
fn version_marker_path(part: &Path) -> PathBuf {
    let mut name = part.file_name().unwrap_or_default().to_os_string();
    name.push(".ver");
    part.with_file_name(name)
}

/// Maps a Content-Type (ignoring parameters) onto a caller-supplied extension.
fn extension_override<'a>(content_type: &str, overrides: &[(&str, &'a str)]) -> Option<&'a str> {
    let got = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    overrides
        .iter()
        .find(|(ct, _)| got == ct.to_ascii_lowercase())
        .map(|&(_, ext)| ext)
}

/// True when the response type (sans parameters) is in `expected`, or `expected` is empty.
fn content_type_matches(got: &str, expected: &[&str]) -> bool {
    if expected.is_empty() {
        return true;
    }
    let got = got.split(';').next().unwrap_or(got).trim();
    expected.iter().any(|kind| kind.eq_ignore_ascii_case(got))
}

/// True for `text/*`, JSON, XML, or `+json`/`+xml` types (safe to include in errors).
fn is_text_like(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/xml"
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
}

/// True when the error body says the title is sold as separately downloaded parts.
fn is_multipart_message(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("individual part") || lower.contains("download the parts")
}

/// Builds a download error from an unexpected Content-Type, quoting a short text body.
async fn content_type_error(
    response: reqwest::Response,
    got: &str,
    expected: &[&str],
) -> AudibleError {
    let detail = if is_text_like(got) {
        let body: String = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        if is_multipart_message(&body) {
            return AudibleError::Download(format!("multipart title: {}", body.trim()));
        }
        format!(": {}", body.trim())
    } else {
        String::new()
    };
    AudibleError::Download(format!(
        "unexpected Content-Type: got {got}, expected {}{detail}",
        expected.join(", ")
    ))
}
