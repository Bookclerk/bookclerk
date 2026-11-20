//! Tests of the Administrator switch routes and the checks they drive,
//! through the router.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::auth::tests::{phase2_harness, portal_cookie_for};

/// Sends `method uri` with `cookie` and an optional JSON body; returns the
/// status and decoded body.
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    cookie: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.unwrap_or("{}").to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// The user id behind a test portal identity.
async fn user_of(library: &bookclerk_library::LibraryStore, external: &str) -> i64 {
    library
        .get_portal_identity("test", external)
        .await
        .expect("read")
        .and_then(|identity| identity.user_id)
        .expect("user")
}

#[tokio::test]
async fn only_an_owner_sets_switches_and_an_administrator_reads_their_own() {
    let (_state, app, library) = phase2_harness("op-token-admin-switches").await;
    let owner = portal_cookie_for(&library, "test", "admin-ext").await;
    let admin = portal_cookie_for(&library, "test", "administrator-ext").await;
    let administrator = user_of(&library, "administrator-ext").await;
    let member = user_of(&library, "member-ext").await;

    // Out of the box every switch is on.
    let (status, body) = send(
        &app,
        "GET",
        &format!("/api/users/{administrator}/admin-controls"),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["effective"]["manage_members"], true);
    assert_eq!(body["applies"], true);

    // An Administrator reads only their own, and sets none.
    let (status, _) = send(
        &app,
        "GET",
        &format!("/api/users/{member}/admin-controls"),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let off = r#"{"overrides":{"manage_members":false}}"#;
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/api/users/{administrator}/admin-controls"),
        &admin,
        Some(off),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        &app,
        "PUT",
        "/api/users/default-admin-controls",
        &admin,
        Some(r#"{"run_jobs":false}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The Owner turns member management off for this Administrator.
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/users/{administrator}/admin-controls"),
        &owner,
        Some(off),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["overrides"],
        serde_json::json!({"manage_members": false})
    );
    assert_eq!(body["effective"]["manage_members"], false);
    let new_member = r#"{"role":"member","display_name":"Kit"}"#;
    let (status, _) = send(&app, "POST", "/api/users", &admin, Some(new_member)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/users/{member}"),
        &admin,
        Some(r#"{"display_name":"Sam"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // The user list stays readable, and the Owner is never limited.
    let (status, _) = send(&app, "GET", "/api/users", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(&app, "POST", "/api/users", &owner, Some(new_member)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A unknown switch is refused.
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/api/users/{administrator}/admin-controls"),
        &owner,
        Some(r#"{"overrides":{"fly":false}}"#),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
}

#[tokio::test]
async fn the_default_limits_every_administrator_route_by_route() {
    let (_state, app, library) = phase2_harness("op-token-admin-default").await;
    let owner = portal_cookie_for(&library, "test", "admin-ext").await;
    let admin = portal_cookie_for(&library, "test", "administrator-ext").await;

    let all_off = r#"{"manage_members":false,"set_patron_limits":false,"library_scan":false,"run_jobs":false,"fix_catalog":false,"invite_branches":false,"approve_requests":false}"#;
    let (status, body) = send(
        &app,
        "PUT",
        "/api/users/default-admin-controls",
        &owner,
        Some(all_off),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["controls"]["run_jobs"], false);
    let (status, body) = send(
        &app,
        "GET",
        "/api/users/default-admin-controls",
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["controls"]["fix_catalog"], false);

    // Set patron limits: the default scope is read-only.
    let (status, _) = send(&app, "GET", "/api/users/default-scope", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(
        &app,
        "PUT",
        "/api/users/default-scope",
        &admin,
        Some(r#"{"rules":[]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Run the job queue: watching stays, acting is refused.
    let (status, _) = send(&app, "GET", "/api/jobs", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, "POST", "/api/jobs/no-such-job/cancel", &admin, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Fix the catalog: conflicts are listed, pins refused.
    let (status, _) = send(&app, "GET", "/api/library/conflicts", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        "PUT",
        "/api/library/listings/no-such-listing/pin",
        &admin,
        Some(r#"{"edition_id":"x"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Invite branches: no new enrollment link.
    let (status, _) = send(
        &app,
        "POST",
        "/api/config/node-invites",
        &admin,
        Some(r#"{"label":"Study"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The Owner keeps every power: their cancel reaches the job lookup.
    let (status, _) = send(&app, "POST", "/api/jobs/no-such-job/cancel", &owner, None).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn queue_and_integration_scan_actions_follow_their_switches() {
    let (_state, app, library) = phase2_harness("op-token-admin-queue").await;
    let owner = portal_cookie_for(&library, "test", "admin-ext").await;
    let admin = portal_cookie_for(&library, "test", "administrator-ext").await;

    let off = r#"{"manage_members":true,"set_patron_limits":true,"library_scan":false,"run_jobs":false,"fix_catalog":true,"invite_branches":true,"approve_requests":true}"#;
    let (status, body) = send(
        &app,
        "PUT",
        "/api/users/default-admin-controls",
        &owner,
        Some(off),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Run the job queue: every delivery action, the legacy cancel and the
    // listening sync are refused before they look anything up.
    for (method, uri) in [
        ("POST", "/api/events/deliveries/no-such/retry"),
        ("POST", "/api/events/deliveries/no-such/acknowledge"),
        ("POST", "/api/events/deliveries/no-such/cancel"),
        ("POST", "/api/events/deliveries/no-such/resume"),
        ("POST", "/jobs/no-such-job/cancel"),
        ("POST", "/api/discover/sync-listening"),
        // Library-wide scan: an integration scan reaches every account.
        ("POST", "/integrations/no-such/scan"),
    ] {
        let (status, body) = send(&app, method, uri, &admin, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
        // The Owner is never limited and gets past the switch.
        let (status, body) = send(&app, method, uri, &owner, None).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
    }
}

#[tokio::test]
async fn switch_routes_refuse_strangers_and_unknown_users() {
    let (_state, app, library) = phase2_harness("op-token-admin-errors").await;
    let owner = portal_cookie_for(&library, "test", "admin-ext").await;

    // No session at all.
    let (status, body) = send(&app, "GET", "/api/users/1/admin-controls", "", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // A user who does not exist.
    let missing = "/api/users/987654/admin-controls";
    let (status, body) = send(&app, "GET", missing, &owner, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = send(
        &app,
        "PUT",
        missing,
        &owner,
        Some(r#"{"overrides":{"run_jobs":false}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // The operator token writes the default without a user behind it.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/default-admin-controls")
                .header(header::AUTHORIZATION, "Bearer op-token-admin-errors")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"run_jobs":false}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}
