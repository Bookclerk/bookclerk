//! Discover's two queued jobs and how they are admitted.
//!
//! The page never builds shelves itself any more. A `discover_candidates`
//! job fetches one subject's storefront candidates
//! ([`bookclerk_discover::fetch_subject_candidates`]) and, when it finishes,
//! admits that subject's `discover_feed` job, which ranks them into stored
//! shelves ([`bookclerk_discover::rank_subject_feed`]) with no network.
//! Both are deduplicated per subject, so a burst of triggers folds into the
//! job already waiting. See `docs/internals/discovery.md`, *Precomputed shelves*.

use std::time::Duration;

use bookclerk_library::{DiscoverSubject, EnqueueJobSpec, JobKind, JobPayload, JobTrigger};
use chrono::Utc;
use tracing::info;

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};

/// How old a subject's candidates may get before a read queues a refresh
/// (the daily refresh; prices on cards still load live).
pub const CANDIDATES_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);

/// Builds the ranking options the jobs use from the live `[discovery]` config.
///
/// # Arguments
///
/// * `cfg` - Live configuration.
/// * `use_onnx` - Whether ranking may load the ONNX embedder.
/// * `similarity` - Whether embedding similarity scores candidates (local-hash
///   unless `use_onnx`).
/// * `embedding_model` - Model id the owned works were embedded with.
pub(crate) fn discover_options(
    cfg: &bookclerk_config::Config,
    use_onnx: bool,
    similarity: bool,
    embedding_model: String,
) -> bookclerk_discover::RecommendOptions {
    bookclerk_discover::RecommendOptions {
        limit: cfg.discovery.recommend_limit.max(36),
        embedding_model,
        region: String::from("us"),
        include_purchase_hints: false,
        external_user_id: None,
        include_listening: true,
        listening_providers: Vec::new(),
        fetch_storefront_candidates: cfg.discovery.storefront_candidates,
        storefront_seed_limit: cfg.discovery.storefront_seed_limit,
        storefront_max_remote_calls: cfg.discovery.storefront_max_remote_calls,
        exclude_graphicaudio_series_sets: cfg.discovery.exclude_graphicaudio_series_sets,
        disabled_shelves: Vec::new(),
        models_dir: use_onnx.then(|| cfg.paths().models_dir.clone()),
        embed_intra_threads: cfg.discovery.embed_intra_threads,
        embeddings_enabled: similarity,
    }
}

/// Admits one Discover job for `subject`, claimable after `delay`.
///
/// # Errors
///
/// Returns the admission error.
async fn enqueue(
    state: &AppState,
    kind: JobKind,
    subject: &DiscoverSubject,
    trigger: JobTrigger,
    delay: Duration,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    let run_after = (!delay.is_zero())
        .then(|| chrono::Duration::from_std(delay).ok())
        .flatten()
        .map(|d| Utc::now() + d);
    admit(
        state,
        EnqueueJobSpec {
            kind,
            payload: JobPayload {
                trigger,
                subject: Some(subject.key()),
                ..JobPayload::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after,
        },
    )
    .await
}

/// Admits a storefront candidate fetch for `subject`.
///
/// # Errors
///
/// Returns the admission error.
pub async fn enqueue_discover_candidates(
    state: &AppState,
    subject: &DiscoverSubject,
    trigger: JobTrigger,
    delay: Duration,
) -> anyhow::Result<AdmitJob> {
    enqueue(state, JobKind::DiscoverCandidates, subject, trigger, delay).await
}

/// Admits a feed ranking for `subject`.
///
/// # Errors
///
/// Returns the admission error.
pub async fn enqueue_discover_feed(
    state: &AppState,
    subject: &DiscoverSubject,
    trigger: JobTrigger,
    delay: Duration,
) -> anyhow::Result<AdmitJob> {
    enqueue(state, JobKind::DiscoverFeed, subject, trigger, delay).await
}

/// Parses the subject a Discover job carries.
fn job_subject(payload: &JobPayload) -> anyhow::Result<DiscoverSubject> {
    payload
        .subject
        .as_deref()
        .and_then(DiscoverSubject::parse)
        .ok_or_else(|| anyhow::anyhow!("discover job has no valid subject"))
}

/// Drops a deleted subject's Discover rows when it no longer exists.
///
/// Called before a job does any work and again after it wrote, so a job
/// queued (or still running) when its user or portal identity was deleted
/// leaves nothing behind for the daily refresh to pick up again.
///
/// # Returns
///
/// `true` when the subject is gone (and its rows were deleted).
///
/// # Errors
///
/// Returns the library error.
async fn forget_if_removed(
    library: &bookclerk_library::LibraryStore,
    subject: &DiscoverSubject,
) -> anyhow::Result<bool> {
    if library.discover_subject_exists(subject).await? {
        return Ok(false);
    }
    library.forget_discover_subject(subject).await?;
    Ok(true)
}

/// Runs a `discover_candidates` job, then admits the subject's ranking.
///
/// # Errors
///
/// Fails on a missing subject, cancellation, or a library error. A store
/// that fails is reported in the summary, not as a job failure.
pub async fn run_discover_candidates(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let subject = job_subject(payload)?;
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    if forget_if_removed(&library, &subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    let opts = discover_options(
        &cfg,
        false,
        false,
        String::from(bookclerk_discover::MODEL_LOCAL_HASH_V1),
    );
    let report = if opts.fetch_storefront_candidates {
        let sources = state.sources.read().await.clone();
        bookclerk_discover::fetch_subject_candidates(&library, &sources, &subject, &opts).await?
    } else {
        // Nothing to fetch, but the fetch counts as done for the day.
        library.mark_discover_fetch(&subject, Utc::now()).await?;
        bookclerk_discover::FetchReport::default()
    };
    if forget_if_removed(&library, &subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    // The ranking reads what was just stored; a failure to admit it leaves
    // the next read to queue it.
    if let Err(err) = enqueue_discover_feed(state, &subject, payload.trigger, Duration::ZERO).await
    {
        tracing::warn!(subject = %subject.key(), error = %err, "could not queue the discover ranking");
    }
    let pruned = library
        .prune_discover_store_calls(Utc::now() - bookclerk_discover::STORE_CALL_MAX_AGE)
        .await
        .unwrap_or(0);
    // Catalog listings no candidate, cached call, holding or override
    // refers to any more, and no store has reported for a while. Every
    // subject's fetch ends here, so the scan runs at most once an hour per
    // daemon rather than once per subject.
    let pruned_listings = if catalog_prune_due(Utc::now()) {
        library
            .prune_catalog_listings(Utc::now() - CATALOG_LISTING_RETENTION)
            .await
            .unwrap_or(0)
    } else {
        0
    };
    Ok(format!(
        "subject={} seeds={} stores={} candidates={} failed={} pruned_calls={pruned} \
         pruned_listings={pruned_listings}",
        subject.key(),
        report.seeds,
        report.stores_saved,
        report.candidates,
        report.stores_failed.join(","),
    ))
}

/// How long an unreferenced catalog listing is kept after a store last
/// reported it, so a title that drops off one subject's shelves and comes
/// back within the week is not resolved again.
const CATALOG_LISTING_RETENTION: chrono::Duration = chrono::Duration::days(7);

/// Least time between two catalog listing prunes on one daemon.
const CATALOG_PRUNE_INTERVAL: chrono::Duration = chrono::Duration::hours(1);

/// Unix seconds of this daemon's last catalog listing prune (0: never).
static LAST_CATALOG_PRUNE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Whether this daemon should prune catalog listings now, claiming the
/// slot when it should so concurrent jobs do not prune together.
///
/// # Arguments
///
/// * `now` - The current time.
fn catalog_prune_due(now: chrono::DateTime<Utc>) -> bool {
    use std::sync::atomic::Ordering;
    let now_secs = now.timestamp();
    let last = LAST_CATALOG_PRUNE.load(Ordering::Relaxed);
    now_secs.saturating_sub(last) >= CATALOG_PRUNE_INTERVAL.num_seconds()
        && LAST_CATALOG_PRUNE
            .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// Most rankings one `discover_feed` job runs when candidate fetches keep
/// finishing under it; past this the next read or event queues another.
const MAX_RANK_PASSES: u32 = 3;

/// Runs a `discover_feed` job: ranks stored candidates into stored shelves,
/// again when a candidate fetch finished during the ranking.
///
/// # Errors
///
/// Fails on a missing subject, cancellation, or a library error.
pub async fn run_discover_feed(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let subject = job_subject(payload)?;
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    if forget_if_removed(&library, &subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    let has_books = library.count_books(None).await? > 0;
    let use_onnx = has_books && cfg.discovery.embeddings_enabled;
    // One embedder at a time, as the request-queue ranking job does.
    let _gate = state
        .discover_gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| anyhow::anyhow!("discover gate closed"))?;
    // Owned works are embedded with local-hash, or with ONNX MiniLM when
    // `discovery.embeddings_enabled` opts into it; without ONNX this opens the
    // local-hash embedder straight away.
    let model_id = if has_books {
        let models_dir = cfg.paths().models_dir.clone();
        let threads = cfg.discovery.embed_intra_threads;
        let mut embedder = tokio::task::spawn_blocking(move || {
            bookclerk_discover::open_embedder(&models_dir, threads, use_onnx)
        })
        .await??;
        let model_id = embedder.model_id().to_string();
        let _ = bookclerk_discover::embed_dirty_works(&library, embedder.as_mut()).await;
        model_id
    } else {
        String::from(bookclerk_discover::MODEL_LOCAL_HASH_V1)
    };
    let opts = discover_options(&cfg, use_onnx, has_books, model_id);
    let sources = state.sources.read().await.clone();
    // A candidate fetch that finishes while this ranking runs folds its
    // follow-up ranking into this job (the dedup key covers running jobs),
    // so rank again until the candidates read are the newest stored.
    let mut passes = 0;
    let report = loop {
        passes += 1;
        let read = library.newest_discover_candidate_fetch(&subject).await?;
        let report =
            bookclerk_discover::rank_subject_feed(&library, &sources, &subject, &opts).await?;
        if passes >= MAX_RANK_PASSES
            || library.newest_discover_candidate_fetch(&subject).await? == read
        {
            break report;
        }
    };
    if forget_if_removed(&library, &subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    info!(
        subject = %subject.key(),
        scope = report.scope.as_str(),
        shelves = report.shelves,
        cards = report.cards,
        bytes = report.stored_bytes,
        "stored discover feed"
    );
    // Open Discover pages on this node re-read at once; other nodes see the
    // new `computed_at` on their next revision read.
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Discover);
    Ok(format!(
        "subject={} scope={} shelves={} candidates={} passes={passes}",
        subject.key(),
        report.scope.as_str(),
        report.shelves,
        report.candidates
    ))
}

/// True when a fetch or ranking for `subject` is waiting or running, so the
/// page can say its shelves are being rebuilt.
///
/// # Errors
///
/// Returns the library read error.
pub async fn subject_refresh_active(
    library: &bookclerk_library::LibraryStore,
    subject: &DiscoverSubject,
) -> anyhow::Result<bool> {
    let payload = JobPayload {
        subject: Some(subject.key()),
        ..JobPayload::default()
    };
    for kind in [JobKind::DiscoverCandidates, JobKind::DiscoverFeed] {
        if library.job_is_active(&kind.dedup_key(&payload)).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What a read of a subject's stored feed should queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedRefresh {
    /// Nothing: the feed is current.
    None,
    /// Rank again from stored candidates (they are newer than the feed).
    Rank,
    /// Fetch candidates (none yet, or older than [`CANDIDATES_MAX_AGE`]).
    Fetch,
}

/// Decides what a read should queue for a subject.
///
/// # Arguments
///
/// * `feed` - The stored feed, when one decodes.
/// * `newest_candidates` - Newest stored candidate fetch, when any.
/// * `now` - The current time.
#[must_use]
pub fn refresh_needed(
    feed: Option<&bookclerk_discover::StoredSubjectFeed>,
    newest_candidates: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> FeedRefresh {
    let Some(fetched) = newest_candidates else {
        return FeedRefresh::Fetch;
    };
    if fetched + CANDIDATES_MAX_AGE <= now {
        return FeedRefresh::Fetch;
    }
    match feed {
        None => FeedRefresh::Rank,
        Some(feed) if feed.candidates_fetched_at.is_none_or(|used| used < fetched) => {
            FeedRefresh::Rank
        }
        Some(_) => FeedRefresh::None,
    }
}

/// Queues whatever [`refresh_needed`] asks for, logging (not failing) on error.
pub async fn queue_refresh(state: &AppState, subject: &DiscoverSubject, refresh: FeedRefresh) {
    let result = match refresh {
        FeedRefresh::None => return,
        FeedRefresh::Rank => {
            enqueue_discover_feed(state, subject, JobTrigger::Api, Duration::ZERO).await
        }
        FeedRefresh::Fetch => {
            enqueue_discover_candidates(state, subject, JobTrigger::Api, Duration::ZERO).await
        }
    };
    if let Err(err) = result {
        tracing::warn!(subject = %subject.key(), error = %err, "could not queue a discover refresh");
    }
}

/// Queues a candidate fetch for every subject with a stored feed whose
/// candidates are older than [`CANDIDATES_MAX_AGE`] (the daily refresh).
///
/// # Returns
///
/// How many fetches were admitted. Failures are logged, not returned.
pub async fn queue_daily_refresh(state: &AppState) -> usize {
    let library = state.library_snapshot().await;
    let subjects = match library.list_discover_feed_subjects().await {
        Ok(subjects) => subjects,
        Err(err) => {
            tracing::warn!(error = %err, "discover refresh could not list subjects");
            return 0;
        }
    };
    let now = Utc::now();
    let mut queued = 0;
    for subject in subjects {
        match forget_if_removed(&library, &subject).await {
            Ok(false) => {}
            Ok(true) => continue,
            Err(err) => {
                tracing::warn!(subject = %subject.key(), error = %err, "discover refresh could not check a subject");
                continue;
            }
        }
        let newest = library
            .newest_discover_candidate_fetch(&subject)
            .await
            .ok()
            .flatten();
        if newest.is_some_and(|at| at + CANDIDATES_MAX_AGE > now) {
            continue;
        }
        match enqueue_discover_candidates(state, &subject, JobTrigger::Scheduler, Duration::ZERO)
            .await
        {
            Ok(_) => queued += 1,
            Err(err) => {
                tracing::warn!(subject = %subject.key(), error = %err, "daily discover refresh enqueue failed");
            }
        }
    }
    queued
}

#[cfg(test)]
mod tests {
    use bookclerk_library::SeedScope;

    use super::*;

    #[test]
    fn the_catalog_prune_runs_at_most_once_an_interval() {
        // Far in the future, so no other test's real-time claim interferes.
        let t = Utc::now() + chrono::Duration::days(3650);
        assert!(catalog_prune_due(t));
        assert!(!catalog_prune_due(t + chrono::Duration::minutes(5)));
        assert!(catalog_prune_due(t + CATALOG_PRUNE_INTERVAL));
    }

    fn feed(used: Option<chrono::DateTime<Utc>>) -> bookclerk_discover::StoredSubjectFeed {
        bookclerk_discover::StoredSubjectFeed {
            feed: bookclerk_discover::DiscoverFeed {
                shelves: Vec::new(),
                shelf_kinds: Vec::new(),
                state: None,
                computed_at: None,
            },
            scope: SeedScope::Own,
            candidates_fetched_at: used,
            computed_at: Utc::now(),
        }
    }

    #[test]
    fn a_read_queues_only_what_is_stale() {
        let now = Utc::now();
        let fresh = now - chrono::Duration::hours(1);
        let old = now - chrono::Duration::hours(25);
        assert_eq!(refresh_needed(None, None, now), FeedRefresh::Fetch);
        assert_eq!(
            refresh_needed(Some(&feed(Some(old))), Some(old), now),
            FeedRefresh::Fetch
        );
        assert_eq!(refresh_needed(None, Some(fresh), now), FeedRefresh::Rank);
        assert_eq!(
            refresh_needed(Some(&feed(Some(old))), Some(fresh), now),
            FeedRefresh::Rank
        );
        assert_eq!(
            refresh_needed(Some(&feed(None)), Some(fresh), now),
            FeedRefresh::Rank
        );
        assert_eq!(
            refresh_needed(Some(&feed(Some(fresh))), Some(fresh), now),
            FeedRefresh::None
        );
    }

    #[test]
    fn job_subjects_must_parse() {
        let payload = JobPayload {
            subject: Some(String::from("user:3")),
            ..JobPayload::default()
        };
        assert_eq!(job_subject(&payload).ok(), Some(DiscoverSubject::User(3)));
        assert!(job_subject(&JobPayload::default()).is_err());
        let bad = JobPayload {
            subject: Some(String::from("user:x")),
            ..JobPayload::default()
        };
        assert!(job_subject(&bad).is_err());
    }

    /// Answers one GET on the daemon's router as JSON.
    async fn get(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Sends an empty JSON `POST` to `uri` and decodes the JSON answer.
    async fn post(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Sends a JSON `POST` of `body` to `uri` and decodes the JSON answer.
    async fn post_json(
        app: &axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// The uuid of the one library title stored.
    async fn book_uuid(library: &bookclerk_library::LibraryStore) -> String {
        library.list_books(None).await.unwrap().remove(0).uuid
    }

    /// Pending Discover jobs as `(kind, subject)` pairs.
    async fn queued(library: &bookclerk_library::LibraryStore) -> Vec<(JobKind, String)> {
        let mut jobs: Vec<(JobKind, String)> = library
            .list_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|j| matches!(j.kind, JobKind::DiscoverCandidates | JobKind::DiscoverFeed))
            .filter(|j| j.state == bookclerk_library::JobState::Pending)
            .map(|j| (j.kind, j.payload.subject.unwrap_or_default()))
            .collect();
        jobs.sort_by_key(|(kind, subject)| (kind.as_str().to_string(), subject.clone()));
        jobs
    }

    /// A wish records the library work it resolves to, and a sequel of an
    /// owned book gets a catalog work of its own.
    #[tokio::test]
    async fn a_wish_stores_the_work_it_resolves_to() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        library
            .upsert_account("libro", "reader@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut book =
            bookclerk_library::NewBook::minimal("p1", "reader@example.com", "us", "Dune");
        book.source = "libro".into();
        book.plugin_key = "libro".into();
        book.authors = Some("Frank Herbert".into());
        library.upsert_book(&book).await.unwrap();
        let app = crate::api::router(std::sync::Arc::clone(&state), None);

        let wish = |title: &str| serde_json::json!({ "title": title, "authors": "Frank Herbert" });
        let (status, owned) = post_json(&app, "/api/wishlist", wish("Dune")).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{owned}");
        let dune_work = library.work_id_for_book(&book_uuid(&library).await).await;
        assert_eq!(
            owned["work_id"].as_str(),
            dune_work.unwrap().as_deref(),
            "{owned}"
        );
        let (status, sequel) = post_json(&app, "/api/wishlist", wish("Dune Messiah")).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{sequel}");
        assert!(sequel["work_id"].is_string(), "{sequel}");
        assert_ne!(sequel["work_id"], owned["work_id"], "{sequel}");
    }

    /// A wish whose title is over the listing limits names no work, so it is a
    /// bad request.
    #[tokio::test]
    async fn an_overlong_wish_title_is_a_bad_request() {
        let files = tempfile::tempdir().unwrap();
        let (state, _library) = crate::node_api::tests::test_state(&files).await;
        let app = crate::api::router(std::sync::Arc::clone(&state), None);
        let long = "x".repeat(10_000);
        let (status, body) =
            post_json(&app, "/api/wishlist", serde_json::json!({ "title": long })).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn the_page_reads_stored_shelves_and_queues_what_is_stale() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state_with(
            &files,
            "[discovery]\nembeddings_enabled = false\n",
        )
        .await;
        let app = crate::api::router(std::sync::Arc::clone(&state), None);

        // An empty library answers at once and queues nothing.
        let (status, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{feed}");
        assert_eq!(feed["state"], "ready");
        assert!(queued(&library).await.is_empty());

        library
            .upsert_account("libro", "reader@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut book =
            bookclerk_library::NewBook::minimal("p1", "reader@example.com", "us", "Dune");
        book.source = "libro".into();
        book.plugin_key = "libro".into();
        library.upsert_book(&book).await.unwrap();

        // No feed yet: placeholders, and the fetch is queued once.
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "preparing");
        assert!(feed["shelves"].as_array().unwrap().is_empty());
        get(&app, "/api/discover/recommendations").await;
        assert_eq!(
            queued(&library).await,
            vec![(JobKind::DiscoverCandidates, String::from("operator"))]
        );

        // The fetch runs (no stores deployed) and admits the ranking.
        let payload = JobPayload {
            subject: Some(String::from("operator")),
            ..JobPayload::default()
        };
        let summary = run_discover_candidates(&state, &payload, None)
            .await
            .unwrap();
        assert!(summary.starts_with("subject=operator"), "{summary}");
        assert!(queued(&library)
            .await
            .contains(&(JobKind::DiscoverFeed, String::from("operator"))));
        let summary = run_discover_feed(&state, &payload, None).await.unwrap();
        assert!(summary.contains("scope=household"), "{summary}");
        assert!(summary.ends_with("passes=1"), "{summary}");

        // The handlers ran here, not under the worker, so their rows are
        // still pending: the page reports the rebuild as in progress.
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "refreshing");
        let (_, report) = get(&app, "/api/discover/status").await;
        assert_eq!(report["updating"], true);
        {
            use sea_orm::ConnectionTrait;
            library
                .db()
                .execute_unprepared(
                    "UPDATE jobs SET state = 'succeeded' \
                     WHERE kind IN ('discover_candidates', 'discover_feed')",
                )
                .await
                .unwrap();
        }

        // A fetch that found nothing still counts as fresh: the feed is ready
        // and nothing more is queued.
        let jobs_before = queued(&library).await;
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "ready");
        assert!(feed["computed_at"].is_string());
        assert_eq!(queued(&library).await, jobs_before);

        let (status, report) = get(&app, "/api/discover/status").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{report}");
        assert_eq!(report["subject"], "operator");
        assert_eq!(report["ready"], true);
        assert_eq!(report["scope"], "household");
        assert!(report["candidates_fetched_at"].is_string());
        assert_eq!(report["updating"], false);

        // The Refresh button queues a fetch now and reports it.
        let (status, report) = post(&app, "/api/discover/refresh").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{report}");
        assert_eq!(report["updating"], true);
        assert_eq!(
            queued(&library).await,
            vec![(JobKind::DiscoverCandidates, String::from("operator"))]
        );
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "refreshing");
        // A second press finds the fetch waiting, which counts as queued.
        let (status, report) = post(&app, "/api/discover/refresh").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{report}");
        assert_eq!(queued(&library).await.len(), 1);
        {
            use sea_orm::ConnectionTrait;
            library
                .db()
                .execute_unprepared("UPDATE jobs SET state = 'succeeded'")
                .await
                .unwrap();
        }
        // A full job queue is reported, not answered as queued.
        let max_pending = std::mem::replace(&mut state.config.write().await.jobs.max_pending, 0);
        let (status, _) = post(&app, "/api/discover/refresh").await;
        assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert!(queued(&library).await.is_empty());
        state.config.write().await.jobs.max_pending = max_pending;

        // A feed another release wrote, which does not decode, counts as
        // missing, so the page shows placeholders until a ranking replaces it.
        library
            .put_discover_feed(
                &DiscoverSubject::Operator,
                "not a feed",
                SeedScope::Household,
                Some(Utc::now()),
            )
            .await
            .unwrap();
        let (_, report) = get(&app, "/api/discover/status").await;
        assert_eq!(report["ready"], false);
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "preparing");

        // A job without a subject fails instead of guessing one.
        assert!(run_discover_feed(&state, &JobPayload::default(), None)
            .await
            .is_err());
        assert!(
            run_discover_candidates(&state, &JobPayload::default(), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_job_for_a_deleted_user_writes_nothing_back() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        let gone = DiscoverSubject::User(9_999);
        library
            .put_discover_feed(&gone, "{}", SeedScope::Own, None)
            .await
            .unwrap();
        let payload = JobPayload {
            subject: Some(gone.key()),
            ..JobPayload::default()
        };
        let summary = run_discover_candidates(&state, &payload, None)
            .await
            .unwrap();
        assert!(summary.ends_with("removed"), "{summary}");
        let summary = run_discover_feed(&state, &payload, None).await.unwrap();
        assert!(summary.ends_with("removed"), "{summary}");
        assert!(library.get_discover_feed(&gone).await.unwrap().is_none());
        assert!(library
            .newest_discover_candidate_fetch(&gone)
            .await
            .unwrap()
            .is_none());
        assert!(queued(&library).await.is_empty());
    }

    #[tokio::test]
    async fn the_daily_refresh_queues_only_stale_subjects() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        assert_eq!(queue_daily_refresh(&state).await, 0);
        let mut ids = Vec::new();
        for name in ["Fresh", "Stale"] {
            let user = library
                .create_user(bookclerk_library::UserRole::Member, Some(name), None)
                .await
                .unwrap();
            ids.push(user.id);
        }
        let fresh = DiscoverSubject::User(ids[0]);
        let stale = DiscoverSubject::User(ids[1]);
        // A deleted user's leftover feed is dropped, not refreshed.
        let deleted = DiscoverSubject::User(9_999);
        for subject in [&fresh, &stale, &deleted] {
            library
                .put_discover_feed(subject, "{}", SeedScope::Own, None)
                .await
                .unwrap();
        }
        library
            .mark_discover_fetch(&fresh, Utc::now())
            .await
            .unwrap();
        library
            .mark_discover_fetch(&stale, Utc::now() - chrono::Duration::hours(25))
            .await
            .unwrap();
        assert_eq!(queue_daily_refresh(&state).await, 1);
        assert_eq!(
            queued(&library).await,
            vec![(JobKind::DiscoverCandidates, stale.key())]
        );
        assert!(library.get_discover_feed(&deleted).await.unwrap().is_none());
    }
}
