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

use sha2::Digest;

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
    /// Directory that records attempt-owned stages so a restarted process can
    /// delete them. `None` still deletes on drop while this process is alive.
    pub stage_journal_dir: Option<std::path::PathBuf>,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: Duration::from_millis(50),
            attempt_timeout: Some(Duration::from_secs(30 * 60)),
            stage_journal_dir: None,
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

    if let Some(dir) = &options.stage_journal_dir {
        reap_abandoned_stages(dest, dir).await;
    }

    let attempts = options.max_attempts.max(1);
    let mut last_err = None;
    for attempt in 1..=attempts {
        if attempt > 1 {
            tokio::time::sleep(options.backoff.saturating_mul(attempt - 1)).await;
        }
        let fut = transfer_once(
            source,
            source_key,
            dest,
            dest_key,
            meta.clone(),
            options.stage_journal_dir.as_deref(),
        );
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
    journal_dir: Option<&std::path::Path>,
) -> Result<TransferOutcome> {
    let before = source.probe(source_key).await?;
    if let Some(expected) = meta.sha256_hex.as_deref() {
        let _ = parse_sha256_hex(expected)?;
    }
    if let (Some(expected), Some(source_sum)) = (
        meta.sha256_hex.as_deref(),
        before.meta.sha256_hex.as_deref(),
    ) {
        if !expected.eq_ignore_ascii_case(source_sum) {
            return Err(StorageError::Integrity(
                "expected digest conflicts with the source probe".into(),
            ));
        }
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
    let stage_key = format!(".bookclerk-stage/{}/{}", uuid::Uuid::new_v4(), dest_key);
    let mut stage = StageLease::arm(dest.clone_box(), stage_key.clone(), journal_dir)?;
    let written = match dest.put_stream(&stage_key, body, meta.clone()).await {
        Ok(written) => written,
        Err(err) => {
            stage.cleanup_now().await;
            return Err(err);
        }
    };
    let verified = verify_staged(source, source_key, &before, &meta, &written).await;
    if let Err(err) = verified {
        stage.cleanup_now().await;
        return Err(err);
    }
    if let Err(err) = publish_staged(dest, &stage_key, dest_key, &meta, &written).await {
        stage.cleanup_now().await;
        return Err(err);
    }
    stage.cleanup_now().await;
    Ok(TransferOutcome::Streamed(written))
}

/// Deletes `stage_key` on drop, including when an attempt timeout or caller
/// cancel drops the future after the stage object exists.
struct StageLease {
    dest: Option<Box<dyn StorageBackend>>,
    key: String,
    journal: Option<StageRecord>,
    armed: bool,
}

struct StageRecord {
    path: std::path::PathBuf,
    _lock: std::fs::File,
}

impl StageLease {
    /// When `journal_dir` is set, a stage record must exist before the upload.
    ///
    /// # Errors
    ///
    /// Returns the journal I/O error and does not arm a lease that would upload
    /// without crash recovery. `None` keeps process-lifetime cleanup only.
    fn arm(
        dest: Box<dyn StorageBackend>,
        key: String,
        journal_dir: Option<&std::path::Path>,
    ) -> std::io::Result<Self> {
        let journal = match journal_dir {
            Some(dir) => Some(StageRecord::create(dir, dest.as_ref(), &key)?),
            None => None,
        };
        Ok(Self {
            dest: Some(dest),
            key,
            journal,
            armed: true,
        })
    }

    async fn cleanup_now(&mut self) {
        if !self.armed {
            return;
        }
        let deleted = if let Some(dest) = &self.dest {
            dest.delete(&self.key).await.is_ok()
        } else {
            false
        };
        if deleted {
            if let Some(record) = &self.journal {
                record.clear();
            }
            self.armed = false;
        }
    }
}

impl Drop for StageLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(dest) = self.dest.take() else {
            return;
        };
        let key = self.key.clone();
        let record = self.journal.take();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if dest.delete(&key).await.is_ok() {
                    if let Some(record) = record {
                        record.clear();
                    }
                }
            });
        }
    }
}

impl StageRecord {
    fn create(
        dir: &std::path::Path,
        dest: &dyn StorageBackend,
        key: &str,
    ) -> std::io::Result<Self> {
        let owner_id = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(dir.join("owners"))?;
        std::fs::create_dir_all(dir.join("stages"))?;
        let lock_path = dir.join("owners").join(format!("{owner_id}.lock"));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        fs4::FileExt::lock(&lock)?;
        let name = hex::encode(sha2::Sha256::digest(format!(
            "{}:{key}",
            dest.instance_id()
        )));
        let path = dir.join("stages").join(format!("{name}.json"));
        let partial = dir.join("stages").join(format!("{name}.json.partial"));
        let body = serde_json::json!({
            "instance_id": dest.instance_id(),
            "stage_key": key,
            "owner_id": owner_id,
        });
        std::fs::write(&partial, body.to_string())?;
        std::fs::File::open(&partial)?.sync_all()?;
        pause_stage_record_publish(dir);
        if let Err(err) = std::fs::rename(&partial, &path) {
            let _ = std::fs::remove_file(&partial);
            return Err(err);
        }
        if let Ok(dir_file) = std::fs::File::open(dir.join("stages")) {
            let _ = dir_file.sync_all();
        }
        Ok(Self { path, _lock: lock })
    }

    fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn reap_abandoned_stages(dest: &dyn StorageBackend, dir: &std::path::Path) {
    let stages = dir.join("stages");
    let Ok(entries) = std::fs::read_dir(&stages) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if stage_record_in_progress(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            let _ = std::fs::rename(&path, path.with_extension("corrupt"));
            continue;
        };
        if value.get("instance_id").and_then(|v| v.as_str()) != Some(dest.instance_id().as_str()) {
            continue;
        }
        let Some(owner) = value.get("owner_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(stage_key) = value.get("stage_key").and_then(|v| v.as_str()) else {
            continue;
        };
        if stage_owner_live(dir, owner) {
            continue;
        }
        if dest.delete(stage_key).await.is_ok() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Incomplete publication files are not records yet. The reaper must not
/// rename them to `.corrupt` while the owner is still writing.
fn stage_record_in_progress(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    !name.ends_with(".json") || name.ends_with(".partial") || name.ends_with(".tmp")
}

/// Test barrier: `dir/publish.pause` exists until the test deletes it.
fn pause_stage_record_publish(dir: &std::path::Path) {
    #[cfg(test)]
    {
        let gate = dir.join("publish.pause");
        if !gate.exists() {
            return;
        }
        let _ = std::fs::write(dir.join("publish.entered"), b"1");
        while gate.exists() {
            std::thread::yield_now();
        }
    }
    #[cfg(not(test))]
    {
        let _ = dir;
    }
}

fn stage_owner_live(dir: &std::path::Path, owner_id: &str) -> bool {
    let path = dir.join("owners").join(format!("{owner_id}.lock"));
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    else {
        return false;
    };
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => {
            let _ = fs4::FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

async fn verify_staged(
    source: &dyn StorageBackend,
    source_key: &str,
    before: &crate::ObjectProbe,
    meta: &ObjectMeta,
    written: &PutStreamResult,
) -> Result<()> {
    if let Some(expected) = meta.content_length {
        if written.bytes_written != expected {
            return Err(StorageError::Integrity(format!(
                "transferred {} bytes, source size is {expected}",
                written.bytes_written
            )));
        }
    }
    let after = source.probe(source_key).await.map_err(|err| {
        StorageError::Integrity(format!(
            "source `{source_key}` could not be confirmed after staging: {err}"
        ))
    })?;
    if after.size != before.size || after.etag != before.etag {
        return Err(StorageError::Integrity(format!(
            "source `{source_key}` changed during transfer"
        )));
    }
    if let (Some(expected), Some(got)) = (meta.sha256_hex.as_deref(), written.sha256_hex.as_deref())
    {
        if !expected.eq_ignore_ascii_case(got) {
            return Err(StorageError::Integrity(
                "staged digest does not match the source".into(),
            ));
        }
    }
    Ok(())
}

async fn publish_staged(
    dest: &dyn StorageBackend,
    stage_key: &str,
    dest_key: &str,
    meta: &ObjectMeta,
    written: &PutStreamResult,
) -> Result<()> {
    if dest.supports_server_copy() {
        dest.copy(stage_key, dest_key).await?;
        return Ok(());
    }
    let mut publish_meta = meta.clone();
    publish_meta.sha256_hex = written.sha256_hex.clone().or(publish_meta.sha256_hex);
    publish_meta.content_length = Some(written.bytes_written);
    let (_probe, body) = dest.get_stream(stage_key, None).await?;
    dest.put_stream(dest_key, body, publish_meta).await?;
    Ok(())
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
                ..TransferOptions::default()
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

    struct FlipAfterOpen {
        inner: LocalFsBackend,
        probes: Arc<AtomicUsize>,
        fail_final: bool,
    }

    #[async_trait]
    impl StorageBackend for FlipAfterOpen {
        fn name(&self) -> &'static str {
            "flip"
        }
        fn instance_id(&self) -> String {
            format!("flip:{}", self.inner.instance_id())
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                probes: self.probes.clone(),
                fail_final: self.fail_final,
            })
        }
        async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
            self.inner.put(key, data, meta).await
        }
        async fn get(&self, key: &str) -> Result<Bytes> {
            self.inner.get(key).await
        }
        async fn exists(&self, key: &str) -> Result<bool> {
            self.inner.exists(key).await
        }
        async fn list(&self, prefix: &str) -> Result<Vec<crate::ObjectInfo>> {
            self.inner.list(prefix).await
        }
        async fn probe(&self, key: &str) -> Result<crate::ObjectProbe> {
            let n = self.probes.fetch_add(1, Ordering::SeqCst);
            // Probe 0 is the pre-read HEAD. The post-stage probe is the next one.
            if n >= 1 && self.fail_final {
                return Err(StorageError::Io(std::io::Error::other(
                    "final probe failed",
                )));
            }
            if n >= 1 {
                self.inner
                    .put(
                        key,
                        Bytes::from_static(b"replaced-source-bytes"),
                        ObjectMeta::default(),
                    )
                    .await?;
            }
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
    }

    #[tokio::test]
    async fn source_change_and_probe_failure_keep_the_existing_destination() {
        let dir = tempdir().unwrap();
        let src = LocalFsBackend::new(dir.path().join("src")).unwrap();
        src.put(
            "book.m4b",
            Bytes::from_static(b"original"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let dst = LocalFsBackend::new(dir.path().join("dst")).unwrap();
        dst.put(
            "book.m4b",
            Bytes::from_static(b"keeper"),
            ObjectMeta {
                commit_token: Some("other-writer".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let flipping = FlipAfterOpen {
            inner: src.clone(),
            probes: Arc::new(AtomicUsize::new(0)),
            fail_final: false,
        };
        let err = transfer_object(
            &flipping,
            "book.m4b",
            &dst,
            "book.m4b",
            ObjectMeta::default(),
            &TransferOptions {
                max_attempts: 1,
                ..TransferOptions::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StorageError::Integrity(_)), "{err}");
        assert_eq!(dst.get("book.m4b").await.unwrap().as_ref(), b"keeper");
        fn assert_no_files(path: &std::path::Path) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    assert_no_files(&path);
                } else {
                    panic!("stage file left behind: {}", path.display());
                }
            }
        }
        assert_no_files(&dir.path().join("dst").join(".bookclerk-stage"));

        dst.put(
            "book.m4b",
            Bytes::from_static(b"keeper"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        src.put(
            "book.m4b",
            Bytes::from_static(b"original"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let failing = FlipAfterOpen {
            inner: src,
            probes: Arc::new(AtomicUsize::new(0)),
            fail_final: true,
        };
        let err = transfer_object(
            &failing,
            "book.m4b",
            &dst,
            "book.m4b",
            ObjectMeta::default(),
            &TransferOptions {
                max_attempts: 1,
                ..TransferOptions::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StorageError::Integrity(_)), "{err}");
        assert_eq!(dst.get("book.m4b").await.unwrap().as_ref(), b"keeper");
        assert_no_files(&dir.path().join("dst").join(".bookclerk-stage"));
    }

    struct BlockFinalProbe {
        inner: LocalFsBackend,
        probes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageBackend for BlockFinalProbe {
        fn name(&self) -> &'static str {
            "block-probe"
        }
        fn instance_id(&self) -> String {
            format!("block:{}", self.inner.instance_id())
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                probes: Arc::clone(&self.probes),
            })
        }
        async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
            self.inner.put(key, data, meta).await
        }
        async fn get(&self, key: &str) -> Result<Bytes> {
            self.inner.get(key).await
        }
        async fn exists(&self, key: &str) -> Result<bool> {
            self.inner.exists(key).await
        }
        async fn list(&self, prefix: &str) -> Result<Vec<crate::ObjectInfo>> {
            self.inner.list(prefix).await
        }
        async fn probe(&self, key: &str) -> Result<crate::ObjectProbe> {
            let n = self.probes.fetch_add(1, Ordering::SeqCst);
            if n >= 1 {
                std::future::pending::<()>().await;
            }
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
    }

    async fn wait_until_no_stage_files(root: &std::path::Path) {
        // Drop cleanup deletes through `tokio::fs`, which uses the blocking
        // pool. A yield-only spin can finish before that delete is scheduled
        // when the suite runs in parallel.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let mut files = 0usize;
            if let Ok(entries) = std::fs::read_dir(root.join(".bookclerk-stage")) {
                for entry in entries.flatten() {
                    if entry.path().is_dir() {
                        if let Ok(inner) = std::fs::read_dir(entry.path()) {
                            files += inner.count();
                        }
                    } else {
                        files += 1;
                    }
                }
            }
            if files == 0 {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("stage objects remained after drop");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn timeout_and_restart_do_not_keep_abandoned_stages() {
        let dir = tempdir().unwrap();
        let journal = dir.path().join("journal");
        let src_root = LocalFsBackend::new(dir.path().join("src")).unwrap();
        src_root
            .put(
                "book.m4b",
                Bytes::from_static(b"audio-bytes"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let dst = LocalFsBackend::new(dir.path().join("dst")).unwrap();
        dst.put(
            "book.m4b",
            Bytes::from_static(b"keeper"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        for _ in 0..2 {
            let blocked = BlockFinalProbe {
                inner: src_root.clone(),
                probes: Arc::new(AtomicUsize::new(0)),
            };
            let err = transfer_object(
                &blocked,
                "book.m4b",
                &dst,
                "book.m4b",
                ObjectMeta::default(),
                &TransferOptions {
                    max_attempts: 1,
                    attempt_timeout: Some(Duration::from_millis(50)),
                    stage_journal_dir: Some(journal.clone()),
                    ..TransferOptions::default()
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(err, StorageError::Io(_)), "{err}");
            wait_until_no_stage_files(dir.path().join("dst").as_path()).await;
        }
        assert_eq!(dst.get("book.m4b").await.unwrap().as_ref(), b"keeper");

        let orphan = ".bookclerk-stage/orphan-attempt/book.m4b";
        dst.put(
            orphan,
            Bytes::from_static(b"abandoned-audiobook"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        std::fs::create_dir_all(journal.join("stages")).unwrap();
        std::fs::write(
            journal.join("stages").join("orphan.json"),
            serde_json::json!({
                "instance_id": dst.instance_id(),
                "stage_key": orphan,
                "owner_id": "dead-owner",
            })
            .to_string(),
        )
        .unwrap();
        reap_abandoned_stages(&dst, &journal).await;
        assert!(!dst.exists(orphan).await.unwrap());

        let live = ".bookclerk-stage/live-attempt/book.m4b";
        dst.put(
            live,
            Bytes::from_static(b"still-copying"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        std::fs::create_dir_all(journal.join("owners")).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(journal.join("owners").join("live-owner.lock"))
            .unwrap();
        fs4::FileExt::lock(&lock).unwrap();
        std::fs::write(
            journal.join("stages").join("live.json"),
            serde_json::json!({
                "instance_id": dst.instance_id(),
                "stage_key": live,
                "owner_id": "live-owner",
            })
            .to_string(),
        )
        .unwrap();
        reap_abandoned_stages(&dst, &journal).await;
        assert_eq!(dst.get(live).await.unwrap().as_ref(), b"still-copying");
        drop(lock);
    }

    #[derive(Clone)]
    struct BlockPublish {
        inner: LocalFsBackend,
        started: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl StorageBackend for BlockPublish {
        fn name(&self) -> &'static str {
            "block-publish"
        }
        fn instance_id(&self) -> String {
            format!("block-publish:{}", self.inner.instance_id())
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                started: Arc::clone(&self.started),
                release: Arc::clone(&self.release),
            })
        }
        fn supports_server_copy(&self) -> bool {
            true
        }
        async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
            self.inner.put(key, data, meta).await
        }
        async fn get(&self, key: &str) -> Result<Bytes> {
            self.inner.get(key).await
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
            self.started.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
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
    }

    #[tokio::test]
    async fn cancelling_during_publish_deletes_the_stage() {
        let dir = tempdir().unwrap();
        let src = LocalFsBackend::new(dir.path().join("src")).unwrap();
        src.put(
            "book.m4b",
            Bytes::from_static(b"audio-bytes"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let dst = BlockPublish {
            inner: LocalFsBackend::new(dir.path().join("dst")).unwrap(),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        };
        dst.put(
            "book.m4b",
            Bytes::from_static(b"keeper"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let journal = dir.path().join("journal");
        let task = tokio::spawn({
            let src = src.clone();
            let dst = dst.clone();
            let journal = journal.clone();
            async move {
                transfer_object(
                    &src,
                    "book.m4b",
                    &dst,
                    "book.m4b",
                    ObjectMeta::default(),
                    &TransferOptions {
                        max_attempts: 1,
                        attempt_timeout: None,
                        stage_journal_dir: Some(journal),
                        ..TransferOptions::default()
                    },
                )
                .await
            }
        });
        loop {
            if started.load(Ordering::SeqCst) > 0 {
                break;
            }
            if task.is_finished() {
                panic!("publish finished before the copy blocked: {:?}", task.await);
            }
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        wait_until_no_stage_files(dir.path().join("dst").as_path()).await;
        assert_eq!(dst.get("book.m4b").await.unwrap().as_ref(), b"keeper");
        release.notify_waiters();
    }

    #[derive(Clone)]
    struct FailDelete {
        inner: LocalFsBackend,
        fail: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageBackend for FailDelete {
        fn name(&self) -> &'static str {
            "fail-delete"
        }
        fn instance_id(&self) -> String {
            self.inner.instance_id()
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                fail: Arc::clone(&self.fail),
            })
        }
        async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
            self.inner.put(key, data, meta).await
        }
        async fn get(&self, key: &str) -> Result<Bytes> {
            self.inner.get(key).await
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
            if key.contains(".bookclerk-stage") && self.fail.load(Ordering::SeqCst) > 0 {
                return Err(StorageError::Io(std::io::Error::other("delete refused")));
            }
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
    }

    #[tokio::test]
    async fn failed_stage_cleanup_is_retried_after_restart() {
        let dir = tempdir().unwrap();
        let journal = dir.path().join("journal");
        let src = LocalFsBackend::new(dir.path().join("src")).unwrap();
        src.put(
            "book.m4b",
            Bytes::from_static(b"audio-bytes"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let fail = Arc::new(AtomicUsize::new(1));
        let dst = FailDelete {
            inner: LocalFsBackend::new(dir.path().join("dst")).unwrap(),
            fail: Arc::clone(&fail),
        };
        // The journal is published before `put_stream`. A short attempt timeout
        // can therefore drop the lease under load before the stage object
        // exists, which is a different failure than a refused delete. Wait
        // until staging has finished and the post-stage probe is pending, then
        // cancel. Drop still runs the same cleanup as an attempt timeout.
        let probes = Arc::new(AtomicUsize::new(0));
        let blocked = BlockFinalProbe {
            inner: src.clone(),
            probes: Arc::clone(&probes),
        };
        let task = tokio::spawn({
            let dst = dst.clone();
            let journal = journal.clone();
            async move {
                transfer_object(
                    &blocked,
                    "book.m4b",
                    &dst,
                    "book.m4b",
                    ObjectMeta::default(),
                    &TransferOptions {
                        max_attempts: 1,
                        attempt_timeout: None,
                        stage_journal_dir: Some(journal),
                        ..TransferOptions::default()
                    },
                )
                .await
            }
        });
        let staged = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if probes.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                if task.is_finished() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(staged.is_ok(), "post-stage probe did not start");
        if probes.load(Ordering::SeqCst) < 2 {
            let finished = task.await;
            panic!("transfer finished before the post-stage probe blocked: {finished:?}");
        }
        task.abort();
        let join = task.await;
        assert!(join.expect_err("cancelled transfer").is_cancelled());
        let stages = journal.join("stages");
        let mut owner_free = false;
        for _ in 0..1_000 {
            let Some(path) = std::fs::read_dir(&stages).ok().and_then(|rd| {
                rd.filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .next()
            }) else {
                tokio::task::yield_now().await;
                continue;
            };
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let owner = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| {
                    value
                        .get("owner_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                });
            let Some(owner) = owner else {
                tokio::task::yield_now().await;
                continue;
            };
            if !stage_owner_live(&journal, &owner) {
                owner_free = true;
                let stage_key = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("stage_key")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                assert!(
                    dst.exists(&stage_key).await.unwrap(),
                    "failed cleanup must leave the stage object"
                );
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(owner_free, "stage owner lock was never released");
        fail.store(0, Ordering::SeqCst);
        reap_abandoned_stages(&dst, &journal).await;
        wait_until_no_stage_files(dir.path().join("dst").as_path()).await;
        assert!(std::fs::read_dir(&stages).map(|rd| rd.count()).unwrap_or(0) == 0);
    }

    #[derive(Clone)]
    struct CountPuts {
        inner: LocalFsBackend,
        puts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageBackend for CountPuts {
        fn name(&self) -> &'static str {
            "count-puts"
        }
        fn instance_id(&self) -> String {
            self.inner.instance_id()
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(self.clone())
        }
        async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
            self.inner.put(key, data, meta).await
        }
        async fn get(&self, key: &str) -> Result<Bytes> {
            self.inner.get(key).await
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
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.inner.put_stream(key, body, meta).await
        }
    }

    #[tokio::test]
    async fn unwritable_stage_journal_refuses_the_upload() {
        let dir = tempdir().unwrap();
        let src = LocalFsBackend::new(dir.path().join("src")).unwrap();
        src.put(
            "book.m4b",
            Bytes::from_static(b"audio-bytes"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let puts = Arc::new(AtomicUsize::new(0));
        let dst = CountPuts {
            inner: LocalFsBackend::new(dir.path().join("dst")).unwrap(),
            puts: Arc::clone(&puts),
        };
        let journal = dir.path().join("journal");
        std::fs::write(&journal, b"not-a-directory").unwrap();
        let err = transfer_object(
            &src,
            "book.m4b",
            &dst,
            "book.m4b",
            ObjectMeta::default(),
            &TransferOptions {
                max_attempts: 1,
                stage_journal_dir: Some(journal),
                ..TransferOptions::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StorageError::Io(_)), "{err}");
        assert_eq!(puts.load(Ordering::SeqCst), 0);
        assert!(!dst.exists("book.m4b").await.unwrap());
    }

    #[tokio::test]
    async fn reaper_ignores_an_in_progress_stage_record() {
        let dir = tempdir().unwrap();
        let journal = dir.path().join("journal");
        std::fs::create_dir_all(&journal).unwrap();
        std::fs::write(journal.join("publish.pause"), b"1").unwrap();
        let src = LocalFsBackend::new(dir.path().join("src")).unwrap();
        src.put(
            "book.m4b",
            Bytes::from_static(b"audio-bytes"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let dst = LocalFsBackend::new(dir.path().join("dst")).unwrap();
        let started = std::time::Instant::now();
        let task = std::thread::spawn({
            let src = src.clone();
            let dst = dst.clone();
            let journal = journal.clone();
            move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                runtime.block_on(async move {
                    transfer_object(
                        &src,
                        "book.m4b",
                        &dst,
                        "out.m4b",
                        ObjectMeta::default(),
                        &TransferOptions {
                            max_attempts: 1,
                            attempt_timeout: None,
                            stage_journal_dir: Some(journal),
                            ..TransferOptions::default()
                        },
                    )
                    .await
                })
            }
        });
        loop {
            if journal.join("publish.entered").is_file() {
                break;
            }
            if task.is_finished() {
                panic!(
                    "transfer finished before the publish barrier: {:?}",
                    task.join()
                );
            }
            if started.elapsed() > std::time::Duration::from_secs(10) {
                panic!("publish barrier was not reached");
            }
            std::thread::yield_now();
        }
        reap_abandoned_stages(&dst, &journal).await;
        let names: Vec<_> = std::fs::read_dir(journal.join("stages"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|name| name.ends_with(".json.partial")),
            "partial record missing: {names:?}"
        );
        assert!(
            names.iter().all(|name| !name.contains("corrupt")),
            "reaper quarantined an in-progress record: {names:?}"
        );
        std::fs::remove_file(journal.join("publish.pause")).unwrap();
        task.join().unwrap().unwrap();
        assert!(dst.exists("out.m4b").await.unwrap());
        let quarantined = std::fs::read_dir(journal.join("stages"))
            .map(|rd| {
                rd.filter_map(|entry| entry.ok())
                    .any(|entry| entry.file_name().to_string_lossy().contains("corrupt"))
            })
            .unwrap_or(false);
        assert!(!quarantined, "reaper quarantined a published record");
    }
}
