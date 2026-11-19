//! Tests of the patron scope routes, driven through the router.

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
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
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

#[tokio::test]
async fn an_administrator_sets_the_default_and_a_patrons_own_scope() {
    let (_state, app, library) = phase2_harness("op-token-scopes").await;
    let admin = portal_cookie_for(&library, "test", "administrator-ext").await;
    let member_cookie = portal_cookie_for(&library, "test", "member-ext").await;
    let member = library
        .get_portal_identity("test", "member-ext")
        .await
        .expect("read")
        .and_then(|identity| identity.user_id)
        .expect("member user");
    let administrator = library
        .get_portal_identity("test", "administrator-ext")
        .await
        .expect("read")
        .and_then(|identity| identity.user_id)
        .expect("administrator user");

    // Out of the box: the default has no rules and the member follows it.
    let (status, body) = send(
        &app,
        "GET",
        &format!("/api/users/{member}/scope"),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scope"]["rules"], serde_json::json!([]));
    assert_eq!(body["own"], false);
    assert_eq!(body["applies"], true);

    let kids = r#"{"rules":[{"field":"genre","value":" Kids ","effect":"include"}]}"#;
    let (status, body) = send(&app, "PUT", "/api/users/default-scope", &admin, Some(kids)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scope"]["rules"][0]["value"], "Kids", "trimmed");
    let (_, body) = send(&app, "GET", "/api/users/default-scope", &admin, None).await;
    assert_eq!(body["scope"]["rules"][0]["field"], "genre");
    let (_, body) = send(
        &app,
        "GET",
        &format!("/api/users/{member}/scope"),
        &admin,
        None,
    )
    .await;
    assert_eq!(
        body["scope"]["rules"][0]["value"], "Kids",
        "follows default"
    );
    assert_eq!(body["own"], false);

    // The member's own scope, then back to the default.
    let own = r#"{"scope":{"rules":[{"field":"series","value":"Discworld","effect":"exclude"}]}}"#;
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/users/{member}/scope"),
        &admin,
        Some(own),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["own"], true);
    assert_eq!(body["scope"]["rules"][0]["effect"], "exclude");
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/users/{member}/scope"),
        &admin,
        Some(r#"{"scope":null}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["own"], false);

    // Bad scopes, unknown users and self-service are refused.
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/users/{member}/scope"),
        &admin,
        Some(r#"{"scope":{"rules":[{"field":"genre","value":"  ","effect":"include"}]}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"],
        "invalid patron scope: a scope rule needs a value"
    );
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/api/users/{member}/scope"),
        &admin,
        Some(r#"{"scope":{"rules":[{"field":"shelf","value":"x","effect":"include"}]}}"#),
    )
    .await;
    assert!(status.is_client_error(), "unknown field: {status}");
    let (status, _) = send(&app, "GET", "/api/users/99999/scope", &admin, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/users/{administrator}/scope"),
        &admin,
        Some(r#"{"scope":null}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "own scope: {body}");

    // A member may neither read nor change scopes.
    let (status, _) = send(
        &app,
        "GET",
        &format!("/api/users/{member}/scope"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        &app,
        "PUT",
        "/api/users/default-scope",
        &member_cookie,
        Some("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An Owner or Administrator is never scoped.
    let owner = library
        .get_portal_identity("test", "admin-ext")
        .await
        .expect("read")
        .and_then(|identity| identity.user_id)
        .expect("owner user");
    let owner_cookie = portal_cookie_for(&library, "test", "admin-ext").await;
    let (_, body) = send(
        &app,
        "GET",
        &format!("/api/users/{owner}/scope"),
        &owner_cookie,
        None,
    )
    .await;
    assert_eq!(body["applies"], false, "{body}");

    let audited = library
        .list_security_audit_events(50)
        .await
        .expect("audit")
        .into_iter()
        .map(|event| event.action)
        .collect::<Vec<_>>();
    assert!(audited.iter().any(|e| e == "patron_scope_default"));
    assert!(audited.iter().any(|e| e == "user_patron_scope"));
}

#[tokio::test]
async fn an_administrator_sets_the_default_and_a_patrons_own_controls() {
    let (_state, app, library) = phase2_harness("op-token-controls").await;
    let admin = portal_cookie_for(&library, "test", "administrator-ext").await;
    let member_cookie = portal_cookie_for(&library, "test", "member-ext").await;
    let user_of = |external: &'static str| {
        let library = library.clone();
        async move {
            library
                .get_portal_identity("test", external)
                .await
                .expect("read")
                .and_then(|identity| identity.user_id)
                .expect("user")
        }
    };
    let member = user_of("member-ext").await;
    let administrator = user_of("administrator-ext").await;

    // Out of the box every control is on and nothing is overridden.
    let (status, body) = send(
        &app,
        "GET",
        &format!("/api/users/{member}/controls"),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["overrides"], serde_json::json!({}));
    assert_eq!(body["effective"]["download"], true);
    assert_eq!(body["applies"], true);
    assert_eq!(body["follows_default"], true);

    // The default turns prices off; the member follows it.
    let (status, body) = send(
        &app,
        "PUT",
        "/api/users/default-controls",
        &admin,
        Some(r#"{"see_prices":false}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["controls"]["see_prices"], false);
    assert_eq!(body["controls"]["download"], true);
    let (_, body) = send(&app, "GET", "/api/users/default-controls", &admin, None).await;
    assert_eq!(body["controls"]["see_prices"], false);

    // The member alone loses account linking and keeps prices.
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/users/{member}/controls"),
        &admin,
        Some(r#"{"overrides":{"link_accounts":false,"see_prices":true}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["overrides"]["link_accounts"], false);
    assert_eq!(body["effective"]["link_accounts"], false);
    assert_eq!(body["effective"]["see_prices"], true);
    assert_eq!(body["defaults"]["see_prices"], false);
    assert!(
        !bookclerk_library::control_plane::member_may_connect_accounts(&library, member)
            .await
            .unwrap()
    );
    let (_, users) = send(&app, "GET", "/api/users", &admin, None).await;
    let row = users["users"]
        .as_array()
        .expect("users")
        .iter()
        .find(|row| row["id"] == member)
        .expect("member row")
        .clone();
    assert_eq!(row["controls"]["link_accounts"], false, "{row}");
    assert!(library
        .list_security_audit_events(20)
        .await
        .unwrap()
        .iter()
        .any(|event| event.action == "user_member_controls"));

    // An unknown control is refused, and so is setting one's own.
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/api/users/{member}/controls"),
        &admin,
        Some(r#"{"overrides":{"fly":false}}"#),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/api/users/{administrator}/controls"),
        &admin,
        Some(r#"{"overrides":{}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An Owner limits an Administrator; the patrons' default does not.
    let owner = portal_cookie_for(&library, "test", "admin-ext").await;
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/users/{administrator}/controls"),
        &owner,
        Some(r#"{"overrides":{"link_accounts":false}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applies"], true);
    assert_eq!(body["follows_default"], false);
    assert_eq!(body["effective"]["link_accounts"], false);
    assert_eq!(body["effective"]["see_prices"], true, "not the default");
    assert!(
        !bookclerk_library::control_plane::member_may_connect_accounts(&library, administrator)
            .await
            .unwrap()
    );

    // A member may neither read nor write controls.
    let (status, _) = send(
        &app,
        "GET",
        "/api/users/default-controls",
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        &app,
        "PUT",
        "/api/users/default-controls",
        &member_cookie,
        Some("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Back to the default for every control.
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/users/{member}/controls"),
        &admin,
        Some(r#"{"overrides":{}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["overrides"], serde_json::json!({}));
    assert_eq!(body["effective"]["see_prices"], false);
}

#[tokio::test]
async fn a_patron_without_requests_is_refused_before_the_ask_is_read() {
    let (_state, app, library) = phase2_harness("op-token-requests").await;
    let member_cookie = portal_cookie_for(&library, "test", "member-ext").await;
    let member = library
        .get_portal_identity("test", "member-ext")
        .await
        .expect("read")
        .and_then(|identity| identity.user_id)
        .expect("member user");
    let ask = r#"{"work_id":"0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b"}"#;
    // Allowed: the ask reaches the library, which has no branch listing it.
    let (status, _) = send(
        &app,
        "POST",
        "/api/branch-requests",
        &member_cookie,
        Some(ask),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    bookclerk_library::control_plane::set_member_control_overrides(
        &library,
        member,
        bookclerk_library::control_plane::ControlOverrides {
            request_books: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (status, _) = send(
        &app,
        "POST",
        "/api/branch-requests",
        &member_cookie,
        Some(ask),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_caller_check_follows_the_callers_controls() {
    use bookclerk_library::control_plane::{
        set_member_control_overrides, ControlOverrides, MemberControl,
    };
    let (state, _app, library) = phase2_harness("op-token-caller-may").await;
    let member_cookie = portal_cookie_for(&library, "test", "member-ext").await;
    let member = library
        .get_portal_identity("test", "member-ext")
        .await
        .expect("read")
        .and_then(|identity| identity.user_id)
        .expect("member user");
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::COOKIE,
        member_cookie.parse().expect("cookie header"),
    );
    assert!(super::caller_may(&state, &headers, MemberControl::SeePrices).await);
    set_member_control_overrides(
        &library,
        member,
        ControlOverrides {
            see_prices: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!super::caller_may(&state, &headers, MemberControl::SeePrices).await);
    assert!(super::caller_may(&state, &headers, MemberControl::Download).await);
    // The operator token is never limited.
    let mut operator = axum::http::HeaderMap::new();
    operator.insert(
        header::AUTHORIZATION,
        "Bearer op-token-caller-may".parse().expect("bearer header"),
    );
    assert!(super::caller_may(&state, &operator, MemberControl::SeePrices).await);
}

#[tokio::test]
async fn discover_answers_drop_prices_for_a_patron_who_may_not_see_them() {
    let (_state, app, library) = phase2_harness("op-token-hide-prices").await;
    let member_cookie = portal_cookie_for(&library, "test", "member-ext").await;
    let member = library
        .get_portal_identity("test", "member-ext")
        .await
        .expect("read")
        .and_then(|identity| identity.user_id)
        .expect("member user");
    bookclerk_library::control_plane::set_member_control_overrides(
        &library,
        member,
        bookclerk_library::control_plane::ControlOverrides {
            see_prices: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let query = r#"{"title":"The Left Hand of Darkness","isbn":"9780441478125"}"#;
    let (status, body) = send(
        &app,
        "POST",
        "/api/discover/purchase-hints",
        &member_cookie,
        Some(query),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["best"].is_null() || body["best"]["url"].is_null(),
        "{body}"
    );
    let (status, body) = send(
        &app,
        "POST",
        "/api/discover/purchase-hints/batch",
        &member_cookie,
        Some(&format!(r#"{{"queries":[{query}]}}"#)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        &app,
        "GET",
        "/api/discover/search?q=darkness",
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        &app,
        "GET",
        "/api/discover/recommendations",
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The wishlist routes drop them too: create, list and remove a wish.
    let (status, wish) = send(&app, "POST", "/api/wishlist", &member_cookie, Some(query)).await;
    assert_eq!(status, StatusCode::OK, "{wish}");
    let (status, body) = send(&app, "GET", "/api/wishlist", &member_cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(1), "{body}");
    let uuid = wish["uuid"].as_str().expect("wish uuid");
    let (status, body) = send(
        &app,
        "DELETE",
        &format!("/api/wishlist/{uuid}"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn a_wishlist_row_keeps_its_stores_and_loses_prices_and_links() {
    let mut row: bookclerk_library::TitleRequestRecord =
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "uuid": "0b7e4c1d-2a3f-4e5d-9c8b-7a6f5e4d3c2b",
            "title": "The Left Hand of Darkness",
            "status": "open",
            "work_key": "isbn:9780441478125",
            "work_id": "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b",
            "sources": [],
            "purchase_hints": [{
                "source": "libro",
                "product_id": "9780441478125",
                "url": "https://libro.fm/audiobooks/9780441478125",
                "price_cents": 1499,
                "currency": "USD",
                "price_label": "$14.99",
                "list_price_cents": 2999,
                "list_price_label": "$29.99",
                "member_price_cents": 1499,
                "member_price_label": "$14.99"
            }],
            "created_at": "2026-10-29T10:12:00Z",
            "updated_at": "2026-10-29T10:12:00Z"
        }))
        .expect("wishlist row");
    row.hide_prices_and_links();
    let hint = serde_json::to_value(&row.purchase_hints[0]).expect("hint");
    assert_eq!(
        hint,
        serde_json::json!({"source": "libro", "product_id": "9780441478125"})
    );
}

#[tokio::test]
async fn the_operator_sets_the_default_controls_and_a_stale_session_may_not() {
    use bookclerk_library::control_plane::MemberControl;
    let (state, app, _library) = phase2_harness("op-token-default-controls").await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/default-controls")
                .header(header::AUTHORIZATION, "Bearer op-token-default-controls")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"upload":false}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    // A cookie for no session is refused rather than given the default.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::COOKIE,
        "bookclerk_portal_session=not-a-session"
            .parse()
            .expect("cookie header"),
    );
    assert!(!super::caller_may(&state, &headers, MemberControl::Download).await);
}
