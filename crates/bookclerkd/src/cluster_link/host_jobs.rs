//! How a host runs a job an owner cluster handed it (H-4 step 9).
//!
//! [`HostJobTransport`] is the [`JobTransport`] behind every host client.
//! It never runs a job in process against this install's library: the
//! job's guest is the owner's plugin version materialized here, spawned
//! jailed under the grant this host's operator consented to (through the
//! mandatory `bookclerk-workerd` front door), opened with the bindings
//! [`super::remote_bindings::host_job_bindings`] built, and given only the
//! store credentials the owner handed with the job. Any failure to open it
//! fails the job; there is no fallback.
//!
//! - **Scan.** The guest's rows come back unwritten
//!   ([`bookclerk_plugin_host::ExternalSource::scan_handed`]) and go to the
//!   owner in fenced `scan.results` batches of at most
//!   [`MAX_SCAN_BATCH_ROWS`] rows and [`MAX_BATCH_BYTES`] (a row larger
//!   than [`MAX_SCAN_ROW_BYTES`] is skipped). The owner validates and
//!   writes them (decision 6); the scan completes with `complete`.
//! - **Acquire.** The host asks the owner for the job's titles
//!   (`acquire.plan`, again until no new title comes back), acquires each
//!   with the owner's naming options into a per-title outbox under this
//!   host's files dir (the acquire pipeline runs against a scratch library
//!   opened through this host's database guest, seeded with the owner's
//!   row; media work stays in `bookclerk-media-worker`), copies the files
//!   to this host's own deployed destinations (H-2), and streams them to
//!   the owner (`upload.grant`, one `PUT` per file, `commit`), which writes
//!   them straight to its destinations. The job completes with its closing
//!   upload's commit, never with `complete`. A host may stage; the owner
//!   never does.
//!
//! The guest work itself sits behind [`HostGuests`], so tests can stand in
//! a guest without spawning one; [`GuestHost`] is the daemon's.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bookclerk_library::{BookRecord, JobKind, LibraryStore, NewBook};
use bookclerk_node_client::{HostJobCtx, OwnerCallError};
use bookclerk_node_proto::{
    AcquireTarget, UploadFile, MAX_SCAN_BATCH_ROWS, MAX_SCAN_ROW_BYTES, MAX_UPLOAD_FILES,
};
use bookclerk_plugin_abi::ScanBook;
use bookclerk_source::DownloadOptions;
use bookclerk_storage::{ObjectMeta, StorageBackend};
use sha2::Digest as _;
use tokio::io::AsyncReadExt as _;

use super::remote_bindings::HostJobBindings;
use crate::api::AppState;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// Largest serialized `scan.results` batch (well under a frame).
pub const MAX_BATCH_BYTES: usize = 768 * 1024;

/// Attempts for one owner call that found the owner unavailable.
const OWNER_ATTEMPTS: u32 = 3;

/// What a host scan covers.
#[derive(Debug, Clone, Default)]
pub struct HostScan {
    /// Account ids; empty means every handed account.
    pub accounts: Vec<String>,
}

/// One title an acquire fetches here.
#[derive(Debug, Clone)]
pub struct HostTitle {
    /// The owner's library row.
    pub book: BookRecord,
    /// The owner's naming and packaging options.
    pub options: DownloadOptions,
}

/// What acquiring one title left in its outbox.
#[derive(Debug, Clone)]
pub struct AcquiredTitle {
    /// Storage key of the primary audio file.
    pub primary_key: String,
    /// Storage key of the companion PDF, when one was acquired.
    pub pdf_key: Option<String>,
    /// Every file, as storage keys relative to the outbox.
    pub keys: Vec<String>,
}

/// The guest work a host job does; the daemon's is [`GuestHost`].
pub trait HostGuests: Send + Sync + 'static {
    /// Runs the job's guest scan with the handed credentials and answers
    /// its rows unwritten.
    fn scan(
        &self,
        bindings: &HostJobBindings,
        scan: HostScan,
    ) -> impl Future<Output = anyhow::Result<Vec<ScanBook>>> + Send;

    /// Acquires `title` with the job's guest into `outbox`.
    fn acquire(
        &self,
        bindings: &HostJobBindings,
        title: &HostTitle,
        outbox: &Path,
    ) -> impl Future<Output = anyhow::Result<AcquiredTitle>> + Send;

    /// This host's own deployed output destinations (H-2: a host's local
    /// outputs add to the cluster's).
    fn local_destinations(&self) -> impl Future<Output = Vec<Box<dyn StorageBackend>>> + Send;

    /// Where per-job scratch lives (under this host's files dir).
    fn work_dir(&self) -> PathBuf;
}

/// The daemon's [`HostGuests`]: spawns the owner's plugin version
/// materialized here as a jailed guest.
pub struct GuestHost {
    /// Daemon state (config, this host's destinations).
    state: Arc<AppState>,
    /// This host's files dir.
    files_dir: PathBuf,
}

impl GuestHost {
    /// Guests for the daemon at `state`, whose files dir is `files_dir`.
    #[must_use]
    pub fn new(state: Arc<AppState>, files_dir: PathBuf) -> Self {
        Self { state, files_dir }
    }

    /// Spawns the job's guest under the consented grant, with only the
    /// handed credentials. Fails closed.
    async fn open_source(
        &self,
        bindings: &HostJobBindings,
    ) -> anyhow::Result<bookclerk_plugin_host::ExternalSource> {
        let mut plugin =
            bookclerk_plugin_host::open_installed_plugin(&bindings.root, &bindings.version)
                .context("the owner's plugin is not usable on this host")?;
        // Exactly the grant this host's operator consented to.
        plugin.version_grant = Some(bindings.grant.clone());
        let config = self.state.config.read().await.clone();
        let source = bookclerk_plugin_host::ExternalSource::spawn_prepared(
            &plugin,
            &config,
            bookclerk_plugin_host::SessionServices::default(),
            bindings.open.clone(),
        )
        .await
        .context("the owner's plugin did not start on this host")?;
        Ok(source.with_handed_credentials(bindings.credentials.clone()))
    }
}

impl HostGuests for GuestHost {
    async fn scan(
        &self,
        bindings: &HostJobBindings,
        scan: HostScan,
    ) -> anyhow::Result<Vec<ScanBook>> {
        let source = self.open_source(bindings).await?;
        let (import_episodes, import_plus_titles) = {
            let cfg = self.state.config.read().await;
            (cfg.library.import_episodes, cfg.library.import_plus_titles)
        };
        Ok(source
            .scan_handed(bookclerk_source::ScanOptions {
                accounts: scan.accounts,
                page_size: 50,
                import_episodes,
                import_plus_titles,
                cancel: None,
            })
            .await?)
    }

    async fn acquire(
        &self,
        bindings: &HostJobBindings,
        title: &HostTitle,
        outbox: &Path,
    ) -> anyhow::Result<AcquiredTitle> {
        let source = self.open_source(bindings).await?;
        let cfg = self.state.config.read().await.clone();
        let scratch_dir = outbox.with_extension("library");
        tokio::fs::create_dir_all(&scratch_dir).await?;
        let scratch = scratch_library(&cfg, &scratch_dir).await?;
        let result = acquire_into(&scratch.store, &cfg, &source, title, outbox).await;
        drop(scratch);
        let _ = tokio::fs::remove_dir_all(&scratch_dir).await;
        result
    }

    async fn local_destinations(&self) -> Vec<Box<dyn StorageBackend>> {
        let cfg = self.state.config.read().await.clone();
        match bookclerk_plugin_host::build_acquire_destinations(
            &cfg,
            &*self.state.destinations.read().await,
        ) {
            Ok(destinations) => destinations
                .items
                .into_iter()
                .map(|item| item.backend)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn work_dir(&self) -> PathBuf {
        self.files_dir.join("cluster-work")
    }
}

/// A scratch library for one title: its own database guest on a file in
/// the title's scratch dir, so the acquire pipeline never touches this
/// install's library.
struct Scratch {
    /// The store; dropped before the guest.
    store: LibraryStore,
    /// Keeps the database guest alive.
    _registry: bookclerk_plugin_host::DatabaseRegistry,
}

/// Opens [`Scratch`] under `dir` through the `sqlite` database guest.
async fn scratch_library(cfg: &bookclerk_config::Config, dir: &Path) -> anyhow::Result<Scratch> {
    let mut scratch = cfg.clone();
    scratch.database.plugin = "sqlite".into();
    scratch.database.sqlite.path = Some(dir.join("scratch.db"));
    let registry = bookclerk_plugin_host::load_external_database(&scratch)
        .await
        .map_err(|err| anyhow::anyhow!("the scratch database guest did not start: {err}"))?;
    let store = bookclerk_plugin_host::open_library_store(&scratch, &registry).await?;
    Ok(Scratch {
        store,
        _registry: registry,
    })
}

/// The owner's row as a scratch-library insert (same uuid).
fn new_book_from(book: &BookRecord) -> NewBook {
    NewBook {
        uuid: Some(book.uuid.clone()),
        product_id: book.product_id.clone(),
        plugin_key: book.plugin_key.clone(),
        source: book.source.clone(),
        account_id: book.account_id.clone(),
        asin: book.asin.clone(),
        isbn: book.isbn.clone(),
        marketplace: book.marketplace.clone(),
        title: book.title.clone(),
        authors: book.authors.clone(),
        narrators: book.narrators.clone(),
        series: book.series.clone(),
        series_index: book.series_index.clone(),
        series_asin: book.series_asin.clone(),
        purchased_at: book.purchased_at,
        publisher: book.publisher.clone(),
        length_minutes: book.length_minutes,
        is_abridged: book.is_abridged,
        content_kind: book.content_kind.clone(),
        categories: book.categories.clone(),
        subtitle: book.subtitle.clone(),
        published_at: book.published_at,
    }
}

/// Runs the acquire pipeline for `title` against `scratch`, writing only
/// to `outbox`.
async fn acquire_into(
    scratch: &LibraryStore,
    cfg: &bookclerk_config::Config,
    source: &bookclerk_plugin_host::ExternalSource,
    title: &HostTitle,
    outbox: &Path,
) -> anyhow::Result<AcquiredTitle> {
    let book = &title.book;
    scratch
        .scope(book.plugin_key.as_str(), book.source.as_str())
        .ensure_account(&book.account_id, &book.marketplace, None)
        .await?;
    scratch.upsert_book(&new_book_from(book)).await?;
    tokio::fs::create_dir_all(outbox).await?;
    let destinations = bookclerk_acquire::AcquireDestinations {
        items: vec![bookclerk_acquire::AcquireDestination {
            kind: bookclerk_config::OutputBackendKind::Local,
            backend: Box::new(bookclerk_storage::LocalFsBackend::new(
                outbox.to_path_buf(),
            )?),
            options: title.options.clone(),
        }],
        primary: bookclerk_config::OutputBackendKind::Local,
        multi_destination: cfg.output.multi_destination,
    };
    let paths = cfg.paths();
    let request = bookclerk_acquire::AcquireRequest {
        asin: book.download_product_id().to_string(),
        book_uuid: Some(book.uuid.clone()),
        source: book.source.clone(),
        account_id: book.account_id.clone(),
        title: book.title.clone(),
        authors: book.authors.clone(),
        narrators: book.narrators.clone(),
        series: book.series.clone(),
        series_index: book.series_index.clone(),
        options: title.options.clone(),
        files_dir: paths.files_dir.clone(),
        cache_dir: cfg.download_cache_dir(),
        force: false,
        write_destinations: None,
        job_id: None,
        temp_quota_bytes: Some(cfg.jobs.temp_quota_bytes),
    };
    let result =
        bookclerk_acquire::acquire_book_indexed(scratch, &destinations, request, None, source)
            .await?;
    let pdf_key = scratch
        .get_book_by_uuid(&book.uuid)
        .await?
        .filter(|row| row.pdf_status == bookclerk_library::AcquireStatus::Acquired)
        .and_then(|row| row.pdf_storage_key);
    let mut keys = result.written_keys;
    if keys.is_empty() {
        keys.push(result.storage_key.clone());
    }
    Ok(AcquiredTitle {
        primary_key: result.storage_key,
        pdf_key,
        keys,
    })
}

/// The [`JobTransport`] for jobs an owner cluster handed this host.
pub struct HostJobTransport<G> {
    /// Guest work.
    guests: G,
}

impl<G: HostGuests> HostJobTransport<G> {
    /// A transport over `guests`.
    pub fn new(guests: G) -> Self {
        Self { guests }
    }
}

impl<G: HostGuests> JobTransport for HostJobTransport<G> {
    async fn execute(&self, cmd: JobCommand, ctx: JobExecCtx) -> anyhow::Result<String> {
        let (Some(host), Some(bindings)) = (ctx.host.clone(), ctx.host_bindings.clone()) else {
            anyhow::bail!("only jobs an owner cluster handed this host run here");
        };
        if ctx.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        match cmd.kind {
            JobKind::Scan => {
                let scan = HostScan {
                    accounts: cmd.payload.account.clone().into_iter().collect(),
                };
                host_scan(&self.guests, &host, &bindings, scan, &ctx).await
            }
            JobKind::Acquire => host_acquire(&self.guests, &host, &bindings, &ctx).await,
            other => anyhow::bail!("{} jobs do not run on a host", other.as_str()),
        }
    }
}

/// Retries `call` while the owner is unavailable.
async fn with_retries<T, F, Fut>(mut call: F) -> Result<T, OwnerCallError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, OwnerCallError>>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match call().await {
            Err(OwnerCallError::Unavailable) if attempt < OWNER_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
            }
            other => return other,
        }
    }
}

/// An owner-call failure as the job's error.
fn owner_error(what: &str, err: &OwnerCallError) -> anyhow::Error {
    match err {
        OwnerCallError::Lost => anyhow::anyhow!("cancelled: the job's fence was lost ({what})"),
        OwnerCallError::Refused(reason) => anyhow::anyhow!("the owner refused {what}: {reason}"),
        OwnerCallError::Unavailable => anyhow::anyhow!("the owner was unavailable for {what}"),
    }
}

/// A host scan: the guest's rows go back in fenced batches.
async fn host_scan<G: HostGuests>(
    guests: &G,
    host: &HostJobCtx,
    bindings: &HostJobBindings,
    scan: HostScan,
    ctx: &JobExecCtx,
) -> anyhow::Result<String> {
    let rows = guests.scan(bindings, scan).await?;
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    let mut written = 0u64;
    let mut skipped = 0u64;
    let total = rows.len();
    let send = |batch: Vec<serde_json::Value>| async move {
        with_retries(|| host.send_scan_results(&bindings.plugin_key, batch.clone()))
            .await
            .map_err(|err| owner_error("the scan results", &err))
    };
    for row in rows {
        if ctx.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        let value = serde_json::to_value(&row)?;
        let size = value.to_string().len();
        if size > MAX_SCAN_ROW_BYTES {
            skipped += 1;
            tracing::warn!(job_id = %host.job.job.id, size, "skipping a scan row too large to return");
            continue;
        }
        if batch.len() >= MAX_SCAN_BATCH_ROWS || batch_bytes + size > MAX_BATCH_BYTES {
            written += send(std::mem::take(&mut batch)).await?;
            batch_bytes = 0;
        }
        batch_bytes += size;
        batch.push(value);
    }
    if !batch.is_empty() {
        written += send(batch).await?;
    }
    tracing::info!(
        job_id = %host.job.job.id,
        fence = host.job.fence,
        rows = total,
        written,
        skipped,
        "host scan returned its rows to the owner"
    );
    Ok(format!(
        "{written} book upsert(s) from a host scan ({skipped} skipped)"
    ))
}

/// A host acquire: titles from the owner, files streamed back, completed
/// by the closing upload.
async fn host_acquire<G: HostGuests>(
    guests: &G,
    host: &HostJobCtx,
    bindings: &HostJobBindings,
    ctx: &JobExecCtx,
) -> anyhow::Result<String> {
    let job_dir = guests.work_dir().join(&host.job.job.id);
    let result = acquire_titles(guests, host, bindings, ctx, &job_dir).await;
    let _ = tokio::fs::remove_dir_all(&job_dir).await;
    let (acquired, failed) = result?;
    let detail = format!("acquired={acquired} failed={failed} (on a host)");
    if failed > 0 {
        anyhow::bail!("{detail}");
    }
    if ctx.is_cancelled() {
        anyhow::bail!("cancelled");
    }
    let closing = with_retries(|| host.grant_upload("", "", None, Vec::new()))
        .await
        .map_err(|err| owner_error("the closing upload", &err))?;
    let completed = with_retries(|| host.commit_upload(&closing, Some(&detail)))
        .await
        .map_err(|err| owner_error("the closing commit", &err))?;
    if !completed {
        anyhow::bail!("the owner did not complete the job with its closing upload");
    }
    Ok(detail)
}

/// Acquires and uploads every title the owner hands out; answers how many
/// were acquired and how many failed.
async fn acquire_titles<G: HostGuests>(
    guests: &G,
    host: &HostJobCtx,
    bindings: &HostJobBindings,
    ctx: &JobExecCtx,
    job_dir: &Path,
) -> anyhow::Result<(u32, u32)> {
    let mut attempted = BTreeSet::new();
    let mut acquired = 0u32;
    let mut failed = 0u32;
    loop {
        let (targets, options) = with_retries(|| host.acquire_plan())
            .await
            .map_err(|err| owner_error("the acquire plan", &err))?;
        let options: DownloadOptions =
            serde_json::from_value(options).context("the owner's acquire options")?;
        let fresh: Vec<AcquireTarget> = targets
            .into_iter()
            .filter(|target| !attempted.contains(&target.book_uuid))
            .collect();
        if fresh.is_empty() {
            return Ok((acquired, failed));
        }
        for target in fresh {
            if ctx.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            attempted.insert(target.book_uuid.clone());
            if target.plugin_key != bindings.plugin_key {
                failed += 1;
                continue;
            }
            let book: BookRecord =
                serde_json::from_value(target.book).context("the owner's library row")?;
            let outbox = job_dir.join(&target.book_uuid);
            let title = HostTitle {
                book,
                options: options.clone(),
            };
            let outcome = guests.acquire(bindings, &title, &outbox).await;
            let uploaded = match outcome {
                Ok(done) => upload_title(guests, host, &title.book.uuid, &outbox, &done).await,
                Err(err) => Err(err),
            };
            let _ = tokio::fs::remove_dir_all(&outbox).await;
            match uploaded {
                Ok(()) => acquired += 1,
                Err(err) if ctx.is_cancelled() => return Err(err),
                Err(err) => {
                    tracing::warn!(job_id = %host.job.job.id, book_uuid = %title.book.uuid, error = %err, "a title failed on the host");
                    failed += 1;
                }
            }
        }
    }
}

/// Size and SHA-256 of a file, read once.
async fn digest_file(path: &Path) -> anyhow::Result<(u64, String)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = sha2::Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((
        size,
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    ))
}

/// MIME type for a storage key.
fn content_type(key: &str) -> Option<String> {
    let ext = key.rsplit('.').next()?.to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "m4b" | "m4a" | "mp4" => "audio/mp4",
            "mp3" => "audio/mpeg",
            "pdf" => "application/pdf",
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "json" => "application/json",
            "cue" | "txt" | "nfo" => "text/plain",
            _ => return None,
        }
        .to_string(),
    )
}

/// Copies one title's files to this host's own destinations (H-2), then
/// streams them to the owner and commits the title.
async fn upload_title<G: HostGuests>(
    guests: &G,
    host: &HostJobCtx,
    book_uuid: &str,
    outbox: &Path,
    title: &AcquiredTitle,
) -> anyhow::Result<()> {
    if title.keys.is_empty() || title.keys.len() > MAX_UPLOAD_FILES {
        anyhow::bail!("the title has {} files to upload", title.keys.len());
    }
    let mut files = Vec::with_capacity(title.keys.len());
    for key in &title.keys {
        let (size, sha256) = digest_file(&outbox.join(key)).await?;
        let file = UploadFile {
            key: key.clone(),
            size,
            sha256,
            content_type: content_type(key),
        };
        if !file.is_well_formed() {
            anyhow::bail!("an acquired file has an unusable storage key");
        }
        files.push(file);
    }
    for destination in guests.local_destinations().await {
        for file in &files {
            let meta = ObjectMeta {
                content_type: file.content_type.clone(),
                content_length: Some(file.size),
                sha256_hex: Some(file.sha256.clone()),
                ..ObjectMeta::default()
            };
            if let Err(err) = destination
                .put_file(&file.key, &outbox.join(&file.key), meta)
                .await
            {
                tracing::warn!(book_uuid, key = %file.key, error = %err, "could not copy a title to this host's own destination");
            }
        }
    }
    let upload = with_retries(|| {
        host.grant_upload(
            book_uuid,
            &title.primary_key,
            title.pdf_key.as_deref(),
            files.clone(),
        )
    })
    .await
    .map_err(|err| owner_error("the upload", &err))?;
    for (index, file) in files.iter().enumerate() {
        let path = outbox.join(&file.key);
        with_retries(|| async {
            let body = tokio::fs::File::open(&path)
                .await
                .map_err(|_| OwnerCallError::Unavailable)?;
            host.upload_file(&upload, index, file.size, body).await
        })
        .await
        .map_err(|err| owner_error("an uploaded file", &err))?;
    }
    with_retries(|| host.commit_upload(&upload, None))
        .await
        .map_err(|err| owner_error("the title's commit", &err))?;
    tracing::info!(
        job_id = %host.job.job.id,
        book_uuid,
        files = files.len(),
        "streamed a title to the owner"
    );
    Ok(())
}
