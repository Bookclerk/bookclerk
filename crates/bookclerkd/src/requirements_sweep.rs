//! The pending-job requirements sweep.
//!
//! Admission fixes a job's required plugin instances
//! (`control_plane::resolve_job_requirements`); this loop re-resolves them
//! for jobs still pending after deployments change, using the node-bound
//! output keys only the daemon can discover
//! (`jobs::sweep_node_bound_output_keys`: this node's discovered keys plus
//! every instance keyed as the first-party `local` destination, so a holder
//! without that plugin, or whose discovery failed, never strips them; a pass
//! that cannot read them is skipped). The rules (apply at once when
//! some node runs everything, otherwise wait out [`REQUIREMENTS_GRACE`]
//! before dropping a retired instance or failing the job as `unroutable`)
//! live in `control_plane::sweep_job_requirements`.
//!
//! Every daemon runs the loop, but only the one holding the cluster's
//! [`REQUIREMENTS_SWEEP_LEASE`] sweeps; the holder renews it on each pass and
//! a dead holder's lease expires to the next daemon. A pass runs on a
//! deployment or install wakeup on this node, after a scheduler pass that
//! moved replicas, and at least every [`SWEEP_INTERVAL`]. A wakeup that
//! reaches a node not holding the lease does nothing; the holder's timer
//! picks the change up within one interval, well inside the grace period.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    acquire_lease, load_or_create_node_identity, sweep_job_requirements, RequirementsSweepReport,
    REQUIREMENTS_GRACE, REQUIREMENTS_SWEEP_LEASE, REQUIREMENTS_SWEEP_LEASE_TTL,
};
use tracing::{debug, info, warn};

use crate::api::AppState;

/// Longest gap between sweeps; well inside [`REQUIREMENTS_SWEEP_LEASE_TTL`]
/// so the holder keeps its lease.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Quiet time a wakeup waits for before the pass runs.
///
/// Deployment changes arrive in bursts (enabling several plugins writes one
/// deployment each), and every pass rediscovers the installed plugins, so a
/// burst is swept once after it settles instead of once per change.
pub const WAKEUP_SETTLE: Duration = Duration::from_millis(500);

/// Longest a burst of wakeups may hold a pass back, so a steady stream of
/// changes still gets swept.
pub const WAKEUP_SETTLE_MAX: Duration = Duration::from_secs(3);

/// Starts the sweep loop.
pub fn spawn_requirements_sweep(state: Arc<AppState>) {
    tokio::spawn(async move {
        sweep_loop(
            || {
                let state = state.clone();
                async move {
                    run_requirements_sweep(&state).await;
                }
            },
            crate::node_queue::requirements_wakeup,
        )
        .await;
    });
}

/// Runs `pass` now, then after each settled burst of `wakeup`s and at least
/// every [`SWEEP_INTERVAL`]. Never returns.
async fn sweep_loop<P, PFut, W, WFut>(mut pass: P, mut wakeup: W)
where
    P: FnMut() -> PFut,
    PFut: std::future::Future<Output = ()>,
    W: FnMut() -> WFut,
    WFut: std::future::Future<Output = ()>,
{
    loop {
        pass().await;
        tokio::select! {
            () = wakeup() => {
                settle(&mut wakeup, WAKEUP_SETTLE, WAKEUP_SETTLE_MAX).await;
            }
            () = tokio::time::sleep(SWEEP_INTERVAL) => {}
        }
    }
}

/// Waits until `wakeup` has been quiet for `quiet`, or `max` has passed.
///
/// Each wakeup inside the quiet time restarts it, so a burst of changes
/// ends in one pass.
async fn settle<F, Fut>(mut wakeup: F, quiet: Duration, max: Duration)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let until = (tokio::time::Instant::now() + quiet).min(deadline);
        tokio::select! {
            () = wakeup() => {
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
            }
            () = tokio::time::sleep_until(until) => return,
        }
    }
}

/// One pass: take or renew the lease, then sweep. Returns `None` when
/// another node holds the lease or the pass failed.
pub async fn run_requirements_sweep(state: &AppState) -> Option<RequirementsSweepReport> {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let node_id = match load_or_create_node_identity(&files_dir) {
        Ok(identity) => identity.node_id,
        Err(err) => {
            warn!(error = %err, "node identity unavailable; requirements sweep idle");
            return None;
        }
    };
    // Hold off while a database swap is in progress, as admission does.
    let _permit = state.job_runtime.read().await;
    let library = state.library_snapshot().await;
    let bound = match crate::jobs::sweep_node_bound_output_keys(state, &library).await {
        Ok(bound) => bound,
        Err(err) => {
            warn!(error = %err, "node-bound outputs unavailable; requirements sweep skipped");
            return None;
        }
    };
    match acquire_lease(
        &library,
        REQUIREMENTS_SWEEP_LEASE,
        &node_id,
        REQUIREMENTS_SWEEP_LEASE_TTL,
    )
    .await
    {
        Ok(Some(lease)) => debug!(token = lease.token, "holding the requirements sweep lease"),
        Ok(None) => return None,
        Err(err) => {
            warn!(error = %err, "requirements sweep lease unavailable");
            return None;
        }
    }
    match sweep_job_requirements(&library, &bound, REQUIREMENTS_GRACE, chrono::Utc::now()).await {
        Ok(report) => {
            if report.changed() {
                info!(
                    examined = report.examined,
                    rewritten = report.rewritten,
                    settled = report.settled,
                    failed = report.failed,
                    unsettled = report.unsettled,
                    "requirements sweep updated pending jobs"
                );
            }
            if report.rewritten + report.settled > 0 {
                // A rewritten job may now be claimable on this node.
                state.job_notify.notify_one();
            }
            Some(report)
        }
        Err(err) => {
            warn!(error = %err, "requirements sweep failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bookclerk_config::Config;
    use bookclerk_library::control_plane::{
        acquire_lease, bootstrap_control_plane, create_plugin_instance, ensure_plugin_deployment,
        load_deployment_for_node, release_lease, replace_plugin_deployment, ConfigActor,
        DesiredState, REQUIREMENTS_SWEEP_LEASE,
    };
    use bookclerk_library::{EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, JobState};

    /// Only the lease holder sweeps; the holder marks a pending job whose
    /// pinned instance was retired as unsettled without failing it.
    #[tokio::test]
    async fn only_the_lease_holder_sweeps_and_a_retired_pin_waits() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let files_path = files.path();
        std::fs::write(files_path.join("config.toml"), "").unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let session = bootstrap_control_plane(
            &store,
            files_path,
            &files_path.join("cache"),
            None,
            &Config::default(),
        )
        .await
        .unwrap();
        let node = session.node.node_id.clone();
        let operator = ConfigActor::Operator {
            id: "operator".into(),
        };
        let instance =
            create_plugin_instance(&store, &operator, "path:file:///tmp/daemon-sweep#pin", "")
                .await
                .unwrap();
        ensure_plugin_deployment(
            &store,
            &ConfigActor::Bootstrap,
            &instance.id,
            &node,
            DesiredState::Present,
        )
        .await
        .unwrap();
        let job = match store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Scan,
                payload: JobPayload {
                    plugin_instance_id: Some(instance.id.to_string()),
                    requires: vec![instance.id.to_string()],
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 16,
                run_after: None,
            })
            .await
            .unwrap()
        {
            EnqueueOutcome::Created { id } => id,
            other => panic!("{other:?}"),
        };
        let deployment = load_deployment_for_node(&store, &instance.id, &node)
            .await
            .unwrap()
            .unwrap();
        replace_plugin_deployment(
            &store,
            &operator,
            &deployment.deployment_id,
            deployment.revision,
            DesiredState::Absent,
            "retire-pin",
        )
        .await
        .unwrap();

        let config = Config::load(
            Some(files_path.to_path_buf()),
            Some(files_path.join("config.toml")),
        )
        .unwrap();
        let state = Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            config,
        ));

        // Another node holds the lease: this one does not sweep.
        let other = acquire_lease(
            &store,
            REQUIREMENTS_SWEEP_LEASE,
            "other-node",
            Duration::from_secs(60),
        )
        .await
        .unwrap()
        .expect("free lease");
        assert!(super::run_requirements_sweep(&state).await.is_none());
        release_lease(&store, &other).await.unwrap();

        let report = super::run_requirements_sweep(&state)
            .await
            .expect("holds the lease");
        assert_eq!(report.unsettled, 1, "{report:?}");
        let pending = store.get_job(&job).await.unwrap().unwrap();
        assert_eq!(pending.state, JobState::Pending);
        assert!(pending.payload.requires_unsettled_since_ms.is_some());
        assert_eq!(pending.payload.requires, vec![instance.id.to_string()]);
    }

    /// A sweeping node without the `local` plugin installed (discovery finds
    /// none here) keeps a pending acquire's `local` requirement, even with
    /// the grace period long expired, instead of reading its absence as a
    /// retired output and letting any node run the acquire.
    #[tokio::test]
    async fn holder_without_the_local_plugin_keeps_local_requirements() {
        const LOCAL: &str = "platform:bookclerk/bookclerk-plugin-destination-local";
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let files_path = files.path();
        std::fs::write(files_path.join("config.toml"), "").unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let session = bootstrap_control_plane(
            &store,
            files_path,
            &files_path.join("cache"),
            None,
            &Config::default(),
        )
        .await
        .unwrap();
        let operator = ConfigActor::Operator {
            id: "operator".into(),
        };
        let local = create_plugin_instance(&store, &operator, LOCAL, "")
            .await
            .unwrap();
        ensure_plugin_deployment(
            &store,
            &ConfigActor::Bootstrap,
            &local.id,
            "node-with-disk",
            DesiredState::Present,
        )
        .await
        .unwrap();
        let expired_since = chrono::Utc::now()
            - chrono::Duration::from_std(super::REQUIREMENTS_GRACE).unwrap()
            - chrono::Duration::minutes(1);
        let job = match store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Acquire,
                payload: JobPayload {
                    title: Some("one book".into()),
                    requires: vec![local.id.to_string()],
                    requires_unsettled_since_ms: Some(expired_since.timestamp_millis()),
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 16,
                run_after: None,
            })
            .await
            .unwrap()
        {
            EnqueueOutcome::Created { id } => id,
            other => panic!("{other:?}"),
        };
        let config = Config::load(
            Some(files_path.to_path_buf()),
            Some(files_path.join("config.toml")),
        )
        .unwrap();
        let state = Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            config,
        ));
        assert_ne!(session.node.node_id, "node-with-disk");

        let report = super::run_requirements_sweep(&state)
            .await
            .expect("holds the lease");
        assert_eq!((report.rewritten, report.failed), (0, 0), "{report:?}");
        let pending = store.get_job(&job).await.unwrap().unwrap();
        assert_eq!(pending.state, JobState::Pending);
        assert_eq!(pending.payload.requires, vec![local.id.to_string()]);
    }

    /// A burst of wakeups ends in one pass once it has been quiet for the
    /// settle time.
    #[tokio::test(start_paused = true)]
    async fn a_burst_of_wakeups_settles_once_it_goes_quiet() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let sender = notify.clone();
        let burst = tokio::spawn(async move {
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                sender.notify_one();
            }
        });
        let started = tokio::time::Instant::now();
        let waiter = notify.clone();
        super::settle(
            move || {
                let waiter = waiter.clone();
                async move { waiter.notified().await }
            },
            Duration::from_millis(100),
            Duration::from_secs(5),
        )
        .await;
        let waited = started.elapsed();
        burst.await.unwrap();
        // The last wakeup lands at 200 ms; the pass waits a quiet 100 ms after it.
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        assert!(waited < Duration::from_millis(320), "{waited:?}");
    }

    /// A stream of wakeups that never goes quiet still lets a pass run.
    #[tokio::test(start_paused = true)]
    async fn a_steady_stream_of_wakeups_is_capped() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let sender = notify.clone();
        let stream = tokio::spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                sender.notify_one();
            }
        });
        let started = tokio::time::Instant::now();
        let waiter = notify.clone();
        super::settle(
            move || {
                let waiter = waiter.clone();
                async move { waiter.notified().await }
            },
            Duration::from_millis(100),
            Duration::from_millis(300),
        )
        .await;
        let waited = started.elapsed();
        stream.abort();
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        assert!(waited < Duration::from_millis(320), "{waited:?}");
    }

    /// The loop sweeps once at start, then once for a whole burst of
    /// wakeups after it settles, well before the periodic sweep is due.
    #[tokio::test(start_paused = true)]
    async fn the_loop_sweeps_a_burst_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let passes = Arc::new(AtomicUsize::new(0));
        let counted = passes.clone();
        let notify = Arc::new(tokio::sync::Notify::new());
        let waiter = notify.clone();
        let sweeper = tokio::spawn(super::sweep_loop(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                std::future::ready(())
            },
            move || {
                let waiter = waiter.clone();
                async move { waiter.notified().await }
            },
        ));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "the first pass runs at once"
        );

        for _ in 0..6 {
            notify.notify_one();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(passes.load(Ordering::SeqCst), 1, "still settling");
        tokio::time::sleep(super::WAKEUP_SETTLE * 2).await;
        assert_eq!(passes.load(Ordering::SeqCst), 2, "one pass for the burst");
        assert!(super::WAKEUP_SETTLE * 4 < super::SWEEP_INTERVAL);
        sweeper.abort();
    }
}
