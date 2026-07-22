//! Background job runners for scan / liberate.

use std::sync::Arc;

use libation_audible::DownloadOptions;
use libation_config::BadBookAction;
use libation_liberate::{liberate_book_indexed, LiberateRequest, ReconcileOptions, StorageIndex};
use libation_library::LiberateStatus;
use libation_source::{ScanOptions, SourceKind};
use libation_storage::from_config;
use tracing::{error, info, warn};

use crate::api::{AppState, JobInfo};
use crate::registry::default_registry;

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
                if let Some(diag) = libation_config::diagnostics_global() {
                    diag.request_upload("job_failed");
                }
            }
        }
    });
    id
}

/// Enqueue liberate for pending titles (optional ASIN / account filter).
pub async fn enqueue_liberate(
    state: Arc<AppState>,
    asin: Option<String>,
    account: Option<String>,
) -> String {
    let id = new_job_id("liberate");
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
            kind: "liberate".into(),
            status: "accepted".into(),
            detail,
        },
    )
    .await;
    let job_id = id.clone();
    tokio::spawn(async move {
        set_job_status(&state, &job_id, "running", None).await;
        match run_liberate(&state, asin.as_deref(), account.as_deref()).await {
            Ok(detail) => {
                info!(%job_id, %detail, "liberate job finished");
                set_job_status(&state, &job_id, "succeeded", Some(detail)).await;
            }
            Err(err) => {
                error!(%job_id, error = %err, "liberate job failed");
                set_job_status(&state, &job_id, "failed", Some(err.to_string())).await;
                if let Some(diag) = libation_config::diagnostics_global() {
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
    let registry = default_registry();
    let summary = registry
        .scan_all(
            &paths.files_dir,
            &state.library,
            ScanOptions {
                accounts: account.map(|a| vec![a.to_string()]).unwrap_or_default(),
                page_size: 50,
                import_episodes: cfg.library.import_episodes,
                import_plus_titles: cfg.library.import_plus_titles,
            },
        )
        .await?;
    if cfg.library.enrich_from_audible {
        if let Err(err) = libation_enrich::enrich_books_from_audible(
            &state.library,
            cfg.library.enrich_min_confidence,
        )
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

/// Liberate pending titles synchronously.
pub async fn run_liberate(
    state: &AppState,
    asin: Option<&str>,
    account: Option<&str>,
) -> anyhow::Result<String> {
    let _guard = state.work_lock.lock().await;
    let cfg = state.config.read().await.clone();
    let paths = cfg.paths();
    paths.ensure_dirs()?;
    let storage = from_config(&cfg).await?;
    let options = DownloadOptions::from(&cfg);
    let registry = default_registry();

    // Match existing media first so auto-liberate does not re-download.
    let _ = libation_liberate::reconcile_library(
        &state.library,
        storage.as_ref(),
        ReconcileOptions {
            account: account.map(str::to_string),
            clear_missing: true,
            download: options.clone(),
            ..Default::default()
        },
    )
    .await?;

    let books = state.library.list_books(account)?;
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
        .filter(|b| b.liberate_status != LiberateStatus::Liberated)
        .filter(|b| libation_library::is_downloadable(&b.content_kind))
        .filter(|b| cfg.library.download_episodes || b.content_kind != "episode")
        .collect();

    if targets.is_empty() {
        return Ok("nothing to liberate".into());
    }

    let mut index = StorageIndex::from_storage(storage.as_ref()).await?;
    let mut ok = 0u32;
    let mut matched = 0u32;
    let mut failed = 0u32;
    let bad_book = cfg.download.bad_book_action;
    for book in targets {
        let source_kind = SourceKind::parse(&book.source).unwrap_or(SourceKind::Audible);
        let content_source = registry.require(source_kind).ok();
        let req = LiberateRequest {
            asin: book.download_product_id().to_string(),
            book_uuid: Some(book.uuid.clone()),
            source: source_kind,
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
            preloaded_license: None,
        };
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            match liberate_book_indexed(
                &state.library,
                storage.as_ref(),
                req.clone(),
                Some(&mut index),
                content_source.as_deref(),
            )
            .await
            {
                Ok(result) if result.matched_existing => {
                    info!(asin = %result.asin, key = %result.storage_key, "matched existing");
                    matched += 1;
                    break;
                }
                Ok(result) => {
                    info!(asin = %result.asin, key = %result.storage_key, "liberated");
                    ok += 1;
                    break;
                }
                Err(err) => {
                    if bad_book == BadBookAction::Retry && attempts < 2 {
                        warn!(asin = %book.asin_or_isbn(), error = %err, "liberate failed; retrying");
                        continue;
                    }
                    warn!(asin = %book.asin_or_isbn(), error = %err, "liberate failed");
                    failed += 1;
                    if matches!(bad_book, BadBookAction::Ask | BadBookAction::Abort) {
                        anyhow::bail!("liberate aborted on {}: {err}", book.asin_or_isbn());
                    }
                    break;
                }
            }
        }
    }
    let detail = format!("liberated={ok} matched={matched} failed={failed}");
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
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{kind}-{secs}")
}
