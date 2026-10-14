//! Placement, host labels, and the scheduler lease across two connections.

use std::collections::BTreeSet;
use std::time::Duration;

use bookclerk_config::EventsConfig;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::identity::{register_and_heartbeat, HostObservation};
use super::*;
use crate::master_key::master_key_test_lock_async;
use crate::store::LibraryStore;

async fn file_store(path: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(path)
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    LibraryStore::from_connection(db)
}

fn operator() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// Registers another host in the cluster.
pub(super) async fn register(store: &LibraryStore, host_id: &str, cluster_id: &str) {
    let empty = HostObservation {
        logical_cpus: None,
        cpu_max_quota_us: None,
        cpu_max_period_us: None,
        memory_max_bytes: None,
        memory_current_bytes: None,
        memory_anon_bytes: None,
        files_dir_free_bytes: None,
        scratch_bytes: None,
    };
    register_and_heartbeat(store, host_id, cluster_id, "test", &empty)
        .await
        .expect("register host");
}

/// Writes a heartbeat for `host_id` as if sent `age` ago, with a 4-core,
/// 8 GiB host and nothing reserved.
pub(super) async fn heartbeat_consumer(store: &LibraryStore, host_id: &str, age: Duration) {
    heartbeat_with(store, host_id, age, |_| {}).await;
}

/// Like [`heartbeat_consumer`], with `edit` applied to the status first.
pub(super) async fn heartbeat_with(
    store: &LibraryStore,
    host_id: &str,
    age: Duration,
    edit: impl FnOnce(&mut HostStatus),
) {
    let now = chrono::Utc::now().timestamp_millis();
    let at = now - i64::try_from(age.as_millis()).unwrap();
    let mut status = HostStatus {
        host_id: host_id.to_string(),
        incarnation: "test".into(),
        started_at: String::new(),
        heartbeat_at: String::new(),
        heartbeat_ms: at,
        interval_ms: 15_000,
        cpu_millicores: 4000,
        memory_bytes: Some(8 << 30),
        cpu_budget_millicores: 3200,
        memory_budget_bytes: Some(6 << 30),
        schedulable: true,
        cpu_used_millicores: Some(0),
        memory_available_bytes: Some(6 << 30),
        files_dir_free_bytes: None,
        reserved: GuestReservation::default(),
        running_guests: 0,
        availability_ppm: 1_000_000,
        online_since_ms: at,
    };
    edit(&mut status);
    super::host_status::upsert_host_status(store, &status)
        .await
        .expect("host status");
}

/// Hosts with a present deployment of `instance`.
pub(super) async fn present_hosts(
    store: &LibraryStore,
    instance: &PluginInstanceId,
) -> BTreeSet<String> {
    list_deployments_for_instance(store, instance)
        .await
        .unwrap()
        .into_iter()
        .filter(|d| d.desired.runs())
        .map(|d| d.host_id)
        .collect()
}

fn spec(replicas: u32, constraints: &[&str]) -> PlacementSpec {
    PlacementSpec {
        replicas,
        constraints: constraints.iter().map(|c| (*c).to_string()).collect(),
    }
}

fn labels(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_string()).collect()
}

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|v| (*v).to_string()).collect()
}

#[tokio::test]
async fn the_scheduler_places_moves_and_scales_replicas_across_two_connections() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    let store_a = file_store(&db).await;
    let session = bootstrap_control_plane(
        &store_a,
        dir.path(),
        &dir.path().join("cache"),
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    let store_b = file_store(&db).await;
    let host_a = session.host.host_id.clone();
    assert_eq!(session.host.target, host_target());
    register(&store_b, "host-b", &session.cluster_id).await;
    register(&store_b, "host-c", &session.cluster_id).await;
    for host in [host_a.as_str(), "host-b", "host-c"] {
        heartbeat_consumer(&store_a, host, Duration::ZERO).await;
    }
    assert_eq!(
        live_host_ids(&store_b).await.unwrap(),
        set(&[&host_a, "host-b", "host-c"])
    );

    let host = set_host_labels(&store_a, &operator(), &host_a, &labels(&["zone=eu", "nas"]))
        .await
        .unwrap();
    assert_eq!(host.labels, labels(&["nas", "zone=eu"]));
    set_host_labels(&store_a, &operator(), "host-b", &labels(&["nas"]))
        .await
        .unwrap();
    assert!(
        set_host_labels(&store_a, &operator(), "host-b", &labels(&["host=x"]))
            .await
            .is_err(),
        "implicit label names are not assignable"
    );
    assert!(
        set_host_labels(&store_a, &ConfigActor::Bootstrap, "host-b", &[])
            .await
            .is_err(),
        "only an operator assigns labels"
    );

    let instance =
        create_plugin_instance(&store_a, &operator(), "path:file:///tmp/placed#placed", "")
            .await
            .unwrap();
    ensure_hand_managed(&store_a, &instance.id).await.unwrap();
    let PlacementWrite::Applied(placed) = put_plugin_placement(
        &store_a,
        &operator(),
        &instance.id,
        &spec(2, &["nas"]),
        None,
    )
    .await
    .unwrap() else {
        panic!("first write applies");
    };
    assert_eq!(placed.revision, 1);
    assert_eq!(
        put_plugin_placement(&store_b, &operator(), &instance.id, &spec(3, &[]), None)
            .await
            .unwrap(),
        PlacementWrite::Conflict {
            current_revision: 1
        },
        "a create over an existing placement conflicts"
    );
    assert!(
        ensure_hand_managed(&store_b, &instance.id).await.is_err(),
        "a placed instance refuses hand-written enable/disable"
    );
    assert!(host_messages(&store_a, "host-c").await.iter().any(|m| matches!(
        m,
        HostMessage::PlacementChanged { subject, revision: 1 } if subject.starts_with("instance:")
    )));

    // Host B holds the scheduler lease; host A's attempt waits its turn.
    let lease = acquire_lease(&store_b, SCHEDULER_LEASE, "host-b", SCHEDULER_LEASE_TTL)
        .await
        .unwrap()
        .expect("free lease");
    assert!(
        acquire_lease(&store_a, SCHEDULER_LEASE, &host_a, SCHEDULER_LEASE_TTL)
            .await
            .unwrap()
            .is_none()
    );
    let report = schedule_placements(&store_b).await.unwrap();
    assert_eq!((report.added, report.removed), (2, 0));
    assert_eq!(
        present_hosts(&store_a, &instance.id).await,
        set(&[&host_a, "host-b"])
    );
    let status = load_plugin_placement(&store_a, &instance.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((status.placed, status.detail.as_str()), (2, ""));
    let again = schedule_placements(&store_b).await.unwrap();
    assert_eq!(
        (again.added, again.removed),
        (0, 0),
        "a settled pass writes nothing"
    );
    let b_messages = host_messages(&store_a, "host-b").await;
    assert!(b_messages.iter().any(|m| matches!(
        m,
        HostMessage::DeploymentChanged { desired, .. } if desired == "placed"
    )));

    // Host B's daemon stops heartbeating: its replica is set absent, and
    // with no other `nas` host the placement reports the shortfall.
    heartbeat_consumer(&store_a, "host-b", Duration::from_secs(600)).await;
    let moved = schedule_placements(&store_b).await.unwrap();
    assert_eq!((moved.added, moved.removed), (0, 1));
    assert_eq!(present_hosts(&store_a, &instance.id).await, set(&[&host_a]));
    let short = load_plugin_placement(&store_a, &instance.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(short.placed, 1);
    assert!(short.detail.starts_with("1 of 2"), "{}", short.detail);

    // Labelling host C makes it a candidate; the next pass fills it.
    set_host_labels(&store_a, &operator(), "host-c", &labels(&["nas"]))
        .await
        .unwrap();
    let filled = schedule_placements(&store_b).await.unwrap();
    assert_eq!(filled.added, 1);
    assert_eq!(
        present_hosts(&store_a, &instance.id).await,
        set(&[&host_a, "host-c"])
    );

    // Pinning by the implicit host label, then scaling to zero.
    let PlacementWrite::Applied(pinned) = put_plugin_placement(
        &store_a,
        &operator(),
        &instance.id,
        &spec(1, &[&format!("host={host_a}")]),
        Some(1),
    )
    .await
    .unwrap() else {
        panic!("current revision applies");
    };
    assert_eq!(pinned.revision, 2);
    schedule_placements(&store_b).await.unwrap();
    assert_eq!(present_hosts(&store_a, &instance.id).await, set(&[&host_a]));
    put_plugin_placement(&store_a, &operator(), &instance.id, &spec(0, &[]), Some(2))
        .await
        .unwrap();
    schedule_placements(&store_b).await.unwrap();
    assert!(present_hosts(&store_a, &instance.id).await.is_empty());

    // Every row the scheduler wrote carries its own states, never the
    // operator's, and only the scheduler may write them.
    let rows = list_deployments_for_instance(&store_a, &instance.id)
        .await
        .unwrap();
    assert!(!rows.is_empty());
    assert!(
        rows.iter().all(|d| d.desired == DesiredState::Unplaced),
        "scaled to zero: {rows:?}"
    );
    assert!(
        replace_plugin_deployment(
            &store_a,
            &operator(),
            &rows[0].deployment_id,
            rows[0].revision,
            DesiredState::Placed,
            "operator-may-not-place",
        )
        .await
        .is_err(),
        "an operator cannot write a scheduler state"
    );
    assert!(
        replace_plugin_deployment(
            &store_a,
            &ConfigActor::Scheduler,
            &rows[0].deployment_id,
            rows[0].revision,
            DesiredState::Present,
            "scheduler-may-not-enable",
        )
        .await
        .is_err(),
        "the scheduler cannot write an operator state"
    );

    // Scale back to one so a replica is running when the placement goes.
    put_plugin_placement(&store_a, &operator(), &instance.id, &spec(1, &[]), Some(3))
        .await
        .unwrap();
    schedule_placements(&store_b).await.unwrap();
    assert_eq!(present_hosts(&store_a, &instance.id).await.len(), 1);

    // Dropping the placement hands the instance back to enable/disable:
    // `placed` becomes `present`, `unplaced` becomes `absent`, and every
    // host keeps doing what it did.
    let before = present_hosts(&store_a, &instance.id).await;
    assert!(delete_plugin_placement(&store_a, &operator(), &instance.id)
        .await
        .unwrap());
    ensure_hand_managed(&store_a, &instance.id).await.unwrap();
    let handed = list_deployments_for_instance(&store_a, &instance.id)
        .await
        .unwrap();
    assert!(handed
        .iter()
        .all(|d| matches!(d.desired, DesiredState::Present | DesiredState::Absent)));
    assert_eq!(present_hosts(&store_a, &instance.id).await, before);
    assert_eq!(
        schedule_placements(&store_b).await.unwrap(),
        ScheduleReport::default()
    );
    release_lease(&store_b, &lease).await.unwrap();
    assert!(
        acquire_lease(&store_a, SCHEDULER_LEASE, &host_a, SCHEDULER_LEASE_TTL)
            .await
            .unwrap()
            .is_some(),
        "a released lease is free at once"
    );
}

#[tokio::test]
async fn a_release_without_an_artifact_for_a_host_target_skips_that_host() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir.path().join("library.db")).await;
    let session = bootstrap_control_plane(
        &store,
        dir.path(),
        &dir.path().join("cache"),
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    register(&store, "host-arm", &session.cluster_id).await;
    // A row from an arm host: rewrite its target as that host would report.
    crate::entities::hosts::Entity::update_many()
        .col_expr(
            crate::entities::hosts::Column::Target,
            sea_orm::sea_query::Expr::value("linux-arm64-gnu"),
        )
        .filter(crate::entities::hosts::Column::HostId.eq("host-arm"))
        .exec(store.db())
        .await
        .unwrap();
    for host in [session.host.host_id.as_str(), "host-arm"] {
        heartbeat_consumer(&store, host, Duration::ZERO).await;
    }
    let key = "path:file:///tmp/x86-only#x86only";
    let release = PluginRelease {
        plugin_key: key.into(),
        version: "1.0.0".into(),
        manifest_id: "x86only".into(),
        manifest_toml: "id = \"x86only\"\nversion = \"1.0.0\"\n".into(),
        settings: bookclerk_plugin_abi::SettingsSchema::default(),
        artifacts: vec![ReleaseArtifact {
            target: host_target().into(),
            url: format!("{RELEASE_BLOB_SCHEME}{}", "a".repeat(64)),
            format: "tar.gz".into(),
            archive_root: ".".into(),
            archive_sha256: "a".repeat(64),
            payload_root_sha256: "b".repeat(64),
            executable: "guest".into(),
        }],
        publisher_verified: true,
        grant_json: String::new(),
        coordinate: String::new(),
        published_at: String::new(),
        published_by: String::new(),
    };
    publish_release(&store, &operator(), release).await.unwrap();
    activate_release(&store, &operator(), key, "1.0.0", &session.host.host_id)
        .await
        .unwrap();
    let instance = create_plugin_instance(&store, &operator(), key, "")
        .await
        .unwrap();
    put_plugin_placement(&store, &operator(), &instance.id, &spec(2, &[]), None)
        .await
        .unwrap();
    schedule_placements(&store).await.unwrap();
    assert_eq!(
        present_hosts(&store, &instance.id).await,
        set(&[&session.host.host_id])
    );
}

#[tokio::test]
async fn the_scheduler_lease_has_one_holder_and_a_fencing_token() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    let a = file_store(&db).await;
    let b = file_store(&db).await;
    let ttl = Duration::from_secs(60);
    let first = acquire_lease(&a, "test", "h1", ttl).await.unwrap().unwrap();
    assert_eq!(first.token, 1);
    assert!(acquire_lease(&b, "test", "h2", ttl)
        .await
        .unwrap()
        .is_none());
    let renewed = acquire_lease(&a, "test", "h1", ttl).await.unwrap().unwrap();
    assert_eq!(renewed.token, 1, "renewing keeps the token");

    // An expired lease passes to the next holder with a larger token.
    acquire_lease(&a, "test", "h1", Duration::ZERO)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let taken = acquire_lease(&b, "test", "h2", ttl).await.unwrap().unwrap();
    assert_eq!(taken.token, 2);
    assert!(acquire_lease(&a, "test", "h1", ttl)
        .await
        .unwrap()
        .is_none());

    // Releasing with a stale token does nothing; the holder's release frees it.
    release_lease(&a, &first).await.unwrap();
    assert!(acquire_lease(&a, "test", "h1", ttl)
        .await
        .unwrap()
        .is_none());
    release_lease(&b, &taken).await.unwrap();
    let back = acquire_lease(&a, "test", "h1", ttl).await.unwrap().unwrap();
    assert_eq!(back.token, 3);
    assert_eq!(load_lease(&b, "test").await.unwrap().unwrap().holder, "h1");
}

/// Host-queue messages on `host`'s queue, oldest first.
async fn host_messages(store: &LibraryStore, host: &str) -> Vec<HostMessage> {
    let mut rows = store
        .list_domain_events_for_queue(&host_queue_name(host), 200)
        .await
        .expect("queue rows");
    rows.reverse();
    rows.into_iter()
        .map(|row| serde_json::from_str(&row.payload).expect("host message body"))
        .collect()
}
