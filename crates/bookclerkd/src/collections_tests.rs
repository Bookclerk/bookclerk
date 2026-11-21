//! Tests of the collections routes, driven through the router.

use axum::http::{header, StatusCode};
use bookclerk_library::{FileRole, LibraryStore, NewBook};

/// The operator token of these tests.
const TOKEN: &str = "op-token-collections";

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

/// Scans `title` with a file at `key`, indexed; returns its work id.
async fn scan(library: &LibraryStore, asin: &str, title: &str, key: &str) -> String {
    let saved = library
        .upsert_book(&NewBook::minimal(asin, "acct", "us", title))
        .await
        .expect("book");
    let owner = library
        .file_owner_of_book(&saved.uuid)
        .await
        .expect("owner read")
        .expect("owner");
    library
        .record_edition_file("local", key, &owner, FileRole::Audio)
        .await
        .expect("file");
    let work = library
        .work_id_for_book(&saved.uuid)
        .await
        .expect("work read")
        .expect("work");
    library
        .refresh_library_works(std::slice::from_ref(&work))
        .await
        .expect("index");
    work
}

#[tokio::test]
async fn an_administrator_defines_collections_and_the_library_filters_by_them() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let gruffalo = scan(&library, "B001", "The Gruffalo", "kids/Gruffalo.m4b").await;
    let dune = scan(&library, "B002", "Dune", "scifi/Dune.m4b").await;
    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;

    let (status, json) = call(
        &app,
        Some(&admin),
        "PUT",
        "/api/library/collections",
        Some(r#"{"collections":[{"name":"Kids","prefixes":["kids"]}]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["collections"][0]["prefixes"][0], "kids/");
    assert!(
        !library.library_index_built().await.expect("read"),
        "a change drops the build"
    );
    // What the queued rebuild does for these two works.
    library
        .refresh_library_works(&[gruffalo, dune])
        .await
        .expect("index");

    let (status, json) = call(
        &app,
        Some(&member),
        "GET",
        "/api/library/works?facet=collection&value=Kids",
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

    let (status, json) = call(&app, None, "GET", "/api/library/collections", None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["collections"][0]["name"], "Kids");
    let (status, _) = call(&app, Some(&member), "GET", "/api/library/collections", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        Some(&member),
        "PUT",
        "/api/library/collections",
        Some(r#"{"collections":[]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, json) = call(
        &app,
        Some(&admin),
        "PUT",
        "/api/library/collections",
        Some(r#"{"collections":[{"name":"A","prefixes":["a"]},{"name":"a","prefixes":["b"]}]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
}

#[tokio::test]
async fn the_patron_limits_switch_stops_an_administrator_changing_collections() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
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
    let body = r#"{"collections":[{"name":"Kids","prefixes":["kids/"]}]}"#;
    let (status, _) = call(
        &app,
        Some(&admin),
        "PUT",
        "/api/library/collections",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, json) = call(
        &app,
        Some(&owner),
        "PUT",
        "/api/library/collections",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "an Owner is never limited: {json}");
}
