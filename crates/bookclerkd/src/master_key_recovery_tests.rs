//! Tests of the recovery-copy routes, through the router.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use bookclerk_library::{RecoveryCopyChoice, RecoveryCopyState};

use super::{status_from, MasterKeyStore, RecoveryCopyAnswer};
use crate::auth::tests::{phase2_harness, portal_cookie_for};

/// Who sends a request: the operator's Bearer token, or a session cookie.
enum Caller<'a> {
    /// `Authorization: Bearer <token>`.
    Operator(&'a str),
    /// `Cookie: <cookie>`.
    Cookie(&'a str),
}

/// Sends `method uri` as `caller` with a JSON `body`; returns the status,
/// the headers and the raw body.
async fn send(
    app: &axum::Router,
    caller: Caller<'_>,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    let request = match caller {
        Caller::Operator(token) => request.header(header::AUTHORIZATION, format!("Bearer {token}")),
        Caller::Cookie(cookie) => request.header(header::COOKIE, cookie),
    };
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

/// Decodes a JSON body.
fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

/// Points the harness at `files_dir`, holding the process's key in
/// `master.key` unwrapped, as a running install would.
async fn use_files_dir(state: &crate::api::AppState, files_dir: &std::path::Path) {
    let key = bookclerk_library::require_master_key(None).expect("process key");
    let file = bookclerk_library::encode_master_key_file(key.as_bytes(), None).expect("encode");
    std::fs::write(bookclerk_library::master_key_path(files_dir), &*file).expect("write key");
    let loaded =
        bookclerk_config::Config::load(Some(files_dir.to_path_buf()), None).expect("config");
    state.config.write().await.paths = loaded.paths;
}

#[tokio::test]
async fn the_operator_saves_a_recovery_copy_and_records_it() {
    const TOKEN: &str = "op-token-recovery-copy";
    let (state, app, _library) = phase2_harness(TOKEN).await;
    let files = tempfile::tempdir().expect("files dir");
    use_files_dir(&state, files.path()).await;
    let op = || Caller::Operator(TOKEN);

    let (status, _, body) = send(&app, op(), "GET", "/api/config/master-key", "").await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["store"], "file");
    assert_eq!(body["recovery_copy"], "unrecorded");
    assert_eq!(body["needs_recovery_copy"], false);
    assert!(body.get("recorded_at").is_none());

    let (status, _, body) = send(
        &app,
        op(),
        "POST",
        "/api/config/master-key/recovery-copy",
        r#"{"password": "  short  "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["error"], "password_too_short");

    let password = "correct horse battery";
    let (status, headers, copy) = send(
        &app,
        op(),
        "POST",
        "/api/config/master-key/recovery-copy",
        &format!(r#"{{"password": "{password}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"bookclerk-recovery-key.bck\""
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert!(copy.starts_with(b"BCK2"));

    // The copy restores the same key on another install.
    let elsewhere = tempfile::tempdir().expect("other files dir");
    let restored =
        bookclerk_library::import_master_key(elsewhere.path(), &copy, Some(password), None)
            .expect("import");
    let ours = bookclerk_library::master_key_fingerprint(
        &bookclerk_library::require_master_key(None).expect("process key"),
    );
    assert_eq!(restored, ours);

    // Making a copy records nothing; the web app says when it was saved.
    let (_, _, body) = send(&app, op(), "GET", "/api/config/master-key", "").await;
    assert_eq!(json(&body)["recovery_copy"], "unrecorded");
    let (status, _, body) = send(
        &app,
        op(),
        "PUT",
        "/api/config/master-key/recovery-copy/state",
        r#"{"state": "skipped"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["recovery_copy"], "skipped");
    assert!(body["recorded_at"].is_string());
    let (status, _, body) = send(
        &app,
        op(),
        "PUT",
        "/api/config/master-key/recovery-copy/state",
        r#"{"state": "saved"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["recovery_copy"], "saved");
    let (_, _, body) = send(&app, op(), "GET", "/api/config/master-key", "").await;
    assert_eq!(json(&body)["recovery_copy"], "saved");
}

#[tokio::test]
async fn an_owner_reads_the_status_and_only_the_operator_reaches_the_key() {
    const TOKEN: &str = "op-token-recovery-owner";
    let (state, app, library) = phase2_harness(TOKEN).await;
    let files = tempfile::tempdir().expect("files dir");
    use_files_dir(&state, files.path()).await;
    let owner = portal_cookie_for(&library, "test", "admin-ext").await;
    // An Owner reads the status, so the web app can say to elevate.
    let (status, _, bytes) = send(
        &app,
        Caller::Cookie(&owner),
        "GET",
        "/api/config/master-key",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&bytes)["store"], "file");
    for (method, uri, body) in [
        (
            "POST",
            "/api/config/master-key/recovery-copy",
            r#"{"password": "correct horse battery"}"#,
        ),
        (
            "PUT",
            "/api/config/master-key/recovery-copy/state",
            r#"{"state": "saved"}"#,
        ),
    ] {
        let (status, _, bytes) = send(&app, Caller::Cookie(&owner), method, uri, body).await;
        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
            "{method} {uri}: {status}"
        );
        assert!(!bytes.starts_with(b"BCK"), "{method} {uri} answered a key");
    }
    assert_eq!(
        bookclerk_library::recovery_copy_state(
            files.path(),
            &bookclerk_library::require_master_key(None).expect("process key"),
        ),
        RecoveryCopyState::Unrecorded
    );
}

#[tokio::test]
async fn a_missing_key_file_is_reported() {
    const TOKEN: &str = "op-token-recovery-missing";
    let (state, app, _library) = phase2_harness(TOKEN).await;
    let files = tempfile::tempdir().expect("files dir");
    use_files_dir(&state, files.path()).await;
    std::fs::remove_file(bookclerk_library::master_key_path(files.path())).expect("remove");
    let op = || Caller::Operator(TOKEN);

    let (status, _, body) = send(&app, op(), "GET", "/api/config/master-key", "").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json(&body)["error"], "master_key_failed");

    let (status, _, body) = send(
        &app,
        op(),
        "POST",
        "/api/config/master-key/recovery-copy",
        r#"{"password": "correct horse battery"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json(&body)["error"], "master_key_failed");

    // Nothing is recorded for a key the install doesn't have.
    let (status, _, body) = send(
        &app,
        op(),
        "PUT",
        "/api/config/master-key/recovery-copy/state",
        r#"{"state": "skipped"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json(&body)["error"], "master_key_failed");
    assert!(!bookclerk_library::recovery_record_path(files.path()).exists());
}

#[test]
fn a_keychain_key_asks_for_a_copy_until_one_is_saved() {
    let unrecorded = status_from(MasterKeyStore::Keychain, RecoveryCopyState::Unrecorded);
    assert!(unrecorded.needs_recovery_copy);
    assert_eq!(unrecorded.recovery_copy, RecoveryCopyAnswer::Unrecorded);

    let skipped = status_from(
        MasterKeyStore::Keychain,
        RecoveryCopyState::Recorded {
            choice: RecoveryCopyChoice::Skipped,
            recorded_at: 1_795_000_000,
        },
    );
    assert!(skipped.needs_recovery_copy);
    assert_eq!(skipped.recovery_copy, RecoveryCopyAnswer::Skipped);
    assert!(skipped.recorded_at.is_some());

    let saved = status_from(
        MasterKeyStore::Keychain,
        RecoveryCopyState::Recorded {
            choice: RecoveryCopyChoice::Saved,
            recorded_at: 1_795_000_000,
        },
    );
    assert!(!saved.needs_recovery_copy);

    // A sealed credential isn't in backups of the files directory either.
    assert!(
        status_from(MasterKeyStore::SystemdCreds, RecoveryCopyState::Unrecorded)
            .needs_recovery_copy
    );

    // A key in a file is backed up with the files directory.
    for store in [MasterKeyStore::File, MasterKeyStore::PasswordFile] {
        assert!(!status_from(store, RecoveryCopyState::Unrecorded).needs_recovery_copy);
    }
}
