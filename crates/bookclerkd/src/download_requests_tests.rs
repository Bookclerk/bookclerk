//! `POST /api/download-requests`: malformed bodies are refused, a work no
//! account holds is not found, and a held work with no copy queues one
//! acquire of its edition (asking again returns the same job).

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::routing::post;
use bookclerk_library::{JobKind, NewBook, UserRole};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;

use super::*;

const PLUGIN: &str = "path:file:///tmp/audible";
const ASIN: &str = "B0DOWNLD01";

/// Sends `body` to the route.
async fn send(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/download-requests")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}

/// A work a store account of the library holds, with no copy downloaded
/// yet. Answers the work id.
pub(crate) async fn work_held_without_a_copy(store: &bookclerk_library::LibraryStore) -> String {
    store
        .upsert_account(PLUGIN, "download-acct", "us", None, true, "test")
        .await
        .unwrap();
    let book = store
        .scope(PLUGIN, "audible")
        .upsert_book(&NewBook {
            uuid: None,
            product_id: ASIN.into(),
            plugin_key: PLUGIN.into(),
            source: "audible".into(),
            account_id: "download-acct".into(),
            asin: Some(ASIN.into()),
            isbn: None,
            marketplace: "us".into(),
            title: "Download Title".into(),
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: "book".into(),
            categories: None,
            subtitle: None,
            published_at: None,
        })
        .await
        .unwrap();
    store.work_id_for_book(&book.uuid).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_held_work_without_a_copy_queues_one_download() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = crate::node_api::tests::test_state(&files).await;
    let app = axum::Router::new()
        .route("/api/download-requests", post(create_download_request))
        .with_state(Arc::clone(&state));
    store
        .create_user(UserRole::Owner, Some("Owner"), None)
        .await
        .unwrap();
    let work = work_held_without_a_copy(&store).await;

    let (status, _) = send(&app, json!({"work_id": "not a work"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(&app, json!({"work_id": work, "node_id": "n"})).await;
    assert!(status.is_client_error(), "unknown fields are refused");
    let (status, body) = send(&app, json!({"work_id": "no-such-work"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = send(&app, json!({"work_id": work})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["already_queued"], false);
    let job_id = body["job_id"].as_str().unwrap().to_string();
    let job = store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(job.kind, JobKind::Acquire);
    assert!(
        job.payload.paced,
        "the library's download waits for its pace"
    );

    let (status, again) = send(&app, json!({"work_id": work})).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["job_id"], job_id.as_str());
    assert_eq!(again["already_queued"], true);
    assert_eq!(again["edition_id"], body["edition_id"]);
}
