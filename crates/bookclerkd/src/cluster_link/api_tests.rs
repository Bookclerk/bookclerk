//! Handler tests for `/api/config/cluster-consents` as the daemon mounts
//! them: operator only, the list (links without key material), approve
//! with narrowing and the reviewed-version check, refusals, and revoke.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bookclerk_library::control_plane::{load_remote_plugin_consent, record_remote_plugin_request};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_host::PluginGrant;
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;

/// The operator token of the test daemon.
const TOKEN: &str = "consent-api-token";

/// Owner cluster id of the recorded request.
const CLUSTER: &str = "cluster-a";

/// Plugin key of the recorded request.
const PLUGIN: &str = "path:file:///tmp/store#store";

/// What the owner asks to run here.
fn requested() -> PluginGrant {
    PluginGrant {
        schema_version: bookclerk_plugin_host::GRANT_SCHEMA_VERSION,
        plugin_key: PLUGIN.into(),
        plugin_id: "store".into(),
        network_mode: "outbound".into(),
        domains: [
            "api.store.example".to_string(),
            "cdn.store.example".to_string(),
        ]
        .into(),
        manifest_domains: [
            "api.store.example".to_string(),
            "cdn.store.example".to_string(),
        ]
        .into(),
        bindings: ["config".to_string(), "secrets".to_string()].into(),
        approved_at: "2026-10-01T00:00:00Z".into(),
        ..PluginGrant::empty()
    }
}

/// A daemon with operator auth on, the request recorded.
async fn daemon(
    files: &tempfile::TempDir,
) -> (Arc<crate::api::AppState>, LibraryStore, axum::Router) {
    let (state, store) = crate::node_api::tests::test_state(files).await;
    *state.auth.write().await = Arc::new(crate::auth::OperatorAuthState::new(
        TOKEN.into(),
        1,
        true,
        5,
        30,
    ));
    record_remote_plugin_request(
        &store,
        CLUSTER,
        PLUGIN,
        "1.2.0",
        &serde_json::to_string(&requested()).unwrap(),
    )
    .await
    .unwrap();
    let app = crate::api::router(Arc::clone(&state), None);
    (state, store, app)
}

/// Sends a request with `auth` (an `Authorization` value) and answers the
/// status and JSON body.
async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        builder = builder.header("authorization", auth);
    }
    if method != "GET" {
        builder = builder.header("content-type", "application/json");
    }
    let mut request = builder
        .body(if method == "GET" {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            40000,
        ))));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Only the operator reaches the routes: no credential, a wrong token,
/// and a signed-in member are all refused, and nothing is approved.
#[tokio::test]
async fn consent_routes_are_operator_only() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, app) = daemon(&files).await;
    store
        .upsert_portal_identity("test", "member-ext", Some("Member"))
        .await
        .unwrap();
    let cookie = crate::auth::tests::portal_cookie_for(&store, "test", "member-ext").await;
    for auth in [None, Some("Bearer wrong")] {
        let (status, _) = call(
            &app,
            "GET",
            "/api/config/cluster-consents",
            auth,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{auth:?}");
        let (status, _) = call(
            &app,
            "POST",
            "/api/config/cluster-consents/approve",
            auth,
            json!({ "cluster_id": CLUSTER, "plugin_key": PLUGIN }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{auth:?}");
    }
    let request = Request::builder()
        .method("GET")
        .uri("/api/config/cluster-consents")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let status = app.clone().oneshot(request).await.unwrap().status();
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "a member is refused: {status}"
    );
    let row = load_remote_plugin_consent(&store, CLUSTER, PLUGIN)
        .await
        .unwrap()
        .unwrap();
    assert!(row.approved_at.is_none());
}

/// The operator lists the request, approves it narrowed (one domain and
/// the secrets binding denied) for the version reviewed, is refused for
/// another version or a domain the request does not ask for, and
/// revokes it again.
#[tokio::test]
async fn the_operator_approves_narrowed_and_revokes() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, app) = daemon(&files).await;
    let bearer = format!("Bearer {TOKEN}");
    let auth = Some(bearer.as_str());

    let (status, listed) = call(
        &app,
        "GET",
        "/api/config/cluster-consents",
        auth,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed["clusters"].as_array().unwrap().is_empty());
    let consents = listed["consents"].as_array().unwrap();
    assert_eq!(consents.len(), 1);
    assert_eq!(consents[0]["state"], "awaiting_host_consent");
    assert_eq!(consents[0]["requested_version"], "1.2.0");
    assert_eq!(
        consents[0]["requested_grant"]["domains"],
        json!(["api.store.example", "cdn.store.example"])
    );

    let approve = |body: Value| {
        let app = app.clone();
        let bearer = bearer.clone();
        async move {
            call(
                &app,
                "POST",
                "/api/config/cluster-consents/approve",
                Some(&bearer),
                body,
            )
            .await
        }
    };
    let (status, refused) = approve(json!({
        "cluster_id": CLUSTER,
        "plugin_key": PLUGIN,
        "version": "1.1.0",
    }))
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "approval_refused");
    let (status, refused) = approve(json!({
        "cluster_id": CLUSTER,
        "plugin_key": PLUGIN,
        "version": "1.2.0",
        "deny_domains": ["unasked.example"],
    }))
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    let (status, refused) = approve(json!({
        "cluster_id": CLUSTER,
        "plugin_key": PLUGIN,
        "surprise": true,
    }))
    .await;
    assert!(
        status.is_client_error(),
        "unknown fields are refused: {status} {refused}"
    );
    assert!(
        load_remote_plugin_consent(&store, CLUSTER, PLUGIN)
            .await
            .unwrap()
            .unwrap()
            .approved_at
            .is_none(),
        "no refused approval was stored"
    );

    let (status, approved) = approve(json!({
        "cluster_id": CLUSTER,
        "plugin_key": PLUGIN,
        "version": "1.2.0",
        "deny_domains": ["cdn.store.example"],
        "deny_bindings": ["secrets"],
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    let consent = &approved["consent"];
    assert_eq!(consent["state"], "consented");
    assert_eq!(consent["approved_by"], "operator");
    assert_eq!(consent["grant"]["domains"], json!(["api.store.example"]));
    assert_eq!(consent["grant"]["bindings"], json!(["config"]));

    let (status, revoked) = call(
        &app,
        "POST",
        "/api/config/cluster-consents/revoke",
        auth,
        json!({ "cluster_id": CLUSTER, "plugin_key": PLUGIN }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["consent"]["state"], "awaiting_host_consent");
    assert!(revoked["consent"]["grant"].is_null());
    let (status, _) = call(
        &app,
        "POST",
        "/api/config/cluster-consents/revoke",
        auth,
        json!({ "cluster_id": CLUSTER, "plugin_key": "other" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Denying all network approves the request with network off.
#[tokio::test]
async fn deny_network_turns_network_off() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, _store, app) = daemon(&files).await;
    let bearer = format!("Bearer {TOKEN}");
    let (status, approved) = call(
        &app,
        "POST",
        "/api/config/cluster-consents/approve",
        Some(&bearer),
        json!({ "cluster_id": CLUSTER, "plugin_key": PLUGIN, "deny_network": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_ne!(approved["consent"]["grant"]["networkMode"], "outbound");
    assert!(approved["consent"]["grant"]["domains"]
        .as_array()
        .is_none_or(|domains| domains.is_empty()));
}
