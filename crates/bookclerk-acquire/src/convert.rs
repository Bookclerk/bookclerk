//! Convert acquired m4b/m4a to mp3 (classic LibationCli: `convert`).

#![allow(clippy::missing_docs_in_private_items)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bookclerk_library::{AcquireStatus, BookRecord, LibraryStore};
use bookclerk_media::encode_to_mp3;
use bookclerk_storage::{ObjectMeta, StorageBackend};

use crate::error::{AcquireError, Result};
use crate::naming::swap_audio_extension;

/// Options for [`convert_book`].
#[derive(Debug, Clone)]
pub struct ConvertRequest {
    /// Scratch directory for temporary acquire/convert files.
    pub cache_dir: PathBuf,
    /// When true, re-download or re-convert even if output exists.
    pub force: bool,
    /// LAME MP3 encoder settings from config.
    pub lame: bookclerk_config::LameConfig,
    /// Optional ceiling on output sample rate in Hz.
    pub max_sample_rate: Option<u32>,
    /// Durable job that owns the scratch reservation, when this convert is queued.
    pub job_id: Option<String>,
    /// Byte budget for this conversion's input and output files together.
    ///
    /// `None` does not impose a quota. The advertised source size is not the quota:
    /// bytes are counted as they are written, and encoder output is counted before
    /// the MP3 is published.
    pub temp_quota_bytes: Option<u64>,
    /// Set when the caller wants the copy loop to stop.
    pub cancel: Option<Arc<AtomicBool>>,
}

/// Summary of a batch convert run.
#[derive(Debug, Clone, Default)]
pub struct ConvertSummary {
    /// Titles successfully converted in this run.
    pub converted: u32,
    /// Titles skipped (already done or ineligible).
    pub skipped: u32,
    /// Titles that failed conversion or matching.
    pub failed: u32,
}

/// Convert one acquired m4b/m4a to mp3 and update the library storage key.
///
/// # Arguments
///
/// * `library` - Library store used to update acquire status / storage key.
/// * `storage` - Object storage backend holding the source and destination objects.
/// * `book` - Acquired book row whose `storage_key` points at m4b/m4a.
/// * `req` - Cache directory, force flag, and LAME settings.
///
/// # Returns
///
/// Object-storage key of the written MP3.
///
/// # Errors
///
/// Returns [`AcquireError`] when the source is missing/ineligible, encode fails,
/// or library/storage updates fail.
pub async fn convert_book(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    book: &BookRecord,
    req: &ConvertRequest,
) -> Result<String> {
    let title_id = book.title_id();
    let key = book
        .storage_key
        .as_ref()
        .ok_or_else(|| AcquireError::Other(anyhow::anyhow!("{title_id}: no storage_key")))?;
    let ext = key.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(ext.as_str(), "m4b" | "m4a") {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "{title_id}: not an m4b/m4a file ({ext})",
        )));
    }

    let mp3_key = swap_audio_extension(key, "mp3");
    if !req.force && storage.exists(&mp3_key).await? {
        library
            .set_acquire_status(
                book.title_id(),
                &book.account_id,
                AcquireStatus::Acquired,
                Some(&mp3_key),
                None,
            )
            .await?;
        return Ok(mp3_key);
    }

    let file_id = book.asin_or_isbn();
    let work_dir = req.cache_dir.join("convert").join(format!(
        "{file_id}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let lame = req.lame.clone();
    let max_sample_rate = req.max_sample_rate;
    convert_with_encoder(
        library,
        storage,
        book,
        req,
        &work_dir,
        &mp3_key,
        |input, output, allowance| async move {
            encode_to_mp3(&input, &output, &lame, max_sample_rate, Some(allowance))
                .await
                .map(|_| ())
                .map_err(|err| err.to_string())
        },
    )
    .await
}

/// Streams conversion scratch under `work_dir` using `encode` for the output file.
pub(crate) async fn convert_with_encoder<F, Fut>(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    book: &BookRecord,
    req: &ConvertRequest,
    work_dir: &Path,
    mp3_key: &str,
    encode: F,
) -> Result<String>
where
    F: FnOnce(PathBuf, PathBuf, u64) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(), String>>,
{
    let key = book.storage_key.as_deref().unwrap_or("");
    let ext = key.rsplit('.').next().unwrap_or("bin");
    let file_id = book.asin_or_isbn();
    tokio::fs::create_dir_all(work_dir).await?;
    let input = work_dir.join(format!("{file_id}.{ext}"));
    let output = work_dir.join(format!("{file_id}.mp3"));
    let mut scratch = ScratchDir::new(work_dir.to_path_buf());
    let mut reserved = false;

    let result = async {
        let (probe, mut body) = storage.get_stream(key, None).await?;
        let quota = req.temp_quota_bytes.unwrap_or(u64::MAX);
        if probe.size > quota {
            return Err(AcquireError::Other(anyhow::anyhow!(
                "convert source is {} bytes, quota is {quota}",
                probe.size
            )));
        }
        if let Some(job_id) = req.job_id.as_deref() {
            let reserve = if probe.size > 0 {
                probe.size.saturating_mul(2).min(quota)
            } else {
                quota
            };
            library
                .reserve_job_temp_path(job_id, &work_dir.display().to_string(), reserve, quota)
                .await
                .map_err(|err| AcquireError::Other(anyhow::anyhow!(err)))?;
            reserved = true;
        }
        let mut file = tokio::fs::File::create(&input).await?;
        let mut copied = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if req
                .cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::SeqCst))
            {
                return Err(AcquireError::Other(anyhow::anyhow!("convert cancelled")));
            }
            let n = tokio::io::AsyncReadExt::read(&mut body, &mut buf).await?;
            if n == 0 {
                break;
            }
            let next = copied.saturating_add(n as u64);
            if probe.size > 0 && next > probe.size {
                return Err(AcquireError::Storage(
                    bookclerk_storage::StorageError::Integrity(format!(
                        "convert read {next} bytes, source size is {}",
                        probe.size
                    )),
                ));
            }
            if next > quota {
                return Err(AcquireError::Other(anyhow::anyhow!(
                    "convert wrote {next} bytes, quota is {quota}"
                )));
            }
            tokio::io::AsyncWriteExt::write_all(&mut file, &buf[..n]).await?;
            copied = next;
        }
        file.sync_all().await?;
        drop(file);
        if probe.size > 0 && copied != probe.size {
            return Err(AcquireError::Storage(
                bookclerk_storage::StorageError::Integrity(format!(
                    "convert read {copied} bytes, source size is {}",
                    probe.size
                )),
            ));
        }
        if copied == 0 {
            return Err(AcquireError::Other(anyhow::anyhow!(
                "convert source `{key}` was empty"
            )));
        }
        let output_allowance = copied.min(quota.saturating_sub(copied));
        if output_allowance == 0 {
            return Err(AcquireError::Other(anyhow::anyhow!(
                "convert quota has no room for encoder output ({copied} bytes already used)"
            )));
        }
        let reserved_total = copied.saturating_add(output_allowance);
        if let Some(job_id) = req.job_id.as_deref() {
            library
                .reserve_job_temp_path(
                    job_id,
                    &work_dir.display().to_string(),
                    reserved_total,
                    quota,
                )
                .await
                .map_err(|err| AcquireError::Other(anyhow::anyhow!(err)))?;
            reserved = true;
        }
        let encode_fut = encode(input.clone(), output.clone(), output_allowance);
        tokio::pin!(encode_fut);
        let encode_result = tokio::select! {
            biased;
            _ = wait_for_cancel(req.cancel.clone()) => {
                Err("convert cancelled during encode".to_string())
            }
            result = encode_fut => result,
        };
        encode_result.map_err(|err| AcquireError::Other(anyhow::anyhow!(err)))?;
        let out_len = tokio::fs::metadata(&output).await?.len();
        if out_len > output_allowance {
            let _ = tokio::fs::remove_file(&output).await;
            return Err(AcquireError::Other(anyhow::anyhow!(
                "encoder output is {out_len} bytes, reserved output allowance is {output_allowance}"
            )));
        }
        let meta = ObjectMeta {
            content_type: Some("audio/mpeg".into()),
            content_length: Some(out_len),
            asin: Some(file_id.to_string()),
            title: Some(book.title.clone()),
            creation_time: None,
            last_write_time: None,
            ..Default::default()
        };
        storage.put_file(mp3_key, &output, meta).await?;
        library
            .set_acquire_status(
                book.title_id(),
                &book.account_id,
                AcquireStatus::Acquired,
                Some(mp3_key),
                None,
            )
            .await?;
        if mp3_key != key {
            let _ = storage.delete(key).await;
        }
        Ok(mp3_key.to_string())
    }
    .await;

    let removed = if scratch_delete_forced_failure() {
        false
    } else {
        match tokio::fs::remove_dir_all(work_dir).await {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    };
    if removed {
        scratch.disarm();
        if reserved {
            if let Some(job_id) = req.job_id.as_deref() {
                let _ = library
                    .unregister_job_temp_path(job_id, &work_dir.display().to_string())
                    .await;
            }
        }
    } else if scratch_delete_forced_failure() {
        // Leave the files and the reservation. Drop must not delete them either.
        scratch.disarm();
    }
    result
}

/// Removes conversion scratch on drop and on explicit success.
struct ScratchDir {
    path: std::path::PathBuf,
    armed: bool,
}

impl ScratchDir {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

async fn wait_for_cancel(cancel: Option<Arc<AtomicBool>>) {
    let Some(cancel) = cancel else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

fn scratch_delete_forced_failure() -> bool {
    #[cfg(test)]
    {
        FAIL_SCRATCH_DELETE.load(Ordering::SeqCst)
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
static FAIL_SCRATCH_DELETE: AtomicBool = AtomicBool::new(false);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bookclerk_library::{EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, NewBook};
    use bookclerk_storage::{
        ByteRange, ListPage, LocalFsBackend, ObjectInfo, ObjectProbe, PutStreamResult, StorageError,
    };
    use std::sync::atomic::AtomicU64;

    use bytes::Bytes;

    type SResult<T> = bookclerk_storage::Result<T>;

    struct SizedSource {
        inner: LocalFsBackend,
        advertised: u64,
    }

    #[async_trait]
    impl StorageBackend for SizedSource {
        fn name(&self) -> &'static str {
            "sized"
        }
        fn instance_id(&self) -> String {
            format!("sized:{}", self.inner.instance_id())
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                advertised: self.advertised,
            })
        }
        async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> SResult<()> {
            self.inner.put(key, data, meta).await
        }
        async fn get(&self, key: &str) -> SResult<Bytes> {
            self.inner.get(key).await
        }
        async fn exists(&self, key: &str) -> SResult<bool> {
            self.inner.exists(key).await
        }
        async fn list(&self, prefix: &str) -> SResult<Vec<ObjectInfo>> {
            self.inner.list(prefix).await
        }
        async fn probe(&self, key: &str) -> SResult<ObjectProbe> {
            self.inner.probe(key).await
        }
        async fn copy(&self, from: &str, to: &str) -> SResult<()> {
            self.inner.copy(from, to).await
        }
        async fn delete(&self, key: &str) -> SResult<()> {
            self.inner.delete(key).await
        }
        async fn list_page(
            &self,
            prefix: &str,
            cursor: Option<&str>,
            limit: u32,
        ) -> SResult<ListPage> {
            self.inner.list_page(prefix, cursor, limit).await
        }
        async fn get_stream(
            &self,
            key: &str,
            range: Option<ByteRange>,
        ) -> SResult<(
            ObjectProbe,
            std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        )> {
            let (mut probe, body) = self.inner.get_stream(key, range).await?;
            probe.size = self.advertised;
            Ok((probe, body))
        }
        async fn put_stream(
            &self,
            key: &str,
            body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            meta: ObjectMeta,
        ) -> SResult<PutStreamResult> {
            self.inner.put_stream(key, body, meta).await
        }
        async fn put_file(&self, key: &str, path: &Path, meta: ObjectMeta) -> SResult<()> {
            self.inner.put_file(key, path, meta).await
        }
    }

    async fn library() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    async fn acquired_book(store: &LibraryStore) -> bookclerk_library::BookRecord {
        store
            .upsert_account("acct", "us", None, true, "audible")
            .await
            .unwrap();
        store
            .upsert_book(&NewBook::minimal("B00CONVERT1", "acct", "us", "Convert"))
            .await
            .unwrap();
        store
            .set_acquire_status(
                "B00CONVERT1",
                "acct",
                AcquireStatus::Acquired,
                Some("book.m4b"),
                None,
            )
            .await
            .unwrap();
        store
            .get_book("B00CONVERT1", "acct")
            .await
            .unwrap()
            .unwrap()
    }

    fn request(cache: &Path, quota: u64) -> ConvertRequest {
        ConvertRequest {
            cache_dir: cache.to_path_buf(),
            force: true,
            lame: bookclerk_config::LameConfig::default(),
            max_sample_rate: None,
            job_id: None,
            temp_quota_bytes: Some(quota),
            cancel: None,
        }
    }

    #[tokio::test]
    async fn tiny_quota_understated_length_and_encoder_growth_do_not_publish() {
        let store = library().await;
        let book = acquired_book(&store).await;
        let dir = tempfile::tempdir().unwrap();
        let root = LocalFsBackend::new(dir.path().join("store")).unwrap();
        root.put(
            "book.m4b",
            Bytes::from_static(b"0123456789abcdefghij"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let cache = dir.path().join("cache");
        let work = cache.join("convert").join("case");
        let err = convert_with_encoder(
            &store,
            &SizedSource {
                inner: root.clone(),
                advertised: 20,
            },
            &book,
            &request(&cache, 8),
            &work,
            "book.mp3",
            |_input, _output, _allowance| async { Ok(()) },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("quota"), "{err}");
        assert!(!root.exists("book.mp3").await.unwrap());
        assert!(!work.exists());

        let err = convert_with_encoder(
            &store,
            &SizedSource {
                inner: root.clone(),
                advertised: 4,
            },
            &book,
            &request(&cache, 10_000),
            &work,
            "book.mp3",
            |_input, _output, _allowance| async { Ok(()) },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AcquireError::Storage(StorageError::Integrity(_))),
            "{err}"
        );

        let err = convert_with_encoder(
            &store,
            &SizedSource {
                inner: root.clone(),
                advertised: 0,
            },
            &book,
            &request(&cache, 8),
            &work,
            "book.mp3",
            |_input, _output, _allowance| async { Ok(()) },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("quota"), "{err}");

        root.put(
            "book.m4b",
            Bytes::from_static(b"1234"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        convert_with_encoder(
            &store,
            &SizedSource {
                inner: root.clone(),
                advertised: 4,
            },
            &book,
            &request(&cache, 8),
            &work,
            "book.mp3",
            |_input, output, allowance| async move {
                let mut file = std::fs::File::create(&output).unwrap();
                let mut written = 0u64;
                let chunk = vec![7u8; 64];
                let mut peak = 0u64;
                while bookclerk_media::write_output_within_budget(
                    &mut file,
                    &chunk,
                    &mut written,
                    Some(allowance),
                )
                .is_ok()
                {
                    peak = peak.max(file.metadata().unwrap().len());
                }
                let huge = vec![9u8; allowance as usize + 4096];
                assert!(bookclerk_media::write_output_within_budget(
                    &mut file,
                    &huge,
                    &mut written,
                    Some(allowance),
                )
                .is_err());
                let len = file.metadata().unwrap().len();
                assert!(
                    peak <= allowance && len <= allowance,
                    "peak {peak} len {len}"
                );
                Ok(())
            },
        )
        .await
        .unwrap();
        let published = tokio::fs::metadata(dir.path().join("store").join("book.mp3"))
            .await
            .unwrap()
            .len();
        assert!(published <= 4, "published {published}");
    }

    #[tokio::test]
    async fn cancellation_releases_the_reservation_only_after_scratch_is_gone() {
        let store = library().await;
        let book = acquired_book(&store).await;
        let created = store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Acquire,
                payload: JobPayload {
                    title: Some("convert".into()),
                    ..JobPayload::default()
                },
                priority: 0,
                max_attempts: 1,
                max_pending: 4,
                run_after: None,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Created { id } = created else {
            panic!("expected a new job");
        };
        let dir = tempfile::tempdir().unwrap();
        let root = LocalFsBackend::new(dir.path().join("store")).unwrap();
        root.put(
            "book.m4b",
            Bytes::from_static(b"1234"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let mut req = request(dir.path(), 10_000);
        req.job_id = Some(id.clone());
        req.cancel = Some(Arc::new(AtomicBool::new(true)));
        let work = dir.path().join("convert").join("cancel");
        let err = convert_with_encoder(
            &store,
            &SizedSource {
                inner: root.clone(),
                advertised: 4,
            },
            &book,
            &req,
            &work,
            "book.mp3",
            |_input, _output, _allowance| async { Ok(()) },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(store.list_job_temp_paths(&id).await.unwrap().is_empty());
        assert!(!work.exists());
        assert!(!root.exists("book.mp3").await.unwrap());
    }

    #[tokio::test]
    async fn cancel_after_input_eof_drops_the_encoder_and_releases_the_reservation() {
        let store = library().await;
        let book = acquired_book(&store).await;
        let created = store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Acquire,
                payload: JobPayload {
                    title: Some("cancel-encode".into()),
                    ..JobPayload::default()
                },
                priority: 0,
                max_attempts: 1,
                max_pending: 4,
                run_after: None,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Created { id } = created else {
            panic!("expected a new job");
        };
        let dir = tempfile::tempdir().unwrap();
        let root = LocalFsBackend::new(dir.path().join("store")).unwrap();
        root.put(
            "book.m4b",
            Bytes::from_static(b"1234"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut req = request(dir.path(), 10_000);
        req.job_id = Some(id.clone());
        req.cancel = Some(Arc::clone(&cancel));
        let work = dir.path().join("convert").join("eof");
        let dropped = Arc::new(AtomicBool::new(false));
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped_flag = Arc::clone(&dropped);
        let cancel_flag = Arc::clone(&cancel);
        let err = convert_with_encoder(
            &store,
            &SizedSource {
                inner: root.clone(),
                advertised: 4,
            },
            &book,
            &req,
            &work,
            "book.mp3",
            move |_input, _output, allowance| async move {
                assert_eq!(allowance, 4);
                let _flag = DropFlag(dropped_flag);
                cancel_flag.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(
            dropped.load(Ordering::SeqCst),
            "encoder future was not dropped"
        );
        assert!(store.list_job_temp_paths(&id).await.unwrap().is_empty());
        assert!(!work.exists());
    }

    #[tokio::test]
    async fn concurrent_jobs_cannot_expand_past_their_reservations() {
        let store = library().await;
        let book = acquired_book(&store).await;
        let dir = tempfile::tempdir().unwrap();
        let root = LocalFsBackend::new(dir.path().join("store")).unwrap();
        root.put(
            "book.m4b",
            Bytes::from_static(b"1234"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        let quota = 20u64;
        let peaks = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
        let aggregate = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for idx in 0..2 {
            let store = store.clone();
            let book = book.clone();
            let root = root.clone();
            let peaks = Arc::clone(&peaks);
            let aggregate = Arc::clone(&aggregate);
            let cache = dir.path().join(format!("cache-{idx}"));
            let created = store
                .enqueue_job(EnqueueJobSpec {
                    kind: JobKind::Acquire,
                    payload: JobPayload {
                        title: Some(format!("convert-{idx}")),
                        ..JobPayload::default()
                    },
                    priority: 0,
                    max_attempts: 1,
                    max_pending: 4,
                    run_after: None,
                })
                .await
                .unwrap();
            let EnqueueOutcome::Created { id } = created else {
                panic!("job");
            };
            tasks.push(tokio::spawn(async move {
                let mut req = request(&cache, quota);
                req.job_id = Some(id);
                let work = cache.join("convert").join("job");
                convert_with_encoder(
                    &store,
                    &SizedSource {
                        inner: root,
                        advertised: 4,
                    },
                    &book,
                    &req,
                    &work,
                    &format!("out-{idx}.mp3"),
                    |_input, output, allowance| async move {
                        assert_eq!(
                            allowance, 4,
                            "output budget must match the reservation slice"
                        );
                        let mut file = std::fs::File::create(&output).unwrap();
                        let mut written = 0u64;
                        let chunk = vec![1u8; 64];
                        let mut peak = 0u64;
                        while bookclerk_media::write_output_within_budget(
                            &mut file,
                            &chunk,
                            &mut written,
                            Some(allowance),
                        )
                        .is_ok()
                        {
                            let len = file.metadata().unwrap().len();
                            peak = peak.max(len);
                            peaks[idx].store(len, Ordering::SeqCst);
                            let sum =
                                peaks[0].load(Ordering::SeqCst) + peaks[1].load(Ordering::SeqCst);
                            aggregate.fetch_max(sum, Ordering::SeqCst);
                            assert!(
                                sum <= 8,
                                "concurrent encoder output {sum} exceeded the reserved 8 bytes"
                            );
                        }
                        assert!(peak <= allowance);
                        Ok(())
                    },
                )
                .await
                .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let peak = aggregate.load(Ordering::SeqCst);
        assert!(peak <= 8, "peak concurrent encoder output was {peak}");
    }

    #[tokio::test]
    async fn failed_scratch_cleanup_keeps_the_reservation() {
        let store = library().await;
        let book = acquired_book(&store).await;
        let created = store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Acquire,
                payload: JobPayload {
                    title: Some("cleanup".into()),
                    ..JobPayload::default()
                },
                priority: 0,
                max_attempts: 1,
                max_pending: 4,
                run_after: None,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Created { id } = created else {
            panic!("job");
        };
        let dir = tempfile::tempdir().unwrap();
        let root = LocalFsBackend::new(dir.path().join("store")).unwrap();
        root.put(
            "book.m4b",
            Bytes::from_static(b"1234"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        FAIL_SCRATCH_DELETE.store(true, Ordering::SeqCst);
        let mut req = request(dir.path(), 100);
        req.job_id = Some(id.clone());
        let work = dir.path().join("convert").join("keep");
        let result = convert_with_encoder(
            &store,
            &SizedSource {
                inner: root,
                advertised: 4,
            },
            &book,
            &req,
            &work,
            "book.mp3",
            |_input, output, allowance| async move {
                std::fs::write(&output, vec![1u8; allowance as usize]).unwrap();
                Ok(())
            },
        )
        .await;
        FAIL_SCRATCH_DELETE.store(false, Ordering::SeqCst);
        assert!(result.is_ok(), "{result:?}");
        assert!(!store.list_job_temp_paths(&id).await.unwrap().is_empty());
        assert!(work.exists());
    }
}
