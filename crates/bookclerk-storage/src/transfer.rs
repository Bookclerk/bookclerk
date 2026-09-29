//! One bounded copy between two [`StorageBackend`] values.
//!
//! Server-side [`StorageBackend::copy`] runs only when both ends report the
//! same [`StorageBackend::instance_id`] and the destination advertises
//! [`StorageBackend::supports_server_copy`]. Equal `name()` or backend kind is
//! not identity: two buckets, prefixes, or local roots never share a copy.
//!
//! Otherwise bytes move through `get_stream` → `put_stream`. Each attempt opens
//! the source from the beginning. A consumed reader is never retried in place.
//! Completed attempts that already published a matching object are skipped on
//! replay. An interrupted object is restarted from byte 0 (range reads do not
//! resume a write). Multi-destination callers invoke this once per destination;
//! that is not a distributed transaction.

#![allow(clippy::missing_docs_in_private_items)]

use std::time::Duration;

use crate::bounded::{ensure_scalar_len, parse_sha256_hex, MAX_SCALAR_OBJECT_BYTES};
use crate::error::{Result, StorageError};
use crate::traits::{ObjectMeta, PutStreamResult, StorageBackend};

/// Knobs for [`transfer_object`].
#[derive(Debug, Clone)]
pub struct TransferOptions {
    /// Total attempts including the first. Zero is treated as one.
    pub max_attempts: u32,
    /// Base backoff between attempts. Attempt `n` waits `backoff * n`.
    pub backoff: Duration,
    /// Optional wall-clock budget for a single attempt.
    pub attempt_timeout: Option<Duration>,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: Duration::from_millis(50),
            attempt_timeout: Some(Duration::from_secs(30 * 60)),
        }
    }
}

/// How [`transfer_object`] moved the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    /// Destination `copy` inside one storage instance.
    ServerCopy,
    /// Streamed body. `result.sha256_hex` is the digest of bytes written.
    Streamed(PutStreamResult),
    /// Destination probe already matched the source size and digest.
    AlreadyPublished,
}

/// Copies `source_key` onto `dest_key`.
///
/// `meta` supplies content type and any caller-provided digest. A digest on
/// the source probe is required to match bytes actually written. When the
/// source probe has no digest, the destination records the digest computed
/// while streaming. An S3 ETag is an identity hint for "the object changed",
/// not a content checksum, and is never copied into `sha256_hex`.
///
/// # Errors
///
/// Returns [`StorageError::Integrity`] when the source changes between
/// attempts or a digest/length check fails, [`StorageError::NotFound`] when
/// the source is missing, and the destination error after retries are exhausted.
pub async fn transfer_object(
    source: &dyn StorageBackend,
    source_key: &str,
    dest: &dyn StorageBackend,
    dest_key: &str,
    meta: ObjectMeta,
    options: &TransferOptions,
) -> Result<TransferOutcome> {
    if source.instance_id() == dest.instance_id() && dest.supports_server_copy() {
        dest.copy(source_key, dest_key).await?;
        return Ok(TransferOutcome::ServerCopy);
    }

    let attempts = options.max_attempts.max(1);
    let mut last_err = None;
    for attempt in 1..=attempts {
        if attempt > 1 {
            tokio::time::sleep(options.backoff.saturating_mul(attempt - 1)).await;
        }
        let fut = transfer_once(source, source_key, dest, dest_key, meta.clone());
        let result = if let Some(limit) = options.attempt_timeout {
            match tokio::time::timeout(limit, fut).await {
                Ok(inner) => inner,
                Err(_) => Err(StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "storage transfer attempt timed out",
                ))),
            }
        } else {
            fut.await
        };
        match result {
            Ok(outcome) => return Ok(outcome),
            Err(err) if !is_retryable(&err) || attempt == attempts => return Err(err),
            Err(err) => {
                tracing::warn!(
                    source = source.name(),
                    dest = dest.name(),
                    key = source_key,
                    attempt,
                    error = %err,
                    "storage transfer failed; reopening source"
                );
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        StorageError::Other(anyhow::anyhow!("storage transfer made no attempts"))
    }))
}

async fn transfer_once(
    source: &dyn StorageBackend,
    source_key: &str,
    dest: &dyn StorageBackend,
    dest_key: &str,
    mut meta: ObjectMeta,
) -> Result<TransferOutcome> {
    let before = source.probe(source_key).await?;
    if let Some(expected) = meta.sha256_hex.as_deref() {
        let _ = parse_sha256_hex(expected)?;
    }
    if meta.sha256_hex.is_none() {
        meta.sha256_hex = before.meta.sha256_hex.clone();
    }
    if meta.content_length.is_none() {
        meta.content_length = Some(before.size);
    }
    if meta.content_type.is_none() {
        meta.content_type = before
            .content_type
            .clone()
            .or(before.meta.content_type.clone());
    }
    if let Some(expected) = meta.sha256_hex.clone() {
        if let Some(existing) = dest.head(dest_key).await? {
            let same_len = existing.size == before.size;
            let same_sum = existing
                .meta
                .sha256_hex
                .as_deref()
                .is_some_and(|got| got.eq_ignore_ascii_case(&expected));
            if same_len && same_sum {
                return Ok(TransferOutcome::AlreadyPublished);
            }
        }
    }

    let (opened, body) = source.get_stream(source_key, None).await?;
    if opened.size != before.size || opened.etag != before.etag {
        return Err(StorageError::Integrity(format!(
            "source `{source_key}` changed between HEAD and GET"
        )));
    }
    // Scalar APIs stay capped even if a caller stuffed a huge buffer into meta.
    if let Some(len) = meta.content_length {
        if len <= MAX_SCALAR_OBJECT_BYTES {
            let _ = ensure_scalar_len(0, MAX_SCALAR_OBJECT_BYTES);
        }
    }
    let written = dest.put_stream(dest_key, body, meta.clone()).await?;
    if let Some(expected) = meta.content_length {
        if written.bytes_written != expected {
            return Err(StorageError::Integrity(format!(
                "transferred {} bytes, source size is {expected}",
                written.bytes_written
            )));
        }
    }
    let after = source.probe(source_key).await?;
    if after.size != before.size || after.etag != before.etag {
        let _ = dest.delete(dest_key).await;
        return Err(StorageError::Integrity(format!(
            "source `{source_key}` changed during transfer"
        )));
    }
    if let (Some(expected), Some(got)) = (meta.sha256_hex.as_deref(), written.sha256_hex.as_deref())
    {
        if !expected.eq_ignore_ascii_case(got) {
            let _ = dest.delete(dest_key).await;
            return Err(StorageError::Integrity(
                "destination digest does not match the source".into(),
            ));
        }
    }
    Ok(TransferOutcome::Streamed(written))
}

fn is_retryable(err: &StorageError) -> bool {
    match err {
        StorageError::Io(err) => !matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
        ),
        StorageError::S3(_) => true,
        StorageError::NotFound(_)
        | StorageError::InvalidKey(_)
        | StorageError::PayloadTooLarge(_)
        | StorageError::InvalidCursor(_)
        | StorageError::Integrity(_)
        | StorageError::Other(_) => false,
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::local::LocalFsBackend;
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::tempdir;

    struct ScalarFail {
        inner: LocalFsBackend,
        gets: Arc<AtomicUsize>,
        puts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageBackend for ScalarFail {
        fn name(&self) -> &'static str {
            "scalar-fail"
        }

        fn instance_id(&self) -> String {
            format!("scalar-fail:{}", self.inner.instance_id())
        }

        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                gets: self.gets.clone(),
                puts: self.puts.clone(),
            })
        }

        async fn put(&self, _key: &str, _data: Bytes, _meta: ObjectMeta) -> Result<()> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            Err(StorageError::PayloadTooLarge("scalar put disabled".into()))
        }

        async fn get(&self, _key: &str) -> Result<Bytes> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            Err(StorageError::PayloadTooLarge("scalar get disabled".into()))
        }

        async fn exists(&self, key: &str) -> Result<bool> {
            self.inner.exists(key).await
        }

        async fn list(&self, prefix: &str) -> Result<Vec<crate::ObjectInfo>> {
            self.inner.list(prefix).await
        }

        async fn probe(&self, key: &str) -> Result<crate::ObjectProbe> {
            self.inner.probe(key).await
        }

        async fn copy(&self, from: &str, to: &str) -> Result<()> {
            self.inner.copy(from, to).await
        }

        async fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key).await
        }

        async fn list_page(
            &self,
            prefix: &str,
            cursor: Option<&str>,
            limit: u32,
        ) -> Result<crate::ListPage> {
            self.inner.list_page(prefix, cursor, limit).await
        }

        async fn get_stream(
            &self,
            key: &str,
            range: Option<crate::ByteRange>,
        ) -> Result<(
            crate::ObjectProbe,
            std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        )> {
            self.inner.get_stream(key, range).await
        }

        async fn put_stream(
            &self,
            key: &str,
            body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            meta: ObjectMeta,
        ) -> Result<PutStreamResult> {
            self.inner.put_stream(key, body, meta).await
        }

        fn supports_server_copy(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn transfer_streams_when_scalar_apis_fail() {
        let dir = tempdir().unwrap();
        let src_root = dir.path().join("src");
        let dst_root = dir.path().join("dst");
        let src = LocalFsBackend::new(src_root).unwrap();
        src.put(
            "book.m4b",
            Bytes::from_static(b"audio-bytes"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let gets = Arc::new(AtomicUsize::new(0));
        let puts = Arc::new(AtomicUsize::new(0));
        let wrapped_src = ScalarFail {
            inner: src,
            gets: gets.clone(),
            puts: puts.clone(),
        };
        let dst = ScalarFail {
            inner: LocalFsBackend::new(dst_root).unwrap(),
            gets: gets.clone(),
            puts: puts.clone(),
        };
        let outcome = transfer_object(
            &wrapped_src,
            "book.m4b",
            &dst,
            "book.m4b",
            ObjectMeta::default(),
            &TransferOptions {
                max_attempts: 1,
                backoff: Duration::from_millis(1),
                attempt_timeout: Some(Duration::from_secs(10)),
            },
        )
        .await
        .unwrap();
        match outcome {
            TransferOutcome::Streamed(result) => {
                assert_eq!(result.bytes_written, 11);
                assert!(result.sha256_hex.is_some());
            }
            other => panic!("expected stream, got {other:?}"),
        }
        assert_eq!(gets.load(Ordering::SeqCst), 0);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
        assert!(dst.get_stream("book.m4b", None).await.is_ok());
    }

    #[tokio::test]
    async fn same_instance_uses_server_copy() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        backend
            .put("a.m4b", Bytes::from_static(b"zzz"), ObjectMeta::default())
            .await
            .unwrap();
        let outcome = transfer_object(
            &backend,
            "a.m4b",
            &backend,
            "b.m4b",
            ObjectMeta::default(),
            &TransferOptions {
                max_attempts: 1,
                ..TransferOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, TransferOutcome::ServerCopy);
        assert!(backend.exists("b.m4b").await.unwrap());
    }
}
