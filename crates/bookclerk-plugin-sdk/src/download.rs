//! Resumable downloads to a file for native guests.
//!
//! Audience: storefront and other native guests that download large files
//! (an audiobook can be several gigabytes). [`Download::to_file`] streams
//! the body to `<dest>.part` and, when the connection drops, stalls or
//! the server answers a transient error, asks again for only the bytes it
//! is missing (`Range: bytes=<n>-`, with `If-Range` when the server named
//! a validator). The decisions about each answer are the sans-IO
//! functions in [`crate::resume`]; this module only does the IO.
//!
//! The partial survives a failed call: a later call for the same `dest`
//! resumes it when the server gave a validator (kept beside it in
//! `<dest>.part.meta`), and starts over when it did not. On success the
//! partial is renamed onto `dest` and the metadata file is removed.
//!
//! ```no_run
//! # async fn demo(http: &bookclerk_plugin_sdk::http::Client) -> Result<(), Box<dyn std::error::Error>> {
//! use bookclerk_plugin_sdk::download::Download;
//! let done = Download::new()
//!     .max_bytes(8 * 1024 * 1024 * 1024)
//!     .to_file(std::path::Path::new("/tmp/book.m4b"), || {
//!         http.get("https://cdn.example.com/book.m4b")
//!     })
//!     .await?;
//! println!("{} bytes, resumed {} times", done.bytes, done.resumes);
//! # Ok(()) }
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};

use crate::http::{header, RequestBuilder, Response, StatusCode};
use crate::resume::{next_step, Partial, ResponseHead, Step, Validator};

/// Default longest wait for the next body chunk (or the response head)
/// before the connection is treated as stalled and the range is asked
/// for again.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Default number of attempts in a row that may fail without writing a
/// byte before the download gives up.
pub const DEFAULT_RETRIES: u32 = 5;

/// Default cap on attempts in one call, progress or not, so a server that
/// drops every connection after a few bytes cannot keep a call alive
/// forever.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 64;

/// Default first wait between attempts; it doubles after each failed
/// attempt without progress, up to [`MAX_BACKOFF`].
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(1);

/// Longest wait between attempts, including a server's `Retry-After`.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Buffer between the network and the partial file, in bytes.
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

/// Why [`Download::to_file`] failed.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// The server answered a status that is not a download (`403`,
    /// `404`, …), or kept answering a transient one past the retries.
    #[error("download refused with HTTP {status}")]
    Status {
        /// The status the server answered last.
        status: StatusCode,
    },
    /// The connection kept failing or stalling past the retries; the
    /// partial is kept, so a later call resumes it when it can.
    #[error("download interrupted after {written} bytes: {reason}")]
    Interrupted {
        /// Bytes the partial holds.
        written: u64,
        /// The last transport failure, without the URL.
        reason: String,
    },
    /// The file is larger than [`Download::max_bytes`] allows.
    #[error("download is larger than the {limit}-byte limit")]
    TooLarge {
        /// The limit, in bytes.
        limit: u64,
    },
    /// The server's length disagrees with [`Download::expected_size`], or
    /// the body ended at another length than the one the server stated.
    #[error("download is {actual} bytes, expected {expected}")]
    SizeMismatch {
        /// The length the caller or the server stated.
        expected: u64,
        /// The length received or stated instead.
        actual: u64,
    },
    /// Reading or writing the partial file failed.
    #[error("download file: {0}")]
    Io(#[from] std::io::Error),
}

impl DownloadError {
    /// True when calling again later may succeed: the connection failed,
    /// or the server answered `408`, `429` or `5xx`.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Interrupted { .. } => true,
            Self::Status { status } => {
                matches!(status.as_u16(), 408 | 429) || status.is_server_error()
            }
            Self::TooLarge { .. } | Self::SizeMismatch { .. } | Self::Io(_) => false,
        }
    }
}

/// A finished download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    /// The file written (the `dest` passed in).
    pub path: PathBuf,
    /// Its length in bytes.
    pub bytes: u64,
    /// How many times a dropped or refused transfer was picked up again
    /// from where it stopped (0 when the first attempt finished).
    pub resumes: u32,
}

/// Settings for one resumable download; see the [module docs](self).
#[derive(Debug, Clone)]
pub struct Download {
    /// The file's exact length, when the caller knows it.
    expected_size: Option<u64>,
    /// The largest file accepted, in bytes.
    max_bytes: Option<u64>,
    /// Longest wait for the response head or the next body chunk.
    idle_timeout: Duration,
    /// Attempts in a row that may fail without progress.
    retries: u32,
    /// Attempts allowed in one call.
    max_attempts: u32,
    /// First wait between failed attempts.
    backoff: Duration,
}

impl Default for Download {
    fn default() -> Self {
        Self::new()
    }
}

/// What `<dest>.part.meta` records about the partial beside it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PartMeta {
    /// `If-Range` value naming the version the partial's bytes belong to.
    validator: String,
    /// The file's length, when the server stated it.
    total: Option<u64>,
}

impl Download {
    /// Settings with the defaults: no size check or limit,
    /// [`DEFAULT_IDLE_TIMEOUT`], [`DEFAULT_RETRIES`],
    /// [`DEFAULT_MAX_ATTEMPTS`] and [`DEFAULT_BACKOFF`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            expected_size: None,
            max_bytes: None,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            retries: DEFAULT_RETRIES,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff: DEFAULT_BACKOFF,
        }
    }

    /// Requires the file to be exactly `bytes` long (a store's stated
    /// size). A server stating another length fails the download.
    #[must_use]
    pub fn expected_size(mut self, bytes: u64) -> Self {
        self.expected_size = Some(bytes);
        self
    }

    /// Refuses a file longer than `bytes`, before reading it when the
    /// server states its length and as soon as it passes the limit
    /// otherwise.
    #[must_use]
    pub fn max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = Some(bytes);
        self
    }

    /// Sets the longest wait for the response head or the next body
    /// chunk before the attempt counts as stalled.
    #[must_use]
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Sets how many attempts in a row may fail without writing a byte.
    #[must_use]
    pub fn retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    /// Sets how many attempts one call may make in all.
    #[must_use]
    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }

    /// Sets the first wait between failed attempts (doubling, capped at
    /// [`MAX_BACKOFF`]).
    #[must_use]
    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Downloads to `dest`, resuming across dropped connections.
    ///
    /// `request` builds the `GET` for the file (URL, auth, cookies,
    /// user agent); it is called once per attempt, and the download adds
    /// `Range` and `If-Range` itself. Signed URLs that expire belong in
    /// the caller's retry, not here: a `403` fails the call and keeps the
    /// partial for the next one.
    ///
    /// # Arguments
    ///
    /// * `dest` - The final path. `<dest>.part` and `<dest>.part.meta`
    ///   are written beside it, so the directory must be writable and the
    ///   path used by this download only.
    /// * `request` - Builds a fresh request for each attempt.
    ///
    /// # Returns
    ///
    /// The finished file's path, length and resume count.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadError`]: `Status` for a refusal, `Interrupted`
    /// after the retries, `TooLarge` / `SizeMismatch` when the length is
    /// wrong (the partial is removed), `Io` when the file cannot be
    /// written.
    pub async fn to_file<F>(&self, dest: &Path, mut request: F) -> Result<Downloaded, DownloadError>
    where
        F: FnMut() -> RequestBuilder,
    {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let part = sibling(dest, ".part");
        let meta_path = sibling(dest, ".part.meta");
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&part)
            .await?;
        let mut state = State {
            partial: Partial {
                offset: 0,
                total: None,
            },
            validator: None,
            status: None,
        };
        let held = file.metadata().await?.len();
        match read_meta(&meta_path).await {
            Some(meta)
                if held > 0
                    && meta.total.is_none_or(|total| held <= total)
                    && self.length_allowed(meta.total).is_ok() =>
            {
                state.partial = Partial {
                    offset: held,
                    total: meta.total,
                };
                state.validator = Some(meta.validator);
            }
            _ => {
                // Without a validator nothing proves the bytes belong to
                // the file the server holds now.
                file.set_len(0).await?;
                let _ = tokio::fs::remove_file(&meta_path).await;
            }
        }
        let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
        let mut resumes = 0u32;
        let mut failures = 0u32;
        let mut wait = self.backoff;
        let mut last_reason = String::new();
        for attempt in 0..self.max_attempts {
            if attempt > 0 && failures > 0 {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(MAX_BACKOFF);
            }
            let before = state.partial.offset;
            if before > 0 {
                resumes += 1;
            }
            state.status = None;
            let outcome = self
                .attempt(&mut request, &mut writer, &mut state, &meta_path)
                .await;
            let progressed = state.partial.offset > before;
            match outcome {
                Ok(Attempt::Done) => {
                    writer.flush().await?;
                    let file = writer.into_inner();
                    file.sync_all().await?;
                    drop(file);
                    let bytes = state.partial.offset;
                    if let Some(expected) = self.expected_size {
                        if bytes != expected {
                            return Err(self
                                .discard(
                                    &part,
                                    &meta_path,
                                    DownloadError::SizeMismatch {
                                        expected,
                                        actual: bytes,
                                    },
                                )
                                .await);
                        }
                    }
                    tokio::fs::rename(&part, dest).await?;
                    let _ = tokio::fs::remove_file(&meta_path).await;
                    return Ok(Downloaded {
                        path: dest.to_path_buf(),
                        bytes,
                        resumes,
                    });
                }
                Ok(Attempt::Again { delay }) => {
                    if progressed {
                        failures = 0;
                        wait = self.backoff;
                    } else {
                        failures += 1;
                        if let Some(delay) = delay {
                            wait = delay.min(MAX_BACKOFF);
                        }
                    }
                    if failures > self.retries {
                        break;
                    }
                }
                Ok(Attempt::Transport(reason)) => {
                    last_reason = reason;
                    if progressed {
                        failures = 0;
                        wait = self.backoff;
                    } else {
                        failures += 1;
                    }
                    if failures > self.retries {
                        break;
                    }
                }
                Err(Fatal::Status(status)) => {
                    writer.flush().await?;
                    return Err(DownloadError::Status { status });
                }
                Err(Fatal::Length(err)) => {
                    drop(writer);
                    return Err(self.discard(&part, &meta_path, err).await);
                }
                Err(Fatal::Io(err)) => return Err(DownloadError::Io(err)),
            }
        }
        writer.flush().await?;
        if let Some(status) = state.last_status() {
            return Err(DownloadError::Status { status });
        }
        Err(DownloadError::Interrupted {
            written: state.partial.offset,
            reason: if last_reason.is_empty() {
                "no complete answer".into()
            } else {
                last_reason
            },
        })
    }

    /// Checks a stated file length against the expected size and limit.
    fn length_allowed(&self, total: Option<u64>) -> Result<(), DownloadError> {
        let Some(total) = total else {
            return Ok(());
        };
        if let Some(expected) = self.expected_size {
            if total != expected {
                return Err(DownloadError::SizeMismatch {
                    expected,
                    actual: total,
                });
            }
        }
        if let Some(limit) = self.max_bytes {
            if total > limit {
                return Err(DownloadError::TooLarge { limit });
            }
        }
        Ok(())
    }

    /// Removes the partial and its metadata, then returns `err`.
    async fn discard(&self, part: &Path, meta: &Path, err: DownloadError) -> DownloadError {
        let _ = tokio::fs::remove_file(part).await;
        let _ = tokio::fs::remove_file(meta).await;
        err
    }

    /// Runs one request and writes what it can of the answer.
    async fn attempt<F>(
        &self,
        request: &mut F,
        writer: &mut BufWriter<tokio::fs::File>,
        state: &mut State,
        meta_path: &Path,
    ) -> Result<Attempt, Fatal>
    where
        F: FnMut() -> RequestBuilder,
    {
        let mut builder = request();
        let offset = state.partial.offset;
        if offset > 0 {
            builder = builder.header(header::RANGE, format!("bytes={offset}-"));
            if let Some(validator) = &state.validator {
                builder = builder.header(header::IF_RANGE, validator.as_str());
            }
        }
        let response = match tokio::time::timeout(self.idle_timeout, builder.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => return Ok(Attempt::Transport(err.to_string())),
            Err(_) => {
                return Ok(Attempt::Transport(
                    "timed out waiting for the answer".into(),
                ))
            }
        };
        let headers = response.headers();
        let step = next_step(
            state.partial,
            ResponseHead {
                status: response.status().as_u16(),
                content_range: headers
                    .get(header::CONTENT_RANGE)
                    .map(header::HeaderValue::as_bytes),
                content_length: response.content_length(),
            },
        );
        state.status = Some(response.status());
        match step {
            Step::Complete => Ok(Attempt::Done),
            Step::Fail => Err(Fatal::Status(response.status())),
            Step::Retry => Ok(Attempt::Again {
                delay: retry_after(&response),
            }),
            Step::StartOver => {
                state.partial = Partial {
                    offset: 0,
                    total: None,
                };
                state.validator = None;
                writer.flush().await?;
                writer.get_mut().set_len(0).await?;
                writer.get_mut().rewind().await?;
                let _ = tokio::fs::remove_file(meta_path).await;
                Ok(Attempt::Again { delay: None })
            }
            Step::Replace { total } | Step::Append { total } => {
                self.length_allowed(total).map_err(Fatal::Length)?;
                if matches!(step, Step::Replace { .. }) {
                    writer.flush().await?;
                    writer.get_mut().set_len(0).await?;
                    writer.get_mut().rewind().await?;
                    state.partial.offset = 0;
                    let validator = Validator::from_headers(
                        headers.get(header::ETAG).map(header::HeaderValue::as_bytes),
                        headers
                            .get(header::LAST_MODIFIED)
                            .map(header::HeaderValue::as_bytes),
                    );
                    state.validator = validator.map(|v| v.as_str().to_owned());
                    match &state.validator {
                        Some(validator) => {
                            write_meta(
                                meta_path,
                                &PartMeta {
                                    validator: validator.clone(),
                                    total,
                                },
                            )
                            .await?;
                        }
                        None => {
                            let _ = tokio::fs::remove_file(meta_path).await;
                        }
                    }
                } else {
                    writer.seek(std::io::SeekFrom::Start(offset)).await?;
                }
                state.partial.total = total;
                self.stream_body(response, writer, state).await
            }
        }
    }

    /// Appends the response body at the partial's end until it ends,
    /// stalls or fails.
    async fn stream_body(
        &self,
        mut response: Response,
        writer: &mut BufWriter<tokio::fs::File>,
        state: &mut State,
    ) -> Result<Attempt, Fatal> {
        loop {
            let chunk = match tokio::time::timeout(self.idle_timeout, response.chunk()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) => break,
                Ok(Err(err)) => {
                    writer.flush().await?;
                    return Ok(Attempt::Transport(err.to_string()));
                }
                Err(_) => {
                    writer.flush().await?;
                    return Ok(Attempt::Transport("timed out waiting for data".into()));
                }
            };
            let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            let next = state.partial.offset.saturating_add(len);
            if let Some(total) = state.partial.total {
                if next > total {
                    return Err(Fatal::Length(DownloadError::SizeMismatch {
                        expected: total,
                        actual: next,
                    }));
                }
            }
            if let Some(limit) = self.max_bytes {
                if next > limit {
                    return Err(Fatal::Length(DownloadError::TooLarge { limit }));
                }
            }
            writer.write_all(&chunk).await?;
            state.partial.offset = next;
        }
        writer.flush().await?;
        match state.partial.total {
            // A body that ended early on a server stating the length is a
            // drop the client did not see as one: ask for the rest.
            Some(total) if state.partial.offset < total => Ok(Attempt::Transport(format!(
                "the answer ended at {} of {total} bytes",
                state.partial.offset
            ))),
            _ => Ok(Attempt::Done),
        }
    }
}

/// What a download holds between attempts.
#[derive(Debug)]
struct State {
    /// Bytes written and the stated length.
    partial: Partial,
    /// `If-Range` value for the next resume.
    validator: Option<String>,
    /// The last status answered, for a final transient refusal.
    status: Option<StatusCode>,
}

impl State {
    /// The last status, when it was a transient refusal.
    fn last_status(&self) -> Option<StatusCode> {
        self.status
            .filter(|status| matches!(status.as_u16(), 408 | 429) || status.is_server_error())
    }
}

/// How one attempt ended without failing the download.
#[derive(Debug)]
enum Attempt {
    /// The file is complete.
    Done,
    /// The server asked for another attempt (transient status or a range
    /// the partial cannot use), after `delay` when it named one.
    Again {
        /// The server's `Retry-After`, when sent as seconds.
        delay: Option<Duration>,
    },
    /// The connection failed or stalled; the partial keeps what arrived.
    Transport(String),
}

/// How one attempt failed the download.
#[derive(Debug)]
enum Fatal {
    /// A status that is not a download.
    Status(StatusCode),
    /// A length the settings refuse.
    Length(DownloadError),
    /// The partial file could not be written.
    Io(std::io::Error),
}

impl From<std::io::Error> for Fatal {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// `dest` with `suffix` appended to its file name.
fn sibling(dest: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = dest.file_name().map(OsString::from).unwrap_or_default();
    name.push(suffix);
    dest.with_file_name(name)
}

/// A server's `Retry-After` in seconds, when it sent one.
fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Reads `<dest>.part.meta`; a missing or unreadable file is `None`.
async fn read_meta(path: &Path) -> Option<PartMeta> {
    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let meta: PartMeta = serde_json::from_slice(&bytes).ok()?;
    // The validator is sent back as a header, so it must still be one.
    Validator::from_headers(
        Some(meta.validator.as_bytes()),
        Some(meta.validator.as_bytes()),
    )
    .filter(|v| v.as_str() == meta.validator)
    .map(|_| meta)
}

/// Writes `<dest>.part.meta`.
async fn write_meta(path: &Path, meta: &PartMeta) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(meta).map_err(std::io::Error::other)?;
    tokio::fs::write(path, bytes).await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::http::Client;

    /// One scripted answer: the bytes to write, then how long to hold the
    /// connection before closing it.
    struct Answer {
        /// Raw response bytes (head and as much body as the test wants).
        bytes: Vec<u8>,
        /// Pause after writing, to simulate a stalled transfer.
        hold: Duration,
    }

    /// An answer sent in full, then closed.
    fn answer(head: &str, body: &[u8]) -> Answer {
        let mut bytes = head.replace('\n', "\r\n").into_bytes();
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(body);
        Answer {
            bytes,
            hold: Duration::ZERO,
        }
    }

    /// Serves `answers` one connection each and records the `Range` and
    /// `If-Range` lines of every request.
    async fn serve(answers: Vec<Answer>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/book.m4b", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for answer in answers {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let log = log.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let lines: Vec<String> = text
                        .lines()
                        .filter(|line| line.starts_with("range:") || line.starts_with("if-range:"))
                        .map(str::to_owned)
                        .collect();
                    log.lock().expect("log").push(lines.join(" | "));
                    let _ = socket.write_all(&answer.bytes).await;
                    let _ = socket.flush().await;
                    tokio::time::sleep(answer.hold).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (url, seen)
    }

    /// 200 bytes of recognisable content.
    fn body() -> Vec<u8> {
        (0..200u8).collect()
    }

    /// Settings that retry immediately.
    fn quick() -> Download {
        Download::new().backoff(Duration::from_millis(1))
    }

    #[tokio::test]
    async fn a_dropped_transfer_resumes_where_it_stopped() {
        let body = body();
        let (url, seen) = serve(vec![
            answer(
                "HTTP/1.1 200 OK\nContent-Length: 200\nETag: \"v1\"\nConnection: close\n",
                &body[..80],
            ),
            answer(
                "HTTP/1.1 206 Partial Content\nContent-Length: 120\nContent-Range: bytes 80-199/200\nConnection: close\n",
                &body[80..],
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("book.m4b");
        let http = Client::new();
        let done = quick()
            .expected_size(200)
            .to_file(&dest, || http.get(&url))
            .await
            .expect("download");
        assert_eq!(done.bytes, 200);
        assert_eq!(done.resumes, 1);
        assert_eq!(std::fs::read(&dest).expect("read"), body);
        assert!(!sibling(&dest, ".part").exists());
        assert!(!sibling(&dest, ".part.meta").exists());
        let seen = seen.lock().expect("log").clone();
        assert_eq!(
            seen,
            vec![String::new(), "range: bytes=80- | if-range: \"v1\"".into()]
        );
    }

    #[tokio::test]
    async fn a_server_that_ignores_the_range_replaces_the_partial() {
        let body = body();
        let (url, _) = serve(vec![
            answer(
                "HTTP/1.1 200 OK\nContent-Length: 200\nConnection: close\n",
                &body[..50],
            ),
            answer(
                "HTTP/1.1 200 OK\nContent-Length: 200\nConnection: close\n",
                &body,
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("book.m4b");
        let http = Client::new();
        let done = quick()
            .to_file(&dest, || http.get(&url))
            .await
            .expect("download");
        assert_eq!(done.bytes, 200);
        assert_eq!(std::fs::read(&dest).expect("read"), body);
    }

    #[tokio::test]
    async fn a_mismatched_range_starts_over() {
        let body = body();
        let (url, seen) = serve(vec![
            answer(
                "HTTP/1.1 200 OK\nContent-Length: 200\nConnection: close\n",
                &body[..50],
            ),
            answer(
                "HTTP/1.1 206 Partial Content\nContent-Length: 100\nContent-Range: bytes 100-199/200\nConnection: close\n",
                &body[100..],
            ),
            answer("HTTP/1.1 200 OK\nContent-Length: 200\nConnection: close\n", &body),
        ])
        .await;
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("book.m4b");
        let http = Client::new();
        quick()
            .to_file(&dest, || http.get(&url))
            .await
            .expect("download");
        assert_eq!(std::fs::read(&dest).expect("read"), body);
        let seen = seen.lock().expect("log").clone();
        assert_eq!(
            seen,
            vec![String::new(), "range: bytes=50-".into(), String::new()]
        );
    }

    #[tokio::test]
    async fn a_later_call_resumes_a_partial_with_a_validator() {
        let body = body();
        let (url, seen) = serve(vec![
            answer(
                "HTTP/1.1 200 OK\nContent-Length: 200\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\nConnection: close\n",
                &body[..120],
            ),
            answer(
                "HTTP/1.1 206 Partial Content\nContent-Length: 80\nContent-Range: bytes 120-199/200\nConnection: close\n",
                &body[120..],
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("book.m4b");
        let http = Client::new();
        let err = quick()
            .max_attempts(1)
            .to_file(&dest, || http.get(&url))
            .await
            .expect_err("first call is cut off");
        assert!(err.is_transient(), "{err}");
        assert_eq!(
            std::fs::metadata(sibling(&dest, ".part"))
                .expect("part")
                .len(),
            120
        );
        let done = quick()
            .to_file(&dest, || http.get(&url))
            .await
            .expect("second call");
        assert_eq!(done.resumes, 1);
        assert_eq!(std::fs::read(&dest).expect("read"), body);
        let seen = seen.lock().expect("log").clone();
        assert_eq!(
            seen[1],
            "range: bytes=120- | if-range: wed, 21 oct 2015 07:28:00 gmt"
        );
    }

    #[tokio::test]
    async fn a_partial_without_a_validator_is_not_trusted() {
        let body = body();
        let (url, seen) = serve(vec![answer(
            "HTTP/1.1 200 OK\nContent-Length: 200\nConnection: close\n",
            &body,
        )])
        .await;
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("book.m4b");
        std::fs::write(sibling(&dest, ".part"), b"stale bytes").expect("stale");
        let http = Client::new();
        quick()
            .to_file(&dest, || http.get(&url))
            .await
            .expect("download");
        assert_eq!(std::fs::read(&dest).expect("read"), body);
        assert_eq!(seen.lock().expect("log").clone(), vec![String::new()]);
    }

    #[tokio::test]
    async fn a_stalled_transfer_resumes() {
        let body = body();
        let mut stall = answer(
            "HTTP/1.1 200 OK\nContent-Length: 200\nETag: \"s\"\nConnection: close\n",
            &body[..30],
        );
        stall.hold = Duration::from_secs(5);
        let (url, _) = serve(vec![
            stall,
            answer(
                "HTTP/1.1 206 Partial Content\nContent-Length: 170\nContent-Range: bytes 30-199/200\nConnection: close\n",
                &body[30..],
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("book.m4b");
        let http = Client::new();
        let done = quick()
            .idle_timeout(Duration::from_millis(300))
            .to_file(&dest, || http.get(&url))
            .await
            .expect("download");
        assert_eq!(done.resumes, 1);
        assert_eq!(std::fs::read(&dest).expect("read"), body);
    }

    #[tokio::test]
    async fn refusals_and_oversized_files_fail() {
        let (url, _) = serve(vec![
            answer(
                "HTTP/1.1 404 Not Found\nContent-Length: 0\nConnection: close\n",
                b"",
            ),
            answer(
                "HTTP/1.1 200 OK\nContent-Length: 200\nConnection: close\n",
                &body(),
            ),
            answer(
                "HTTP/1.1 503 Busy\nContent-Length: 0\nConnection: close\n",
                b"",
            ),
            answer(
                "HTTP/1.1 503 Busy\nContent-Length: 0\nConnection: close\n",
                b"",
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("book.m4b");
        let http = Client::new();
        let err = quick()
            .to_file(&dest, || http.get(&url))
            .await
            .expect_err("404");
        assert!(matches!(err, DownloadError::Status { status } if status.as_u16() == 404));
        assert!(!err.is_transient());
        let err = quick()
            .max_bytes(100)
            .to_file(&dest, || http.get(&url))
            .await
            .expect_err("too large");
        assert!(matches!(err, DownloadError::TooLarge { limit: 100 }));
        assert!(!sibling(&dest, ".part").exists());
        let err = quick()
            .retries(1)
            .to_file(&dest, || http.get(&url))
            .await
            .expect_err("busy");
        assert!(matches!(err, DownloadError::Status { status } if status.as_u16() == 503));
        assert!(err.is_transient());
        assert!(!dest.exists());
    }
}
