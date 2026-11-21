//! Tests of the library tag routes, driven through the router.

use axum::http::{header, StatusCode};
use bookclerk_library::{LibraryStore, NewBook};

/// The operator token of these tests.
const TOKEN: &str = "op-token-work-tags";

/// Scans `title` onto account `acct` and indexes it; returns its work id.
async fn scan(library: &LibraryStore, asin: &str, title: &str) -> String {
    let saved = library
        .upsert_book(&NewBook::minimal(asin, "acct", "us", title))
        .await
        .expect("book");
    let work = library
        .work_id_for_book(&saved.uuid)
        .await
        .expect("work read")
        .expect("a scanned title has a work");
    library
        .refresh_library_works(std::slice::from_ref(&work))
        .await
        .expect("index");
    work
}

/// Sends `method uri` with `body`, as the operator or with `cookie`.
async fn call(
    app: &axum::Router,
    cookie: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder().method(method).uri(uri);
    request = match cookie {
        Some(cookie) => request.header(header::COOKIE, cookie),
        None => request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
    };
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(body.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).expect("request"))
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
async fn an_administrator_tags_a_work_and_the_library_filters_by_it() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let gruffalo = scan(&library, "B001", "The Gruffalo").await;
    scan(&library, "B002", "Dune").await;
    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let uri = format!("/api/library/works/{gruffalo}/tags");

    let (status, json) = call(
        &app,
        Some(&admin),
        "PUT",
        &uri,
        Some(r#"{"tags":["Kids"," kids","Bedtime"]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["tags"],
        serde_json::json!([
            {"value": "Bedtime", "source": "admin"},
            {"value": "Kids", "source": "admin"}
        ])
    );
    let (status, json) = call(&app, None, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "the operator reads them: {json}");
    assert_eq!(json["tags"].as_array().map(Vec::len), Some(2));

    // Everyone filters the Library by a library tag.
    let (status, json) = call(
        &app,
        Some(&member),
        "GET",
        "/api/library/works?facet=library_tag&value=kids",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let titles: Vec<&str> = json["works"]
        .as_array()
        .expect("works")
        .iter()
        .filter_map(|w| w["title"].as_str())
        .collect();
    assert_eq!(titles, ["The Gruffalo"]);
    let (status, json) = call(
        &app,
        Some(&member),
        "GET",
        "/api/library/works/facets/library_tag",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["values"].as_array().map(Vec::len), Some(2), "{json}");

    // A Member neither reads nor sets them here.
    let (status, _) = call(&app, Some(&member), "GET", &uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, Some(&member), "PUT", &uri, Some(r#"{"tags":[]}"#)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Bounds and unknown works.
    let many: Vec<String> = (0..=32).map(|n| format!("t{n}")).collect();
    let body = serde_json::json!({ "tags": many }).to_string();
    let (status, json) = call(&app, Some(&admin), "PUT", &uri, Some(&body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    let (status, _) = call(
        &app,
        Some(&admin),
        "PUT",
        "/api/library/works/no-such-work/tags",
        Some(r#"{"tags":["x"]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_patron_limits_switch_stops_an_administrator_tagging() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let work = scan(&library, "B001", "The Gruffalo").await;
    let owner = crate::auth::tests::portal_cookie_for(&library, "test", "admin-ext").await;
    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    let (status, json) = call(
        &app,
        Some(&owner),
        "PATCH",
        "/api/users/default-admin-controls",
        Some(r#"{"changes":{"set_patron_limits":false}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let uri = format!("/api/library/works/{work}/tags");
    let (status, _) = call(
        &app,
        Some(&admin),
        "PUT",
        &uri,
        Some(r#"{"tags":["Kids"]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, Some(&admin), "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "reading stays open");
    let (status, json) = call(
        &app,
        Some(&owner),
        "PUT",
        &uri,
        Some(r#"{"tags":["Kids"]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "an Owner is never limited: {json}");
}
