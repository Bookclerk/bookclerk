use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;

use crate::error::Result;

/// Metadata attached to a stored object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectMeta {
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    /// Free-form ASIN / title tags for S3 object metadata.
    pub asin: Option<String>,
    pub title: Option<String>,
    /// Creation timestamp as RFC 3339 (S3 metadata `creation-time`).
    pub creation_time: Option<String>,
    /// Last-write timestamp as RFC 3339 (S3 metadata `last-write-time`).
    pub last_write_time: Option<String>,
}

/// Listing entry returned by [`StorageBackend::list`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
}

/// Cheap object probe (S3 `HeadObject` / local sidecar meta) — never downloads
/// object bodies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectProbe {
    pub key: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub meta: ObjectMeta,
}

/// Audio extensions considered liberated media for storage matching.
///
/// Includes default remux/encode outputs (`m4b` / `mp3` / `m4a`) plus plain
/// passthrough containers Chirp / GraphicAudio may store under noop/`as_is`
/// output (`flac` / `aac` / `ogg` / `oga`). Keep aligned with
/// `libation_source::media` sniffing and GraphicAudio ZIP audio filters.
pub const AUDIO_EXTENSIONS: &[&str] = &["m4b", "mp3", "m4a", "flac", "aac", "ogg", "oga"];

/// True when `key` ends with a known liberated audio extension.
#[must_use]
pub fn is_audio_key(key: &str) -> bool {
    let Some((_, ext)) = key.rsplit_once('.') else {
        return false;
    };
    AUDIO_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// Sidecar key for local probe metadata (`stem.libation-meta.json`).
#[must_use]
pub fn libation_meta_sidecar_key(audio_or_object_key: &str) -> String {
    let base = audio_or_object_key
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(audio_or_object_key);
    format!("{base}.libation-meta.json")
}

/// Pluggable storage for liberated audio and sidecar files.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Backend name for logs (`local`, `s3`).
    fn name(&self) -> &'static str;

    /// Write bytes under `key`.
    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()>;

    /// Stream a local file into storage (preferred for large audiobooks).
    ///
    /// Default implementation reads the whole file then calls [`put`].
    async fn put_file(&self, key: &str, path: &Path, meta: ObjectMeta) -> Result<()> {
        let data = tokio::fs::read(path).await?;
        let mut meta = meta;
        if meta.content_length.is_none() {
            meta.content_length = Some(data.len() as u64);
        }
        self.put(key, Bytes::from(data), meta).await
    }

    /// Read the full object.
    async fn get(&self, key: &str) -> Result<Bytes>;

    /// True when the object exists.
    async fn exists(&self, key: &str) -> Result<bool>;

    /// List objects under `prefix`.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>>;

    /// List liberated audio objects under `prefix`.
    ///
    /// See [`AUDIO_EXTENSIONS`] (includes plain Chirp/GA passthrough formats).
    async fn list_audio(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let all = self.list(prefix).await?;
        Ok(all.into_iter().filter(|o| is_audio_key(&o.key)).collect())
    }

    /// Probe object metadata without downloading the body.
    ///
    /// S3 uses `HeadObject` (user metadata). Local reads an optional
    /// `.libation-meta.json` sidecar written on put.
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
