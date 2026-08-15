//! Durable job admission and scan / acquire / listen-sync executors.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_acquire::{
    acquire_book_indexed, match_storage_to_library, AcquireRequest, MatchStorageOptions,
    StorageIndex,
};
use bookclerk_config::BadBookAction;
use bookclerk_library::{
    is_downloadable, AcquireStatus, BookRecord, EnqueueJobSpec, EnqueueOutcome, JobKind,
    JobPayload, JobTrigger,
};
use bookclerk_source::{DownloadOptions, ScanOptions};
use tracing::{error, info, warn};

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::registry::default_registry_with_plugins;

/// Result of admitting work into the durable queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitJob {
    /// A new row was inserted.
    Created(String),
    /// An equivalent pending/running job already exists.
    Duplicate(String),
    /// The pending+running cap was reached.
    QueueFull,
}

impl From<EnqueueOutcome> for AdmitJob {
    fn from(value: EnqueueOutcome) -> Self {
        match value {
            EnqueueOutcome::Created { id } => Self::Created(id),
            EnqueueOutcome::Duplicate { existing_id } => Self::Duplicate(existing_id),
            EnqueueOutcome::QueueFull => Self::QueueFull,
        }
    }
}

/// Emits `book_acquired` to loaded integrations after a successful acquire or storage match.
async fn notify_integrations(state: &AppState, asin: &str, storage_key: &str) {
    let library = state.library.read().await.clone();
    let integrations = state.integrations.read().await.clone();
    bookclerk_integrations::emit_book_acquired(&integrations, &library, asin, storage_key).await;
}

/// Builds an [`EnqueueJobSpec`] for scan, acquire, or listen-sync admission.
fn enqueue_spec(
    kind: JobKind,
    account: Option<String>,
    title: Option<String>,
    trigger: JobTrigger,
    max_pending: i64,
    max_attempts: i64,
) -> EnqueueJobSpec {
    EnqueueJobSpec {
        kind,
        payload: JobPayload {
            account,
            title,
            trigger,
            ..Default::default()
        },
        priority: 0,
        max_attempts,
        max_pending,
        run_after: None,
    }
}

/// Writes a job row and wakes the worker when a new row is created.
///
/// Takes a shared [`AppState::job_runtime`] permit so a database swap cannot
/// interleave with admission.
async fn admit(state: &AppState, spec: EnqueueJobSpec) -> anyhow::Result<AdmitJob> {
    let _permit = tokio::time::timeout(Duration::from_secs(15), state.job_runtime.read())
        .await
        .map_err(|_| anyhow::anyhow!("job admission paused (database reload in progress)"))?;
    let library = state.library_snapshot().await;
    let outcome = library.enqueue_job(spec).await?;
    if matches!(outcome, EnqueueOutcome::Created { .. }) {
        state.job_notify.notify_one();
    }
    Ok(outcome.into())
}

/// True when the fence asked to stop or the row is flagged cancelled.
async fn job_cancelled(
    ctx: Option<&JobExecCtx>,
    library: &bookclerk_library::LibraryStore,
) -> bool {
    let Some(ctx) = ctx else {
        return false;
    };
    ctx.is_cancelled()
        || library
            .job_cancel_requested(&ctx.fence.job_id)
            .await
            .unwrap_or(false)
}

/// Returns `(max_pending, max_attempts)` from the live `[jobs]` config.
async fn queue_limits(state: &AppState) -> (i64, i64) {
    let cfg = state.config.read().await;
    (
        i64::from(cfg.jobs.max_pending),
        i64::from(cfg.jobs.max_attempts),
    )
}

/// Admit a library scan. Does not spawn a per-request task.
pub async fn enqueue_scan(
    state: Arc<AppState>,
    account: Option<String>,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        enqueue_spec(
            JobKind::Scan,
            account,
            None,
            trigger,
            max_pending,
            max_attempts,
        ),
    )
    .await
}

/// Admit acquire for pending titles (optional title / account filter).
pub async fn enqueue_acquire(
    state: Arc<AppState>,
    title: Option<String>,
    account: Option<String>,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        enqueue_spec(
            JobKind::Acquire,
            account,
            title,
            trigger,
            max_pending,
            max_attempts,
        ),
    )
    .await
}

/// Admit a listening-progress sync.
pub async fn enqueue_listen_sync(
    state: Arc<AppState>,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        enqueue_spec(
            JobKind::ListenSync,
            None,
            None,
            trigger,
            max_pending,
            max_attempts,
        ),
    )
    .await
}

/// Admit a remote integration library scan.
pub async fn enqueue_integration_scan(
    state: Arc<AppState>,
    integration_id: String,
    force: bool,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        EnqueueJobSpec {
            kind: JobKind::IntegrationScan,
            payload: JobPayload {
                trigger,
                integration_id: Some(integration_id),
                force,
                ..Default::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Admit an ABI v2 `JobHandler` stream-copy invocation.
#[allow(dead_code)] // API/CLI enqueue lands with the #118 broker follow-up.
pub async fn enqueue_plugin_copy(
    state: Arc<AppState>,
    plugin_id: String,
    source_key: String,
    dest_key: String,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        EnqueueJobSpec {
            kind: JobKind::PluginCopy,
            payload: JobPayload {
                trigger,
                plugin_id: Some(plugin_id),
                source_key: Some(source_key),
                dest_key: Some(dest_key),
                ..Default::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Progress/detail prefix after a fenced [`bookclerk_plugin_host::JobOutcome::Suspended`] commit.
///
/// [`crate::job_worker`] must not call `complete_job` for this outcome: suspend already moved
/// the row to `pending`.
pub const JOB_SUSPENDED_DETAIL_PREFIX: &str = "suspended until ";

/// Runs `plugin.worker().handle(stream_copy)` on a loaded v2 destination guest.
pub async fn run_plugin_copy(
    state: &AppState,
    plugin_id: Option<&str>,
    source_key: Option<&str>,
    dest_key: Option<&str>,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let plugin_id = plugin_id.unwrap_or("local");
    let from = source_key.ok_or_else(|| anyhow::anyhow!("plugin_copy missing source_key"))?;
    let to = dest_key.ok_or_else(|| anyhow::anyhow!("plugin_copy missing dest_key"))?;
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    if let Some(ctx) = ctx {
        let _ = library.set_job_progress(&ctx.fence, "copying").await;
    }
    let destinations = state.destinations.read().await;
    let session = destinations
        .v2_session(plugin_id, bookclerk_plugin_host::OPERATOR_ACCOUNT)
        .ok_or_else(|| {
            anyhow::anyhow!("no abi v2 session for plugin `{plugin_id}` (guest not loaded)")
        })?;
    let lease = match ctx {
        Some(ctx) => bookclerk_plugin_host::JobInvocationLease {
            job_id: ctx.fence.job_id.clone(),
            attempt: u32::try_from(ctx.attempt_count.max(1)).unwrap_or(1),
            generation: ctx.fence.generation,
            dedup_key: ctx.dedup_key.clone(),
            deadline_unix_ms: ctx
                .lease_expires_at
                .map(|t| u64::try_from(t.timestamp_millis()).unwrap_or(u64::MAX / 2))
                .unwrap_or(u64::MAX / 2),
            checkpoint: ctx.checkpoint.clone(),
            invocation_sequence: ctx.invocation_sequence,
        },
        None => bookclerk_plugin_host::JobInvocationLease {
            job_id: "plugin_copy".into(),
            attempt: 1,
            generation: 1,
            dedup_key: "plugin_copy".into(),
            deadline_unix_ms: u64::MAX / 2,
            checkpoint: None,
            invocation_sequence: 1,
        },
    };
    let cancel = ctx
        .map(|c| std::sync::Arc::clone(&c.cancel))
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let outcome = tokio::select! {
        () = async {
            loop {
                if job_cancelled(ctx, &library).await {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => {
            anyhow::bail!("cancelled");
        }
        outcome = session.stream_copy_with_cancel(
            lease,
            from,
            to,
            cancel,
            ctx.map(|c| (library.clone(), c.fence.clone())),
        ) => outcome?,
    };
    match outcome {
        bookclerk_plugin_host::JobOutcome::Completed {
            message,
            bytes_copied,
        } => Ok(format!(
            "copied {from} -> {to} ({bytes_copied} bytes) {message}"
        )),
        bookclerk_plugin_host::JobOutcome::Cancelled { message } => {
            anyhow::bail!("cancelled: {message}")
        }
        bookclerk_plugin_host::JobOutcome::Retryable { message, .. } => {
            anyhow::bail!("plugin_copy retryable: {message}")
        }
        bookclerk_plugin_host::JobOutcome::Rejected { message } => {
            anyhow::bail!("plugin_copy rejected: {message}")
        }
        bookclerk_plugin_host::JobOutcome::Suspended {
            checkpoint,
            wake_at_unix_ms,
        } => {
            let Some(ctx) = ctx else {
                anyhow::bail!("plugin_copy suspended without a job fence");
            };
            let wake = chrono::DateTime::from_timestamp_millis(
                i64::try_from(wake_at_unix_ms).unwrap_or(i64::MAX),
            )
            .ok_or_else(|| anyhow::anyhow!("plugin_copy suspend wake_at out of range"))?;
            let committed = library.suspend_job(&ctx.fence, &checkpoint, wake).await?;
            if !committed {
                anyhow::bail!("plugin_copy suspend lost the fence; checkpoint was not committed");
            }
            Ok(format!("{JOB_SUSPENDED_DETAIL_PREFIX}{wake}"))
        }
    }
}

/// Run a scan synchronously (worker / tests).
///
/// Cancel is checked between sources. An in-flight source page fetch finishes
/// before the next source starts.
pub async fn run_scan(
    state: &AppState,
    account: Option<&str>,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let _guard = state.work_lock.lock().await;
    let cfg = state.config.read().await.clone();
    let paths = cfg.paths();
    paths.ensure_dirs()?;
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let registry = default_registry_with_plugins(&cfg).await?;
    let summary = registry
        .scan_all(
            &library,
            ScanOptions {
                accounts: account.map(|a| vec![a.to_string()]).unwrap_or_default(),
                page_size: 50,
                import_episodes: cfg.library.import_episodes,
                import_plus_titles: cfg.library.import_plus_titles,
                cancel: ctx.map(|c| c.cancel.clone()),
            },
        )
        .await?;
    info!(
        account = account.unwrap_or("*"),
        accounts = summary.accounts,
        books_upserted = summary.books_upserted,
        pages = summary.pages,
        skipped_disabled = summary.skipped_disabled,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "run_scan finished"
    );
    if cfg.library.enrich_from_audible {
        if let Err(err) =
            bookclerk_enrich::enrich_books_from_audible(&library, cfg.library.enrich_min_confidence)
                .await
        {
            warn!(error = %err, "Audible enrichment failed");
        }
    }
    Ok(format!(
        "{} account(s), {} book upsert(s), {} page(s), {} skipped (scan disabled)",
        summary.accounts, summary.books_upserted, summary.pages, summary.skipped_disabled
    ))
}

/// Acquire pending titles synchronously.
///
/// When `ctx` is set, progress is fenced and cancel is checked between titles.
pub async fn run_acquire(
    state: &AppState,
    title: Option<&str>,
    account: Option<&str>,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let _guard = state.work_lock.lock().await;
    let cfg = state.config.read().await.clone();
    let paths = cfg.paths();
    paths.ensure_dirs()?;
    let library = state.library.read().await.clone();
    let destinations = bookclerk_plugin_host::build_acquire_destinations(
        &cfg,
        Some(&library),
        &*state.destinations.read().await,
    )
    .await?;
    let storage = destinations.listing_backend()?;
    let options = DownloadOptions::from(&cfg);
    let registry = default_registry_with_plugins(&cfg).await?;

    let _ = match_storage_to_library(
        &library,
        storage.as_ref(),
        MatchStorageOptions {
            account: account.map(str::to_string),
            clear_missing: true,
            fix_layout: cfg.library.fix_storage_layout,
            download: options.clone(),
            ..Default::default()
        },
    )
    .await?;

    let books = library.list_books(account).await?;
    let targets: Vec<_> = books
        .into_iter()
        .filter(|b| {
            title.is_none_or(|a| {
                a.eq_ignore_ascii_case(&b.uuid)
                    || a.eq_ignore_ascii_case(&b.product_id)
                    || b.isbn.as_deref().is_some_and(|i| a.eq_ignore_ascii_case(i))
                    || b.asin.as_deref().is_some_and(|x| a.eq_ignore_ascii_case(x))
            })
        })
        .filter(|b| {
            acquire_job_includes_book(b, options.download_pdf, cfg.library.download_episodes)
        })
        .collect();

    if targets.is_empty() {
        info!(
            title = title.unwrap_or("*"),
            account = account.unwrap_or("*"),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "run_acquire finished: nothing to acquire"
        );
        return Ok("nothing to acquire".into());
    }

    let mut index = StorageIndex::from_storage(storage.as_ref()).await?;
    let mut ok = 0u32;
    let mut matched = 0u32;
    let mut failed = 0u32;
    let bad_book = cfg.output.bad_book_action;
    let total = targets.len();
    for (idx, book) in targets.into_iter().enumerate() {
        if let Some(ctx) = ctx {
            if ctx.is_cancelled()
                || library
                    .job_cancel_requested(&ctx.fence.job_id)
                    .await
                    .unwrap_or(false)
            {
                anyhow::bail!("cancelled after {idx}/{total} titles");
            }
            let progress = format!("{}/{} acquiring {}", idx + 1, total, book.title);
            if !library
                .set_job_progress(&ctx.fence, &progress)
                .await
                .unwrap_or(false)
            {
                ctx.request_cancel();
                anyhow::bail!("cancelled after {idx}/{total} titles (lease fence lost)");
            }
        }
        let content_source = registry.get(&book.source).ok_or_else(|| {
            anyhow::anyhow!(
                "no content source registered for `{}` (title {})",
                book.source,
                book.asin_or_isbn()
            )
        })?;
        let req = AcquireRequest {
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
            files_dir: paths.files_dir.clone(),
            cache_dir: cfg.download_cache_dir(),
            force: false,
            write_destinations: None,
            job_id: ctx.map(|c| c.fence.job_id.clone()),
            temp_quota_bytes: Some(cfg.jobs.temp_quota_bytes),
        };
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            match acquire_book_indexed(
                &library,
                &destinations,
                req.clone(),
                Some(&mut index),
                content_source.as_ref(),
            )
            .await
            {
                Ok(result) if result.matched_existing => {
                    info!(asin = %result.asin, key = %result.storage_key, "matched existing");
                    notify_integrations(state, &result.asin, &result.storage_key).await;
                    matched += 1;
                    break;
                }
                Ok(result) => {
                    info!(asin = %result.asin, key = %result.storage_key, "acquired");
                    notify_integrations(state, &result.asin, &result.storage_key).await;
                    ok += 1;
                    break;
                }
                Err(err) => {
                    if bad_book == BadBookAction::Retry && attempts < 2 {
                        warn!(asin = %book.asin_or_isbn(), error = %err, "acquire failed; retrying");
                        continue;
                    }
                    warn!(asin = %book.asin_or_isbn(), error = %err, "acquire failed");
                    failed += 1;
                    if matches!(bad_book, BadBookAction::Ask | BadBookAction::Abort) {
                        anyhow::bail!("acquire aborted on {}: {err}", book.asin_or_isbn());
                    }
                    break;
                }
            }
        }
    }
    let detail = format!("acquired={ok} matched={matched} failed={failed}");
    info!(
        title = title.unwrap_or("*"),
        account = account.unwrap_or("*"),
        acquired = ok,
        matched,
        failed,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "run_acquire finished"
    );
    if failed > 0 && bad_book == BadBookAction::Retry {
        anyhow::bail!("{detail}");
    }
    Ok(detail)
}

/// Run a remote integration library scan synchronously.
///
/// The remote `scan_library` RPC is not interruptible. Cancel is checked
/// before the call; a repeat scan is idempotent (remote upsert / rescan).
pub async fn run_integration_scan(
    state: &AppState,
    integration_id: &str,
    force: bool,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let integrations = state.integrations.read().await.clone();
    let Some(integration) = integrations.get(integration_id) else {
        anyhow::bail!("integration `{integration_id}` not found");
    };
    if !integration.supports_library_scan() {
        anyhow::bail!("integration `{integration_id}` does not support library scan");
    }
    integration.scan_library(force).await?;
    info!(
        integration = integration_id,
        force,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "run_integration_scan finished"
    );
    Ok(format!("scanned integration {integration_id}"))
}

/// Sync listening progress from capable integrations.
///
/// Cancel is checked between providers. An in-flight provider RPC finishes.
pub async fn run_listen_sync(state: &AppState, ctx: Option<&JobExecCtx>) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let library = state.library.read().await.clone();
    let integrations = state.integrations.read().await.clone();
    let mut summary = bookclerk_integrations::SyncListeningSummary::default();
    for integration in integrations.listening_sync_providers() {
        if job_cancelled(ctx, &library).await {
            anyhow::bail!("cancelled");
        }
        match integration.sync_listening_progress(&library).await {
            Ok(n) => {
                summary.upserted += n;
                summary
                    .by_provider
                    .push(bookclerk_integrations::SyncListeningProviderResult {
                        id: integration.id().to_string(),
                        upserted: n,
                        error: None,
                    });
            }
            Err(err) => {
                warn!(id = %integration.id(), %err, "listening sync provider failed");
                summary
                    .by_provider
                    .push(bookclerk_integrations::SyncListeningProviderResult {
                        id: integration.id().to_string(),
                        upserted: 0,
                        error: Some(err.to_string()),
                    });
            }
        }
    }
    if summary.by_provider.is_empty() {
        info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "listen_sync skipped (no capable integrations)"
        );
        return Ok("no capable integrations".into());
    }
    for p in &summary.by_provider {
        if let Some(err) = &p.error {
            warn!(id = %p.id, %err, "listening sync provider failed");
        }
    }
    let detail = format!(
        "upserted={} providers={}",
        summary.upserted,
        summary.by_provider.len()
    );
    info!(
        upserted = summary.upserted,
        providers = summary.by_provider.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "listen_sync finished"
    );
    if summary.by_provider.iter().any(|p| p.error.is_some()) {
        anyhow::bail!("{detail}");
    }
    Ok(detail)
}

/// Whether [`run_acquire`] should visit this library row.
///
/// Already-acquired audio is still included when companion PDF download is
/// enabled and the PDF is not stored, so a later job can resume the sidecar
/// through [`bookclerk_acquire::acquire_book_indexed`].
fn acquire_job_includes_book(
    book: &BookRecord,
    download_pdf: bool,
    download_episodes: bool,
) -> bool {
    if !is_downloadable(&book.content_kind) {
        return false;
    }
    if !download_episodes && book.content_kind == "episode" {
        return false;
    }
    if book.acquire_status != AcquireStatus::Acquired {
        return true;
    }
    download_pdf && companion_pdf_needs_resume(book)
}

/// True when audio is acquired but the companion PDF is missing or incomplete.
fn companion_pdf_needs_resume(book: &BookRecord) -> bool {
    book.pdf_status != AcquireStatus::Acquired
        || book
            .pdf_storage_key
            .as_deref()
            .is_none_or(|key| key.is_empty())
}

/// Log a handler failure and request a diagnostics upload.
pub fn note_job_failure(job_id: &str, err: &anyhow::Error) {
    error!(%job_id, error = %err, "job failed");
    if let Some(diag) = bookclerk_config::diagnostics_global() {
        diag.request_upload("job_failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{LibraryStore, NewBook};

    async fn test_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    async fn book_with(
        store: &LibraryStore,
        acquire: AcquireStatus,
        audio_key: Option<&str>,
        pdf: AcquireStatus,
        pdf_key: Option<&str>,
        content_kind: &str,
    ) -> BookRecord {
        store
            .upsert_account("user-1", "us", None, true, "audible")
            .await
            .unwrap();
        let mut new_book = NewBook::minimal("B00TEST", "user-1", "us", "Test");
        new_book.content_kind = content_kind.to_string();
        store.upsert_book(&new_book).await.unwrap();
        store
            .set_acquire_status("B00TEST", "user-1", acquire, audio_key, None)
            .await
            .unwrap();
        store
            .set_pdf_status("B00TEST", "user-1", pdf, pdf_key)
            .await
            .unwrap();
        store
            .get_book("B00TEST", "user-1")
            .await
            .unwrap()
            .expect("book")
    }

    #[tokio::test]
    async fn acquire_job_keeps_acquired_audio_when_pdf_needs_resume() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::Acquired,
            Some("audio.m4b"),
            AcquireStatus::Error,
            None,
            "book",
        )
        .await;
        assert!(
            acquire_job_includes_book(&book, true, true),
            "job path must visit acquired audio so companion PDF can resume"
        );
        assert!(
            !acquire_job_includes_book(&book, false, true),
            "without download_pdf the acquired title stays skipped"
        );
    }

    #[tokio::test]
    async fn acquire_job_keeps_acquired_audio_when_pdf_key_is_missing() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::Acquired,
            Some("audio.m4b"),
            AcquireStatus::Acquired,
            Some(""),
            "book",
        )
        .await;
        assert!(acquire_job_includes_book(&book, true, true));
    }

    #[tokio::test]
    async fn acquire_job_skips_fully_acquired_title() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::Acquired,
            Some("audio.m4b"),
            AcquireStatus::Acquired,
            Some("audio.pdf"),
            "book",
        )
        .await;
        assert!(!acquire_job_includes_book(&book, true, true));
    }

    #[tokio::test]
    async fn acquire_job_includes_pending_audio() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::NotAcquired,
            None,
            AcquireStatus::NotAcquired,
            None,
            "book",
        )
        .await;
        assert!(acquire_job_includes_book(&book, false, true));
    }

    #[tokio::test]
    async fn acquire_job_skips_podcast_parent() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::NotAcquired,
            None,
            AcquireStatus::Error,
            None,
            "podcast",
        )
        .await;
        assert!(!acquire_job_includes_book(&book, true, true));
    }
}
