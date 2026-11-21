//! Tests of the edition chapter routes, driven through the router.

use axum::http::{header, StatusCode};
use bookclerk_library::{
    ChapterCheck, ChapterClaimWrite, ChapterInput, ChapterList, ChapterSource, LibraryStore,
    MarkerKind, NewBook,
};

/// The operator token of these tests.
const TOKEN: &str = "op-token-chapters";

/// Scans `title` (product `asin`) onto account `acct` and returns its
/// edition.
async fn scan(library: &LibraryStore, asin: &str, title: &str) -> String {
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let saved = library
        .upsert_book(&NewBook::minimal(asin, "acct", "us", title))
        .await
        .expect("book");
    let work = library
        .work_of_holding(&saved.holding_id)
        .await
        .expect("work read")
        .expect("a scanned title has a work");
    let detail = library
        .library_work_detail(&work)
        .await
        .expect("detail")
        .expect("the work exists");
    detail.editions.first().expect("an edition").id.clone()
}

/// Sends `method uri` with `body` (as JSON unless `content_type` says
/// otherwise), as the operator or with `cookie`.
async fn call(
    app: &axum::Router,
    cookie: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<(&str, &str)>,
) -> (StatusCode, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let mut request = axum::http::Request::builder().method(method).uri(uri);
    request = match cookie {
        Some(cookie) => request.header(header::COOKIE, cookie),
        None => request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
    };
    let body = match body {
        Some((content_type, body)) => {
            request = request.header(header::CONTENT_TYPE, content_type);
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

/// The JSON content type.
const JSON: &str = "application/json";

/// Records a store listing claim of two tracks for `edition`.
async fn listing_claim(library: &LibraryStore, edition: &str) {
    let list = ChapterList::new(
        vec![
            ChapterInput {
                title: "Track 1".into(),
                start_ms: 0,
                depth: 0,
            },
            ChapterInput {
                title: "Track 2".into(),
                start_ms: 60_000,
                depth: 0,
            },
        ],
        ChapterCheck::Lenient,
    )
    .expect("list");
    library
        .record_chapter_claim(
            edition,
            &ChapterClaimWrite {
                source: ChapterSource::Listing {
                    plugin: "plugin:audible".into(),
                    kind: MarkerKind::Tracks,
                },
                list,
                runtime_ms: Some(120_000),
            },
        )
        .await
        .expect("claim");
}

#[tokio::test]
async fn an_administrator_edits_pins_and_restores_chapters() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let edition = scan(&library, "B001", "Dune").await;
    let uri = format!("/api/library/editions/{edition}/chapters");

    let (status, json) = call(&app, None, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["chapters"], serde_json::json!([]));
    assert!(json["source_kind"].is_null(), "no chapters yet: {json}");

    listing_claim(&library, &edition).await;
    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let (status, json) = call(&app, Some(&member), "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "a member reads chapters: {json}");
    assert_eq!(json["source_kind"], "listing");
    assert_eq!(json["source_id"], "plugin:audible");
    assert_eq!(json["chapters"][1]["start_ms"], 60_000);

    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    let edit = r#"{"chapters":[{"title":"Part One","start_ms":0},{"title":"Chapter 1","start_ms":1500,"depth":1}]}"#;
    let (status, json) = call(&app, Some(&admin), "PUT", &uri, Some((JSON, edit))).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["source_kind"], "person");
    assert_eq!(json["chapters"][1]["depth"], 1);

    let (status, json) = call(&app, Some(&admin), "GET", &format!("{uri}/claims"), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let claims = json["claims"].as_array().expect("claims");
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0]["source_kind"], "person", "newest first");
    assert_eq!(claims[0]["plays"], true);
    assert_eq!(claims[1]["quality"], "tracks");
    assert_eq!(claims[1]["plays"], false);
    assert_eq!(claims[1]["chapters"][0]["title"], "Track 1");

    let pin = r#"{"source_kind":"listing","source_id":"plugin:audible"}"#;
    let (status, json) = call(
        &app,
        Some(&admin),
        "PUT",
        &format!("{uri}/pin"),
        Some((JSON, pin)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["source_kind"], "listing", "a pin beats the edit");
    let (status, json) = call(&app, None, "DELETE", &format!("{uri}/pin"), None).await;
    assert_eq!(status, StatusCode::OK, "the operator unpins too: {json}");
    assert_eq!(json["source_kind"], "person");

    let (status, json) = call(
        &app,
        Some(&admin),
        "PUT",
        &uri,
        Some((JSON, r#"{"chapters":[]}"#)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["chapters"],
        serde_json::json!([]),
        "cleared on purpose"
    );
    assert_eq!(json["source_kind"], "person");

    let (status, json) = call(&app, Some(&admin), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["source_kind"], "listing", "the store's tracks again");
}

#[tokio::test]
async fn chapter_changes_refuse_members_and_bad_bodies() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let edition = scan(&library, "B001", "Dune").await;
    let uri = format!("/api/library/editions/{edition}/chapters");
    let edit = r#"{"chapters":[{"title":"One","start_ms":0}]}"#;

    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    for (method, path, body) in [
        ("PUT", uri.clone(), Some((JSON, edit))),
        ("DELETE", uri.clone(), None),
        ("GET", format!("{uri}/claims"), None),
        (
            "PUT",
            format!("{uri}/pin"),
            Some((JSON, r#"{"source_kind":"processed"}"#)),
        ),
        ("DELETE", format!("{uri}/pin"), None),
    ] {
        let (status, json) = call(&app, Some(&member), method, &path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert_eq!(json["error"], "only an Administrator can change chapters");
    }
    assert!(library
        .chapter_claims(&edition)
        .await
        .expect("claims")
        .is_empty());

    let (status, json) = call(&app, None, "PUT", &uri, Some(("text/plain", edit))).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{json}");
    let backwards = r#"{"chapters":[{"title":"x","start_ms":9},{"title":"y","start_ms":1}]}"#;
    let (status, json) = call(&app, None, "PUT", &uri, Some((JSON, backwards))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    let (status, json) = call(&app, None, "PUT", &uri, Some((JSON, "[]"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    let long = format!(
        r#"{{"chapters":[{{"title":"{}","start_ms":0}}]}}"#,
        "x".repeat(bookclerk_library::MAX_CHAPTER_EDIT_BYTES)
    );
    let (status, _) = call(&app, None, "PUT", &uri, Some((JSON, &long))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    let (status, json) = call(
        &app,
        None,
        "PUT",
        "/api/library/editions/gone/chapters",
        Some((JSON, edit)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "edition gone not found");
    let (status, _) = call(
        &app,
        None,
        "GET",
        "/api/library/editions/gone/chapters",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, json) = call(
        &app,
        None,
        "PUT",
        &format!("{uri}/pin"),
        Some((
            JSON,
            r#"{"source_kind":"listing","source_id":"plugin:libro"}"#,
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    let (status, _) = call(
        &app,
        None,
        "PUT",
        &format!("{uri}/pin"),
        Some((JSON, r#"{"source_kind":"wizard"}"#)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
