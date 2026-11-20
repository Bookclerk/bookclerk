//! Rate-limited HTTP download helper.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use audible_rs::api::client::Client;
use audible_rs::downloader::{self, DownloadOutcome};
use bookclerk_plugin_sdk::http::header::{CONTENT_RANGE, CONTENT_TYPE, RANGE};
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

    let job = LimitedJob {
        client,
        url,
        expected_size,
        limit_kbps,
        expected_content_type,
        ext_overrides,
        version_tag,
        dest,
        part: &part,
        marker: &marker,
    };
    let mut wait = Duration::from_secs(1);
    let mut attempt = 1;
    loop {
        match job.attempt().await {
            Ok(done) => return Ok(done),
            Err(Interrupted::Retry(reason)) if attempt < crate::retry::TRANSFER_ATTEMPTS => {
                tracing::warn!(attempt, %reason, "Audible transfer interrupted; resuming");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(30));
                attempt += 1;
            }
            Err(Interrupted::Retry(reason)) => return Err(AudibleError::Download(reason)),
            Err(Interrupted::Fatal(err)) => return Err(err),
        }
    }
}

/// Longest wait for the next body chunk before a throttled transfer
/// counts as stalled (the client's own timeout covers the answer's head
/// only).
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// How a throttled attempt ended without the file.
enum Interrupted {
    /// The connection failed, stalled, ended short, or the answer did not
    /// fit the partial: another attempt resumes (or restarts) it.
    Retry(String),
    /// A failure another attempt would not fix.
    Fatal(AudibleError),
}

impl From<std::io::Error> for Interrupted {
    fn from(err: std::io::Error) -> Self {
        Self::Fatal(err.into())
    }
}

/// One throttled download, with what each attempt needs.
struct LimitedJob<'a> {
    /// Authenticated Audible client.
    client: &'a Client,
    /// Content URL.
    url: &'a str,
    /// The file's size from the license, when known.
    expected_size: Option<u64>,
    /// Speed limit in KiB/s (non-zero).
    limit_kbps: u32,
    /// Accepted response Content-Types (empty: any).
    expected_content_type: &'a [&'a str],
    /// Content-Type → extension corrections for the final path.
    ext_overrides: &'a [(&'a str, &'a str)],
    /// License content version; a partial resumes only under the same one.
    version_tag: Option<&'a str>,
    /// Planned final path.
    dest: &'a Path,
    /// `<dest>.part`.
    part: &'a Path,
    /// `<dest>.part.ver`.
    marker: &'a Path,
}

impl LimitedJob<'_> {
    /// Bytes of the partial that may be resumed: only under a matching
    /// version marker, and never more than the expected size.
    async fn resumable_offset(&self) -> u64 {
        let Some(tag) = self.version_tag else {
            return 0;
        };
        let Ok(stamped) = tokio::fs::read_to_string(self.marker).await else {
            return 0;
        };
        if stamped != tag {
            return 0;
        }
        let held = tokio::fs::metadata(self.part)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        match self.expected_size {
            Some(size) if held > size => 0,
            _ => held,
        }
    }

    /// One request, resuming the partial with `Range` when it can.
    async fn attempt(&self) -> std::result::Result<(DownloadOutcome, PathBuf), Interrupted> {
        use bookclerk_plugin_sdk::resume::{next_step, Partial, ResponseHead, Step};

        let offset = self.resumable_offset().await;
        let mut request = self
            .client
            .authed_get(self.url)
            .await
            .map_err(|e| Interrupted::Fatal(AudibleError::Download(e.to_string())))?;
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let response = request
            .send()
            .await
            .map_err(|e| Interrupted::Retry(e.to_string()))?;
        let status = response.status();
        let step = next_step(
            Partial {
                offset,
                total: self.expected_size,
            },
            ResponseHead {
                status: status.as_u16(),
                content_range: response.headers().get(CONTENT_RANGE).map(|v| v.as_bytes()),
                content_length: response.content_length(),
            },
        );
        let (append, total, end) = match step {
            Step::Append { total, end } => (true, total, end),
            Step::Replace { total, end } => (false, total, end),
            Step::Complete if self.ext_overrides.is_empty() => {
                return self.finish(self.dest.to_path_buf()).await;
            }
            Step::Complete => {
                // The final extension comes from the Content-Type, which a
                // 416 does not carry: fetch the file again to learn it.
                let _ = tokio::fs::remove_file(self.part).await;
                let _ = tokio::fs::remove_file(self.marker).await;
                return Err(Interrupted::Retry(
                    "the partial is complete but its type is unknown; restarting".into(),
                ));
            }
            Step::StartOver => {
                let _ = tokio::fs::remove_file(self.part).await;
                let _ = tokio::fs::remove_file(self.marker).await;
                return Err(Interrupted::Retry(format!(
                    "the answer ({status}) does not continue the partial at byte {offset}"
                )));
            }
            Step::Retry => return Err(Interrupted::Retry(format!("HTTP {status}"))),
            Step::Fail => {
                return Err(Interrupted::Fatal(AudibleError::Download(format!(
                    "download failed with HTTP {status}"
                ))))
            }
        };

        let got_content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if !self.expected_content_type.is_empty() {
            if let Some(got) = &got_content_type {
                if !content_type_matches(got, self.expected_content_type) {
                    return Err(Interrupted::Fatal(
                        content_type_error(response, got, self.expected_content_type).await,
                    ));
                }
            }
        }
        let final_dest = got_content_type
            .as_deref()
            .and_then(|ct| extension_override(ct, self.ext_overrides))
            .map(|ext| self.dest.with_extension(ext))
            .unwrap_or_else(|| self.dest.to_path_buf());

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(self.part)
            .await?;
        if let Some(tag) = self.version_tag {
            let _ = tokio::fs::write(self.marker, tag).await;
        }

        let limit_bps = u64::from(self.limit_kbps) * 1024;
        let mut written = if append { offset } else { 0 };
        let mut window_bytes = 0u64;
        let mut window_start = Instant::now();
        let mut stream = response.bytes_stream();
        use futures::StreamExt;
        loop {
            let chunk = match tokio::time::timeout(READ_TIMEOUT, stream.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(None) => break,
                Ok(Some(Err(err))) => {
                    file.flush().await?;
                    return Err(Interrupted::Retry(err.to_string()));
                }
                Err(_) => {
                    file.flush().await?;
                    return Err(Interrupted::Retry(format!(
                        "the transfer stalled at {written} bytes"
                    )));
                }
            };
            let n = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            if end.is_some_and(|end| written.saturating_add(n) > end) {
                // The body runs past the range it stated.
                drop(file);
                let _ = tokio::fs::remove_file(self.part).await;
                let _ = tokio::fs::remove_file(self.marker).await;
                return Err(Interrupted::Retry(
                    "the answer ran past the range it stated".into(),
                ));
            }
            file.write_all(&chunk).await?;
            written = written.saturating_add(n);
            window_bytes = window_bytes.saturating_add(n);
            let elapsed = window_start.elapsed();
            let nanos = u128::from(window_bytes) * 1_000_000_000 / u128::from(limit_bps.max(1));
            let expected = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
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

        if let Some(size) = self.expected_size.or(total) {
            if written > size {
                let _ = tokio::fs::remove_file(self.part).await;
                let _ = tokio::fs::remove_file(self.marker).await;
                return Err(Interrupted::Fatal(AudibleError::Download(format!(
                    "size mismatch: wrote {written} bytes, expected {size}"
                ))));
            }
            if written < size {
                // Ended early without an error: keep the partial and resume.
                return Err(Interrupted::Retry(format!(
                    "transfer ended at {written} of {size} bytes"
                )));
            }
        } else if end.is_some() {
            // A range of a file whose length nobody stated: the next
            // attempt asks from here and a 416 there finishes it.
            return Err(Interrupted::Retry(format!(
                "range done at {written} bytes; the file's length is unknown"
            )));
        }
        self.finish(final_dest).await
    }

    /// Moves the finished partial onto `final_dest`.
    async fn finish(
        &self,
        final_dest: PathBuf,
    ) -> std::result::Result<(DownloadOutcome, PathBuf), Interrupted> {
        tokio::fs::rename(self.part, &final_dest).await?;
        let _ = tokio::fs::remove_file(self.marker).await;
        Ok((DownloadOutcome::Downloaded, final_dest))
    }
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
    response: bookclerk_plugin_sdk::http::Response,
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
