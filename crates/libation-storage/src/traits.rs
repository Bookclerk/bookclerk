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
