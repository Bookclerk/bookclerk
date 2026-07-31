//! Background job runners for scan / acquire.

use std::sync::Arc;

use bookclerk_acquire::{
    acquire_book_indexed, match_storage_to_library, AcquireRequest, MatchStorageOptions,
    StorageIndex,
};
use bookclerk_config::BadBookAction;
use bookclerk_library::AcquireStatus;
use bookclerk_source::{DownloadOptions, ScanOptions};
use tracing::{error, info, warn};

use crate::api::{AppState, JobInfo};
use crate::registry::default_registry_with_plugins;

async fn notify_integrations(state: &AppState, asin: &str, storage_key: &str) {
    let library = state.library.read().await.clone();
    bookclerk_integrations::emit_book_acquired(&state.integrations, &library, asin, storage_key)
        .await;
}

/// Enqueue a library scan and run it in the background.
pub async fn enqueue_scan(state: Arc<AppState>, account: Option<String>) -> String {
    let id = new_job_id("scan");
    push_job(
        &state,
        JobInfo {
            id: id.clone(),
            kind: "scan".into(),
            status: "accepted".into(),
            detail: account
                .as_ref()
                .map(|a| format!("account={a}"))
                .or_else(|| Some("all accounts".into())),
        },
    )
    .await;
    let job_id = id.clone();
    tokio::spawn(async move {
        set_job_status(&state, &job_id, "running", None).await;
        match run_scan(&state, account.as_deref()).await {
            Ok(detail) => {
                info!(%job_id, %detail, "scan job finished");
                set_job_status(&state, &job_id, "succeeded", Some(detail)).await;
            }
            Err(err) => {
                error!(%job_id, error = %err, "scan job failed");
                set_job_status(&state, &job_id, "failed", Some(err.to_string())).await;
                if let Some(diag) = bookclerk_config::diagnostics_global() {
                    diag.request_upload("job_failed");
                }
            }
        }
    });
    id
}

/// Enqueue acquire for pending titles (optional ASIN / account filter).
pub async fn enqueue_acquire(
    state: Arc<AppState>,
    asin: Option<String>,
    account: Option<String>,
) -> String {
    let id = new_job_id("acquire");
    let detail = match (&asin, &account) {
        (Some(a), Some(acct)) => Some(format!("asin={a} account={acct}")),
        (Some(a), None) => Some(format!("asin={a}")),
        (None, Some(acct)) => Some(format!("account={acct}")),
        (None, None) => Some("all pending".into()),
    };
    push_job(
        &state,
        JobInfo {
            id: id.clone(),
            kind: "acquire".into(),
            status: "accepted".into(),
            detail,
        },
    )
    .await;
    let job_id = id.clone();
    tokio::spawn(async move {
        set_job_status(&state, &job_id, "running", None).await;
        match run_acquire(&state, asin.as_deref(), account.as_deref()).await {
            Ok(detail) => {
                info!(%job_id, %detail, "acquire job finished");
                set_job_status(&state, &job_id, "succeeded", Some(detail)).await;
            }
            Err(err) => {
                error!(%job_id, error = %err, "acquire job failed");
                set_job_status(&state, &job_id, "failed", Some(err.to_string())).await;
                if let Some(diag) = bookclerk_config::diagnostics_global() {
                    diag.request_upload("job_failed");
                }
            }
        }
    });
    id
}

/// Run a scan synchronously (scheduler / tests).
pub async fn run_scan(state: &AppState, account: Option<&str>) -> anyhow::Result<String> {
    let _guard = state.work_lock.lock().await;
    let cfg = state.config.read().await.clone();
    let paths = cfg.paths();
    paths.ensure_dirs()?;
    let library = state.library.read().await.clone();
    let registry = default_registry_with_plugins(&cfg).await?;
    let summary = registry
        .scan_all(
            &library,
            ScanOptions {
                accounts: account.map(|a| vec![a.to_string()]).unwrap_or_default(),
                page_size: 50,
                import_episodes: cfg.library.import_episodes,
                import_plus_titles: cfg.library.import_plus_titles,
            },
        )
        .await?;
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
pub async fn run_acquire(
    state: &AppState,
    asin: Option<&str>,
    account: Option<&str>,
) -> anyhow::Result<String> {
    let _guard = state.work_lock.lock().await;
    let cfg = state.config.read().await.clone();
    let paths = cfg.paths();
    paths.ensure_dirs()?;
    let library = state.library.read().await.clone();
    let destinations = bookclerk_plugin::build_acquire_destinations(
        &cfg,
        Some(&library),
        &*state.destinations.read().await,
    )
    .await?;
    let storage = destinations.listing_backend()?;
    let options = DownloadOptions::from(&cfg);
    let registry = default_registry_with_plugins(&cfg).await?;

    // Match existing media first so auto-acquire does not re-download.
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
            asin.is_none_or(|a| {
                a.eq_ignore_ascii_case(&b.uuid)
                    || a.eq_ignore_ascii_case(&b.product_id)
                    || b.isbn.as_deref().is_some_and(|i| a.eq_ignore_ascii_case(i))
                    || b.asin.as_deref().is_some_and(|x| a.eq_ignore_ascii_case(x))
            })
        })
        .filter(|b| b.acquire_status != AcquireStatus::Acquired)
        .filter(|b| bookclerk_library::is_downloadable(&b.content_kind))
        .filter(|b| cfg.library.download_episodes || b.content_kind != "episode")
        .collect();

    if targets.is_empty() {
        return Ok("nothing to acquire".into());
    }

    let mut index = StorageIndex::from_storage(storage.as_ref()).await?;
    let mut ok = 0u32;
    let mut matched = 0u32;
    let mut failed = 0u32;
    let bad_book = cfg.output.bad_book_action;
    for book in targets {
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
    if failed > 0 && bad_book == BadBookAction::Retry {
        anyhow::bail!("{detail}");
    }
    Ok(detail)
}

async fn push_job(state: &AppState, job: JobInfo) {
    let mut jobs = state.jobs.write().await;
    jobs.push(job);
    const MAX_JOBS: usize = 100;
    if jobs.len() > MAX_JOBS {
        let drain = jobs.len() - MAX_JOBS;
        jobs.drain(0..drain);
    }
}

async fn set_job_status(state: &AppState, id: &str, status: &str, detail: Option<String>) {
    let mut jobs = state.jobs.write().await;
    if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
        job.status = status.into();
        if detail.is_some() {
            job.detail = detail;
        }
    }
}

fn new_job_id(kind: &str) -> String {
    format!("{kind}-{}", uuid::Uuid::new_v4())
}
