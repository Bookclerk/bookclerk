//! The `cache_sweep` job: one node's image-cache eviction and job-scratch
//! cleanup, queued by that node and claimed by it alone.
//!
//! Every heartbeat asks the cover service whether a sweep is due
//! ([`crate::covers::CoverService::maintain`]) and, when it is, admits a
//! `cache_sweep` job pinned to this node ([`JobPayload::node_id`]). The
//! dedup key (`cache_sweep:node=<id>`) folds a sweep that comes due while
//! another is still queued into that one; one that comes due while a sweep
//! runs is asked for again on the next heartbeat, so it uses the limits in
//! force by then. The job runs the
//! eviction sweep, then deletes job scratch directories under the download
//! cache that no active job has registered and that have been unchanged for
//! [`SCRATCH_GRACE`]; at startup the same cleanup runs without the grace.
//!
//! When the job cannot be queued (the queue is full, or the database
//! refuses the write), the heartbeat sweeps the image cache in the
//! background instead, so retention never depends on the queue.

use anyhow::Context;
use bookclerk_library::{EnqueueJobSpec, JobKind, JobPayload, JobState, JobTrigger};
use tracing::warn;

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::job_worker::{sweep_orphan_temp_dirs, SCRATCH_GRACE};
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};

/// Claim priority of a sweep, above the default 0, so a long backlog of
/// store work does not hold back eviction on a filling disk.
const SWEEP_PRIORITY: i64 = 100;

/// Admits a `cache_sweep` job pinned to `node_id`.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `node_id` - This node, the only one that may claim the job.
///
/// # Returns
///
/// The admission outcome; a sweep already queued or running for the node
/// is a [`AdmitJob::Duplicate`].
///
/// # Errors
///
/// Returns the admission error.
pub(crate) async fn enqueue_cache_sweep(
    state: &AppState,
    node_id: &str,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    admit(
        state,
        EnqueueJobSpec {
            kind: JobKind::CacheSweep,
            payload: JobPayload {
                trigger: JobTrigger::Scheduler,
                node_id: Some(node_id.to_owned()),
                ..JobPayload::default()
            },
            priority: SWEEP_PRIORITY,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Queues a sweep for `node_id`, or sweeps the image cache in the
/// background when the job cannot be queued.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `node_id` - This node.
pub(crate) async fn queue_cache_sweep(state: &AppState, node_id: &str) {
    match enqueue_cache_sweep(state, node_id).await {
        Ok(AdmitJob::Created(_)) => {}
        Ok(AdmitJob::Duplicate(id)) => {
            // A queued sweep reads the limits when it starts, so it covers
            // this request. One already running read them before `maintain`
            // installed the new ones: ask again on the next heartbeat.
            let library = state.library_snapshot().await;
            let still_queued = matches!(
                library.get_job(&id).await,
                Ok(Some(job)) if job.state == JobState::Pending
            );
            if !still_queued {
                state.covers.request_sweep();
            }
        }
        Ok(AdmitJob::QueueFull) => {
            warn!("job queue is full; sweeping the image cache without a job");
            state.covers.sweep_in_background();
        }
        Err(err) => {
            warn!(error = %err, "could not queue the cache sweep; sweeping without a job");
            state.covers.sweep_in_background();
        }
    }
}

/// Runs a `cache_sweep` job on the node it is pinned to.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `payload` - The job's payload, naming its node.
/// * `ctx` - The claim, for cancellation.
///
/// # Returns
///
/// A one-line summary for the job row.
///
/// # Errors
///
/// Fails when the job names another node (or none), when it is cancelled,
/// or when the scratch directories cannot be listed.
pub(crate) async fn run_cache_sweep(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let this_node = bookclerk_library::control_plane::load_or_create_node_identity(&files_dir)
        .context("node identity")?
        .node_id;
    match payload.node_id.as_deref() {
        Some(pinned) if pinned == this_node => {}
        Some(pinned) => anyhow::bail!("cache_sweep for node {pinned} claimed on {this_node}"),
        None => anyhow::bail!("cache_sweep names no node"),
    }
    let library = state.library_snapshot().await;
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let covers = match state.covers.sweep().await {
        Some(report) => format!(
            "covers evicted={} bytes={}->{}{}",
            report.evicted,
            report.before,
            report.after,
            if report.low_disk { " low_disk" } else { "" }
        ),
        None => String::from("covers skipped (a sweep was already running)"),
    };
    let cache_dir = state.config.read().await.download_cache_dir();
    let scratch = sweep_orphan_temp_dirs(&library, &cache_dir, SCRATCH_GRACE).await?;
    Ok(format!("{covers}; scratch removed={scratch}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_sweep_is_queued_once_per_node_and_runs_only_there() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state_with(&files, "").await;
        let files_dir = state.config.read().await.paths().files_dir.clone();
        let node = bookclerk_library::control_plane::load_or_create_node_identity(&files_dir)
            .unwrap()
            .node_id;

        let AdmitJob::Created(id) = enqueue_cache_sweep(&state, &node).await.unwrap() else {
            panic!("expected a new job");
        };
        assert_eq!(
            enqueue_cache_sweep(&state, &node).await.unwrap(),
            AdmitJob::Duplicate(id.clone())
        );
        // The heartbeat's path folds into the queued job as well.
        queue_cache_sweep(&state, &node).await;
        let job = library.get_job(&id).await.unwrap().unwrap();
        assert_eq!(job.kind, JobKind::CacheSweep);
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.priority, SWEEP_PRIORITY);
        assert_eq!(job.payload.node_id.as_deref(), Some(node.as_str()));
        assert_eq!(job.payload.trigger, JobTrigger::Scheduler);

        // A scratch directory younger than the grace period stays.
        let cache_dir = state.config.read().await.download_cache_dir();
        let young = cache_dir.join("acquire").join("just-made");
        tokio::fs::create_dir_all(&young).await.unwrap();
        let summary = run_cache_sweep(&state, &job.payload, None).await.unwrap();
        assert!(summary.ends_with("scratch removed=0"), "{summary}");
        assert!(young.exists());

        let elsewhere = JobPayload {
            node_id: Some(String::from("node-elsewhere")),
            ..JobPayload::default()
        };
        let err = run_cache_sweep(&state, &elsewhere, None).await.unwrap_err();
        assert!(err.to_string().contains("node-elsewhere"), "{err}");
        assert!(run_cache_sweep(&state, &JobPayload::default(), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_sweep_due_while_one_runs_is_asked_for_again() {
        let files = tempfile::tempdir().unwrap();
        let (mut state, library) = crate::node_api::tests::test_state_with(&files, "").await;
        let covers = crate::covers::CoverService::open(
            files.path().join("covers-cache"),
            crate::covers::Limits::default(),
        )
        .unwrap();
        std::sync::Arc::get_mut(&mut state)
            .expect("the state is not shared yet")
            .covers = std::sync::Arc::new(covers);
        let limits = state.covers.limits();
        // A first sweep leaves nothing due.
        state.covers.sweep().await.unwrap();
        assert!(!state.covers.maintain(limits).await);

        let AdmitJob::Created(id) = enqueue_cache_sweep(&state, "node-a").await.unwrap() else {
            panic!("expected a new job");
        };
        // Folded into a queued job, the request is that job's.
        queue_cache_sweep(&state, "node-a").await;
        assert!(!state.covers.maintain(limits).await);

        let job = library.get_job(&id).await.unwrap().unwrap();
        library
            .claim_job_by_id(&id, job.lease_generation, "node-a", 60)
            .await
            .unwrap()
            .expect("claimed");
        // Folded into a running one, it comes back on the next beat.
        queue_cache_sweep(&state, "node-a").await;
        assert!(state.covers.maintain(limits).await);
    }
}
