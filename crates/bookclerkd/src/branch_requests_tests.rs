//! `POST /api/branch-requests`: malformed ids are refused, a work no
//! branch lists is not found, and a listed work queues one request job
//! pinned to its branch (asking again returns the same job).

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::routing::post;
use bookclerk_library::control_plane::{ask_branches, pending_questions, record_answers};
use bookclerk_library::entities::{node_credentials, nodes};
use bookclerk_library::{JobKind, JobState, NewBook, UserRole};
use bookclerk_node_proto::{Availability, CatalogMetadata, LookupAnswer, LookupEdition};
use http_body_util::BodyExt as _;
use sea_orm::{ActiveModelTrait as _, ActiveValue::Set};
use serde_json::{json, Value};
use tower::ServiceExt as _;

use super::*;

const PLUGIN: &str = "path:file:///tmp/audible";
const BRANCH: &str = "branch-b";
const ASIN: &str = "B0ROUTE001";

/// Sends `body` to the route.
async fn send(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/branch-requests")
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

/// Adds branch [`BRANCH`] owned by `owner`, trusted (live credential).
async fn add_branch(store: &bookclerk_library::LibraryStore, owner: i64) {
    let now = chrono::Utc::now().to_rfc3339();
    nodes::ActiveModel {
        node_id: Set(BRANCH.into()),
        cluster_id: Set("cluster".into()),
        created_at: Set(now.clone()),
        incarnation: Set("test".into()),
        heartbeat_at: Set(now.clone()),
        software_version: Set("1.0.0".into()),
        schema_state: Set("current".into()),
        compatible: Set(1),
        target: Set(String::new()),
        labels_json: Set("[]".into()),
        kind: Set("host".into()),
        owner_user_id: Set(Some(owner)),
        ..Default::default()
    }
    .insert(store.db())
    .await
    .unwrap();
    node_credentials::ActiveModel {
        node_id: Set(BRANCH.into()),
        sign_public_key: Set(vec![1; 32]),
        box_public_key: Set(vec![2; 32]),
        link_id: Set("link-b".into()),
        enrolled_at: Set(now),
        revoked_at: Set(None),
        last_connected_at: Set(None),
    }
    .insert(store.db())
    .await
    .unwrap();
}

#[tokio::test]
async fn a_listed_work_queues_one_request_for_its_branch() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = crate::node_api::tests::test_state(&files).await;
    let app = axum::Router::new()
        .route("/api/branch-requests", post(create_branch_request))
        .with_state(Arc::clone(&state));
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    add_branch(&store, riley).await;
    store
        .upsert_account(PLUGIN, "acct", "us", None, true, "test")
        .await
        .unwrap();
    let book = store
        .scope(PLUGIN, "audible")
        .upsert_book(&NewBook {
            uuid: None,
            product_id: ASIN.into(),
            plugin_key: PLUGIN.into(),
            source: "audible".into(),
            account_id: "acct".into(),
            asin: Some(ASIN.into()),
            isbn: None,
            marketplace: "us".into(),
            title: "Route Title".into(),
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
    let work = store.work_id_for_book(&book.uuid).await.unwrap().unwrap();

    let (status, _) = send(&app, json!({"work_id": "not a work"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(&app, json!({"work_id": work, "node_id": "bad node!"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(&app, json!({"work_id": work, "extra": 1})).await;
    assert!(status.is_client_error(), "unknown fields are refused");
    let (status, body) = send(&app, json!({"work_id": work})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not listed yet: {body}");

    ask_branches(&store, std::slice::from_ref(&work))
        .await
        .unwrap();
    pending_questions(&store, BRANCH).await.unwrap().unwrap();
    record_answers(
        &store,
        BRANCH,
        &[LookupAnswer {
            work: work.clone(),
            editions: vec![LookupEdition {
                store: "audible".into(),
                marketplace: "us".into(),
                product_id: ASIN.into(),
                availability: Availability::Owned,
                formats: Vec::new(),
                metadata: CatalogMetadata {
                    title: "Route Title".into(),
                    asin: Some(ASIN.into()),
                    ..CatalogMetadata::default()
                },
            }],
        }],
        &[],
    )
    .await
    .unwrap();

    let (status, body) = send(&app, json!({"work_id": work, "node_id": BRANCH})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["node_id"], BRANCH);
    assert_eq!(body["already_queued"], false);
    let job_id = body["job_id"].as_str().unwrap().to_string();
    let job = store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(job.kind, JobKind::BranchRequest);
    assert_eq!(job.state, JobState::Pending);
    assert_eq!(job.payload.node_id.as_deref(), Some(BRANCH));

    let (status, again) = send(&app, json!({"work_id": work})).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["job_id"], job_id.as_str());
    assert_eq!(again["already_queued"], true);
}
