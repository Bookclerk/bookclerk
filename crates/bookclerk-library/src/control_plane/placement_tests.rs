//! Placement, node labels, and the scheduler lease across two connections.

use std::collections::BTreeSet;
use std::time::Duration;

use bookclerk_config::EventsConfig;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::identity::{register_and_heartbeat, NodeObservation};
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

/// Registers another node in the cluster.
pub(super) async fn register(store: &LibraryStore, node_id: &str, cluster_id: &str) {
    let empty = NodeObservation {
        logical_cpus: None,
        cpu_max_quota_us: None,
        cpu_max_period_us: None,
        memory_max_bytes: None,
        memory_current_bytes: None,
        memory_anon_bytes: None,
        files_dir_free_bytes: None,
        scratch_bytes: None,
    };
    register_and_heartbeat(store, node_id, cluster_id, "test", &empty)
        .await
        .expect("register node");
}

/// Writes a heartbeat for `node_id` as if sent `age` ago, with a 4-core,
/// 8 GiB node and nothing reserved.
pub(super) async fn heartbeat_consumer(store: &LibraryStore, node_id: &str, age: Duration) {
    heartbeat_with(store, node_id, age, |_| {}).await;
}

/// Like [`heartbeat_consumer`], with `edit` applied to the status first.
pub(super) async fn heartbeat_with(
    store: &LibraryStore,
    node_id: &str,
    age: Duration,
    edit: impl FnOnce(&mut NodeStatus),
) {
    let now = chrono::Utc::now().timestamp_millis();
    let at = now - i64::try_from(age.as_millis()).unwrap();
    let mut status = NodeStatus {
        node_id: node_id.to_string(),
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
    super::node_status::upsert_node_status(store, &status)
        .await
        .expect("node status");
}

/// Nodes with a present deployment of `instance`.
pub(super) async fn present_nodes(
    store: &LibraryStore,
    instance: &PluginInstanceId,
) -> BTreeSet<String> {
    list_deployments_for_instance(store, instance)
        .await
        .unwrap()
        .into_iter()
        .filter(|d| d.desired.runs())
        .map(|d| d.node_id)
        .collect()
}

fn spec(replicas: u32, constraints: &[&str]) -> PlacementSpec {
    PlacementSpec {
        replicas: Replicas::Count(replicas),
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
    let node_a = session.node.node_id.clone();
    assert_eq!(session.node.target, node_target());
    register(&store_b, "node-b", &session.cluster_id).await;
    register(&store_b, "node-c", &session.cluster_id).await;
    for node in [node_a.as_str(), "node-b", "node-c"] {
        heartbeat_consumer(&store_a, node, Duration::ZERO).await;
    }
    assert_eq!(
        live_node_ids(&store_b).await.unwrap(),
        set(&[&node_a, "node-b", "node-c"])
    );

    let node = set_node_labels(&store_a, &operator(), &node_a, &labels(&["zone=eu", "nas"]))
        .await
        .unwrap();
    assert_eq!(node.labels, labels(&["nas", "zone=eu"]));
    set_node_labels(&store_a, &operator(), "node-b", &labels(&["nas"]))
        .await
        .unwrap();
    assert!(
        set_node_labels(&store_a, &operator(), "node-b", &labels(&["node=x"]))
            .await
            .is_err(),
        "implicit label names are not assignable"
    );
    assert!(
        set_node_labels(&store_a, &ConfigActor::Bootstrap, "node-b", &[])
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
    assert!(node_messages(&store_a, "node-c").await.iter().any(|m| matches!(
        m,
        NodeMessage::PlacementChanged { subject, revision: 1 } if subject.starts_with("instance:")
    )));

    // Node B holds the scheduler lease; node A's attempt waits its turn.
    let lease = acquire_lease(&store_b, SCHEDULER_LEASE, "node-b", SCHEDULER_LEASE_TTL)
        .await
        .unwrap()
        .expect("free lease");
    assert!(
        acquire_lease(&store_a, SCHEDULER_LEASE, &node_a, SCHEDULER_LEASE_TTL)
            .await
            .unwrap()
            .is_none()
    );
    let report = schedule_placements(&store_b).await.unwrap();
    assert_eq!((report.added, report.removed), (2, 0));
    assert_eq!(
        present_nodes(&store_a, &instance.id).await,
        set(&[&node_a, "node-b"])
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
    let b_messages = node_messages(&store_a, "node-b").await;
    assert!(b_messages.iter().any(|m| matches!(
        m,
        NodeMessage::DeploymentChanged { desired, .. } if desired == "placed"
    )));

    // Node B's daemon stops heartbeating: its replica is set absent, and
    // with no other `nas` node the placement reports the shortfall.
    heartbeat_consumer(&store_a, "node-b", Duration::from_secs(600)).await;
    let moved = schedule_placements(&store_b).await.unwrap();
    assert_eq!((moved.added, moved.removed), (0, 1));
    assert_eq!(present_nodes(&store_a, &instance.id).await, set(&[&node_a]));
    let short = load_plugin_placement(&store_a, &instance.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(short.placed, 1);
    assert!(short.detail.starts_with("1 of 2"), "{}", short.detail);

    // Labelling node C makes it a candidate; the next pass fills it.
    set_node_labels(&store_a, &operator(), "node-c", &labels(&["nas"]))
        .await
        .unwrap();
    let filled = schedule_placements(&store_b).await.unwrap();
    assert_eq!(filled.added, 1);
    assert_eq!(
        present_nodes(&store_a, &instance.id).await,
        set(&[&node_a, "node-c"])
    );

    // Pinning by the implicit node label, then scaling to zero.
    let PlacementWrite::Applied(pinned) = put_plugin_placement(
        &store_a,
        &operator(),
        &instance.id,
        &spec(1, &[&format!("node={node_a}")]),
        Some(1),
    )
    .await
    .unwrap() else {
        panic!("current revision applies");
    };
    assert_eq!(pinned.revision, 2);
    schedule_placements(&store_b).await.unwrap();
    assert_eq!(present_nodes(&store_a, &instance.id).await, set(&[&node_a]));
    put_plugin_placement(&store_a, &operator(), &instance.id, &spec(0, &[]), Some(2))
        .await
        .unwrap();
    schedule_placements(&store_b).await.unwrap();
    assert!(present_nodes(&store_a, &instance.id).await.is_empty());

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
    assert_eq!(present_nodes(&store_a, &instance.id).await.len(), 1);

    // Dropping the placement hands the instance back to enable/disable:
    // `placed` becomes `present`, `unplaced` becomes `absent`, and every
    // node keeps doing what it did.
    let before = present_nodes(&store_a, &instance.id).await;
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
    assert_eq!(present_nodes(&store_a, &instance.id).await, before);
    assert_eq!(
        schedule_placements(&store_b).await.unwrap(),
        ScheduleReport::default()
    );
    release_lease(&store_b, &lease).await.unwrap();
    assert!(
        acquire_lease(&store_a, SCHEDULER_LEASE, &node_a, SCHEDULER_LEASE_TTL)
            .await
            .unwrap()
            .is_some(),
        "a released lease is free at once"
    );
}

#[tokio::test]
async fn a_version_without_an_artifact_for_a_node_target_skips_that_node() {
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
    register(&store, "node-arm", &session.cluster_id).await;
    // A row from an arm node: rewrite its target as that node would report.
    crate::entities::nodes::Entity::update_many()
        .col_expr(
            crate::entities::nodes::Column::Target,
            sea_orm::sea_query::Expr::value("linux-arm64-gnu"),
        )
        .filter(crate::entities::nodes::Column::NodeId.eq("node-arm"))
        .exec(store.db())
        .await
        .unwrap();
    for node in [session.node.node_id.as_str(), "node-arm"] {
        heartbeat_consumer(&store, node, Duration::ZERO).await;
    }
    let key = "path:file:///tmp/x86-only#x86only";
    let release = PluginVersion {
        plugin_key: key.into(),
        version: "1.0.0".into(),
        manifest_id: "x86only".into(),
        manifest_toml: "id = \"x86only\"\nversion = \"1.0.0\"\n".into(),
        settings: bookclerk_plugin_abi::SettingsSchema::default(),
        artifacts: vec![PluginArtifact {
            target: node_target().into(),
            url: format!("{ARTIFACT_BLOB_SCHEME}{}", "a".repeat(64)),
            format: "tar.gz".into(),
            archive_root: ".".into(),
            archive_sha256: "a".repeat(64),
            payload_root_sha256: "b".repeat(64),
            executable: "guest".into(),
        }],
        publisher_verified: true,
        grant_json: String::new(),
        coordinate: String::new(),
        installed_at: String::new(),
        installed_by: String::new(),
    };
    add_plugin_version(&store, &operator(), release)
        .await
        .unwrap();
    activate_version(&store, &operator(), key, "1.0.0", &session.node.node_id)
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
        present_nodes(&store, &instance.id).await,
        set(&[&session.node.node_id])
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

/// Node-queue messages on `node`'s queue, oldest first.
async fn node_messages(store: &LibraryStore, node: &str) -> Vec<NodeMessage> {
    let mut rows = store
        .list_domain_events_for_queue(&node_queue_name(node), 200)
        .await
        .expect("queue rows");
    rows.reverse();
    rows.into_iter()
        .map(|row| serde_json::from_str(&row.payload).expect("node message body"))
        .collect()
}

#[tokio::test]
async fn placement_follows_heartbeat_headroom_availability_and_drains() {
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
    register(&store, "vps", &session.cluster_id).await;
    register(&store, "desk", &session.cluster_id).await;
    let gib: i64 = 1 << 30;
    // A small always-on VPS and a big desktop that is often off.
    let vps = |s: &mut NodeStatus| {
        s.cpu_budget_millicores = 800;
        s.memory_budget_bytes = Some(gib);
        s.memory_available_bytes = Some(gib);
    };
    let desk = |s: &mut NodeStatus| {
        s.cpu_budget_millicores = 12_000;
        s.memory_budget_bytes = Some(24 * gib);
        s.memory_available_bytes = Some(24 * gib);
        s.availability_ppm = 400_000;
    };
    heartbeat_with(&store, "vps", Duration::ZERO, vps).await;
    heartbeat_with(&store, "desk", Duration::ZERO, desk).await;

    let instance = create_plugin_instance(&store, &operator(), "path:file:///tmp/ra#ra", "")
        .await
        .unwrap();
    put_plugin_placement(&store, &operator(), &instance.id, &spec(1, &[]), None)
        .await
        .unwrap();
    schedule_placements(&store).await.unwrap();
    assert_eq!(
        present_nodes(&store, &instance.id).await,
        set(&["desk"]),
        "even at 40% availability the desktop has far more room"
    );

    // The desktop goes to sleep: its replica moves to the VPS.
    heartbeat_with(&store, "desk", Duration::from_secs(600), desk).await;
    schedule_placements(&store).await.unwrap();
    assert_eq!(present_nodes(&store, &instance.id).await, set(&["vps"]));

    // Draining the VPS leaves nowhere to run it; the placement says why.
    heartbeat_with(&store, "vps", Duration::ZERO, |s| {
        vps(s);
        s.schedulable = false;
    })
    .await;
    schedule_placements(&store).await.unwrap();
    assert!(present_nodes(&store, &instance.id).await.is_empty());
    let short = load_plugin_placement(&store, &instance.id)
        .await
        .unwrap()
        .unwrap();
    assert!(short.detail.starts_with("0 of 1"), "{}", short.detail);

    // `all` runs it on every live, schedulable node with room: the VPS is
    // back, the desktop is awake, and this node has no heartbeat at all.
    heartbeat_with(&store, "vps", Duration::ZERO, vps).await;
    heartbeat_with(&store, "desk", Duration::ZERO, desk).await;
    put_plugin_placement(
        &store,
        &operator(),
        &instance.id,
        &PlacementSpec {
            replicas: Replicas::All,
            constraints: Vec::new(),
        },
        Some(1),
    )
    .await
    .unwrap();
    schedule_placements(&store).await.unwrap();
    assert_eq!(
        present_nodes(&store, &instance.id).await,
        set(&["desk", "vps"])
    );

    // A guest bigger than the VPS's memory budget does not go there.
    let big = create_plugin_instance(&store, &operator(), "path:file:///tmp/big#big", "")
        .await
        .unwrap();
    let deployment = ensure_plugin_deployment(
        &store,
        &ConfigActor::Bootstrap,
        &big.id,
        "desk",
        DesiredState::Present,
    )
    .await
    .unwrap();
    upsert_observation(
        &store,
        &DeploymentObservation {
            deployment_id: deployment.deployment_id.clone(),
            node_id: "desk".into(),
            incarnation: "test".into(),
            status: DeploymentStatus::Healthy,
            detail: String::new(),
            applied_config_revision: None,
            observed_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .await
    .unwrap();
    record_deployment_reservation(
        &store,
        &deployment.deployment_id,
        "desk",
        GuestReservation {
            memory_bytes: 2 * gib,
            cpu_millicores: 1000,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        instance_demand(&store, &big.id).await.unwrap(),
        Some(GuestReservation {
            memory_bytes: 2 * gib,
            cpu_millicores: 1000,
        })
    );
    put_plugin_placement(
        &store,
        &operator(),
        &big.id,
        &PlacementSpec {
            replicas: Replicas::All,
            constraints: Vec::new(),
        },
        None,
    )
    .await
    .unwrap();
    schedule_placements(&store).await.unwrap();
    assert_eq!(
        present_nodes(&store, &big.id).await,
        set(&["desk"]),
        "2 GiB does not fit the VPS's 1 GiB budget"
    );
    let rows = list_deployments_for_instance(&store, &big.id)
        .await
        .unwrap();
    assert_eq!(
        rows[0].desired,
        DesiredState::Placed,
        "the operator's row became the scheduler's"
    );
}

#[tokio::test]
async fn deliveries_route_to_the_live_node_with_the_most_room() {
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
    for node in ["vps", "desk", "cold"] {
        register(&store, node, &session.cluster_id).await;
    }
    heartbeat_with(&store, "vps", Duration::ZERO, |s| {
        s.cpu_budget_millicores = 800;
    })
    .await;
    heartbeat_with(&store, "desk", Duration::ZERO, |s| {
        s.cpu_budget_millicores = 12_000;
    })
    .await;
    let instance = create_plugin_instance(&store, &operator(), "path:file:///tmp/rt#rt", "")
        .await
        .unwrap();
    for node in ["vps", "desk", "cold"] {
        let deployment = ensure_plugin_deployment(
            &store,
            &ConfigActor::Bootstrap,
            &instance.id,
            node,
            DesiredState::Present,
        )
        .await
        .unwrap();
        upsert_observation(
            &store,
            &DeploymentObservation {
                deployment_id: deployment.deployment_id,
                node_id: node.into(),
                incarnation: "test".into(),
                status: DeploymentStatus::Healthy,
                detail: String::new(),
                applied_config_revision: None,
                observed_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .await
        .unwrap();
    }
    let ids = vec![instance.id.to_string(), "path:not-an-instance".to_string()];
    let desk = route_ranks(&store, "desk", &ids).await.unwrap();
    assert_eq!(desk.len(), 1, "only instance ids are routed");
    let desk = &desk[instance.id.as_str()];
    assert_eq!(desk.rank, 0);
    assert_eq!(desk.nodes, vec!["desk", "vps"], "`cold` never heartbeated");
    let vps = &route_ranks(&store, "vps", &ids).await.unwrap()[instance.id.as_str()];
    assert_eq!(vps.rank, 1);
    assert_eq!(vps.defer(), ROUTE_DEFER_PER_RANK);
    let cold = &route_ranks(&store, "cold", &ids).await.unwrap()[instance.id.as_str()];
    assert_eq!(
        cold.rank, 2,
        "a node without a heartbeat waits behind every live one"
    );

    // The desktop sleeps: the VPS is now first in line.
    heartbeat_with(&store, "desk", Duration::from_secs(600), |_| {}).await;
    let vps = &route_ranks(&store, "vps", &ids).await.unwrap()[instance.id.as_str()];
    assert_eq!(vps.rank, 0);
    assert_eq!(vps.nodes, vec!["vps"]);
}

#[tokio::test]
async fn a_node_owner_edits_only_its_own_node_runtime() {
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
    let mine = session.node.node_id.clone();
    register(&store, "node-other", &session.cluster_id).await;
    let host = store
        .create_user(crate::UserRole::Host, Some("Hosting"), None)
        .await
        .unwrap();
    let stranger = store
        .create_user(crate::UserRole::Host, Some("Stranger"), None)
        .await
        .unwrap();

    // Every node starts as an owner node with no owner.
    let node = load_node(&store, &mine).await.unwrap();
    assert_eq!(node.kind, NodeKind::Owner);
    assert_eq!(node.owner_user_id, None);

    // Only an operator assigns ownership, and only to a real user.
    let refused = set_node_owner(&store, &ConfigActor::Bootstrap, &mine, Some(host.id))
        .await
        .unwrap_err();
    assert!(matches!(refused, LibraryError::Forbidden(_)), "{refused}");
    let unknown = set_node_owner(&store, &operator(), &mine, Some(host.id + 1000))
        .await
        .unwrap_err();
    assert!(matches!(unknown, LibraryError::NotFound(_)), "{unknown}");
    // Owning a node needs no role: a Member may own one (trust still
    // follows the role). A disabled user cannot sign in, so it may not.
    let member = store
        .create_user(crate::UserRole::Member, Some("Member"), None)
        .await
        .unwrap();
    let member_owned = set_node_owner(&store, &operator(), "node-other", Some(member.id))
        .await
        .unwrap();
    assert_eq!(member_owned.owner_user_id, Some(member.id));
    verify_node_owner(&store, member.id, "node-other")
        .await
        .unwrap();
    set_node_owner(&store, &operator(), "node-other", None)
        .await
        .unwrap();
    let gone = store
        .create_user(crate::UserRole::Host, Some("Gone"), None)
        .await
        .unwrap();
    store
        .set_user_status(gone.id, crate::UserStatus::Disabled)
        .await
        .unwrap();
    let disabled = set_node_owner(&store, &operator(), &mine, Some(gone.id))
        .await
        .unwrap_err();
    assert!(matches!(disabled, LibraryError::Invalid(_)), "{disabled}");
    assert_eq!(load_node(&store, &mine).await.unwrap().owner_user_id, None);
    let owned = set_node_owner(&store, &operator(), &mine, Some(host.id))
        .await
        .unwrap();
    assert_eq!(owned.owner_user_id, Some(host.id));
    assert_eq!(
        nodes_owned_by(&store, host.id)
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.node_id)
            .collect::<Vec<_>>(),
        vec![mine.clone()]
    );
    assert!(nodes_owned_by(&store, stranger.id)
        .await
        .unwrap()
        .is_empty());

    // The owner changes budgets and drain on its node.
    let actor = verify_node_owner(&store, host.id, &mine).await.unwrap();
    let doc = update_node_runtime(&store, &actor, &mine, |rt| {
        rt.cpu_budget_percent = 40;
        rt.schedulable = false;
    })
    .await
    .unwrap();
    assert_eq!(doc.body.cpu_budget_percent, 40);
    assert!(!doc.body.schedulable);
    assert_eq!(doc.updated_by, format!("node-owner:{}", host.id));

    // It cannot touch another node, set labels, or write other documents.
    let other = verify_node_owner(&store, host.id, "node-other")
        .await
        .unwrap_err();
    assert!(matches!(other, LibraryError::Forbidden(_)), "{other}");
    let missing = verify_node_owner(&store, host.id, "no-such-node")
        .await
        .unwrap_err();
    assert!(matches!(missing, LibraryError::Forbidden(_)), "{missing}");
    let foreign = update_node_runtime(&store, &actor, "node-other", |rt| {
        rt.cpu_budget_percent = 10;
    })
    .await
    .unwrap_err();
    assert!(matches!(foreign, LibraryError::Forbidden(_)), "{foreign}");
    let labels = set_node_labels(&store, &actor, &mine, &labels(&["gpu"]))
        .await
        .unwrap_err();
    assert!(matches!(labels, LibraryError::Forbidden(_)), "{labels}");
    let current_events = load_events(&store).await.unwrap();
    let events = replace_events(
        &store,
        &actor,
        current_events.revision,
        &current_events.body,
        "owner-events",
    )
    .await
    .unwrap_err();
    assert!(matches!(events, LibraryError::Forbidden(_)), "{events}");
    let stranger_actor = verify_node_owner(&store, stranger.id, &mine).await;
    assert!(stranger_actor.is_err());

    // Clearing ownership revokes the next verification.
    set_node_owner(&store, &operator(), &mine, None)
        .await
        .unwrap();
    assert!(verify_node_owner(&store, host.id, &mine).await.is_err());
}

/// A two-node cluster with live heartbeats, for the hand-back tests.
async fn two_live_nodes(store: &LibraryStore, dir: &std::path::Path) -> String {
    let session = bootstrap_control_plane(
        store,
        dir,
        &dir.join("cache"),
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    register(store, "node-b", &session.cluster_id).await;
    for node in [session.node.node_id.as_str(), "node-b"] {
        heartbeat_consumer(store, node, Duration::ZERO).await;
    }
    session.node.node_id
}

#[tokio::test]
async fn a_pass_hands_back_scheduler_rows_whose_placement_is_gone() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir.path().join("library.db")).await;
    let node_a = two_live_nodes(&store, dir.path()).await;
    let instance =
        create_plugin_instance(&store, &operator(), "path:file:///tmp/orphan#orphan", "")
            .await
            .unwrap();
    put_plugin_placement(&store, &operator(), &instance.id, &spec(2, &[]), None)
        .await
        .unwrap();
    schedule_placements(&store).await.unwrap();
    assert_eq!(
        present_nodes(&store, &instance.id).await,
        set(&[&node_a, "node-b"])
    );

    // A pass that raced `--manual` wrote its rows after the placement row
    // was deleted and the hand-back had run: the rows stay `placed`.
    crate::entities::plugin_placements::Entity::delete_by_id(instance.id.as_str().to_string())
        .exec(store.db())
        .await
        .unwrap();
    let report = schedule_placements(&store).await.unwrap();
    assert_eq!(report.handed_back, 2, "{report:?}");
    let rows = list_deployments_for_instance(&store, &instance.id)
        .await
        .unwrap();
    assert!(
        rows.iter().all(|row| row.desired == DesiredState::Present),
        "{rows:?}"
    );
    // The scheduler may only hand back a row it owns: never rewrite one the
    // operator already holds.
    let refused = replace_plugin_deployment(
        &store,
        &ConfigActor::Scheduler,
        &rows[0].deployment_id,
        rows[0].revision,
        DesiredState::Absent,
        "scheduler-absent-on-present",
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, LibraryError::Forbidden(_)), "{refused}");
    // The operator owns the rows again.
    ensure_hand_managed(&store, &instance.id).await.unwrap();
    assert_eq!(schedule_placements(&store).await.unwrap().handed_back, 0);
}

#[tokio::test]
async fn undeploy_instance_drops_the_placement_and_stops_every_replica() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir.path().join("library.db")).await;
    two_live_nodes(&store, dir.path()).await;
    let instance = create_plugin_instance(&store, &operator(), "path:file:///tmp/gone#gone", "")
        .await
        .unwrap();
    put_plugin_placement(&store, &operator(), &instance.id, &spec(2, &[]), None)
        .await
        .unwrap();
    schedule_placements(&store).await.unwrap();
    assert_eq!(present_nodes(&store, &instance.id).await.len(), 2);

    assert!(
        undeploy_instance(&store, &ConfigActor::Bootstrap, &instance.id)
            .await
            .is_err()
    );
    assert_eq!(
        undeploy_instance(&store, &operator(), &instance.id)
            .await
            .unwrap(),
        2
    );
    assert!(load_plugin_placement(&store, &instance.id)
        .await
        .unwrap()
        .is_none());
    assert!(present_nodes(&store, &instance.id).await.is_empty());
    // A later pass has nothing to place or hand back.
    let report = schedule_placements(&store).await.unwrap();
    assert_eq!((report.added, report.handed_back), (0, 0), "{report:?}");
}
