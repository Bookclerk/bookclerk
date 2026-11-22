//! Tests for convert jobs: queued over the web API, then run against a
//! stored book.

// Test code: a failed fixture write or read is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use bookclerk_acquire::{AcquireDestination, AcquireDestinations};
use bookclerk_config::{DestinationId, MultiDestinationMode, OutputCopy, OutputFormat};
use bookclerk_library::{AcquireStatus, FileOwner, FileRole, JobKind, LibraryStore, NewBook};
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;

/// Product id of the stored book.
const ASIN: &str = "B00CONV";

/// Records [`ASIN`] as acquired by `user-1`, stored as an M4B at
/// `root/<key>` on the `local` destination, and returns its key.
async fn stored(store: &LibraryStore, root: &std::path::Path) -> String {
    store
        .upsert_account("audible", "user-1", "us", None, true, "audible")
        .await
        .unwrap();
    store
        .upsert_book(&NewBook::minimal(ASIN, "user-1", "us", "Twin"))
        .await
        .unwrap();
    let key = format!("Twin/{ASIN}.m4b");
    store
        .set_acquire_status(ASIN, "user-1", AcquireStatus::Acquired, Some(&key), None)
        .await
        .unwrap();
    let title = store
        .acquire_title("audible", "user-1", "us", ASIN)
        .await
        .unwrap()
        .expect("title");
    let owner = FileOwner::Edition(title.edition_id.clone());
    let main = OutputCopy::parse("m4b:original").unwrap();
    assert!(store
        .record_edition_copy_file("local", &key, &owner, FileRole::Audio, Some(&main))
        .await
        .unwrap());
    let path = root.join(&key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    bookclerk_media::package_m4b_from_pcm(
        &vec![0i16; 48_000 * 2 * 2],
        48_000,
        2,
        &path,
        &[("One".to_string(), 0)],
    )
    .unwrap();
    key
}

/// The one `local` destination at `root`, keeping M4B as its main output.
fn local(root: &std::path::Path) -> AcquireDestinations {
    let options = bookclerk_source::DownloadOptions {
        format: OutputFormat::EnrichedM4b,
        fixup_metadata: false,
        download_pdf: false,
        ..bookclerk_source::DownloadOptions::default()
    };
    AcquireDestinations {
        items: vec![AcquireDestination {
            id: DestinationId::new("local").unwrap(),
            backend: Box::new(bookclerk_storage::LocalFsBackend::new(root.to_path_buf()).unwrap()),
            options,
            filter: bookclerk_config::DestinationFilter::default(),
        }],
        primary: DestinationId::new("local").unwrap(),
        multi_destination: MultiDestinationMode::SyncMissing,
    }
}

/// A convert adds the copy, matches it the second time, and fails for
/// good on a copy it cannot make or a title with nothing stored.
#[tokio::test]
async fn a_convert_job_adds_the_copy_and_then_matches_it() {
    bookclerk_media::replace_pool(bookclerk_media::MediaPool::new(
        bookclerk_media::MediaPoolConfig {
            confinement: bookclerk_media::Confinement::Off,
            ..bookclerk_media::MediaPoolConfig::default()
        },
    ));
    let files = tempfile::tempdir().unwrap();
    let (state, store) = crate::node_api::tests::test_state(&files).await;
    let root = files.path().join("local");
    let main_key = stored(&store, &root).await;
    let destinations = local(&root);
    let copy = OutputCopy::parse("mp3:64k-mono").unwrap();

    let made = convert_titles(&state, &destinations, ASIN, None, &copy, false, None)
        .await
        .unwrap();
    assert_eq!(made, "converted=1 matched=0 failed=0");
    let again = convert_titles(&state, &destinations, ASIN, None, &copy, false, None)
        .await
        .unwrap();
    assert_eq!(again, "converted=0 matched=1 failed=0");
    assert!(root.join(&main_key).exists(), "the original stays");

    let main = OutputCopy::parse("m4b:original").unwrap();
    let err = convert_titles(&state, &destinations, ASIN, None, &main, false, None)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<JobFailedForGood>().is_some(), "{err}");
    assert!(err.to_string().contains("failed=1"), "{err}");

    let err = convert_titles(&state, &destinations, "B00NONE", None, &copy, false, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("names no stored book"), "{err}");
    let err = run_convert(&state, None, None, &copy, false, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("names the title"), "{err}");
    // No destination is deployed in this daemon state.
    assert!(run_convert(&state, Some(ASIN), None, &copy, false, None)
        .await
        .is_err());
}

/// Sends `body` to the convert route as the operator.
async fn post_convert(
    state: &Arc<AppState>,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = Router::new()
        .route("/api/library/convert", post(crate::api::trigger_convert))
        .layer(axum::Extension(
            bookclerk_library::control_plane::ScanRequester::Operator,
        ))
        .with_state(Arc::clone(state));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/library/convert")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// The route refuses copies a convert does not make and bodies that name
/// no book, queues one job carrying the copy, and folds a repeat into it.
#[tokio::test]
async fn the_convert_route_queues_one_job_per_title() {
    let files = tempfile::tempdir().unwrap();
    let (state, store) = crate::node_api::tests::test_state(&files).await;
    stored(&store, &files.path().join("local")).await;
    let mp3 = serde_json::json!({"format": "mp3", "quality": "64k-mono"});

    for (body, said) in [
        (
            serde_json::json!({"copy": {"format": "original", "quality": "original"}, "titles": [ASIN]}),
            "new encoding",
        ),
        (
            serde_json::json!({"copy": {"format": "opus", "quality": "v2"}, "titles": [ASIN]}),
            "",
        ),
        (
            serde_json::json!({"copy": mp3, "titles": ["  "]}),
            "name the books",
        ),
    ] {
        let (status, answer) = post_convert(&state, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        assert!(
            answer["message"].as_str().unwrap().contains(said),
            "{answer}"
        );
    }
    assert!(store.list_jobs(10).await.unwrap().is_empty());

    let body = serde_json::json!({"copy": mp3, "titles": [ASIN], "force": true});
    let (status, answer) = post_convert(&state, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let jobs = store.list_jobs(10).await.unwrap();
    let [job] = jobs.as_slice() else {
        panic!("one job: {jobs:?}");
    };
    assert_eq!(job.kind, JobKind::Acquire);
    assert_eq!(
        job.payload.convert,
        Some(OutputCopy::parse("mp3:64k-mono").unwrap())
    );
    assert!(job.payload.force);
    assert_eq!(answer["job_id"], job.id);

    let (status, again) = post_convert(&state, body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    assert_eq!(again["job_id"], job.id);
}
