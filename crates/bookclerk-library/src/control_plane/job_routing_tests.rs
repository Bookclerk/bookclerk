//! Routed job claims and cluster semaphores: two nodes, one database.
//!
//! Each scenario runs on two SQLite connections to one file and, when
//! `BOOKCLERK_TEST_POSTGRES_URL` is set, on two PostgreSQL pools.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

use super::tests::{postgres_stores, postgres_tests_enabled};
use super::*;
use crate::entities::{accounts, cluster_leases, jobs};
use crate::models::{EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, JobResourceClass};
use crate::store::LibraryStore;

const LOCAL_KEY: &str = "path:file:///tmp/local-out#local";

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

fn limits(class: u32) -> JobClaimLimits {
    JobClaimLimits {
        class,
        plugin_instance: 8,
    }
}

fn request<'a>(
    node_id: &'a str,
    owner: &'a str,
    lease_secs: u64,
    class: u32,
) -> JobClaimRequest<'a> {
    JobClaimRequest {
        resource_class: JobResourceClass::Network,
        node_id,
        claimant: Claimant::Local,
        owner,
        lease_secs,
        limits: limits(class),
        filter: None,
    }
}

/// Deploys `instance` on `node` and records `status` as the node's observation.
async fn deploy(
    store: &LibraryStore,
    instance: &PluginInstanceId,
    node: &str,
    status: DeploymentStatus,
) {
    let deployment = ensure_plugin_deployment(
        store,
        &ConfigActor::Bootstrap,
        instance,
        node,
        DesiredState::Present,
    )
    .await
    .unwrap();
    upsert_observation(
        store,
        &DeploymentObservation {
            deployment_id: deployment.deployment_id,
            node_id: node.into(),
            incarnation: "test".into(),
            status,
            detail: String::new(),
            applied_config_revision: None,
            observed_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .await
    .unwrap();
}

async fn enqueue(store: &LibraryStore, kind: JobKind, payload: JobPayload) -> String {
    match store
        .enqueue_job(EnqueueJobSpec {
            kind,
            payload,
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

async fn acquire_title(store: &LibraryStore, title: &str, account: Option<&str>) -> String {
    enqueue(
        store,
        JobKind::Acquire,
        JobPayload {
            title: Some(title.into()),
            account: account.map(str::to_string),
            ..Default::default()
        },
    )
    .await
}

async fn add_account(store: &LibraryStore, plugin_key: &str, account_id: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    accounts::ActiveModel {
        plugin_key: Set(plugin_key.into()),
        account_id: Set(account_id.into()),
        marketplace: Set("us".into()),
        label: Set(None),
        scan_enabled: Set(0),
        source: Set("test".into()),
        connection_status: Set("active".into()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(store.db())
    .await
    .unwrap();
}

/// An acquire bound to a `local` output deployed only on node A is never
/// claimed by node B (neither routed nor unrouted), and A claims it.
async fn incompatible_node_does_not_claim_a_routed_job(
    store_a: LibraryStore,
    store_b: LibraryStore,
) {
    let local = create_plugin_instance(&store_a, &operator(), LOCAL_KEY, "")
        .await
        .unwrap();
    deploy(&store_a, &local.id, "node-a", DeploymentStatus::Healthy).await;
    // B has the deployment too, but its guest failed: not runnable there.
    deploy(&store_b, &local.id, "node-b", DeploymentStatus::Error).await;

    let bound: BTreeSet<String> = [LOCAL_KEY.to_string()].into();
    let payload = JobPayload {
        title: Some("routed".into()),
        ..Default::default()
    };
    let requires = resolve_job_requirements(&store_b, JobKind::Acquire, &payload, &bound)
        .await
        .unwrap();
    assert_eq!(requires, vec![local.id.to_string()]);
    let routed = enqueue(
        &store_b,
        JobKind::Acquire,
        JobPayload {
            requires,
            ..payload
        },
    )
    .await;

    assert!(
        node_can_run_job(&store_a, "node-a", &[local.id.to_string()])
            .await
            .unwrap()
    );
    assert!(
        !node_can_run_job(&store_b, "node-b", &[local.id.to_string()])
            .await
            .unwrap()
    );
    assert!(!node_can_run_job(&store_b, "", &[local.id.to_string()])
        .await
        .unwrap());

    let none = claim_routed_job(&store_b, &request("node-b", "worker-b", 60, 4))
        .await
        .unwrap();
    assert!(none.is_none(), "node B cannot run the local output");
    let unrouted = store_b
        .claim_next_job(JobResourceClass::Network, "worker-b", 60, "op-b")
        .await
        .unwrap();
    assert!(
        unrouted.is_none(),
        "the unrouted claim never takes a routed job"
    );

    // A job without requirements is still claimable anywhere.
    let free = acquire_title(&store_a, "free", None).await;
    let claimed_b = claim_routed_job(&store_b, &request("node-b", "worker-b", 60, 4))
        .await
        .unwrap()
        .expect("node B claims the unconstrained job");
    assert_eq!(claimed_b.job.id, free);

    let claimed_a = claim_routed_job(&store_a, &request("node-a", "worker-a", 60, 4))
        .await
        .unwrap()
        .expect("node A runs the local output and claims the routed job");
    assert_eq!(claimed_a.job.id, routed);
    let names: BTreeSet<&str> = claimed_a
        .permits
        .iter()
        .map(|p| p.semaphore.as_str())
        .collect();
    assert!(names.contains("jobs:network"));
    assert!(names.contains(instance_semaphore(local.id.as_str()).as_str()));
}

/// Many claimers on two nodes never hold more permits, or run more jobs,
/// than the class limit.
async fn semaphore_never_exceeds_its_limit(store_a: LibraryStore, store_b: LibraryStore) {
    const LIMIT: u32 = 2;
    const JOBS: usize = 10;
    for i in 0..JOBS {
        acquire_title(&store_a, &format!("title-{i}"), None).await;
    }
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for w in 0..6 {
        let store = if w % 2 == 0 {
            store_a.clone()
        } else {
            store_b.clone()
        };
        let (in_flight, peak, done) = (in_flight.clone(), peak.clone(), done.clone());
        workers.push(tokio::spawn(async move {
            let node = if w % 2 == 0 { "node-a" } else { "node-b" };
            let owner = format!("worker-{w}");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            while done.load(Ordering::SeqCst) < JOBS {
                assert!(tokio::time::Instant::now() < deadline, "claimers stalled");
                let claimed =
                    match claim_routed_job(&store, &request(node, &owner, 60, LIMIT)).await {
                        Ok(Some(claimed)) => claimed,
                        Ok(None) | Err(_) => {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                            continue;
                        }
                    };
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let live = live_permits(&store, "jobs:network", MAX_SEMAPHORE_LIMIT)
                    .await
                    .unwrap();
                assert!(live.len() <= LIMIT as usize, "live permits {live:?}");
                tokio::time::sleep(Duration::from_millis(10)).await;
                let fence = claimed.job.fence().unwrap();
                in_flight.fetch_sub(1, Ordering::SeqCst);
                loop {
                    match store.complete_job(&fence, Some("ok")).await {
                        Ok(finished) => {
                            assert!(finished, "the holder finalizes its own job");
                            break;
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
                    }
                }
                while release_permits(&store, &claimed.permits).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                done.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for worker in workers {
        worker.await.unwrap();
    }
    assert!(peak.load(Ordering::SeqCst) <= LIMIT as usize);
    let succeeded = jobs::Entity::find()
        .filter(jobs::Column::State.eq("succeeded"))
        .all(store_a.db())
        .await
        .unwrap();
    assert_eq!(succeeded.len(), JOBS);
    assert!(live_permits(&store_b, "jobs:network", MAX_SEMAPHORE_LIMIT)
        .await
        .unwrap()
        .is_empty());
}

/// Two acquires of one store account never run at once, even with class
/// room to spare; a different account is not blocked.
async fn one_job_per_account(store_a: LibraryStore, store_b: LibraryStore) {
    add_account(&store_a, "path:file:///tmp/store#store", "acct-1").await;
    add_account(&store_a, "path:file:///tmp/store#store", "acct-2").await;
    let first = acquire_title(&store_a, "one", Some("acct-1")).await;
    let second = acquire_title(&store_b, "two", Some("acct-1")).await;
    let other = acquire_title(&store_b, "three", Some("acct-2")).await;

    let a = claim_routed_job(&store_a, &request("node-a", "worker-a", 60, 8))
        .await
        .unwrap()
        .expect("first claim");
    assert_eq!(a.job.id, first);
    assert!(a
        .permits
        .iter()
        .any(|p| p.semaphore == account_semaphore("path:file:///tmp/store#store", "acct-1")));
    let b = claim_routed_job(&store_b, &request("node-b", "worker-b", 60, 8))
        .await
        .unwrap()
        .expect("another account is free");
    assert_eq!(b.job.id, other, "acct-1 is busy, so B skips `{second}`");
    assert!(
        claim_routed_job(&store_b, &request("node-b", "worker-b", 60, 8))
            .await
            .unwrap()
            .is_none()
    );
    store_a
        .complete_job(&a.job.fence().unwrap(), None)
        .await
        .unwrap();
    release_permits(&store_a, &a.permits).await.unwrap();
    let next = claim_routed_job(&store_b, &request("node-b", "worker-b", 60, 8))
        .await
        .unwrap()
        .expect("acct-1 is free again");
    assert_eq!(next.job.id, second);
}

/// A holder that stops heartbeating loses its permit and its job to the
/// other node after expiry, and can neither renew nor finalize afterwards.
async fn crashed_holder_is_reclaimed_and_fenced(store_a: LibraryStore, store_b: LibraryStore) {
    let first = acquire_title(&store_a, "first", None).await;
    let _second = acquire_title(&store_a, "second", None).await;
    let held = claim_routed_job(&store_a, &request("node-a", "worker-a", 60, 1))
        .await
        .unwrap()
        .expect("A claims");
    assert_eq!(held.job.id, first);
    assert!(
        claim_routed_job(&store_b, &request("node-b", "worker-b", 60, 1))
            .await
            .unwrap()
            .is_none(),
        "the single network permit is held by A"
    );
    assert!(
        renew_permits(&store_a, &held.permits, Duration::from_secs(60))
            .await
            .unwrap()
    );

    // A crashes: no heartbeat until its job lease and permit expire.
    let past = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
    jobs::Entity::update_many()
        .col_expr(jobs::Column::LeaseExpiresAt, Expr::value(Some(past)))
        .filter(jobs::Column::Id.eq(first.as_str()))
        .exec(store_a.db())
        .await
        .unwrap();
    for permit in &held.permits {
        cluster_leases::Entity::update_many()
            .col_expr(
                cluster_leases::Column::ExpiresAtMs,
                Expr::value(chrono::Utc::now().timestamp_millis() - 5_000),
            )
            .filter(cluster_leases::Column::Name.eq(permit.lease.name.as_str()))
            .exec(store_a.db())
            .await
            .unwrap();
    }
    assert!(live_permits(&store_b, "jobs:network", 1)
        .await
        .unwrap()
        .is_empty());

    assert_eq!(store_b.reclaim_expired_leases().await.unwrap(), 1);
    let taken = claim_routed_job(&store_b, &request("node-b", "worker-b", 60, 1))
        .await
        .unwrap()
        .expect("B takes over the crashed job");
    assert_eq!(taken.job.id, first);
    let old = &held.permits[0];
    let new = taken
        .permits
        .iter()
        .find(|p| p.semaphore == old.semaphore)
        .unwrap();
    assert_eq!(new.lease.name, old.lease.name);
    assert!(
        new.lease.token > old.lease.token,
        "takeover bumps the token"
    );
    assert!(taken.job.lease_generation > held.job.lease_generation);

    // The stale holder wakes up: every fenced write misses.
    let stale = held.job.fence().unwrap();
    assert!(
        !renew_permits(&store_a, &held.permits, Duration::from_secs(60))
            .await
            .unwrap()
    );
    assert!(!store_a.heartbeat_job(&stale, 60, None).await.unwrap());
    assert!(!store_a.complete_job(&stale, Some("stale")).await.unwrap());
    assert!(!store_a.fail_job(&stale, "handler", "stale").await.unwrap());
    release_permits(&store_a, &held.permits).await.unwrap();
    let live = live_permits(&store_a, "jobs:network", 1).await.unwrap();
    assert_eq!(live.len(), 1, "a stale release does not free B's permit");
    assert_eq!(live[0].lease.holder, new.lease.holder);

    let fence = taken.job.fence().unwrap();
    assert!(store_b.complete_job(&fence, Some("done")).await.unwrap());
    release_permits(&store_b, &taken.permits).await.unwrap();
    assert!(live_permits(&store_b, "jobs:network", 1)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn sqlite_incompatible_node_does_not_claim_a_routed_job() {
    let (_dir, a, b) = sqlite_stores().await;
    incompatible_node_does_not_claim_a_routed_job(a, b).await;
}

#[tokio::test]
async fn sqlite_semaphore_never_exceeds_its_limit_across_two_connections() {
    let (_dir, a, b) = sqlite_stores().await;
    semaphore_never_exceeds_its_limit(a, b).await;
}

#[tokio::test]
async fn sqlite_one_job_per_account_across_two_connections() {
    let (_dir, a, b) = sqlite_stores().await;
    one_job_per_account(a, b).await;
}

#[tokio::test]
async fn sqlite_crashed_holder_is_reclaimed_and_fenced() {
    let (_dir, a, b) = sqlite_stores().await;
    crashed_holder_is_reclaimed_and_fenced(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_incompatible_node_does_not_claim_a_routed_job() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    incompatible_node_does_not_claim_a_routed_job(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_semaphore_never_exceeds_its_limit_across_two_pools() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    semaphore_never_exceeds_its_limit(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_one_job_per_account_across_two_pools() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    one_job_per_account(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_crashed_holder_is_reclaimed_and_fenced() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    crashed_holder_is_reclaimed_and_fenced(a, b).await;
}
