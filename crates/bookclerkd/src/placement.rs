//! The placement scheduler: turns `plugin_placements` intent into
//! deployments.
//!
//! Every daemon runs this loop, but only the one holding the cluster's
//! `scheduler` lease acts; the others find the lease held and wait. The
//! holder renews it on every pass, so a dead holder's lease expires and the
//! next daemon to try takes over with a larger fencing token. A pass runs on
//! a `placement.changed` or `install.changed` wakeup and at least every
//! [`PLACEMENT_INTERVAL`], which also notices nodes whose daemons stopped
//! heartbeating.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    acquire_lease, load_or_create_node_identity, schedule_placements, ScheduleReport,
    SCHEDULER_LEASE, SCHEDULER_LEASE_TTL,
};
use tracing::{debug, info, warn};

use crate::api::AppState;

/// Longest gap between scheduler passes; well inside
/// [`SCHEDULER_LEASE_TTL`] so the holder keeps its lease.
pub const PLACEMENT_INTERVAL: Duration = Duration::from_secs(20);

/// Starts the scheduler loop.
pub fn spawn_placement_scheduler(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            run_placement_pass(&state).await;
            tokio::select! {
                () = crate::node_queue::placement_wakeup() => {}
                () = tokio::time::sleep(PLACEMENT_INTERVAL) => {}
            }
        }
    });
}

/// One pass: take or renew the lease, then schedule. Returns `None` when
/// another node holds the lease or the pass failed.
pub async fn run_placement_pass(state: &AppState) -> Option<ScheduleReport> {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let node_id = match load_or_create_node_identity(&files_dir) {
        Ok(identity) => identity.node_id,
        Err(err) => {
            warn!(error = %err, "node identity unavailable; scheduler idle");
            return None;
        }
    };
    let library = state.library_snapshot().await;
    match acquire_lease(&library, SCHEDULER_LEASE, &node_id, SCHEDULER_LEASE_TTL).await {
        Ok(Some(lease)) => debug!(token = lease.token, "holding the scheduler lease"),
        Ok(None) => return None,
        Err(err) => {
            warn!(error = %err, "scheduler lease unavailable");
            return None;
        }
    }
    match schedule_placements(&library).await {
        Ok(report) => {
            if report.added + report.removed > 0 {
                // The scheduler moved replicas: queued jobs may need re-routing.
                crate::node_queue::wake_requirements_sweep();
            }
            if report.added + report.removed + report.conflicts > 0 {
                info!(
                    placements = report.placements,
                    added = report.added,
                    removed = report.removed,
                    conflicts = report.conflicts,
                    "placement pass changed deployments"
                );
            }
            Some(report)
        }
        Err(err) => {
            warn!(error = %err, "placement pass failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{delete, get, put};
    use bookclerk_config::Config;
    use bookclerk_library::control_plane::{
        bootstrap_control_plane, create_plugin_instance, list_deployments_for_instance,
        ConfigActor, DesiredState,
    };
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    async fn send(app: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn placement_routes_label_place_schedule_and_guard_enable() {
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
            &bookclerk_config::Config::default(),
        )
        .await
        .unwrap();
        let node = session.node.node_id.clone();
        // This daemon is live: it heartbeats its detected capacity.
        let heartbeat = bookclerk_library::control_plane::NodeSampler::new(files_path).sample();
        let status =
            bookclerk_library::control_plane::record_node_heartbeat(&store, &node, &heartbeat)
                .await
                .unwrap();
        assert!(status.cpu_millicores >= 1000);
        assert_eq!(status.running_guests, 0);
        let instance = create_plugin_instance(
            &store,
            &ConfigActor::Operator {
                id: "operator".into(),
            },
            "path:file:///tmp/daemon-placed#placed",
            "",
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
        let app = axum::Router::new()
            .route("/api/config/nodes", get(crate::config_authority::get_nodes))
            .route(
                "/api/config/nodes/{node_id}/labels",
                put(crate::config_authority::put_node_labels),
            )
            .route(
                "/api/config/plugin-instances/{id}/placement",
                put(crate::config_authority::put_plugin_placement_route)
                    .delete(crate::config_authority::delete_plugin_placement_route),
            )
            .route(
                "/api/config/plugin-instances/{id}/deployments/{node_id}",
                put(crate::config_authority::put_plugin_deployment),
            )
            .layer(axum::Extension(crate::auth::NodeAdminCaller::Operator))
            .with_state(Arc::clone(&state));

        let (status, labelled) = send(
            &app,
            "PUT",
            &format!("/api/config/nodes/{node}/labels"),
            json!({ "labels": ["nas"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{labelled}");
        assert_eq!(labelled["labels"], json!(["nas"]));
        assert_eq!(labelled["live"], true);
        let (status, _) = send(
            &app,
            "PUT",
            &format!("/api/config/nodes/{node}/labels"),
            json!({ "labels": ["Bad Label"] }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, nodes) = send(&app, "GET", "/api/config/nodes", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert!(nodes["nodes"][0]["implicit_labels"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("node={node}"))));

        let placement_uri = format!("/api/config/plugin-instances/{}/placement", instance.id);
        let (status, placed) = send(
            &app,
            "PUT",
            &placement_uri,
            json!({ "replicas": 1, "constraints": ["nas"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{placed}");
        assert_eq!(placed["placement"]["revision"], 1);
        let (status, stale) = send(
            &app,
            "PUT",
            &placement_uri,
            json!({ "replicas": 2, "expected_revision": 7 }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{stale}");
        assert_eq!(stale["current_revision"], 1);

        // This daemon takes the scheduler lease and deploys on itself.
        let report = super::run_placement_pass(&state).await.expect("lease");
        assert_eq!(report.added, 1);
        let rows = list_deployments_for_instance(&store, &instance.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].node_id.as_str(), rows[0].desired),
            (node.as_str(), DesiredState::Placed)
        );

        let deployment_uri = format!(
            "/api/config/plugin-instances/{}/deployments/{node}",
            instance.id
        );
        let (status, refused) =
            send(&app, "PUT", &deployment_uri, json!({ "desired": "absent" })).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert!(refused["error"]
            .as_str()
            .unwrap()
            .contains("placed by the scheduler"));

        let (status, dropped) = send(&app, "DELETE", &placement_uri, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dropped["deleted"], true);
        let (status, disabled) =
            send(&app, "PUT", &deployment_uri, json!({ "desired": "absent" })).await;
        assert_eq!(status, StatusCode::OK, "{disabled}");
        assert_eq!(disabled["desired"], "absent");
    }

    /// Node routes for one caller, as the daemon mounts them.
    fn node_routes(
        state: &Arc<crate::api::AppState>,
        caller: crate::auth::NodeAdminCaller,
    ) -> axum::Router {
        axum::Router::new()
            .route("/api/config/nodes", get(crate::config_authority::get_nodes))
            .route(
                "/api/config/nodes/{node_id}/runtime",
                put(crate::config_authority::put_node_runtime),
            )
            .route(
                "/api/cluster/status",
                get(crate::config_authority::get_cluster_status),
            )
            .route(
                "/api/config/nodes/{node_id}/owner",
                put(crate::config_authority::put_node_owner),
            )
            .layer(axum::Extension(caller))
            .with_state(Arc::clone(state))
    }

    #[tokio::test]
    async fn a_node_owner_sees_and_edits_only_its_own_nodes() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let files_path = files.path();
        std::fs::write(files_path.join("config.toml"), "").unwrap();
        let db_path = files_path.join("library.db");
        let db = bookclerk_plugin_database_sqlite::open(&db_path)
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let mine = bootstrap_control_plane(
            &store,
            files_path,
            &files_path.join("cache"),
            None,
            &bookclerk_config::Config::default(),
        )
        .await
        .unwrap()
        .node
        .node_id;
        // A second node in the same cluster, owned by nobody. Every node
        // shares the cluster's `master.key`.
        std::fs::copy(
            bookclerk_library::master_key_path(files_path),
            bookclerk_library::master_key_path(other.path()),
        )
        .unwrap();
        let theirs = bootstrap_control_plane(
            &store,
            other.path(),
            &other.path().join("cache"),
            None,
            &bookclerk_config::Config::default(),
        )
        .await
        .unwrap()
        .node
        .node_id;
        let host = store
            .create_user(bookclerk_library::UserRole::Host, Some("Hosting"), None)
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
        let operator = node_routes(&state, crate::auth::NodeAdminCaller::Operator);
        let owner = node_routes(
            &state,
            crate::auth::NodeAdminCaller::NodeOwner { user_id: host.id },
        );

        // Before an operator assigns it, the host owns nothing.
        let (status, listed) = send(&owner, "GET", "/api/config/nodes", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["nodes"], json!([]));
        assert_eq!(listed["capabilities"]["scope"], "owned");
        assert_eq!(listed["capabilities"]["can_edit_labels"], false);

        let (status, assigned) = send(
            &operator,
            "PUT",
            &format!("/api/config/nodes/{mine}/owner"),
            json!({ "user_id": host.id }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{assigned}");
        assert_eq!(assigned["owner_user_id"], host.id);
        let (status, _) = send(
            &operator,
            "PUT",
            &format!("/api/config/nodes/{mine}/owner"),
            json!({ "user_id": host.id + 1000 }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The owner lists only its node, with the owner attached.
        let (_, listed) = send(&owner, "GET", "/api/config/nodes", Value::Null).await;
        let nodes = listed["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["node_id"], json!(mine));
        assert_eq!(nodes[0]["kind"], "owner");
        assert_eq!(nodes[0]["owner"]["user_id"], host.id);
        assert_eq!(nodes[0]["owner"]["role"], "host");
        let (_, all) = send(&operator, "GET", "/api/config/nodes", Value::Null).await;
        assert_eq!(all["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(all["capabilities"]["scope"], "all");

        // It edits its own node's budgets and drain switch...
        let (status, edited) = send(
            &owner,
            "PUT",
            &format!("/api/config/nodes/{mine}/runtime"),
            json!({ "cpu_budget_percent": 50, "schedulable": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{edited}");
        assert_eq!(edited["runtime"]["cpu_budget_percent"], 50);
        assert_eq!(edited["runtime"]["schedulable"], false);
        // ...but not another node's, and an unknown node reads the same.
        for node in [theirs.as_str(), "no-such-node"] {
            let (status, refused) = send(
                &owner,
                "PUT",
                &format!("/api/config/nodes/{node}/runtime"),
                json!({ "cpu_budget_percent": 10 }),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{node}: {refused}");
        }

        // Cluster status counts every node but details only the owned one.
        let (status, cluster) = send(&owner, "GET", "/api/cluster/status", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{cluster}");
        assert_eq!(cluster["cluster"]["nodes"], 2);
        assert_eq!(cluster["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(cluster["nodes"][0]["node_id"], json!(mine));
        let (_, cluster) = send(&operator, "GET", "/api/cluster/status", Value::Null).await;
        assert_eq!(cluster["nodes"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_owner_lists_every_clerk_and_a_member_only_its_own() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let files_path = files.path();
        std::fs::write(files_path.join("config.toml"), "").unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let mine = bootstrap_control_plane(
            &store,
            files_path,
            &files_path.join("cache"),
            None,
            &bookclerk_config::Config::default(),
        )
        .await
        .unwrap()
        .node
        .node_id;
        // A stopped clerk sharing the database and `master.key`.
        std::fs::copy(
            bookclerk_library::master_key_path(files_path),
            bookclerk_library::master_key_path(other.path()),
        )
        .unwrap();
        let clerk = bootstrap_control_plane(
            &store,
            other.path(),
            &other.path().join("cache"),
            None,
            &bookclerk_config::Config::default(),
        )
        .await
        .unwrap()
        .node
        .node_id;
        let owner = store
            .create_user(bookclerk_library::UserRole::Owner, Some("Owner"), None)
            .await
            .unwrap();
        let member = store
            .create_user(bookclerk_library::UserRole::Member, Some("Member"), None)
            .await
            .unwrap();
        let host = store
            .create_user(bookclerk_library::UserRole::Host, Some("Hosting"), None)
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
        let routes = |caller: crate::auth::NodeAdminCaller| {
            axum::Router::new()
                .route("/api/config/nodes", get(crate::config_authority::get_nodes))
                .route(
                    "/api/config/clerks",
                    get(crate::config_authority::get_clerks),
                )
                .route(
                    "/api/config/nodes/{node_id}",
                    delete(crate::config_authority::delete_owner_node),
                )
                .route(
                    "/api/config/nodes/{node_id}/owner",
                    put(crate::config_authority::put_node_owner),
                )
                .layer(axum::Extension(caller))
                .with_state(Arc::clone(&state))
        };
        let operator = routes(crate::auth::NodeAdminCaller::Operator);
        let as_owner = routes(crate::auth::NodeAdminCaller::NodeOwner { user_id: owner.id });
        let as_member = routes(crate::auth::NodeAdminCaller::NodeOwner { user_id: member.id });
        let as_host = routes(crate::auth::NodeAdminCaller::NodeOwner { user_id: host.id });
        // The member owns the clerk; the Owner owns neither node.
        let (status, assigned) = send(
            &operator,
            "PUT",
            &format!("/api/config/nodes/{clerk}/owner"),
            json!({ "user_id": member.id }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{assigned}");

        let ids = |listed: &Value| -> Vec<String> {
            let mut ids: Vec<String> = listed["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|node| node["node_id"].as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        };
        let mut both = vec![mine.clone(), clerk.clone()];
        both.sort();
        // The operator and the Owner see this node and the clerk.
        for app in [&operator, &as_owner] {
            let (status, listed) = send(app, "GET", "/api/config/clerks", Value::Null).await;
            assert_eq!(status, StatusCode::OK, "{listed}");
            assert_eq!(ids(&listed), both);
            assert_eq!(listed["node_id"], json!(mine));
        }
        // The nodes listing stays scoped to owned nodes for the Owner.
        let (_, listed) = send(&as_owner, "GET", "/api/config/nodes", Value::Null).await;
        assert_eq!(listed["nodes"], json!([]));
        // A member sees only the clerk it owns, as the nodes listing does;
        // a host owning nothing sees none.
        let (_, listed) = send(&as_member, "GET", "/api/config/clerks", Value::Null).await;
        assert_eq!(ids(&listed), std::slice::from_ref(&clerk));
        let (_, nodes) = send(&as_member, "GET", "/api/config/nodes", Value::Null).await;
        assert_eq!(ids(&nodes), std::slice::from_ref(&clerk));
        let (_, listed) = send(&as_host, "GET", "/api/config/clerks", Value::Null).await;
        assert_eq!(listed["nodes"], json!([]));

        // The Owner removes the stopped clerk it sees there.
        let (status, removed) = send(
            &as_owner,
            "DELETE",
            &format!("/api/config/nodes/{clerk}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{removed}");
        let (_, listed) = send(&as_owner, "GET", "/api/config/clerks", Value::Null).await;
        assert_eq!(ids(&listed), [mine]);
    }
}
