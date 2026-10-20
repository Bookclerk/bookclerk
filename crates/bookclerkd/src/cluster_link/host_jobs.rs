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
//!   into a per-title outbox under this host's files dir with one outbox
//!   destination per owner destination, each with that destination's own
//!   naming options, so every destination's keys come out as a local
//!   acquire on the owner would name them (the acquire pipeline runs
//!   against a scratch library
//!   opened through this host's database guest, seeded with the owner's
//!   row; media work stays in `bookclerk-media-worker`), copies the files
//!   to this host's own deployed destinations (H-2), and streams them to
//!   the owner (`upload.grant`, one `PUT` per distinct file content, each
//!   listing its key on every destination, then `commit`), which writes
//!   them straight to its destinations. A title is uploaded only once every
//!   one of this host's own destinations has it; a failed copy fails the
//!   title, and so does any failure of its upload after the copies were
//!   written (the grant, a file, the commit, or a cancel), which removes
//!   those copies again. The job completes with its closing upload's commit, never with
//!   `complete`, and the owner accepts that only once every title of the
//!   job is acquired. A host may stage; the owner never does.
//!
//! **Cancellation.** The job's cancel flag (its fence lost, the operator's
//! cancel, or this host's consent revoked) stops every guest call within
//! [`CANCEL_POLL`]: the call is dropped, which shuts its guest down and
//! fences its host-side authority, so the guest stops using the handed
//! credentials. A title's upload is dropped the same way, and the owner
//! discards what that upload had written.
//!
//! The guest work itself sits behind [`HostGuests`], so tests can stand in
//! a guest without spawning one; [`GuestHost`] is the daemon's.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bookclerk_library::{BookRecord, JobKind, LibraryStore, NewBook};
use bookclerk_node_client::{HostJobCtx, OwnerCallError};
use bookclerk_node_proto::{
    AcquireTarget, FileCopy, PlanDestination, UploadFile, MAX_SCAN_BATCH_ROWS, MAX_SCAN_ROW_BYTES,
    MAX_UPLOAD_FILES,
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

/// How often a running guest call or upload checks the job's cancel flag.
pub const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Runs `work` until it ends or `cancel` turns true. On cancel the work is
/// dropped (for the daemon's guests that shuts the guest down) and the
/// answer is a `cancelled` error. Work that ends while `cancel` is set
/// (a guest that stopped cooperatively, returning what it had so far) is
/// answered `cancelled` too, never with its result.
///
/// # Errors
///
/// Returns `work`'s error, or `cancelled`.
pub async fn until_cancelled<T>(
    cancel: &AtomicBool,
    work: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    if cancel.load(Ordering::SeqCst) {
        anyhow::bail!("cancelled");
    }
    tokio::pin!(work);
    let mut every = tokio::time::interval(CANCEL_POLL);
    loop {
        tokio::select! {
            out = &mut work => {
                if cancel.load(Ordering::SeqCst) {
                    anyhow::bail!("cancelled");
                }
                return out;
            }
            _ = every.tick() => {
                if cancel.load(Ordering::SeqCst) {
                    anyhow::bail!("cancelled");
                }
            }
        }
    }
}

/// What a host scan covers.
#[derive(Debug, Clone, Default)]
pub struct HostScan {
    /// Account ids; empty means every handed account.
    pub accounts: Vec<String>,
    /// Import podcast episodes (the owner's setting).
    pub import_episodes: bool,
    /// Import subscription (Plus) titles (the owner's setting).
    pub import_plus_titles: bool,
}

/// One output destination of the owner node an acquire was planned with.
#[derive(Debug, Clone)]
pub struct HostDestination {
    /// Its id on the owner node (`local`, `s3`).
    pub id: String,
    /// Its kind (one destination per kind).
    pub kind: bookclerk_config::OutputBackendKind,
    /// Its naming and packaging options.
    pub options: DownloadOptions,
}

impl HostDestination {
    /// Parses a plan's destination; refuses an unknown kind or options
    /// this host cannot read.
    ///
    /// # Errors
    ///
    /// Returns an error naming the destination id.
    pub fn from_plan(plan: PlanDestination) -> anyhow::Result<Self> {
        let kind: bookclerk_config::OutputBackendKind =
            serde_json::from_value(serde_json::Value::String(plan.id.clone()))
                .with_context(|| format!("the owner's destination {:?}", plan.id))?;
        let options: DownloadOptions = serde_json::from_value(plan.options)
            .with_context(|| format!("the owner's options for destination {:?}", plan.id))?;
        Ok(Self {
            id: plan.id,
            kind,
            options,
        })
    }
}

/// One title an acquire fetches here.
#[derive(Debug, Clone)]
pub struct HostTitle {
    /// The owner's library row.
    pub book: BookRecord,
    /// The owner's row for the title's podcast parent, when it sent one;
    /// seeded into the scratch library so episodes are named as on the
    /// owner.
    pub parent: Option<BookRecord>,
    /// The owner node's destinations, the primary first; never empty.
    pub destinations: Vec<HostDestination>,
}

/// Where destination `index`'s files go inside a title's outbox.
#[must_use]
pub fn destination_outbox(outbox: &Path, index: usize) -> PathBuf {
    outbox.join(format!("d{index}"))
}

/// What acquiring one title left in its outbox.
#[derive(Debug, Clone)]
pub struct AcquiredTitle {
    /// Storage key of the primary audio file on the primary destination.
    pub primary_key: String,
    /// Storage key of the companion PDF on the primary destination, when
    /// one was acquired.
    pub pdf_key: Option<String>,
    /// Every file per destination (in [`HostTitle::destinations`] order),
    /// as storage keys relative to [`destination_outbox`].
    pub keys: Vec<Vec<String>>,
}

/// The guest work a host job does; the daemon's is [`GuestHost`].
///
/// The transport drops a call once the job's cancel flag turns true (see
/// [`until_cancelled`]); `cancel` is the same flag, for guests that can
/// also stop cooperatively.
pub trait HostGuests: Send + Sync + 'static {
    /// Runs the job's guest scan with the handed credentials and answers
    /// its rows unwritten.
    fn scan(
        &self,
        bindings: &HostJobBindings,
        scan: HostScan,
        cancel: Arc<AtomicBool>,
    ) -> impl Future<Output = anyhow::Result<Vec<ScanBook>>> + Send;

    /// Acquires `title` with the job's guest into `outbox`: destination
    /// `i`'s files, named with its options, under
    /// [`destination_outbox`]`(outbox, i)`.
    fn acquire(
        &self,
        bindings: &HostJobBindings,
        title: &HostTitle,
        outbox: &Path,
        cancel: Arc<AtomicBool>,
    ) -> impl Future<Output = anyhow::Result<AcquiredTitle>> + Send;

    /// This host's own deployed output destinations (H-2: a host's local
    /// outputs add to the cluster's); empty when none is deployed.
    ///
    /// # Errors
    ///
    /// Returns an error when the deployed destinations cannot be used (two
    /// of one kind), so no title is uploaded while this host would miss
    /// its copy.
    fn local_destinations(
        &self,
    ) -> impl Future<Output = anyhow::Result<Vec<Box<dyn StorageBackend>>>> + Send;

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
        cancel: Arc<AtomicBool>,
    ) -> anyhow::Result<Vec<ScanBook>> {
        let source = self.open_source(bindings).await?;
        // The owner's import settings, not this host's: the owner writes
        // every row into its library.
        Ok(source
            .scan_handed(bookclerk_source::ScanOptions {
                accounts: scan.accounts,
                page_size: 50,
                import_episodes: scan.import_episodes,
                import_plus_titles: scan.import_plus_titles,
                cancel: Some(cancel),
            })
            .await?)
    }

    async fn acquire(
        &self,
        bindings: &HostJobBindings,
        title: &HostTitle,
        outbox: &Path,
        _cancel: Arc<AtomicBool>,
    ) -> anyhow::Result<AcquiredTitle> {
        // The pipeline has no cancel input; the transport drops this call
        // (and with it the guest) once the job is cancelled, and all of the
        // title's scratch (its library, and the pipeline's download and
        // packaging dirs) sits under the job dir it removes.
        let source = self.open_source(bindings).await?;
        let cfg = self.state.config.read().await.clone();
        let scratch_dir = outbox.with_extension("library");
        tokio::fs::create_dir_all(&scratch_dir).await?;
        let scratch = scratch_library(&cfg, &scratch_dir).await?;
        let result = acquire_into(
            &scratch.store,
            &cfg,
            &source,
            title,
            outbox,
            &title_cache_dir(outbox),
        )
        .await;
        drop(scratch);
        let _ = tokio::fs::remove_dir_all(&scratch_dir).await;
        result
    }

    async fn local_destinations(&self) -> anyhow::Result<Vec<Box<dyn StorageBackend>>> {
        let registry = self.state.destinations.read().await;
        let mut out = Vec::new();
        for deployed in [registry.require_local(), registry.require_s3()] {
            if let Some(backend) = deployed.map_err(|err| anyhow::anyhow!(err))? {
                out.push(backend.clone_box());
            }
        }
        Ok(out)
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

/// The acquire request for `book` named with `options`: what a host's
/// acquire runs, and what the owner computes the title's key scope from
/// (so both name the title the same way).
#[must_use]
pub fn acquire_request(
    cfg: &bookclerk_config::Config,
    book: &BookRecord,
    options: &DownloadOptions,
) -> bookclerk_acquire::AcquireRequest {
    bookclerk_acquire::AcquireRequest {
        asin: book.download_product_id().to_string(),
        book_uuid: Some(book.uuid.clone()),
        source: book.source.clone(),
        account_id: book.account_id.clone(),
        title: book.title.clone(),
        authors: book.authors.clone(),
        narrators: book.narrators.clone(),
        series: book.series.clone(),
        series_index: book.series_index.clone(),
        options: options.clone(),
        files_dir: cfg.paths().files_dir.clone(),
        cache_dir: cfg.download_cache_dir(),
        force: false,
        write_destinations: None,
        job_id: None,
        temp_quota_bytes: Some(cfg.jobs.temp_quota_bytes),
    }
}

/// Where the acquire pipeline downloads and packages a title fetched into
/// `outbox`: beside its scratch library, inside the job dir, so a dropped
/// acquire (a cancel, a lost fence) leaves nothing once the job dir is
/// removed, and no other run (this host's own jobs, or a retry of the
/// title) shares it. Only this daemon touches it; the guest fetches into
/// its own plugin `tmp`.
#[must_use]
pub fn title_cache_dir(outbox: &Path) -> PathBuf {
    outbox.with_extension("cache")
}

/// Runs the acquire pipeline for `title` against `scratch`, writing only
/// to `outbox`, with its scratch under `cache_dir`.
async fn acquire_into(
    scratch: &LibraryStore,
    cfg: &bookclerk_config::Config,
    source: &bookclerk_plugin_host::ExternalSource,
    title: &HostTitle,
    outbox: &Path,
    cache_dir: &Path,
) -> anyhow::Result<AcquiredTitle> {
    let book = &title.book;
    scratch
        .scope(book.plugin_key.as_str(), book.source.as_str())
        .ensure_account(&book.account_id, &book.marketplace, None)
        .await?;
    if let Some(parent) = &title.parent {
        scratch.upsert_book(&new_book_from(parent)).await?;
    }
    scratch.upsert_book(&new_book_from(book)).await?;
    let Some(primary) = title.destinations.first() else {
        anyhow::bail!("the owner planned no destination");
    };
    let mut items = Vec::with_capacity(title.destinations.len());
    for (index, destination) in title.destinations.iter().enumerate() {
        let dir = destination_outbox(outbox, index);
        tokio::fs::create_dir_all(&dir).await?;
        items.push(bookclerk_acquire::AcquireDestination {
            kind: destination.kind,
            backend: Box::new(bookclerk_storage::LocalFsBackend::new(dir)?),
            options: destination.options.clone(),
        });
    }
    // The pipeline packages once and writes each destination under the
    // keys its own options give, exactly as a local acquire does.
    let destinations = bookclerk_acquire::AcquireDestinations {
        items,
        primary: primary.kind,
        multi_destination: cfg.output.multi_destination,
    };
    // The owner names keys from the same request; the scratch dir does not
    // enter a key.
    let mut request = acquire_request(cfg, book, &primary.options);
    request.cache_dir = cache_dir.to_path_buf();
    let result =
        bookclerk_acquire::acquire_book_indexed(scratch, &destinations, request, None, source)
            .await?;
    let pdf_key = scratch
        .get_book_by_uuid(&book.uuid)
        .await?
        .filter(|row| row.pdf_status == bookclerk_library::AcquireStatus::Acquired)
        .and_then(|row| row.pdf_storage_key);
    let mut keys = Vec::with_capacity(title.destinations.len());
    for index in 0..title.destinations.len() {
        keys.push(outbox_keys(&destination_outbox(outbox, index)).await?);
    }
    Ok(AcquiredTitle {
        primary_key: result.storage_key,
        pdf_key,
        keys,
    })
}

/// Every finished file under `dir`, as storage keys relative to it
/// (sorted; in-flight temp files are skipped).
///
/// # Errors
///
/// Returns an error when the directory cannot be read.
pub async fn outbox_keys(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut keys = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&next).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // In-flight temps, and the outbox backend's own metadata
            // sidecars (each destination writes its own on receipt).
            if (name.starts_with('.') && name.contains(".bookclerk-tmp-"))
                || name.ends_with(".bookclerk-meta.json")
            {
                continue;
            }
            let Ok(relative) = path.strip_prefix(dir) else {
                continue;
            };
            let key = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            keys.push(key);
        }
    }
    keys.sort();
    Ok(keys)
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
                // The owner writes every row, so the scan imports what the
                // owner's library does; an owner that did not say is
                // refused rather than scanned with this host's settings.
                let Some(import) = host.job.job.scan_import else {
                    anyhow::bail!("the owner sent no import settings for the scan");
                };
                let scan = HostScan {
                    accounts: cmd.payload.account.clone().into_iter().collect(),
                    import_episodes: import.import_episodes,
                    import_plus_titles: import.import_plus_titles,
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
    let rows = until_cancelled(
        &ctx.cancel,
        guests.scan(bindings, scan, Arc::clone(&ctx.cancel)),
    )
    .await?;
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
    // A scan cancelled while its last batch went out did not finish: its
    // rows so far stay written, but it never reports success.
    if ctx.is_cancelled() {
        anyhow::bail!("cancelled");
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
    let job_dir = job_work_path(&guests.work_dir(), &host.job.job.id, host.job.fence)?;
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
        let (targets, planned) = with_retries(|| host.acquire_plan())
            .await
            .map_err(|err| owner_error("the acquire plan", &err))?;
        if planned.is_empty() {
            anyhow::bail!("the owner planned no destination");
        }
        let destinations = planned
            .into_iter()
            .map(HostDestination::from_plan)
            .collect::<anyhow::Result<Vec<_>>>()?;
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
            if book.uuid != target.book_uuid {
                anyhow::bail!("the owner's library row is not the title it planned");
            }
            let parent: Option<BookRecord> = target
                .parent
                .map(serde_json::from_value)
                .transpose()
                .context("the owner's podcast parent row")?;
            let outbox = work_path(job_dir, &target.book_uuid)?;
            let title = HostTitle {
                book,
                parent,
                destinations: destinations.clone(),
            };
            // Both the guest's acquire and the title's upload stop once the
            // job is cancelled; a dropped upload's files are discarded by
            // the owner (its stream breaks, or its upload is aborted).
            let copied = std::sync::Mutex::new(Vec::new());
            let commit_unsettled = AtomicBool::new(false);
            let uploaded = until_cancelled(&ctx.cancel, async {
                let done = guests
                    .acquire(bindings, &title, &outbox, Arc::clone(&ctx.cancel))
                    .await?;
                upload_title(
                    guests,
                    host,
                    &title,
                    &outbox,
                    &done,
                    &copied,
                    &commit_unsettled,
                )
                .await
            })
            .await;
            if uploaded.is_err() {
                let copies = std::mem::take(&mut *copied.lock().unwrap_or_else(|e| e.into_inner()));
                if commit_unsettled.load(Ordering::SeqCst) {
                    // The commit was sent but never definitively answered
                    // (its answer was lost, the owner went away, or the job
                    // was cancelled meanwhile): the owner may have marked
                    // the title acquired, and a later plan would then skip
                    // it, so its copies here stay.
                    if !copies.is_empty() {
                        tracing::warn!(
                            job_id = %host.job.job.id,
                            book_uuid = %title.book.uuid,
                            files = copies.len(),
                            "the owner's answer to a title's commit is unknown; keeping its copies on this host's own destinations"
                        );
                    }
                } else {
                    // The owner never committed the title (its grant or a
                    // file failed, or it refused the commit), so this host
                    // keeps no copy of it either, unless the fence is lost:
                    // a replacement run may own those keys by now.
                    remove_local_copies(&title.book.uuid, &copies, host.is_lost()).await;
                }
            }
            let _ = tokio::fs::remove_dir_all(&outbox).await;
            let _ = tokio::fs::remove_dir_all(title_cache_dir(&outbox)).await;
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

/// Longest owner-supplied id used as a path segment under the work dir.
pub const MAX_WORK_ID_BYTES: usize = 128;

/// True when `id` (a job or title id the owner sent) is safe as one path
/// segment: non-empty, at most [`MAX_WORK_ID_BYTES`], and only ASCII
/// letters, digits, `-` and `_`, so no separator, `.` / `..`, drive or
/// root prefix, or other special name can come through.
#[must_use]
pub fn is_work_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_WORK_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `dir`'s child named by the owner-supplied `id`, refused unless `id` is
/// one plain segment ([`is_work_id`]), so the host's job scratch (created,
/// written and removed recursively) never leaves `dir`.
///
/// # Errors
///
/// Returns an error when `id` is not a usable segment.
pub fn work_path(dir: &Path, id: &str) -> anyhow::Result<PathBuf> {
    if !is_work_id(id) {
        anyhow::bail!("the owner sent a job or title id this host cannot use as a path");
    }
    Ok(dir.join(id))
}

/// The scratch dir of one fence generation of job `job_id` under `dir`:
/// `<job_id>-g<fence>`, one plain segment. A stale run still unwinding
/// after its lease expired and the replacement generation this host
/// claimed never share it, so the stale run's cleanup removes only its own
/// scratch. The name is unambiguous because `fence` is all digits: it is
/// whatever follows the last `-g`.
///
/// # Errors
///
/// Returns an error when `job_id` is not a usable segment ([`is_work_id`]).
pub fn job_work_path(dir: &Path, job_id: &str, fence: i64) -> anyhow::Result<PathBuf> {
    let job_dir = work_path(dir, job_id)?;
    let mut name = job_dir.into_os_string();
    name.push(format!("-g{fence}"));
    Ok(PathBuf::from(name))
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

/// The upload manifest of an acquired title: one entry per distinct file
/// content (size and SHA-256), listing its key on every destination that
/// has it, with the local path its bytes are read from.
///
/// # Errors
///
/// Returns an error when a file cannot be read, a key is unusable, a
/// destination has no files, or there are too many files.
pub async fn upload_manifest(
    destinations: &[HostDestination],
    outbox: &Path,
    acquired: &AcquiredTitle,
) -> anyhow::Result<Vec<(UploadFile, PathBuf)>> {
    if acquired.keys.len() != destinations.len() {
        anyhow::bail!("the acquire did not write every destination");
    }
    let mut files: Vec<(UploadFile, PathBuf)> = Vec::new();
    let mut by_content = std::collections::HashMap::new();
    for (index, (destination, keys)) in destinations.iter().zip(&acquired.keys).enumerate() {
        if keys.is_empty() {
            anyhow::bail!("destination {:?} received no file", destination.id);
        }
        let dir = destination_outbox(outbox, index);
        for key in keys {
            let path = dir.join(key);
            let (size, sha256) = digest_file(&path).await?;
            let copy = FileCopy {
                destination: destination.id.clone(),
                key: key.clone(),
            };
            match by_content.get(&(size, sha256.clone())) {
                Some(&at) => {
                    let (file, _): &mut (UploadFile, PathBuf) = &mut files[at];
                    if file.key_on(&destination.id).is_some() {
                        // The same bytes twice on one destination: a
                        // separate entry, since a file has one copy per
                        // destination.
                        by_content.insert((size, sha256.clone()), files.len());
                        files.push((
                            UploadFile {
                                copies: vec![copy],
                                size,
                                sha256,
                                content_type: content_type(key),
                            },
                            path,
                        ));
                    } else {
                        file.copies.push(copy);
                    }
                }
                None => {
                    by_content.insert((size, sha256.clone()), files.len());
                    files.push((
                        UploadFile {
                            copies: vec![copy],
                            size,
                            sha256,
                            content_type: content_type(key),
                        },
                        path,
                    ));
                }
            }
        }
    }
    if files.is_empty() || files.len() > MAX_UPLOAD_FILES {
        anyhow::bail!("the title has {} files to upload", files.len());
    }
    if files.iter().any(|(file, _)| !file.is_well_formed()) {
        anyhow::bail!("an acquired file has an unusable storage key");
    }
    Ok(files)
}

/// One file this run wrote to one of this host's own destinations: the
/// destination handle it was written through and the key. The handle is
/// kept rather than re-read from [`HostGuests::local_destinations`], so a
/// destination deployed or retired before cleanup cannot redirect the
/// delete to a destination this run never wrote.
pub type LocalCopy = (Arc<dyn StorageBackend>, String);

/// Copies one title's files (the primary destination's keys) to every one
/// of this host's own destinations. Any failed copy fails the title. Each
/// copy is put in `copied` as soon as it is written, so the caller removes
/// exactly what this run wrote ([`remove_local_copies`]) whether this
/// fails or is dropped mid-copy.
///
/// # Errors
///
/// Returns an error when the destinations cannot be used or a copy fails.
pub async fn copy_to_local_destinations<G: HostGuests>(
    guests: &G,
    book_uuid: &str,
    outbox: &Path,
    acquired: &AcquiredTitle,
    copied: &std::sync::Mutex<Vec<LocalCopy>>,
) -> anyhow::Result<()> {
    let local: Vec<Arc<dyn StorageBackend>> = guests
        .local_destinations()
        .await
        .context("this host's own destinations")?
        .into_iter()
        .map(Arc::from)
        .collect();
    // This host's own copy follows the owner's primary destination naming.
    let primary = destination_outbox(outbox, 0);
    for key in acquired.keys.first().into_iter().flatten() {
        for destination in &local {
            let meta = ObjectMeta {
                content_type: content_type(key),
                ..ObjectMeta::default()
            };
            if let Err(err) = destination.put_file(key, &primary.join(key), meta).await {
                tracing::warn!(book_uuid, %key, error = %err, "could not copy a title to this host's own destination");
                anyhow::bail!("this host's own destination did not take {key:?}: {err}");
            }
            copied
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((Arc::clone(destination), key.clone()));
        }
    }
    Ok(())
}

/// Deletes `copies` (the files one run wrote for a title) from this host's
/// own destinations, best effort: a title the owner did not commit keeps
/// no copy here. Skipped when `fence_lost`: the owner may have handed the
/// job to a replacement run, here or elsewhere, that writes the same keys,
/// and a host-side copy carries no writer identity to tell the two apart,
/// so a stale run's cleanup could delete the replacement's file. The
/// stale copy is left for the replacement to overwrite.
pub async fn remove_local_copies(book_uuid: &str, copies: &[LocalCopy], fence_lost: bool) {
    if copies.is_empty() {
        return;
    }
    if fence_lost {
        tracing::warn!(
            book_uuid,
            files = copies.len(),
            "the job's fence is lost; leaving an uncommitted title's copies on this host's own destinations to the run that holds it now"
        );
        return;
    }
    for (destination, key) in copies {
        if let Err(err) = destination.delete(key).await {
            tracing::warn!(book_uuid, %key, error = %err, "could not remove an uncommitted title's copy from this host's own destination");
        }
    }
}

/// Copies one title's files to this host's own destinations (H-2), then
/// streams them to the owner and commits the title. Nothing is uploaded
/// unless every one of this host's destinations got the title. Each copy
/// is put in `copied` as it is written, so the caller can remove them when
/// the owner does not commit the title ([`remove_local_copies`]), whether
/// this fails or is dropped. `commit_unsettled` turns true as the commit is
/// first sent and back to false only when the owner definitively refuses
/// it, so a caller seeing an error with it still true cannot tell whether
/// the owner committed the title and must keep the copies.
///
/// # Errors
///
/// Returns an error when the manifest, a local copy, the grant, a file or
/// the commit fails.
async fn upload_title<G: HostGuests>(
    guests: &G,
    host: &HostJobCtx,
    title: &HostTitle,
    outbox: &Path,
    acquired: &AcquiredTitle,
    copied: &std::sync::Mutex<Vec<LocalCopy>>,
    commit_unsettled: &AtomicBool,
) -> anyhow::Result<()> {
    let book_uuid = title.book.uuid.as_str();
    let manifest = upload_manifest(&title.destinations, outbox, acquired).await?;
    copy_to_local_destinations(guests, book_uuid, outbox, acquired, copied).await?;
    let (files, paths): (Vec<UploadFile>, Vec<PathBuf>) = manifest.into_iter().unzip();
    let upload = with_retries(|| {
        host.grant_upload(
            book_uuid,
            &acquired.primary_key,
            acquired.pdf_key.as_deref(),
            files.clone(),
        )
    })
    .await
    .map_err(|err| owner_error("the upload", &err))?;
    for (index, (file, path)) in files.iter().zip(&paths).enumerate() {
        with_retries(|| async {
            let body = tokio::fs::File::open(&path)
                .await
                .map_err(|_| OwnerCallError::Unavailable)?;
            host.upload_file(&upload, index, file.size, body).await
        })
        .await
        .map_err(|err| owner_error("an uploaded file", &err))?;
    }
    // An attempt that got no usable answer may still have committed the
    // title; a refusal after it then only answers the retry, so it does not
    // settle the outcome either.
    let ambiguous = AtomicBool::new(false);
    commit_unsettled.store(true, Ordering::SeqCst);
    let committed = with_retries(|| async {
        let out = host.commit_upload(&upload, None).await;
        if matches!(out, Err(OwnerCallError::Unavailable)) {
            ambiguous.store(true, Ordering::SeqCst);
        }
        out
    })
    .await;
    if matches!(committed, Err(OwnerCallError::Refused(_))) && !ambiguous.load(Ordering::SeqCst) {
        commit_unsettled.store(false, Ordering::SeqCst);
    }
    committed.map_err(|err| owner_error("the title's commit", &err))?;
    tracing::info!(
        job_id = %host.job.job.id,
        book_uuid,
        files = files.len(),
        "streamed a title to the owner"
    );
    Ok(())
}
