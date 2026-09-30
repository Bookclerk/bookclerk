use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::pin::Pin;
use std::time::SystemTime;
use tokio::io::AsyncRead;

use crate::error::{Result, StorageError};

/// Metadata attached to a stored object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectMeta {
    /// MIME type stored with the object (e.g. `audio/mp4`).
    pub content_type: Option<String>,
    /// Object size in bytes when known at put time.
    pub content_length: Option<u64>,
    /// Free-form ASIN / title tags for S3 object metadata.
    pub asin: Option<String>,
    /// Display title stored as object user-metadata when supported.
    pub title: Option<String>,
    /// Creation timestamp as RFC 3339 (S3 metadata `creation-time`).
    pub creation_time: Option<String>,
    /// Last-write timestamp as RFC 3339 (S3 metadata `last-write-time`).
    pub last_write_time: Option<String>,
    /// Lowercase hex SHA-256 (64 chars) of the object body when known.
    ///
    /// This is a content digest. It is never derived from an S3 ETag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_hex: Option<String>,
    /// Retry-stable publication token stored with the object, when staged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_token: Option<String>,
}

/// Listing entry returned by [`StorageBackend::list`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    /// Relative storage key (prefix + path; no leading slash).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
}

/// Cheap object probe (S3 `HeadObject` / local sidecar meta) — never downloads
/// object bodies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectProbe {
    /// Relative storage key that was probed.
    pub key: String,
    /// Object size in bytes from HeadObject / local metadata.
    pub size: u64,
    /// MIME type when the backend exposes it.
    pub content_type: Option<String>,
    /// User-metadata / sidecar fields (ASIN, title, timestamps, …).
    pub meta: ObjectMeta,
    /// Backend entity tag. Opaque; not a content checksum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// Audio extensions considered acquired media for storage matching.
///
/// Includes default remux/encode outputs (`m4b` / `mp3` / `m4a`) plus plain
/// passthrough containers Chirp / GraphicAudio may store under noop/`as_is`
/// output (`flac` / `aac` / `ogg` / `oga`). Keep aligned with
/// `bookclerk_source::media` sniffing and GraphicAudio ZIP audio filters.
pub const AUDIO_EXTENSIONS: &[&str] = &["m4b", "mp3", "m4a", "flac", "aac", "ogg", "oga"];

/// True when `key` ends with a known acquired audio extension.
#[must_use]
pub fn is_audio_key(key: &str) -> bool {
    let Some((_, ext)) = key.rsplit_once('.') else {
        return false;
    };
    AUDIO_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// Sidecar key for probe metadata of this exact object (`{key}.bookclerk-meta.json`).
///
/// The record is not shared with another object that only shares a title stem.
/// `book.m4b` and `book.jpg` keep separate integrity and commit metadata.
#[must_use]
pub fn bookclerk_meta_sidecar_key(object_key: &str) -> String {
    format!("{object_key}.bookclerk-meta.json")
}

/// Inclusive byte range for a streamed read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    /// Starting offset.
    pub offset: u64,
    /// Number of bytes; `None` means to end of object.
    pub length: Option<u64>,
}

/// One page of [`ObjectInfo`] from [`StorageBackend::list_page`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListPage {
    /// Objects in this page.
    pub objects: Vec<ObjectInfo>,
    /// Continuation token; `None` when this is the last page.
    pub next_cursor: Option<String>,
}

/// Result of [`StorageBackend::put_stream`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutStreamResult {
    /// Bytes accepted from the body stream.
    pub bytes_written: u64,
    /// Backend etag when available. Opaque; not a content checksum.
    pub etag: Option<String>,
    /// Hex SHA-256 of the bytes accepted, when the backend computed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_hex: Option<String>,
}

/// Pluggable storage for acquired audio and sidecar files.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Backend name for logs (`local`, `s3`).
    fn name(&self) -> &'static str;

    /// Stable identity of this authorized namespace (root, bucket, prefix, session).
    ///
    /// [`Self::supports_server_copy`] plus equal ids is the only signal that
    /// [`Self::copy`] stays inside one backend. Comparing [`Self::name`] is not
    /// enough.
    fn instance_id(&self) -> String {
        format!("anonymous:{}", self.name())
    }

    /// Host placement when this backend's bytes are not portable across nodes.
    ///
    /// `None` means a scan may resume on any host with the same
    /// [`Self::instance_id`] (object storage). `Some` is a stable host id.
    /// Wrappers and fan-out must forward a child's placement; they must not
    /// guess locality from an `instance_id` prefix. When placement cannot be
    /// proved, return a host id so the scan restarts on another node instead
    /// of adopting the wrong inventory.
    fn scan_placement(&self) -> Option<String> {
        None
    }

    /// Clone into a new boxed backend (same client / root).
    fn clone_box(&self) -> Box<dyn StorageBackend>;

    /// Write bytes under `key`.
    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()>;

    /// Stream a local file into storage.
    ///
    /// The default opens the file and calls [`Self::put_stream`]. It does not
    /// read the body into a `Bytes` buffer. Backends may override with a
    /// native copy that still avoids a userspace buffer of the whole object.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from the file or the backend write.
    async fn put_file(&self, key: &str, path: &Path, mut meta: ObjectMeta) -> Result<()> {
        let file = tokio::fs::File::open(path).await?;
        if meta.content_length.is_none() {
            if let Ok(info) = file.metadata().await {
                meta.content_length = Some(info.len());
            }
        }
        self.put_stream(key, Box::pin(file), meta).await.map(|_| ())
    }

    /// Download the full object body into memory.
    ///
    /// # Errors
    ///
    /// Returns [`crate::StorageError::NotFound`] when missing, and I/O or S3
    /// failures otherwise.
    async fn get(&self, key: &str) -> Result<Bytes>;

    /// Return whether `key` exists without downloading the body.
    ///
    /// # Errors
    ///
    /// Propagates backend probe failures (not merely absence).
    async fn exists(&self, key: &str) -> Result<bool>;

    /// Compatibility collector for a namespace that fits in one page.
    ///
    /// Returns [`StorageError::PayloadTooLarge`] when another page exists so a
    /// product caller cannot rebuild a full inventory by accident. Scans use
    /// [`Self::list_page`].
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::PayloadTooLarge`] when the prefix holds more
    /// than one page, plus listing errors from [`Self::list_page`].
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let page = self
            .list_page(prefix, None, crate::bounded::MAX_LIST_PAGE)
            .await?;
        if page.next_cursor.is_some() {
            return Err(StorageError::PayloadTooLarge(
                "refusing to buffer a multi-page object inventory; use list_page".into(),
            ));
        }
        Ok(page.objects)
    }

    /// Audio objects under `prefix`, capped at one page of matches.
    ///
    /// See [`AUDIO_EXTENSIONS`]. Pages are discarded as they are scanned. More
    /// than [`crate::bounded::MAX_LIST_PAGE`] audio objects is an error.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::PayloadTooLarge`] when the audio match set
    /// exceeds one page, plus listing errors.
    async fn list_audio(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let cap = crate::bounded::MAX_LIST_PAGE as usize;
        let mut out = Vec::new();
        let mut cursor = None;
        let mut hops = 0u32;
        loop {
            hops = hops.saturating_add(1);
            if hops > 1_000_000 {
                return Err(StorageError::InvalidCursor(
                    "list_audio made no progress".into(),
                ));
            }
            let page = self
                .list_page(prefix, cursor.as_deref(), crate::bounded::MAX_LIST_PAGE)
                .await?;
            for obj in page.objects {
                if !is_audio_key(&obj.key) {
                    continue;
                }
                if out.len() >= cap {
                    return Err(StorageError::PayloadTooLarge(
                        "refusing to buffer more than one page of audio keys; use list_page".into(),
                    ));
                }
                out.push(obj);
            }
            match page.next_cursor {
                Some(next) if cursor.as_deref() == Some(next.as_str()) => {
                    return Err(StorageError::InvalidCursor(
                        "list_audio cursor did not advance".into(),
                    ));
                }
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    /// Probe object metadata without downloading the body.
    ///
    /// S3 uses `HeadObject` (user metadata). Local reads an optional
    /// `.bookclerk-meta.json` sidecar written on put.
    async fn probe(&self, key: &str) -> Result<ObjectProbe>;

    /// Copy `from` → `to` within the same backend (S3 server-side copy / local
    /// file copy). Preserves object metadata when the backend supports it.
    async fn copy(&self, from: &str, to: &str) -> Result<()>;

    /// Move `from` → `to` (copy then delete source).
    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        self.copy(from, to).await?;
        self.delete(from).await
    }

    /// Delete an object (no-op if missing).
    async fn delete(&self, key: &str) -> Result<()>;

    /// Probe metadata without downloading the body.
    ///
    /// Default maps [`Self::probe`] absence onto `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Propagates backend probe failures other than not-found.
    async fn head(&self, key: &str) -> Result<Option<ObjectProbe>> {
        match self.probe(key).await {
            Ok(probe) => Ok(Some(probe)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// One page of keys under `prefix`.
    ///
    /// `cursor` is opaque and scoped to this backend's ordering. A missing,
    /// stale, or non-advancing cursor returns [`StorageError::InvalidCursor`]
    /// and does not restart at the first page. `limit` is clamped to
    /// [`crate::bounded::MAX_LIST_PAGE`]. Ordering and mutation behavior are
    /// backend-specific; see `docs/storage-bounds.md`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidCursor`] or a backend listing error.
    async fn list_page(&self, prefix: &str, cursor: Option<&str>, limit: u32) -> Result<ListPage>;

    /// Streamed read. Never reassembles the object into host `Bytes`.
    ///
    /// [`ObjectProbe::size`] is the whole object size, not the returned slice.
    /// `range.length == Some(0)` is rejected. `None` means through EOF, matching
    /// the wire encoding where length `0` is "to end".
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] when missing, [`StorageError::InvalidKey`]
    /// for a bad range, and I/O or S3 failures otherwise.
    async fn get_stream(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(ObjectProbe, Pin<Box<dyn AsyncRead + Send>>)>;

    /// Streamed write. `body` ownership is transferred to the backend.
    ///
    /// # Errors
    ///
    /// Returns I/O or S3 failures from the sink.
    async fn put_stream(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> Result<PutStreamResult>;

    /// True when [`Self::copy`] stays inside this namespace without downloading.
    ///
    /// The default is false. A backend opts in only for copies that are valid
    /// for its own bucket, root, and prefix.
    fn supports_server_copy(&self) -> bool {
        false
    }

    /// Set filesystem timestamps (local) or best-effort logical timestamp tags (S3).
    ///
    /// Local backends update mtime/ctime. S3 backends must **not** CopyObject to
    /// rewrite user-metadata (creates a second full-size version on versioned
    /// buckets). System `Last-Modified` cannot be set on AWS S3; logical times
    /// belong in PutObject `x-amz-meta-*` (S3 backends set them at upload only).
    async fn touch_file(
        &self,
        key: &str,
        created: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<()> {
        let _ = (key, created, modified);
        Ok(())
    }
}
