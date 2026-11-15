//! The `discover_request_queue` job: ranks the shared request queue and
//! stores the ranking, so `GET /api/request-queue` only reads.
//!
//! The queue is not personalized, so one job serves everyone. Discover's
//! taste-event consumer queues it on `wishlist_changed`, `book_acquired` and
//! `listening_synced` (debounced, deduplicated, so a burst folds into one
//! pass), and a read queues it when the stored ranking is missing an entry
//! or older than a day. See `docs/internals/discovery.md`, *Request queue*.

use std::time::Duration;

use bookclerk_library::{EnqueueJobSpec, JobKind, JobPayload, JobTrigger, LibraryStore};
use chrono::{DateTime, Utc};
use tracing::info;

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};

/// The payload every request-queue ranking carries: no subject, since one
/// ranking serves everyone.
fn payload(trigger: JobTrigger) -> JobPayload {
    JobPayload {
        trigger,
        ..JobPayload::default()
    }
}

/// Admits a ranking pass of the shared request queue, claimable after `delay`.
///
/// # Arguments
///
/// * `state` - Daemon state.
/// * `trigger` - Who asked (a read or the event consumer, both host-initiated).
/// * `delay` - How long the job waits before a worker may claim it.
///
/// # Errors
///
/// Returns the admission error.
pub async fn enqueue_request_queue_rank(
    state: &AppState,
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
            kind: JobKind::DiscoverRequestQueue,
            payload: payload(trigger),
            priority: 0,
            max_attempts,
            max_pending,
            run_after,
        },
    )
    .await
}

/// True when a ranking pass was admitted at or after `since`, so it reads
/// a change made then and needs no new job.
///
/// # Errors
///
/// Returns the library error.
pub async fn request_queue_ranked_since(
    library: &LibraryStore,
    since: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let key = JobKind::DiscoverRequestQueue.dedup_key(&JobPayload::default());
    Ok(library.job_admitted_since(&key, since).await?)
}

/// Runs a `discover_request_queue` job: ranks every open queue entry against
/// household taste and replaces the stored ranking.
///
/// # Errors
///
/// Fails on cancellation, a closed Discover gate, or a library error.
pub async fn run_discover_request_queue(
    state: &AppState,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    // Same options the route used when it ranked per request: local-hash
    // taste only, no ONNX, so the pass stays cheap and needs no model files.
    let mut opts = crate::discover_jobs::discover_options(
        &cfg,
        false,
        false,
        String::from(bookclerk_discover::MODEL_LOCAL_HASH_V1),
    );
    opts.fetch_storefront_candidates = false;
    opts.storefront_seed_limit = 0;
    opts.storefront_max_remote_calls = 0;
    // One taste ranking at a time, as the Discover feed jobs do.
    let _gate = state
        .discover_gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| anyhow::anyhow!("discover gate closed"))?;
    let ranks = bookclerk_discover::rank_request_queue_taste(&library, &opts).await?;
    library.replace_request_queue_ranks(&ranks).await?;
    let owned = ranks.iter().filter(|r| r.owned).count();
    info!(
        entries = ranks.len(),
        owned, "stored the request queue ranking"
    );
    Ok(format!("entries={} owned={owned}", ranks.len()))
}

#[cfg(test)]
mod tests {
    use axum::extract::State;
    use bookclerk_library::{JobState, NewTitleRequest, RequestStatus};

    use super::*;

    /// An open operator wish for `title` by `authors`.
    fn wish(title: &str, isbn: &str) -> NewTitleRequest {
        NewTitleRequest {
            uuid: None,
            identity_id: None,
            title: title.into(),
            authors: Some("Ada Author".into()),
            asin: None,
            isbn: Some(isbn.into()),
            notes: None,
            status: RequestStatus::Open,
            resolved_edition_id: None,
            cover_url: None,
        }
    }

    /// Pending ranking passes of the shared queue.
    async fn pending_passes(library: &LibraryStore) -> usize {
        library
            .list_jobs(100)
            .await
            .expect("jobs")
            .into_iter()
            .filter(|j| j.kind == JobKind::DiscoverRequestQueue && j.state == JobState::Pending)
            .count()
    }

    /// The job stores a ranking the route then only reads; a wish made after
    /// the pass still shows, and makes the route queue the next pass.
    #[tokio::test]
    async fn the_route_reads_the_stored_ranking() {
        let files = tempfile::tempdir().expect("tempdir");
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        let first = library
            .create_title_request(&wish("First", "9780000000001"))
            .await
            .expect("wish");

        // Nothing ranked yet: the read still answers, and queues a pass.
        let axum::Json(rows) = crate::api::list_request_queue(State(state.clone()))
            .await
            .expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(pending_passes(&library).await, 1);
        // Another read folds into the waiting pass.
        let _ = crate::api::list_request_queue(State(state.clone()))
            .await
            .expect("read");
        assert_eq!(pending_passes(&library).await, 1);

        let summary = run_discover_request_queue(&state, None)
            .await
            .expect("pass");
        assert_eq!(summary, "entries=1 owned=0");
        let stored = library.list_request_queue_ranks().await.expect("ranks");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].rank.work_id, first.work_id);
        let queue = bookclerk_discover::read_request_queue(&library)
            .await
            .expect("read");
        assert!(!queue.stale, "every entry is ranked");
        assert_eq!(queue.entries[0].reasons, vec!["on the wishlist"]);

        // A new wish shows before the next pass and marks the ranking stale.
        library
            .create_title_request(&wish("Second", "9780000000002"))
            .await
            .expect("wish");
        let queue = bookclerk_discover::read_request_queue(&library)
            .await
            .expect("read");
        assert!(queue.stale);
        assert_eq!(queue.entries.len(), 2);
    }
}
