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

/// Sidecar key for local probe metadata (`stem.bookclerk-meta.json`).
#[must_use]
pub fn bookclerk_meta_sidecar_key(audio_or_object_key: &str) -> String {
    let base = audio_or_object_key
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(audio_or_object_key);
    format!("{base}.bookclerk-meta.json")
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PutStreamResult {
    /// Bytes accepted from the body stream.
    pub bytes_written: u64,
    /// Backend etag when available.
    pub etag: Option<String>,
}

/// Pluggable storage for acquired audio and sidecar files.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Backend name for logs (`local`, `s3`).
    fn name(&self) -> &'static str;

    /// Clone into a new boxed backend (same client / root).
    fn clone_box(&self) -> Box<dyn StorageBackend>;

    /// Write bytes under `key`.
    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()>;

    /// Stream a local file into storage (preferred for large audiobooks).
    ///
    /// Default implementation reads the whole file then calls [`Self::put`].
    async fn put_file(&self, key: &str, path: &Path, meta: ObjectMeta) -> Result<()> {
        let data = tokio::fs::read(path).await?;
        let mut meta = meta;
        if meta.content_length.is_none() {
            meta.content_length = Some(data.len() as u64);
        }
        self.put(key, Bytes::from(data), meta).await
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

    /// List objects under `prefix`.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>>;

    /// List acquired audio objects under `prefix`.
    ///
    /// See [`AUDIO_EXTENSIONS`] (includes plain Chirp/GA passthrough formats).
    async fn list_audio(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let all = self.list(prefix).await?;
        Ok(all.into_iter().filter(|o| is_audio_key(&o.key)).collect())
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

    /// One page of keys under `prefix` (cursor is the last key or backend token).
    ///
    /// # Errors
    ///
    /// Returns listing failures from the backend.
    async fn list_page(&self, prefix: &str, cursor: Option<&str>, limit: u32) -> Result<ListPage>;

    /// Streamed read. Never reassembles the object into host `Bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] when missing, and I/O or S3 failures
    /// otherwise.
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

    /// True when [`Self::copy`] is a server-side operation (no download).
    fn supports_server_copy(&self) -> bool {
        true
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
