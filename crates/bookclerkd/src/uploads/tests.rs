//! Tests of the upload routes and the ingest job, through the router.

use super::*;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use bookclerk_config::Paths;
use http_body_util::BodyExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::auth::tests::{phase2_harness, portal_cookie_for};

/// The operator token the harness accepts.
const TOKEN: &str = "op-token-uploads";

#[test]
fn review_cursors_round_trip_and_refuse_junk() {
    let id = IngestBatchId::new();
    let raw = format!("{id}:12");
    assert_eq!(parse_review_cursor(&raw), Some((id.clone(), 12)));
    assert_eq!(parse_review_cursor(&format!("{id}:")), None);
    assert_eq!(parse_review_cursor(&format!("{id}:+1")), None);
    assert_eq!(parse_review_cursor("nothex:1"), None);
    assert_eq!(parse_review_cursor(id.as_str()), None);
}

/// Sends `request` and returns the status and JSON body (`Null` when empty).
async fn send(app: &axum::Router, request: HttpRequest<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// A request as the operator.
fn operator(method: &str, uri: &str) -> axum::http::request::Builder {
    HttpRequest::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
}

/// A `PATCH` of `bytes` at `offset`.
fn patch(id: &str, member_no: u32, offset: u64, bytes: &'static [u8]) -> HttpRequest<Body> {
    operator("PATCH", &format!("/api/uploads/{id}/files/{member_no}"))
        .header(header::CONTENT_TYPE, OFFSET_STREAM)
        .header(UPLOAD_OFFSET, offset.to_string())
        .body(Body::from(bytes))
        .unwrap()
}

#[tokio::test]
async fn an_upload_is_staged_committed_and_ingested() {
    let (state, app, library) = phase2_harness(TOKEN).await;
    let files = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    {
        let mut cfg = state.config.write().await;
        cfg.paths = Some(Paths::from_files_dir(files.path().to_path_buf()));
        // The test machine's own free space must not decide the outcome.
        cfg.uploads.free_disk_floor_percent = 0;
    }
    crate::node_api::uploads::set_test_destination(files.path(), storage.path());

    // A Member may not upload until an Owner allows it.
    let cookie = portal_cookie_for(&library, "test", "member-ext").await;
    let manifest = json!({ "files": [
        { "path": "Mystery Book/01.mp3", "size": 3, "sha256": hex::encode(Sha256::digest(b"abc")) },
        { "path": "Mystery Book/cover.jpg", "size": 1 },
        { "path": "Mystery Book/setup.exe", "size": 1 },
    ]})
    .to_string();
    let (status, _) = send(
        &app,
        HttpRequest::builder()
            .method("POST")
            .uri("/api/uploads")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-csrf-token", "1")
            .body(Body::from(manifest.clone()))
            .unwrap(),
    )
    .await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED,
        "{status}"
    );

    // The operator's upload is held by the first active Owner.
    let (status, _) = send(
        &app,
        operator("POST", "/api/uploads")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from(manifest.clone()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (status, created) = send(
        &app,
        operator("POST", "/api/uploads")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(manifest))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["files"].as_array().unwrap().len(), 3);

    // Bytes resume at the staged length.
    let (status, body) = send(&app, patch(&id, 0, 0, b"ab")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["received"], 2);
    let (status, _) = send(&app, patch(&id, 0, 0, b"ab")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = send(&app, patch(&id, 0, 2, b"cd")).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, _) = send(&app, patch(&id, 9, 0, b"a")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        operator("POST", &format!("/api/uploads/{id}/commit"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "files still missing");
    let (_, body) = send(&app, patch(&id, 0, 2, b"c")).await;
    assert_eq!(body["complete"], true);
    send(&app, patch(&id, 1, 0, b"j")).await;
    send(&app, patch(&id, 2, 0, b"x")).await;

    let (status, committed) = send(
        &app,
        operator("POST", &format!("/api/uploads/{id}/commit"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{committed}");
    assert_eq!(committed["state"], "committed");

    // The job stores the book under a new local edition for review.
    let payload = JobPayload {
        ingest_batch: Some(id.clone()),
        ..JobPayload::default()
    };
    let summary = run_ingest_batch(&state, &payload, None).await.unwrap();
    let summary: Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(summary["needsReview"], 2, "{summary}");
    assert_eq!(summary["refused"], 1, "{summary}");
    let (_, done) = send(
        &app,
        operator("GET", &format!("/api/uploads/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(done["state"], "done", "{done}");
    let outcomes: Vec<_> = done["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["outcome"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(outcomes, ["needs_review", "needs_review", "refused"]);
    let edition = done["files"][0]["editionId"].as_str().unwrap().to_string();
    let acquired = library
        .edition_acquisition(&edition)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(acquired.status, bookclerk_library::AcquireStatus::Acquired);
    let key = acquired.storage_key.unwrap();
    assert_eq!(std::fs::read(storage.path().join(&key)).unwrap(), b"abc");
    let folder = std::path::Path::new(&key).parent().unwrap();
    assert_eq!(
        std::fs::read(storage.path().join(folder).join("cover.jpg")).unwrap(),
        b"j"
    );
    assert!(!staging::batch_dir(files.path(), &IngestBatchId::parse(&id).unwrap()).exists());

    // An Administrator sees it to review; the upload can no longer change.
    let (status, review) = send(
        &app,
        operator("GET", "/api/uploads/review")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{review}");
    assert_eq!(review["items"].as_array().unwrap().len(), 2);
    let (status, _) = send(
        &app,
        operator("DELETE", &format!("/api/uploads/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, list) = send(
        &app,
        operator("GET", "/api/uploads").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(list["uploads"].as_array().unwrap().len(), 1);

    // Uploading the same book again finds the edition it already has.
    let again = json!({ "files": [{ "path": "Mystery Book/01.mp3", "size": 3 }] }).to_string();
    let (_, created) = send(
        &app,
        operator("POST", "/api/uploads")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(again))
            .unwrap(),
    )
    .await;
    let second = created["id"].as_str().unwrap().to_string();
    send(&app, patch(&second, 0, 0, b"abc")).await;
    send(
        &app,
        operator("POST", &format!("/api/uploads/{second}/commit"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let payload = JobPayload {
        ingest_batch: Some(second.clone()),
        ..JobPayload::default()
    };
    let summary: Value =
        serde_json::from_str(&run_ingest_batch(&state, &payload, None).await.unwrap()).unwrap();
    assert_eq!(summary["duplicate"], 1, "{summary}");

    // A cancelled upload drops its bytes, and a prune pass is clean.
    let third = json!({ "files": [{ "path": "x.mp3", "size": 5 }] }).to_string();
    let (_, created) = send(
        &app,
        operator("POST", "/api/uploads")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(third))
            .unwrap(),
    )
    .await;
    let third = created["id"].as_str().unwrap().to_string();
    send(&app, patch(&third, 0, 0, b"ab")).await;
    let (status, _) = send(
        &app,
        operator("DELETE", &format!("/api/uploads/{third}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!staging::batch_dir(files.path(), &IngestBatchId::parse(&third).unwrap()).exists());
    prune_once(&state).await.unwrap();
}

#[tokio::test]
async fn an_intake_batch_is_queued_ingested_and_announced() {
    use bookclerk_library::control_plane::{
        batch_members, first_active_owner, load_batch, CreateRefusal, IntakeRef, MemberOutcome,
        MemberSpec,
    };
    let (state, _app, library) = phase2_harness(TOKEN).await;
    let files = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    state.config.write().await.paths = Some(Paths::from_files_dir(files.path().to_path_buf()));
    let drop = tempfile::tempdir().unwrap();
    crate::node_api::uploads::set_test_destination(files.path(), storage.path());
    crate::node_api::uploads::set_test_intake(files.path(), "local", drop.path());
    std::fs::create_dir_all(drop.path().join("incoming/Dropped")).unwrap();
    std::fs::write(drop.path().join("incoming/Dropped/01.mp3"), b"drop").unwrap();
    let owner = first_active_owner(&library).await.unwrap().unwrap();

    // Nothing to ingest is refused before anything is queued.
    let empty = queue_intake_batch(&state, &IngestBatchId::new(), owner, "local", &[]).await;
    assert!(
        matches!(empty, Err(IntakeError::Refused(CreateRefusal::Empty))),
        "{empty:?}"
    );

    let member = |path: &str, key: &str, size| MemberSpec {
        path: bookclerk_archive::EntryPath::admit(path).unwrap(),
        size,
        sha256: None,
        intake: Some(IntakeRef {
            key: key.to_string(),
            signature: None,
        }),
    };
    let id = IngestBatchId::new();
    queue_intake_batch(
        &state,
        &id,
        owner,
        "local",
        &[
            member("Dropped/01.mp3", "incoming/Dropped/01.mp3", 4),
            member("Gone/01.mp3", "incoming/Gone/01.mp3", 1),
        ],
    )
    .await
    .unwrap();
    let batch = load_batch(&library, &id).await.unwrap().unwrap();
    assert_eq!(batch.state, IngestState::Committed);

    let payload = JobPayload {
        node_id: Some(batch.node_id.clone()),
        ingest_batch: Some(id.as_str().to_string()),
        ..JobPayload::default()
    };
    run_ingest_batch(&state, &payload, None).await.unwrap();
    let members = batch_members(&library, &id).await.unwrap();
    let outcomes: Vec<_> = members
        .iter()
        .map(|member| (member.outcome, member.reason.clone()))
        .collect();
    assert_eq!(
        outcomes,
        [
            (MemberOutcome::NeedsReview, None),
            (
                MemberOutcome::Failed,
                Some("intake_unavailable".to_string())
            ),
        ]
    );

    let events = library.list_domain_events(50).await.unwrap();
    let finished: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == bookclerk_library::INGEST_BATCH_FINISHED)
        .collect();
    assert_eq!(finished.len(), 1);
    let body: Value = serde_json::from_str(&finished[0].payload).unwrap();
    assert_eq!(body["batchId"], id.as_str());
    assert_eq!(body["state"], "done");
    assert_eq!(body["source"], "intake");
    assert_eq!(body["intakeInstance"], "local");
}

/// An uploaded file's tags are stored as its probe and stream, and the
/// chapters it carries become its copy's file claim.
#[tokio::test]
async fn an_uploaded_files_chapters_and_running_time_are_recorded() {
    bookclerk_media::replace_pool(bookclerk_media::MediaPool::new(
        bookclerk_media::MediaPoolConfig {
            confinement: bookclerk_media::Confinement::Off,
            ..bookclerk_media::MediaPoolConfig::default()
        },
    ));
    let (state, app, library) = phase2_harness(TOKEN).await;
    let files = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    {
        let mut cfg = state.config.write().await;
        cfg.paths = Some(Paths::from_files_dir(files.path().to_path_buf()));
        cfg.uploads.free_disk_floor_percent = 0;
    }
    crate::node_api::uploads::set_test_destination(files.path(), storage.path());
    let bytes: &'static [u8] = Box::leak(bookclerk_tags::fixtures::tagged_m4b().into_boxed_slice());
    let tags = bookclerk_tags::read_all(bytes).unwrap();
    let manifest = json!({ "files": [
        { "path": "Tagged Book/Book.m4b", "size": bytes.len() },
    ]})
    .to_string();
    let (status, created) = send(
        &app,
        operator("POST", "/api/uploads")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(manifest))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let (_, body) = send(&app, patch(&id, 0, 0, bytes)).await;
    assert_eq!(body["complete"], true, "{body}");
    let (status, _) = send(
        &app,
        operator("POST", &format!("/api/uploads/{id}/commit"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let payload = JobPayload {
        ingest_batch: Some(id.clone()),
        ..JobPayload::default()
    };
    run_ingest_batch(&state, &payload, None).await.unwrap();
    let (_, done) = send(
        &app,
        operator("GET", &format!("/api/uploads/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let edition = done["files"][0]["editionId"].as_str().unwrap().to_string();

    let stored = library.edition_audio_files(&edition).await.unwrap();
    assert_eq!(stored.len(), 1, "{stored:?}");
    let file = &stored[0];
    assert_eq!(file.stream.duration_ms, tags.duration.map(|d| d.ms));
    assert_eq!(
        file.probe.as_ref().map(|p| p.content_type.as_str()),
        Some("audio/mp4")
    );

    let resolved = library.edition_chapters(&edition).await.unwrap().unwrap();
    assert_eq!(
        resolved.source_kind,
        bookclerk_library::ChapterSourceKind::File
    );
    assert_eq!(
        resolved.source_id,
        bookclerk_library::copy_source_id(
            &file.destination,
            std::slice::from_ref(&file.storage_key)
        )
    );
    let titles: Vec<_> = resolved
        .list
        .chapters()
        .iter()
        .map(|c| c.title.clone())
        .collect();
    let expected: Vec<_> = tags.chapters.iter().map(|c| c.title.clone()).collect();
    assert_eq!(titles, expected);
    assert_eq!(resolved.runtime_ms, tags.duration.map(|d| d.ms));
}

#[tokio::test]
async fn a_drop_folder_item_is_ingested_and_moved_to_imported() {
    use bookclerk_config::IntakeSettings;
    use bookclerk_library::control_plane::{
        create_plugin_instance, first_active_owner, load_batch, load_intake_items, ConfigActor,
    };
    let (state, _app, library) = phase2_harness(TOKEN).await;
    let files = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    state.config.write().await.paths = Some(Paths::from_files_dir(files.path().to_path_buf()));
    let instance = create_plugin_instance(
        &library,
        &ConfigActor::Operator {
            id: "operator:test".into(),
        },
        "local",
        "Drop folder",
    )
    .await
    .unwrap()
    .id
    .as_str()
    .to_string();
    let storage = tempfile::tempdir().unwrap();
    crate::node_api::uploads::set_test_destination(files.path(), storage.path());
    crate::node_api::uploads::set_test_intake(files.path(), &instance, folder.path());
    std::fs::create_dir_all(folder.path().join("Dropped")).unwrap();
    std::fs::write(folder.path().join("Dropped/01.mp3"), b"drop").unwrap();
    let owner = first_active_owner(&library).await.unwrap().unwrap();
    let settings = IntakeSettings::for_patron(owner);
    let backend = bookclerk_storage::LocalFsBackend::new(folder.path().to_path_buf()).unwrap();
    let handoff = crate::intake::QueueHandOff { state: &state };
    let drop_folder = crate::intake::Folder {
        store: &library,
        instance_id: &instance,
        backend: &backend,
        prefix: "",
        settings: &settings,
    };

    crate::intake::run_pass(drop_folder, &|| 0, Some(&handoff))
        .await
        .unwrap();
    let ready = crate::intake::run_pass(drop_folder, &|| 60_000, Some(&handoff))
        .await
        .unwrap();
    assert_eq!(ready.queued, ["Dropped"]);
    let batch_id = load_intake_items(&library, &instance)
        .await
        .unwrap()
        .remove("Dropped")
        .and_then(|record| record.batch_id)
        .unwrap();

    // Not finished while the job has not run.
    assert_eq!(
        crate::intake::finish_ended(drop_folder, 60_000)
            .await
            .unwrap(),
        0
    );
    let batch = load_batch(
        &library,
        &bookclerk_library::control_plane::IngestBatchId::parse(&batch_id).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let payload = JobPayload {
        node_id: Some(batch.node_id.clone()),
        ingest_batch: Some(batch_id.clone()),
        ..JobPayload::default()
    };
    run_ingest_batch(&state, &payload, None).await.unwrap();

    // 2026-11-20T00:00:00Z.
    let finished = crate::intake::finish_ended(drop_folder, 1_795_132_800_000)
        .await
        .unwrap();
    assert_eq!(finished, 1);
    assert!(folder
        .path()
        .join("_imported/2026-11-20/Dropped/01.mp3")
        .is_file());
    assert!(!folder.path().join("Dropped").join("01.mp3").exists());
    assert!(load_intake_items(&library, &instance)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_intake_file_that_grew_after_settling_fails() {
    use bookclerk_library::control_plane::{
        batch_members, first_active_owner, load_batch, IntakeRef, MemberOutcome, MemberSpec,
    };
    let (state, _app, library) = phase2_harness(TOKEN).await;
    let files = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let drop = tempfile::tempdir().unwrap();
    state.config.write().await.paths = Some(Paths::from_files_dir(files.path().to_path_buf()));
    crate::node_api::uploads::set_test_destination(files.path(), storage.path());
    crate::node_api::uploads::set_test_intake(files.path(), "drop", drop.path());
    std::fs::write(drop.path().join("Growing.mp3"), b"longer than it was").unwrap();
    let owner = first_active_owner(&library).await.unwrap().unwrap();
    let id = IngestBatchId::new();
    queue_intake_batch(
        &state,
        &id,
        owner,
        "drop",
        &[MemberSpec {
            path: bookclerk_archive::EntryPath::admit("Growing.mp3").unwrap(),
            size: 4,
            sha256: None,
            intake: Some(IntakeRef {
                key: "Growing.mp3".to_string(),
                signature: None,
            }),
        }],
    )
    .await
    .unwrap();
    let batch = load_batch(&library, &id).await.unwrap().unwrap();
    let payload = JobPayload {
        node_id: Some(batch.node_id.clone()),
        ingest_batch: Some(id.as_str().to_string()),
        ..JobPayload::default()
    };
    run_ingest_batch(&state, &payload, None).await.unwrap();
    let members = batch_members(&library, &id).await.unwrap();
    assert_eq!(members[0].outcome, MemberOutcome::Failed);
    assert_eq!(members[0].reason.as_deref(), Some("intake_changed"));
}
