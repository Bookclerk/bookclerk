//! The placement scheduler: turns `plugin_placements` intent into
//! deployments.
//!
//! Every daemon runs this loop, but only the one holding the cluster's
//! `scheduler` lease acts; the others find the lease held and wait. The
//! holder renews it on every pass, so a dead holder's lease expires and the
//! next daemon to try takes over with a larger fencing token. A pass runs on
//! a `placement.changed` or `release.changed` wakeup and at least every
//! [`PLACEMENT_INTERVAL`], which also notices hosts whose daemons stopped
//! heartbeating.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    acquire_lease, load_or_create_host_identity, schedule_placements, ScheduleReport,
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
                () = crate::host_queue::placement_wakeup() => {}
                () = tokio::time::sleep(PLACEMENT_INTERVAL) => {}
            }
        }
    });
}

/// One pass: take or renew the lease, then schedule. Returns `None` when
/// another host holds the lease or the pass failed.
pub async fn run_placement_pass(state: &AppState) -> Option<ScheduleReport> {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let host_id = match load_or_create_host_identity(&files_dir) {
        Ok(identity) => identity.host_id,
        Err(err) => {
            warn!(error = %err, "host identity unavailable; scheduler idle");
            return None;
        }
    };
    let library = state.library_snapshot().await;
    match acquire_lease(&library, SCHEDULER_LEASE, &host_id, SCHEDULER_LEASE_TTL).await {
        Ok(Some(lease)) => debug!(token = lease.token, "holding the scheduler lease"),
        Ok(None) => return None,
        Err(err) => {
            warn!(error = %err, "scheduler lease unavailable");
            return None;
        }
    }
    match schedule_placements(&library).await {
        Ok(report) => {
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
    use axum::routing::{get, put};
    use bookclerk_config::{Config, EventsConfig};
    use bookclerk_library::control_plane::{
        bootstrap_control_plane, create_plugin_instance, host_consumer_id, host_queue_name,
        list_deployments_for_instance, ConfigActor, DESIRED_PRESENT,
    };
    use bookclerk_library::EventCatalogSubscription;
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
            &EventsConfig::default(),
        )
        .await
        .unwrap();
        let host = session.host.host_id.clone();
        // This daemon's host queue consumer is live.
        store
            .upsert_event_subscriber(
                "node-test",
                &host_consumer_id(&host),
                &[EventCatalogSubscription::queue(host_queue_name(&host))],
                true,
            )
            .await
            .unwrap();
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
            .route("/api/config/hosts", get(crate::config_authority::get_hosts))
            .route(
                "/api/config/hosts/{host_id}/labels",
                put(crate::config_authority::put_host_labels),
            )
            .route(
                "/api/config/plugin-instances/{id}/placement",
                put(crate::config_authority::put_plugin_placement_route)
                    .delete(crate::config_authority::delete_plugin_placement_route),
            )
            .route(
                "/api/config/plugin-instances/{id}/deployments/{host_id}",
                put(crate::config_authority::put_plugin_deployment),
            )
            .with_state(Arc::clone(&state));

        let (status, labelled) = send(
            &app,
            "PUT",
            &format!("/api/config/hosts/{host}/labels"),
            json!({ "labels": ["nas"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{labelled}");
        assert_eq!(labelled["labels"], json!(["nas"]));
        assert_eq!(labelled["live"], true);
        let (status, _) = send(
            &app,
            "PUT",
            &format!("/api/config/hosts/{host}/labels"),
            json!({ "labels": ["Bad Label"] }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, hosts) = send(&app, "GET", "/api/config/hosts", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert!(hosts["hosts"][0]["implicit_labels"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("host={host}"))));

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
            (rows[0].host_id.as_str(), rows[0].desired.as_str()),
            (host.as_str(), DESIRED_PRESENT)
        );

        let deployment_uri = format!(
            "/api/config/plugin-instances/{}/deployments/{host}",
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
}
