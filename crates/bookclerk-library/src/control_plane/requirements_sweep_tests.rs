//! The pending-job requirements sweep: two nodes, one database.
//!
//! Each scenario runs on two SQLite connections to one file and, when
//! `BOOKCLERK_TEST_POSTGRES_URL` is set, on two PostgreSQL pools.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::Utc;

use super::tests::{postgres_stores, postgres_tests_enabled};
use super::*;
use crate::models::{
    EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, JobResourceClass, JobState,
};
use crate::store::LibraryStore;

const LOCAL_KEY: &str = "path:file:///tmp/sweep-out#local";
const GRACE: Duration = Duration::from_secs(600);

async fn sqlite_stores() -> (tempfile::TempDir, LibraryStore, LibraryStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let open = |path: std::path::PathBuf| async move {
        let db = bookclerk_plugin_database_sqlite::open(&path)
            .await
            .expect("open sqlite file");
        crate::apply_host_schema(&db).await.expect("schema");
        LibraryStore::from_connection(db)
    };
    let a = open(path.clone()).await;
    let b = open(path).await;
    (dir, a, b)
}

fn operator() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

fn bound() -> BTreeSet<String> {
    [LOCAL_KEY.to_string()].into()
}

/// Sets `instance`'s desired state on `node`, creating the row when absent,
/// and reports the guest healthy there.
async fn set_desired(
    store: &LibraryStore,
    instance: &PluginInstanceId,
    node: &str,
    desired: DesiredState,
) {
    let deployment = match load_deployment_for_node(store, instance, node)
        .await
        .unwrap()
    {
        Some(existing) => {
            replace_plugin_deployment(
                store,
                &operator(),
                &existing.deployment_id,
                existing.revision,
                desired,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .unwrap();
            existing
        }
        None => ensure_plugin_deployment(store, &ConfigActor::Bootstrap, instance, node, desired)
            .await
            .unwrap(),
    };
    upsert_observation(
        store,
        &DeploymentObservation {
            deployment_id: deployment.deployment_id,
            node_id: node.into(),
            incarnation: "test".into(),
            status: DeploymentStatus::Healthy,
            detail: String::new(),
            applied_config_revision: None,
            observed_at: Utc::now().to_rfc3339(),
        },
    )
    .await
    .unwrap();
}

/// Admits an acquire the way the daemon does: requirements resolved first.
async fn admit_acquire(store: &LibraryStore, title: &str) -> String {
    let payload = JobPayload {
        title: Some(title.into()),
        ..Default::default()
    };
    let requires = resolve_job_requirements(store, JobKind::Acquire, &payload, &bound())
        .await
        .unwrap();
    match store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Acquire,
            payload: JobPayload {
                requires,
                ..payload
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 256,
            run_after: None,
        })
        .await
        .unwrap()
    {
        EnqueueOutcome::Created { id } => id,
        other => panic!("expected a new job: {other:?}"),
    }
}

async fn sweep(store: &LibraryStore, at: chrono::DateTime<Utc>) -> RequirementsSweepReport {
    sweep_job_requirements(store, &bound(), GRACE, at)
        .await
        .unwrap()
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// A `local` output deployed after admission is added to a pending acquire
/// at once, under a CAS that bumps the generation; a second sweep is a no-op.
async fn new_output_tightens_a_pending_acquire(store_a: LibraryStore, store_b: LibraryStore) {
    let first = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    set_desired(&store_a, &first.id, "node-a", DesiredState::Present).await;
    let job = admit_acquire(&store_a, "tighten").await;
    let before = store_b.get_job(&job).await.unwrap().unwrap();
    assert_eq!(before.payload.requires, vec![first.id.to_string()]);

    let second = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "second")
        .await
        .unwrap();
    set_desired(&store_b, &second.id, "node-a", DesiredState::Present).await;
    let report = sweep(&store_b, Utc::now()).await;
    assert_eq!(report.rewritten, 1, "{report:?}");
    let after = store_a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(
        after.payload.requires,
        sorted(vec![first.id.to_string(), second.id.to_string()])
    );
    assert_eq!(after.lease_generation, before.lease_generation + 1);
    assert_eq!(after.payload.requires_unsettled_since_ms, None);
    assert!(!sweep(&store_a, Utc::now()).await.changed());
}

/// A move that briefly leaves no common node marks the job unsettled but
/// does not fail it inside the grace period; once both outputs share a node
/// again the mark is cleared and the job is claimable.
async fn placement_move_inside_grace_keeps_the_job(store_a: LibraryStore, store_b: LibraryStore) {
    let first = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    let second = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "second")
        .await
        .unwrap();
    set_desired(&store_a, &first.id, "node-a", DesiredState::Present).await;
    set_desired(&store_a, &second.id, "node-a", DesiredState::Present).await;
    let job = admit_acquire(&store_a, "move").await;

    // The second output moves from A to B; the first follows later.
    set_desired(&store_b, &second.id, "node-b", DesiredState::Present).await;
    set_desired(&store_b, &second.id, "node-a", DesiredState::Absent).await;
    let start = Utc::now();
    let report = sweep(&store_b, start).await;
    assert_eq!(report.unsettled, 1, "{report:?}");
    let marked = store_a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(marked.state, JobState::Pending);
    assert_eq!(
        marked.payload.requires_unsettled_since_ms,
        Some(start.timestamp_millis())
    );
    let later = start + chrono::Duration::seconds(300);
    let report = sweep(&store_a, later).await;
    assert_eq!((report.unsettled, report.failed), (1, 0), "{report:?}");
    let held = store_b.get_job(&job).await.unwrap().unwrap();
    assert_eq!(held.state, JobState::Pending);
    assert_eq!(held.lease_generation, marked.lease_generation, "no write");

    set_desired(&store_a, &first.id, "node-b", DesiredState::Present).await;
    set_desired(&store_a, &first.id, "node-a", DesiredState::Absent).await;
    let report = sweep(&store_b, later).await;
    assert_eq!(report.settled, 1, "{report:?}");
    let settled = store_a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(settled.payload.requires_unsettled_since_ms, None);
    let claim = claim_routed_job(
        &store_b,
        &JobClaimRequest {
            resource_class: JobResourceClass::Network,
            node_id: "node-b",
            claimant: Claimant::Local,
            owner: "worker-b",
            lease_secs: 60,
            limits: JobClaimLimits {
                class: 4,
                plugin_instance: 4,
            },
            filter: None,
        },
    )
    .await
    .unwrap()
    .expect("node B now runs both outputs");
    assert_eq!(claim.job.id, job);
}

/// Requirements that stay split past the grace period fail the job with a
/// clear reason, which frees its dedup key for a new admission.
async fn split_past_grace_fails_the_job(store_a: LibraryStore, store_b: LibraryStore) {
    let first = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    let second = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "second")
        .await
        .unwrap();
    set_desired(&store_a, &first.id, "node-a", DesiredState::Present).await;
    set_desired(&store_a, &second.id, "node-a", DesiredState::Present).await;
    let job = admit_acquire(&store_a, "split").await;
    set_desired(&store_b, &second.id, "node-b", DesiredState::Present).await;
    set_desired(&store_b, &second.id, "node-a", DesiredState::Absent).await;

    let start = Utc::now();
    assert_eq!(sweep(&store_a, start).await.unsettled, 1);
    let expired = start + chrono::Duration::from_std(GRACE).unwrap() + chrono::Duration::seconds(1);
    let report = sweep(&store_b, expired).await;
    assert_eq!(report.failed, 1, "{report:?}");
    let failed = store_a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.error_kind.as_deref(), Some(UNROUTABLE_ERROR_KIND));
    let message = failed.error_message.unwrap();
    assert!(message.contains("no node runs all"), "{message}");
    assert!(message.contains(first.id.as_str()), "{message}");

    // The same work can be queued again (it is refused at admission until
    // the outputs share a node, as before).
    set_desired(&store_a, &second.id, "node-a", DesiredState::Present).await;
    let again = admit_acquire(&store_b, "split").await;
    assert_ne!(again, job);
}

/// A required output retired everywhere is dropped only after the grace
/// period, after which any node may claim the job.
async fn retired_output_is_dropped_after_grace(store_a: LibraryStore, store_b: LibraryStore) {
    let local = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    set_desired(&store_a, &local.id, "node-a", DesiredState::Present).await;
    let job = admit_acquire(&store_a, "retired").await;
    set_desired(&store_b, &local.id, "node-a", DesiredState::Absent).await;

    let start = Utc::now();
    assert_eq!(sweep(&store_a, start).await.unsettled, 1);
    assert_eq!(
        store_b
            .get_job(&job)
            .await
            .unwrap()
            .unwrap()
            .payload
            .requires,
        vec![local.id.to_string()],
        "kept inside the grace period"
    );
    let expired = start + chrono::Duration::from_std(GRACE).unwrap();
    let report = sweep(&store_b, expired).await;
    assert_eq!(report.rewritten, 1, "{report:?}");
    let rewritten = store_a.get_job(&job).await.unwrap().unwrap();
    assert!(rewritten.payload.requires.is_empty());
    assert_eq!(rewritten.state, JobState::Pending);
    let claimed = store_b
        .claim_next_job(JobResourceClass::Network, "worker-b", 60, "op-retired")
        .await
        .unwrap()
        .expect("unrouted now");
    assert_eq!(claimed.id, job);
}

/// The sweep's CAS loses to a claim taken after it read the row, and a
/// claimer that read the row before a sweep write loses to the sweep.
async fn sweep_writes_never_overwrite_a_claim(store_a: LibraryStore, store_b: LibraryStore) {
    let local = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    set_desired(&store_a, &local.id, "node-a", DesiredState::Present).await;
    let job = admit_acquire(&store_a, "race").await;
    let read = store_b.get_job(&job).await.unwrap().unwrap();

    let claimed = store_a
        .claim_job_by_id(&job, read.lease_generation, "worker-a", 60)
        .await
        .unwrap();
    assert!(claimed.is_some());
    let cleared = JobPayload {
        requires: Vec::new(),
        ..read.payload.clone()
    };
    assert!(!store_b
        .rewrite_pending_job_payload(&job, read.lease_generation, &cleared)
        .await
        .unwrap());
    assert!(!store_b
        .fail_pending_job(&job, read.lease_generation, UNROUTABLE_ERROR_KIND, "x")
        .await
        .unwrap());
    let running = store_a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(running.state, JobState::Running);
    assert_eq!(running.payload.requires, vec![local.id.to_string()]);

    // A second pending job: the sweep writes first, so a stale claim fails.
    let other = admit_acquire(&store_a, "race-2").await;
    let stale = store_b.get_job(&other).await.unwrap().unwrap();
    assert!(store_a
        .rewrite_pending_job_payload(&other, stale.lease_generation, &stale.payload)
        .await
        .unwrap());
    assert!(!store_b
        .rewrite_pending_job_payload(&other, stale.lease_generation, &stale.payload)
        .await
        .unwrap());
}

fn claim_request(node_id: &str) -> JobClaimRequest<'_> {
    JobClaimRequest {
        resource_class: JobResourceClass::Network,
        node_id,
        claimant: Claimant::Local,
        owner: "worker",
        lease_secs: 60,
        limits: JobClaimLimits {
            class: 4,
            plugin_instance: 4,
        },
        filter: None,
    }
}

/// An output deployed where the job's recorded outputs are not leaves the
/// job unsettled, but the new output is recorded at once: the node that ran
/// the old set can no longer claim the job and skip it during the grace
/// period, and the job fails as unroutable once the grace period is over.
async fn unsettled_gain_is_recorded_inside_grace(store_a: LibraryStore, store_b: LibraryStore) {
    let first = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    set_desired(&store_a, &first.id, "node-a", DesiredState::Present).await;
    let job = admit_acquire(&store_a, "gain").await;
    let second = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "second")
        .await
        .unwrap();
    set_desired(&store_b, &second.id, "node-b", DesiredState::Present).await;

    let start = Utc::now();
    let report = sweep(&store_b, start).await;
    assert_eq!((report.unsettled, report.failed), (1, 0), "{report:?}");
    let held = store_a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(held.state, JobState::Pending);
    assert_eq!(
        held.payload.requires,
        sorted(vec![first.id.to_string(), second.id.to_string()])
    );
    assert_eq!(
        held.payload.requires_unsettled_since_ms,
        Some(start.timestamp_millis())
    );
    for node in ["node-a", "node-b"] {
        assert!(
            claim_routed_job(&store_a, &claim_request(node))
                .await
                .unwrap()
                .is_none(),
            "{node} does not run both outputs"
        );
    }
    // A later sweep inside the grace period writes nothing more.
    let later = start + chrono::Duration::seconds(300);
    let report = sweep(&store_a, later).await;
    assert_eq!((report.unsettled, report.lost), (1, 0), "{report:?}");
    let same = store_b.get_job(&job).await.unwrap().unwrap();
    assert_eq!(same.lease_generation, held.lease_generation, "no write");

    let expired = start + chrono::Duration::from_std(GRACE).unwrap();
    assert_eq!(sweep(&store_b, expired).await.failed, 1);
    let failed = store_a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(failed.error_kind.as_deref(), Some(UNROUTABLE_ERROR_KIND));
}

/// A claimer that evaluated a candidate before the sweep rewrote its
/// requirements loses the claim instead of running the rewritten job on its
/// stale check; it claims the job once it evaluates the new record.
async fn claim_after_rewrite_uses_the_evaluated_generation(
    store_a: LibraryStore,
    store_b: LibraryStore,
) {
    let first = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    set_desired(&store_a, &first.id, "node-a", DesiredState::Present).await;
    let job = admit_acquire(&store_a, "stale-claim").await;

    // Node A evaluates the candidate against its recorded requirements.
    let page = store_a
        .list_job_claim_candidates(JobResourceClass::Network, None, 16)
        .await
        .unwrap();
    let evaluated = page
        .jobs
        .into_iter()
        .find(|candidate| candidate.id == job)
        .expect("candidate");
    assert!(
        node_can_run_job(&store_a, "node-a", &evaluated.payload.requires)
            .await
            .unwrap()
    );

    // The sweep records an output deployed only on node B meanwhile.
    let second = create_plugin_instance(&store_b, &operator(), LOCAL_KEY, "second")
        .await
        .unwrap();
    set_desired(&store_b, &second.id, "node-b", DesiredState::Present).await;
    assert_eq!(sweep(&store_b, Utc::now()).await.unsettled, 1);
    let rewritten = store_b.get_job(&job).await.unwrap().unwrap();
    assert_eq!(rewritten.lease_generation, evaluated.lease_generation + 1);
    assert!(
        !node_can_run_job(&store_a, "node-a", &rewritten.payload.requires)
            .await
            .unwrap()
    );

    assert!(store_a
        .claim_job_by_id(&job, evaluated.lease_generation, "worker-a", 60)
        .await
        .unwrap()
        .is_none());
    let still = store_b.get_job(&job).await.unwrap().unwrap();
    assert_eq!(still.state, JobState::Pending);
    assert_eq!(still.lease_generation, rewritten.lease_generation);

    // The fence is the generation alone: a claimer that read the current
    // record (and checked its requirements) claims it.
    let claimed = store_a
        .claim_job_by_id(&job, rewritten.lease_generation, "worker-a", 60)
        .await
        .unwrap()
        .expect("current generation claims");
    assert_eq!(claimed.state, JobState::Running);
}

/// Jobs that admission never routes (scans that are not pinned) are left
/// untouched.
async fn unrouted_scans_are_not_rewritten(store_a: LibraryStore, _store_b: LibraryStore) {
    let local = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    set_desired(&store_a, &local.id, "node-a", DesiredState::Present).await;
    let scan = match store_a
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Scan,
            payload: JobPayload::default(),
            priority: 0,
            max_attempts: 3,
            max_pending: 256,
            run_after: None,
        })
        .await
        .unwrap()
    {
        EnqueueOutcome::Created { id } => id,
        other => panic!("{other:?}"),
    };
    let before = store_a.get_job(&scan).await.unwrap().unwrap();
    let report = sweep(&store_a, Utc::now()).await;
    assert_eq!(report.examined, 0, "{report:?}");
    let after = store_a.get_job(&scan).await.unwrap().unwrap();
    assert_eq!(after.lease_generation, before.lease_generation);
}

macro_rules! twin {
    ($scenario:ident, $sqlite:ident, $postgres:ident) => {
        #[tokio::test]
        async fn $sqlite() {
            let (_dir, a, b) = sqlite_stores().await;
            $scenario(a, b).await;
        }

        #[tokio::test]
        async fn $postgres() {
            if !postgres_tests_enabled() {
                return;
            }
            let (a, b) = postgres_stores().await;
            $scenario(a, b).await;
        }
    };
}

twin!(
    new_output_tightens_a_pending_acquire,
    sqlite_new_output_tightens_a_pending_acquire,
    postgres_new_output_tightens_a_pending_acquire
);
twin!(
    placement_move_inside_grace_keeps_the_job,
    sqlite_placement_move_inside_grace_keeps_the_job,
    postgres_placement_move_inside_grace_keeps_the_job
);
twin!(
    split_past_grace_fails_the_job,
    sqlite_split_past_grace_fails_the_job,
    postgres_split_past_grace_fails_the_job
);
twin!(
    retired_output_is_dropped_after_grace,
    sqlite_retired_output_is_dropped_after_grace,
    postgres_retired_output_is_dropped_after_grace
);
twin!(
    sweep_writes_never_overwrite_a_claim,
    sqlite_sweep_writes_never_overwrite_a_claim,
    postgres_sweep_writes_never_overwrite_a_claim
);
twin!(
    unrouted_scans_are_not_rewritten,
    sqlite_unrouted_scans_are_not_rewritten,
    postgres_unrouted_scans_are_not_rewritten
);
twin!(
    unsettled_gain_is_recorded_inside_grace,
    sqlite_unsettled_gain_is_recorded_inside_grace,
    postgres_unsettled_gain_is_recorded_inside_grace
);
twin!(
    claim_after_rewrite_uses_the_evaluated_generation,
    sqlite_claim_after_rewrite_uses_the_evaluated_generation,
    postgres_claim_after_rewrite_uses_the_evaluated_generation
);
