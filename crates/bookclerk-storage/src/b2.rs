//! Backblaze B2 storage through the native API (`b2` feature).
//!
//! The `b2` destination plugin stores through a [`B2Backend`]. It signs in
//! with an application key (`b2_authorize_account`), keeps the account
//! token until B2 says it expired, and then signs in again once. Every
//! reply is read by [`crate::b2_api`], the fuzzed boundary.
//!
//! - Writes of up to one part go up in one `b2_upload_file`. Longer bodies
//!   become large files: `b2_start_large_file`, one `b2_upload_part` per
//!   part, then `b2_finish_large_file`. At most two parts are in memory at
//!   once, and a failed upload is cancelled.
//! - Reads and probes use the download URL (`GET` / `HEAD` on
//!   `/file/<bucket>/<name>`), so a probe never lists.
//! - Copies are server-side (`b2_copy_file`, or `b2_copy_part` above 5 GB).
//! - A delete removes every version of the name, so nothing stays billed
//!   behind a hide marker.
//!
//! Conditional writes are not declared: B2 has none, so the host checks
//! before it writes and reports that the write was not atomic.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use bookclerk_plugin_sdk::http::{Client, Method, Response};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio::sync::Mutex;

use crate::b2_api::{
    self, ApiError, Authorization, DownloadHead, FileAction, FileVersion, ReplyResult,
    UploadTarget, MAX_B2_REPLY_BYTES,
};
use crate::bounded::{ReadSpan, MULTIPART_PART_SIZE};
use crate::{
    ByteRange, ListPage, ObjectInfo, ObjectMeta, ObjectProbe, PutStreamResult, Result,
    StorageBackend, StorageCapabilities, StorageError, MAX_SUPPORTED_OBJECT_BYTES,
};

/// Where `b2_authorize_account` lives; every other URL comes from its
/// reply.
pub const B2_AUTHORIZE_URL: &str = "https://api.backblazeb2.com";

/// Path of the version 3 API calls under an API URL.
const API_PATH: &str = "/b2api/v3/";

/// Tries per call, counting the first, for retryable failures.
const ATTEMPTS: u32 = 4;

/// Largest file `b2_copy_file` copies in one call, in bytes.
const COPY_FILE_MAX_BYTES: u64 = 5_000_000_000;

/// Part size of a large server-side copy, in bytes.
const COPY_PART_BYTES: u64 = 1 << 30;

/// Versions asked for per `b2_list_file_versions` page.
const VERSIONS_PAGE: u32 = 1000;

/// Most versions of one name a delete walks.
const MAX_VERSIONS_WALKED: usize = 10_000;

/// Content type that asks B2 to pick one from the file name.
const AUTO_CONTENT_TYPE: &str = "b2/x-auto";

/// Longest percent-encoded file-info value sent; a longer value (a very
/// long title) is left out rather than failing the upload.
const MAX_INFO_VALUE_BYTES: usize = 2048;

/// An application key's secret half. Its `Debug` output never shows it.
#[derive(Clone)]
pub struct B2ApplicationKey(String);

impl B2ApplicationKey {
    /// Wraps the secret.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }
}

impl std::fmt::Debug for B2ApplicationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("B2ApplicationKey([redacted])")
    }
}

/// How to reach one bucket.
#[derive(Debug, Clone)]
pub struct B2Settings {
    /// The application key's id.
    pub key_id: String,
    /// The application key.
    pub application_key: B2ApplicationKey,
    /// The bucket's name.
    pub bucket: String,
    /// Prefix under which every key is stored (may be empty).
    pub prefix: String,
    /// Base URL of `b2_authorize_account` ([`B2_AUTHORIZE_URL`]).
    pub authorize_url: String,
}

impl B2Settings {
    /// Settings for `bucket` under `prefix`, signing in at
    /// [`B2_AUTHORIZE_URL`].
    #[must_use]
    pub fn new(
        key_id: impl Into<String>,
        application_key: B2ApplicationKey,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Self {
        Self {
            key_id: key_id.into(),
            application_key,
            bucket: bucket.into(),
            prefix: prefix.into(),
            authorize_url: B2_AUTHORIZE_URL.into(),
        }
    }
}

/// One sign-in: what the key may do, and the bucket it resolved to.
struct Session {
    /// The authorization reply.
    auth: Authorization,
    /// The bucket's id, which API calls take.
    bucket_id: String,
}

/// State shared by every clone of a [`B2Backend`].
struct Inner {
    /// HTTP client (read and connect timeouts, no whole-request timeout, so
    /// long downloads stream).
    http: Client,
    /// The settings, with the prefix normalized.
    settings: B2Settings,
    /// The current sign-in, made on first use.
    session: Mutex<Option<Arc<Session>>>,
    /// Upload part size; the larger of [`MULTIPART_PART_SIZE`] and B2's
    /// minimum. Tests may shrink the floor.
    part_floor: usize,
}

/// A bucket (and prefix) on Backblaze B2.
#[derive(Clone)]
pub struct B2Backend {
    /// Shared state.
    inner: Arc<Inner>,
}

/// Maps a failure to [`StorageError::Remote`] with what was being done.
fn remote(what: &str, err: impl std::fmt::Display) -> StorageError {
    StorageError::Remote(format!("B2 {what}: {err}"))
}

/// Maps an API error reply.
fn api_error(what: &str, err: &ApiError) -> StorageError {
    if err.not_found() {
        StorageError::NotFound(format!("{what}: {err}"))
    } else {
        remote(what, err)
    }
}

/// Waits before try `attempt + 1`: 250 ms, 500 ms, 1 s.
async fn backoff(attempt: u32) {
    let millis = 250u64.saturating_mul(1 << attempt.saturating_sub(1).min(4));
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

/// Reads a reply body, stopping one byte past `MAX_B2_REPLY_BYTES` so an
/// oversized reply never sits whole in memory (the parser refuses it).
async fn read_reply(mut reply: Response) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = reply.chunk().await.map_err(|err| remote("reply", err))? {
        let room = (MAX_B2_REPLY_BYTES + 1).saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if bytes.len() > MAX_B2_REPLY_BYTES {
            break;
        }
    }
    Ok(bytes)
}

/// Reads a reply into its body, or the API error it carries.
async fn settle(reply: Response) -> Result<std::result::Result<Vec<u8>, ApiError>> {
    let status = reply.status();
    let bytes = read_reply(reply).await?;
    if status.is_success() {
        Ok(Ok(bytes))
    } else {
        Ok(Err(b2_api::parse_api_error(status.as_u16(), &bytes)))
    }
}

/// A content type safe to send as a header, or B2's automatic one.
fn content_type_header(meta: Option<&str>) -> String {
    match meta {
        Some(kind)
            if !kind.is_empty()
                && kind.len() <= 256
                && kind.bytes().all(|b| b.is_ascii_graphic() || b == b' ') =>
        {
            kind.to_string()
        }
        _ => AUTO_CONTENT_TYPE.into(),
    }
}

/// The file info stored with a write: the same names the S3 backend uses
/// for user metadata.
fn file_info(meta: &ObjectMeta, sha256_hex: Option<&str>) -> BTreeMap<String, String> {
    let mut info = BTreeMap::new();
    let fields = [
        ("asin", meta.asin.as_deref()),
        ("title", meta.title.as_deref()),
        ("creation-time", meta.creation_time.as_deref()),
        ("last-write-time", meta.last_write_time.as_deref()),
        ("sha256", sha256_hex),
        ("commit-token", meta.commit_token.as_deref()),
    ];
    for (name, value) in fields {
        if let Some(value) = value {
            if b2_api::encode_file_name(value).len() <= MAX_INFO_VALUE_BYTES {
                info.insert(name.to_string(), value.to_string());
            }
        }
    }
    info
}

/// Lowercase hex SHA-1 of `bytes`.
fn sha1_hex(bytes: &[u8]) -> String {
    hex::encode(Sha1::digest(bytes))
}

/// Reads up to `size` bytes from `body`; fewer only at the end.
async fn read_part(body: &mut Pin<Box<dyn AsyncRead + Send>>, size: usize) -> Result<Vec<u8>> {
    let mut part = vec![0u8; size];
    let mut filled = 0;
    while filled < size {
        let n = body.read(&mut part[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    part.truncate(filled);
    Ok(part)
}

/// Running length and digest of a write's body, checked against what the
/// caller declared.
struct BodyTally {
    /// Bytes seen.
    total: u64,
    /// SHA-256 of the bytes seen.
    hasher: Sha256,
    /// Declared length, when there is one.
    expected_len: Option<u64>,
}

impl BodyTally {
    /// Adds `bytes`.
    fn take(&mut self, bytes: &[u8]) -> Result<()> {
        let len = u64::try_from(bytes.len()).map_err(|_| remote("upload", "part too long"))?;
        let next = self.total.saturating_add(len);
        if let Some(expected) = self.expected_len {
            if next > expected {
                return Err(StorageError::Integrity(format!(
                    "B2 upload exceeded declared length {expected}"
                )));
            }
        }
        if next > MAX_SUPPORTED_OBJECT_BYTES {
            return Err(StorageError::PayloadTooLarge(format!(
                "B2 upload exceeded {MAX_SUPPORTED_OBJECT_BYTES} bytes"
            )));
        }
        self.hasher.update(bytes);
        self.total = next;
        Ok(())
    }

    /// Checks the whole body against the declared length and digest, and
    /// returns its SHA-256.
    fn finish(self, expected_sha: Option<&str>) -> Result<(u64, String)> {
        let digest = hex::encode(self.hasher.finalize());
        if let Some(expected) = expected_sha {
            if !expected.eq_ignore_ascii_case(&digest) {
                return Err(StorageError::Integrity(
                    "B2 upload sha256 does not match the body".into(),
                ));
            }
        }
        if let Some(expected) = self.expected_len {
            if self.total != expected {
                return Err(StorageError::Integrity(format!(
                    "B2 upload body was {} bytes, declared {expected}",
                    self.total
                )));
            }
        }
        Ok((self.total, digest))
    }
}

/// What a download request found.
enum Fetched {
    /// No file has the name.
    Missing,
    /// The range starts past the end of the file.
    Unsatisfiable,
    /// The file, with the reply whose body streams it.
    Found(Box<DownloadHead>, Box<Response>),
}

/// A download body that must hold exactly the length its headers gave.
struct SizedBody {
    /// The body, cut at the expected length.
    inner: tokio::io::Take<Pin<Box<dyn AsyncRead + Send>>>,
    /// Bytes read so far.
    read: u64,
    /// Bytes expected.
    expected: u64,
}

impl AsyncRead for SizedBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len() - before;
                if n == 0 && buf.remaining() > 0 && self.read < self.expected {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "B2 download ended before its Content-Length",
                    )));
                }
                self.read = self
                    .read
                    .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl B2Backend {
    /// A backend for a bucket. Nothing is sent until the first call, which
    /// signs in, finds the bucket, and checks the key may reach the prefix
    /// (a key restricted to another bucket or prefix fails that call with
    /// [`StorageError::InvalidKey`]).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidKey`] for an empty key id or bucket,
    /// or a prefix B2 cannot store, and [`StorageError::Remote`] when the
    /// HTTP client cannot be built.
    pub fn new(settings: B2Settings) -> Result<Self> {
        Self::with_part_floor(settings, MULTIPART_PART_SIZE)
    }

    /// [`Self::new`] with another part-size floor (tests use small parts).
    fn with_part_floor(mut settings: B2Settings, part_floor: usize) -> Result<Self> {
        if settings.key_id.trim().is_empty() {
            return Err(StorageError::InvalidKey(
                "B2 application key id is empty".into(),
            ));
        }
        if settings.bucket.trim().is_empty() {
            return Err(StorageError::InvalidKey("B2 bucket is empty".into()));
        }
        settings.prefix = crate::normalize_prefix(&settings.prefix);
        if let Some(stem) = settings.prefix.strip_suffix('/') {
            b2_api::check_file_name(stem)
                .map_err(|err| StorageError::InvalidKey(format!("B2 prefix: {err}")))?;
        }
        settings.authorize_url = settings.authorize_url.trim_end_matches('/').to_string();
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()
            .map_err(|err| remote("client", err))?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                settings,
                session: Mutex::new(None),
                part_floor,
            }),
        })
    }

    /// The stored name of `key`.
    fn name_for(&self, key: &str) -> Result<String> {
        let name = format!("{}{key}", self.inner.settings.prefix);
        b2_api::check_file_name(&name)
            .map_err(|err| StorageError::InvalidKey(format!("{key}: {err}")))?;
        Ok(name)
    }

    /// The key of a stored name, when it is under the prefix.
    fn key_for<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix(self.inner.settings.prefix.as_str())
    }

    /// Signs in and resolves the bucket.
    async fn authorize(&self) -> Result<Session> {
        let settings = &self.inner.settings;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            settings.key_id, settings.application_key.0
        ));
        let reply = self
            .inner
            .http
            .get(format!(
                "{}{API_PATH}b2_authorize_account",
                settings.authorize_url
            ))
            .header("Authorization", format!("Basic {basic}"))
            .send()
            .await
            .map_err(|err| remote("sign-in", err))?;
        let bytes = match settle(reply).await? {
            Ok(bytes) => bytes,
            Err(err) => return Err(remote("sign-in", err)),
        };
        let auth = b2_api::parse_authorization(&bytes).map_err(|err| remote("sign-in", err))?;
        let bucket_id = match &auth.bucket {
            Some(bucket) if bucket.name == settings.bucket => bucket.id.clone(),
            Some(bucket) => {
                return Err(StorageError::InvalidKey(format!(
                    "the B2 application key only reaches bucket {}",
                    bucket.name
                )));
            }
            None => self.find_bucket(&auth).await?,
        };
        if let Some(prefix) = &auth.name_prefix {
            if !settings.prefix.starts_with(prefix.as_str()) {
                return Err(StorageError::InvalidKey(format!(
                    "the B2 application key only reaches names under {prefix}; set the prefix under it"
                )));
            }
        }
        Ok(Session { auth, bucket_id })
    }

    /// Looks the bucket up by name with an unrestricted key.
    async fn find_bucket(&self, auth: &Authorization) -> Result<String> {
        let bucket = &self.inner.settings.bucket;
        let reply = self
            .inner
            .http
            .post(format!(
                "{}{API_PATH}b2_list_buckets",
                auth.api_url.as_str()
            ))
            .header("Authorization", auth.token.expose())
            .json(&json!({ "accountId": auth.account_id, "bucketName": bucket }))
            .send()
            .await
            .map_err(|err| remote("bucket lookup", err))?;
        let bytes = match settle(reply).await? {
            Ok(bytes) => bytes,
            Err(err) => return Err(remote("bucket lookup", err)),
        };
        b2_api::parse_buckets(&bytes)
            .map_err(|err| remote("bucket lookup", err))?
            .into_iter()
            .find(|found| &found.name == bucket)
            .map(|found| found.id)
            .ok_or_else(|| {
                StorageError::InvalidKey(format!(
                    "B2 bucket {bucket} does not exist or the key cannot see it"
                ))
            })
    }

    /// The current sign-in, made on first use.
    async fn session(&self) -> Result<Arc<Session>> {
        let mut guard = self.inner.session.lock().await;
        if let Some(session) = guard.as_ref() {
            return Ok(Arc::clone(session));
        }
        let fresh = Arc::new(self.authorize().await?);
        *guard = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Signs in again after `stale` expired, unless another call already
    /// did.
    async fn refresh(&self, stale: &Arc<Session>) -> Result<Arc<Session>> {
        let mut guard = self.inner.session.lock().await;
        if let Some(current) = guard.as_ref() {
            if !Arc::ptr_eq(current, stale) {
                return Ok(Arc::clone(current));
            }
        }
        let fresh = Arc::new(self.authorize().await?);
        *guard = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    /// The upload part size for this session.
    fn part_size(&self, session: &Session) -> Result<usize> {
        let minimum = usize::try_from(session.auth.absolute_minimum_part_size)
            .map_err(|_| remote("sign-in", "minimum part size is too large"))?;
        let size = self.inner.part_floor.max(minimum);
        if size > 5_000_000_000 {
            return Err(remote("sign-in", "minimum part size is too large"));
        }
        Ok(size)
    }

    /// Calls an API operation, signing in again once when the token
    /// expired and retrying busy or failed requests.
    async fn call<T>(
        &self,
        op: &str,
        body: &Value,
        parse: impl Fn(&[u8]) -> ReplyResult<T> + Send,
    ) -> Result<T> {
        let mut session = self.session().await?;
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let sent = self
                .inner
                .http
                .post(format!("{}{API_PATH}{op}", session.auth.api_url.as_str()))
                .header("Authorization", session.auth.token.expose())
                .json(body)
                .send()
                .await;
            let settled = match sent {
                Ok(reply) => settle(reply).await,
                Err(err) => Err(remote(op, err)),
            };
            match settled {
                Ok(Ok(bytes)) => return parse(&bytes).map_err(|err| remote(op, err)),
                Ok(Err(err)) if err.expired_token() && !refreshed => {
                    refreshed = true;
                    session = self.refresh(&session).await?;
                }
                Ok(Err(err)) if err.retryable() && attempt < ATTEMPTS => backoff(attempt).await,
                Ok(Err(err)) => return Err(api_error(op, &err)),
                Err(_) if attempt < ATTEMPTS => backoff(attempt).await,
                Err(err) => return Err(err),
            }
        }
    }

    /// Posts `data` to an upload URL (`b2_get_upload_url` or, with
    /// `part`, `b2_get_upload_part_url`), fetching a new URL after any
    /// failure as B2 asks.
    async fn upload(
        &self,
        target_call: (&str, Value),
        data: &[u8],
        headers: &[(String, String)],
    ) -> Result<Vec<u8>> {
        let sha1 = sha1_hex(data);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let target: UploadTarget = self
                .call(target_call.0, &target_call.1, b2_api::parse_upload_target)
                .await?;
            let mut request = self
                .inner
                .http
                .post(target.url.as_str())
                .header("Authorization", target.token.expose())
                .header("X-Bz-Content-Sha1", sha1.as_str());
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let settled = match request.body(data.to_vec()).send().await {
                Ok(reply) => settle(reply).await,
                Err(err) => Err(remote("upload", err)),
            };
            match settled {
                Ok(Ok(bytes)) => return Ok(bytes),
                Ok(Err(err)) if attempt < ATTEMPTS && (err.retryable() || err.status == 401) => {
                    backoff(attempt).await;
                }
                Ok(Err(err)) => return Err(api_error("upload", &err)),
                Err(_) if attempt < ATTEMPTS => backoff(attempt).await,
                Err(err) => return Err(err),
            }
        }
    }

    /// Uploads a whole small file.
    async fn upload_small(
        &self,
        session: &Session,
        name: &str,
        data: &[u8],
        content_type: &str,
        info: &BTreeMap<String, String>,
    ) -> Result<FileVersion> {
        let mut headers = vec![
            ("X-Bz-File-Name".to_string(), b2_api::encode_file_name(name)),
            ("Content-Type".to_string(), content_type.to_string()),
        ];
        for (key, value) in info {
            headers.push((format!("X-Bz-Info-{key}"), b2_api::encode_file_name(value)));
        }
        let reply = self
            .upload(
                (
                    "b2_get_upload_url",
                    json!({ "bucketId": session.bucket_id }),
                ),
                data,
                &headers,
            )
            .await?;
        let version = b2_api::parse_file_version(&reply).map_err(|err| remote("upload", err))?;
        let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if version.file_name != name
            || version.content_length != length
            || version.content_sha1.as_deref() != Some(sha1_hex(data).as_str())
        {
            return Err(remote(
                "upload",
                "the stored file does not match the upload",
            ));
        }
        Ok(version)
    }

    /// Uploads one part of a large file and returns its SHA-1.
    async fn upload_part(&self, file_id: &str, number: u32, data: &[u8]) -> Result<String> {
        let headers = vec![("X-Bz-Part-Number".to_string(), number.to_string())];
        let reply = self
            .upload(
                ("b2_get_upload_part_url", json!({ "fileId": file_id })),
                data,
                &headers,
            )
            .await?;
        let part = b2_api::parse_part(&reply).map_err(|err| remote("upload part", err))?;
        let sha1 = sha1_hex(data);
        if part.part_number != number || part.content_sha1 != sha1 {
            return Err(remote("upload part", "the stored part does not match"));
        }
        Ok(sha1)
    }

    /// Cancels an unfinished large file, best effort.
    async fn cancel_large_file(&self, file_id: &str) {
        let cancelled = self
            .call(
                "b2_cancel_large_file",
                &json!({ "fileId": file_id }),
                |_| Ok(()),
            )
            .await;
        if let Err(err) = cancelled {
            tracing::warn!(error = %err, "could not cancel an unfinished B2 large file");
        }
    }

    /// Starts a large file and returns its id.
    async fn start_large_file(
        &self,
        session: &Session,
        name: &str,
        content_type: &str,
        info: &BTreeMap<String, String>,
    ) -> Result<String> {
        let started = self
            .call(
                "b2_start_large_file",
                &json!({
                    "bucketId": session.bucket_id,
                    "fileName": name,
                    "contentType": content_type,
                    "fileInfo": info,
                }),
                b2_api::parse_file_version,
            )
            .await?;
        if started.file_name != name || started.action != FileAction::Start {
            return Err(remote("start large file", "the reply names another file"));
        }
        started
            .file_id
            .map(|id| id.as_str().to_string())
            .ok_or_else(|| remote("start large file", "no file id"))
    }

    /// Finishes a large file from its parts' SHA-1s.
    async fn finish_large_file(&self, file_id: &str, sha1s: &[String]) -> Result<FileVersion> {
        self.call(
            "b2_finish_large_file",
            &json!({ "fileId": file_id, "partSha1Array": sha1s }),
            b2_api::parse_file_version,
        )
        .await
    }

    /// Streams `body` into `name` and returns what was stored.
    async fn write(
        &self,
        key: &str,
        mut body: Pin<Box<dyn AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> Result<(PutStreamResult, FileVersion)> {
        let name = self.name_for(key)?;
        if let Some(expected) = meta.sha256_hex.as_deref() {
            crate::bounded::parse_sha256_hex(expected)?;
        }
        if let Some(len) = meta.content_length {
            if len > MAX_SUPPORTED_OBJECT_BYTES {
                return Err(StorageError::PayloadTooLarge(format!(
                    "object length {len} exceeds {MAX_SUPPORTED_OBJECT_BYTES}"
                )));
            }
        }
        let session = self.session().await?;
        let part_size = self.part_size(&session)?;
        let content_type = content_type_header(meta.content_type.as_deref());
        let mut tally = BodyTally {
            total: 0,
            hasher: Sha256::new(),
            expected_len: meta.content_length,
        };
        let first = read_part(&mut body, part_size).await?;
        tally.take(&first)?;
        let second = if first.len() == part_size {
            read_part(&mut body, part_size).await?
        } else {
            Vec::new()
        };
        if second.is_empty() {
            let (total, digest) = tally.finish(meta.sha256_hex.as_deref())?;
            let info = file_info(&meta, Some(&digest));
            let version = self
                .upload_small(&session, &name, &first, &content_type, &info)
                .await?;
            return Ok((
                PutStreamResult {
                    bytes_written: total,
                    etag: version.file_id.as_ref().map(|id| id.as_str().to_string()),
                    sha256_hex: Some(digest),
                },
                version,
            ));
        }
        tally.take(&second)?;
        let info = file_info(&meta, meta.sha256_hex.as_deref());
        let file_id = self
            .start_large_file(&session, &name, &content_type, &info)
            .await?;
        let uploaded = async {
            let mut sha1s = vec![
                self.upload_part(&file_id, 1, &first).await?,
                self.upload_part(&file_id, 2, &second).await?,
            ];
            drop((first, second));
            let mut number = 2u32;
            loop {
                let part = read_part(&mut body, part_size).await?;
                if part.is_empty() {
                    break;
                }
                number += 1;
                if number > b2_api::MAX_PARTS {
                    return Err(StorageError::PayloadTooLarge(format!(
                        "B2 upload exceeded {} parts",
                        b2_api::MAX_PARTS
                    )));
                }
                tally.take(&part)?;
                sha1s.push(self.upload_part(&file_id, number, &part).await?);
            }
            let (total, digest) = tally.finish(meta.sha256_hex.as_deref())?;
            let version = self.finish_large_file(&file_id, &sha1s).await?;
            if version.file_name != name || version.content_length != total {
                return Err(remote(
                    "finish large file",
                    "the stored file does not match",
                ));
            }
            Ok((total, digest, version))
        }
        .await;
        match uploaded {
            Ok((total, digest, version)) => Ok((
                PutStreamResult {
                    bytes_written: total,
                    etag: version.file_id.as_ref().map(|id| id.as_str().to_string()),
                    sha256_hex: Some(digest),
                },
                version,
            )),
            Err(err) => {
                self.cancel_large_file(&file_id).await;
                Err(err)
            }
        }
    }

    /// Requests `name` from the download URL.
    async fn fetch(&self, method: Method, name: &str, range: Option<&str>) -> Result<Fetched> {
        let mut session = self.session().await?;
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let url = format!(
                "{}/file/{}/{}",
                session.auth.download_url.as_str(),
                self.inner.settings.bucket,
                b2_api::encode_file_name(name)
            );
            let mut request = self
                .inner
                .http
                .request(method.clone(), url)
                .header("Authorization", session.auth.token.expose());
            if let Some(range) = range {
                request = request.header("Range", range);
            }
            let reply = match request.send().await {
                Ok(reply) => reply,
                Err(_) if attempt < ATTEMPTS => {
                    backoff(attempt).await;
                    continue;
                }
                Err(err) => return Err(remote("download", err)),
            };
            let status = reply.status().as_u16();
            match status {
                200 | 206 => {
                    let headers: Vec<(&str, &[u8])> = reply
                        .headers()
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_bytes()))
                        .collect();
                    let head = b2_api::parse_download_headers(headers)
                        .map_err(|err| remote("download", err))?;
                    if head.version.file_name != name {
                        return Err(remote("download", "the reply names another file"));
                    }
                    return Ok(Fetched::Found(Box::new(head), Box::new(reply)));
                }
                404 => return Ok(Fetched::Missing),
                416 => return Ok(Fetched::Unsatisfiable),
                _ => {}
            }
            let err = if method == Method::HEAD {
                b2_api::parse_api_error(status, &[])
            } else {
                b2_api::parse_api_error(status, &read_reply(reply).await?)
            };
            // A HEAD reply has no body, so an expired token reads as a
            // bare 401: sign in again once for either.
            if status == 401 && !refreshed {
                refreshed = true;
                session = self.refresh(&session).await?;
            } else if err.retryable() && attempt < ATTEMPTS {
                backoff(attempt).await;
            } else {
                return Err(api_error("download", &err));
            }
        }
    }

    /// The probe of a stored version.
    fn probe_of(key: &str, version: &FileVersion) -> ObjectProbe {
        let info = &version.file_info;
        let field = |name: &str| info.get(name).cloned();
        let sha256_hex = info
            .get("sha256")
            .filter(|hex| crate::bounded::parse_sha256_hex(hex).is_ok())
            .map(|hex| hex.to_ascii_lowercase());
        let file_id = version.file_id.as_ref().map(|id| id.as_str().to_string());
        ObjectProbe {
            key: key.to_string(),
            size: version.content_length,
            content_type: version.content_type.clone(),
            meta: ObjectMeta {
                content_type: version.content_type.clone(),
                content_length: Some(version.content_length),
                asin: field("asin"),
                title: field("title"),
                creation_time: field("creation-time"),
                last_write_time: field("last-write-time"),
                sha256_hex,
                commit_token: field("commit-token"),
            },
            etag: file_id.clone(),
            modified_at_ms: (version.upload_timestamp_ms > 0)
                .then_some(version.upload_timestamp_ms),
            version_id: file_id.clone(),
            signature: file_id,
        }
    }

    /// The stored version of `key`, or `None`.
    async fn stat(&self, key: &str) -> Result<Option<FileVersion>> {
        let name = self.name_for(key)?;
        match self.fetch(Method::HEAD, &name, None).await? {
            Fetched::Found(head, _) => Ok(Some(head.version)),
            Fetched::Missing => Ok(None),
            Fetched::Unsatisfiable => Err(remote("probe", "HEAD answered 416")),
        }
    }

    /// Every version of `name`, newest first.
    async fn versions_of(&self, name: &str) -> Result<Vec<FileVersion>> {
        let session = self.session().await?;
        let mut versions = Vec::new();
        let mut start: Option<(String, String)> = None;
        loop {
            let mut body = Map::new();
            body.insert("bucketId".into(), json!(session.bucket_id));
            body.insert("prefix".into(), json!(name));
            body.insert("maxFileCount".into(), json!(VERSIONS_PAGE));
            match &start {
                Some((next_name, next_id)) => {
                    body.insert("startFileName".into(), json!(next_name));
                    body.insert("startFileId".into(), json!(next_id));
                }
                None => {
                    body.insert("startFileName".into(), json!(name));
                }
            }
            let page = self
                .call(
                    "b2_list_file_versions",
                    &Value::Object(body),
                    b2_api::parse_versions_page,
                )
                .await?;
            let mut past = false;
            for file in page.files {
                if file.file_name == name {
                    versions.push(file);
                } else {
                    past = true;
                }
            }
            if past || versions.len() >= MAX_VERSIONS_WALKED {
                return Ok(versions);
            }
            match page.next {
                Some((next_name, next_id)) if next_name == name => {
                    let next = (next_name, next_id.as_str().to_string());
                    if start.as_ref() == Some(&next) {
                        return Err(remote("version listing", "the listing did not move"));
                    }
                    start = Some(next);
                }
                _ => return Ok(versions),
            }
        }
    }

    /// Server-side copy of `from` to `to`, returning the bytes copied.
    async fn copy_file(&self, from: &str, to: &str) -> Result<u64> {
        let source = self
            .stat(from)
            .await?
            .ok_or_else(|| StorageError::NotFound(from.to_string()))?;
        let source_id = source
            .file_id
            .as_ref()
            .map(|id| id.as_str().to_string())
            .ok_or_else(|| remote("copy", "the source has no file id"))?;
        let to_name = self.name_for(to)?;
        if source.content_length <= COPY_FILE_MAX_BYTES {
            let copied = self
                .call(
                    "b2_copy_file",
                    &json!({ "sourceFileId": source_id, "fileName": to_name }),
                    b2_api::parse_file_version,
                )
                .await?;
            if copied.file_name != to_name || copied.content_length != source.content_length {
                return Err(remote("copy", "the copy does not match its source"));
            }
            return Ok(copied.content_length);
        }
        let session = self.session().await?;
        let content_type = content_type_header(source.content_type.as_deref());
        let file_id = self
            .start_large_file(&session, &to_name, &content_type, &source.file_info)
            .await?;
        let copied = async {
            let mut sha1s = Vec::new();
            let mut offset = 0u64;
            let mut number = 0u32;
            while offset < source.content_length {
                number += 1;
                let end = offset
                    .saturating_add(COPY_PART_BYTES)
                    .min(source.content_length)
                    - 1;
                let part = self
                    .call(
                        "b2_copy_part",
                        &json!({
                            "sourceFileId": source_id,
                            "largeFileId": file_id,
                            "partNumber": number,
                            "range": format!("bytes={offset}-{end}"),
                        }),
                        b2_api::parse_part,
                    )
                    .await?;
                if part.part_number != number {
                    return Err(remote("copy part", "the reply names another part"));
                }
                sha1s.push(part.content_sha1);
                offset = end + 1;
            }
            let version = self.finish_large_file(&file_id, &sha1s).await?;
            if version.content_length != source.content_length {
                return Err(remote("copy", "the copy does not match its source"));
            }
            Ok(version.content_length)
        }
        .await;
        if copied.is_err() {
            self.cancel_large_file(&file_id).await;
        }
        copied
    }
}

#[async_trait]
impl StorageBackend for B2Backend {
    fn name(&self) -> &'static str {
        "b2"
    }

    fn instance_id(&self) -> String {
        format!(
            "b2:{}/{}",
            self.inner.settings.bucket, self.inner.settings.prefix
        )
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            translate_keys: true,
            ..StorageCapabilities::default()
        }
    }

    async fn translate_keys(&self, keys: &[crate::key_syntax::HostKey]) -> Result<Vec<String>> {
        let prefix = self.inner.settings.prefix.len();
        Ok(keys
            .iter()
            .map(|key| crate::key_syntax::translate_for_object_store(key, prefix))
            .collect())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
        self.write(key, Box::pin(std::io::Cursor::new(data)), meta)
            .await
            .map(|_| ())
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let (probe, body) = self.get_stream(key, None).await?;
        crate::bounded::reject_scalar_hint(probe.size, crate::bounded::MAX_SCALAR_OBJECT_BYTES)?;
        crate::bounded::read_scalar_body(body, crate::bounded::MAX_SCALAR_OBJECT_BYTES).await
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.stat(key).await?.is_some())
    }

    async fn probe(&self, key: &str) -> Result<ObjectProbe> {
        self.stat(key)
            .await?
            .map(|version| Self::probe_of(key, &version))
            .ok_or_else(|| StorageError::NotFound(key.to_string()))
    }

    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        self.copy_file(from, to).await.map(|_| ())
    }

    async fn copy_sized(&self, from: &str, to: &str) -> Result<u64> {
        self.copy_file(from, to).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let name = self.name_for(key)?;
        for version in self.versions_of(&name).await? {
            let Some(file_id) = version.file_id else {
                continue;
            };
            let removed = match version.action {
                FileAction::Start => {
                    self.call(
                        "b2_cancel_large_file",
                        &json!({ "fileId": file_id.as_str() }),
                        |_| Ok(()),
                    )
                    .await
                }
                FileAction::Upload | FileAction::Copy | FileAction::Hide | FileAction::Folder => {
                    self.call(
                        "b2_delete_file_version",
                        &json!({ "fileName": name, "fileId": file_id.as_str() }),
                        |_| Ok(()),
                    )
                    .await
                }
            };
            match removed {
                Ok(()) | Err(StorageError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    async fn list_page(&self, prefix: &str, cursor: Option<&str>, limit: u32) -> Result<ListPage> {
        let full = format!("{}{prefix}", self.inner.settings.prefix);
        let start = match cursor {
            None => full.clone(),
            Some(cursor) => {
                if !cursor.starts_with(&full)
                    || cursor == full
                    || b2_api::check_file_name(cursor).is_err()
                {
                    return Err(StorageError::InvalidCursor(
                        "B2 listing cursor is not from this listing".into(),
                    ));
                }
                cursor.to_string()
            }
        };
        let session = self.session().await?;
        let mut body = Map::new();
        body.insert("bucketId".into(), json!(session.bucket_id));
        body.insert(
            "maxFileCount".into(),
            json!(crate::bounded::clamp_page_limit(limit)),
        );
        if !full.is_empty() {
            body.insert("prefix".into(), json!(full));
        }
        if !start.is_empty() {
            body.insert("startFileName".into(), json!(start));
        }
        let page = self
            .call(
                "b2_list_file_names",
                &Value::Object(body),
                b2_api::parse_names_page,
            )
            .await?;
        let mut objects = Vec::with_capacity(page.files.len());
        for file in page.files {
            if !file.file_name.starts_with(&full) || file.file_name < start {
                return Err(remote("listing", "the reply lists a name outside the page"));
            }
            if !file.action.is_stored() {
                continue;
            }
            let Some(key) = self.key_for(&file.file_name) else {
                continue;
            };
            let file_id = file.file_id.as_ref().map(|id| id.as_str().to_string());
            objects.push(ObjectInfo {
                key: key.to_string(),
                size: file.content_length,
                etag: file_id.clone(),
                modified_at_ms: (file.upload_timestamp_ms > 0).then_some(file.upload_timestamp_ms),
                version_id: file_id.clone(),
                signature: file_id,
            });
        }
        let next_cursor = page.next_file_name.filter(|next| next.starts_with(&full));
        Ok(ListPage {
            objects,
            next_cursor,
        })
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(ObjectProbe, Pin<Box<dyn AsyncRead + Send>>)> {
        let name = self.name_for(key)?;
        let header = crate::normalize_range(range, None)?.map(|span| match span {
            ReadSpan::ToEnd { offset } => format!("bytes={offset}-"),
            ReadSpan::Exact { offset, length } => {
                format!("bytes={offset}-{}", offset.saturating_add(length) - 1)
            }
        });
        match self.fetch(Method::GET, &name, header.as_deref()).await? {
            Fetched::Missing => Err(StorageError::NotFound(key.to_string())),
            Fetched::Unsatisfiable => {
                // A read that starts exactly at the end is empty, as on the
                // other backends; anything past the end is refused.
                let version = self
                    .stat(key)
                    .await?
                    .ok_or_else(|| StorageError::NotFound(key.to_string()))?;
                crate::normalize_range(range, Some(version.content_length))?;
                let empty: Pin<Box<dyn AsyncRead + Send>> = Box::pin(tokio::io::empty());
                Ok((Self::probe_of(key, &version), empty))
            }
            Fetched::Found(head, reply) => {
                let expected =
                    match crate::normalize_range(range, Some(head.version.content_length))? {
                        None => head.version.content_length,
                        Some(ReadSpan::ToEnd { offset }) => head.version.content_length - offset,
                        Some(ReadSpan::Exact { length, .. }) => length,
                    };
                if head.body_length != expected {
                    return Err(remote("download", "the reply is not the range asked for"));
                }
                let stream = (*reply)
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(|err| std::io::Error::other(err.to_string())));
                let reader: Pin<Box<dyn AsyncRead + Send>> =
                    Box::pin(tokio_util::io::StreamReader::new(Box::pin(stream)));
                let body = SizedBody {
                    inner: reader.take(expected),
                    read: 0,
                    expected,
                };
                Ok((Self::probe_of(key, &head.version), Box::pin(body)))
            }
        }
    }

    async fn put_stream(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> Result<PutStreamResult> {
        self.write(key, body, meta).await.map(|(put, _)| put)
    }

    fn supports_server_copy(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "b2_tests.rs"]
mod tests;
