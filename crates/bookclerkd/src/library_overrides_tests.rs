//! Tests of the library override routes, driven through the router.

use axum::http::{header, StatusCode};
use bookclerk_library::{LibraryStore, NewBook};

/// The operator token of these tests.
const TOKEN: &str = "op-token-pins";

/// Scans `title` (product `asin`) onto account `acct` and returns its
/// listing and edition.
async fn scan(library: &LibraryStore, asin: &str, title: &str) -> (String, String) {
    let mut book = NewBook::minimal(asin, "acct", "us", title);
    book.authors = Some("Frank Herbert".into());
    let saved = library.upsert_book(&book).await.expect("book");
    let work = library
        .work_id_for_book(&saved.uuid)
        .await
        .expect("work read")
        .expect("a scanned title has a work");
    let detail = library
        .library_work_detail(&work)
        .await
        .expect("detail")
        .expect("the work exists");
    let edition = detail.editions.first().expect("an edition");
    let listing = edition.listings.first().expect("a listing");
    (listing.id.clone(), edition.id.clone())
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
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// A pin request body naming `edition`.
fn pin(edition: &str) -> String {
    serde_json::json!({ "edition_id": edition }).to_string()
}

#[tokio::test]
async fn an_administrator_pins_and_unpins_a_listing() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let (dune, dune_edition) = scan(&library, "B001", "Dune").await;
    let (_, messiah_edition) = scan(&library, "B002", "Dune Messiah").await;
    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    let uri = format!("/api/library/listings/{dune}/pin");

    let (status, json) = call(
        &app,
        Some(&admin),
        "PUT",
        &uri,
        Some(&pin(&messiah_edition)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["listing_id"], dune.as_str());
    assert_eq!(json["edition_id"], messiah_edition.as_str());
    assert_eq!(json["moved_from"], dune_edition.as_str());
    assert_eq!(
        library.listing_pin(&dune).await.expect("pin").as_deref(),
        Some(messiah_edition.as_str())
    );

    let (status, json) = call(&app, None, "PUT", &uri, Some(&pin(&messiah_edition))).await;
    assert_eq!(status, StatusCode::OK, "the operator pins too: {json}");
    assert!(json["moved_from"].is_null(), "already there: {json}");

    let (status, _) = call(&app, Some(&admin), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(library.listing_pin(&dune).await.expect("pin"), None);
    let (status, json) = call(&app, Some(&admin), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], format!("listing {dune} has no pin"));
}

#[tokio::test]
async fn pins_name_what_they_cannot_find_and_refuse_members() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let (dune, dune_edition) = scan(&library, "B001", "Dune").await;

    let (status, json) = call(
        &app,
        None,
        "PUT",
        "/api/library/listings/nope/pin",
        Some(&pin(&dune_edition)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "listing nope not found");
    let uri = format!("/api/library/listings/{dune}/pin");
    let (status, json) = call(&app, None, "PUT", &uri, Some(&pin("gone"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "edition gone not found");
    let (status, _) = call(&app, None, "PUT", &uri, Some(r#"{"edition": "x"}"#)).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown fields are refused"
    );

    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let (status, json) = call(&app, Some(&member), "PUT", &uri, Some(&pin(&dune_edition))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        json["error"],
        "only an Administrator can change how listings resolve"
    );
    let (status, _) = call(&app, Some(&member), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, Some(&member), "GET", "/api/library/conflicts", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(library.listing_pin(&dune).await.expect("pin"), None);
}

#[tokio::test]
async fn conflicts_page_with_a_cursor() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let (dune, dune_edition) = scan(&library, "B001", "Dune").await;
    let (messiah, messiah_edition) = scan(&library, "B002", "Dune Messiah").await;
    for (listing, kind) in [(&dune, "ambiguous_work"), (&messiah, "work_kind_mismatch")] {
        library
            .record_resolution_conflict_for_test(listing, kind, &[&dune_edition, &messiah_edition])
            .await;
    }

    let (status, first) = call(&app, None, "GET", "/api/library/conflicts?limit=1", None).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["conflicts"].as_array().map(Vec::len), Some(1));
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a full page has a cursor");
    let (status, second) = call(
        &app,
        None,
        "GET",
        &format!("/api/library/conflicts?limit=1&cursor={cursor}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let mut seen: Vec<String> = [&first, &second]
        .iter()
        .map(|page| {
            page["conflicts"][0]["listing_id"]
                .as_str()
                .expect("id")
                .to_string()
        })
        .collect();
    seen.sort();
    let mut expected = vec![dune.clone(), messiah.clone()];
    expected.sort();
    assert_eq!(seen, expected);
    assert_eq!(first["conflicts"][0]["plugin_key"], "audible");
    assert_eq!(
        first["conflicts"][0]["subject_ids"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );

    let (status, json) = call(
        &app,
        None,
        "GET",
        "/api/library/conflicts?cursor=bad!",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "invalid page cursor");
}
