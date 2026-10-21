//! Join a library from the web UI, end to end in one process: the joining
//! daemon reads the owner's `info`, starts a sign-in, the owner's user
//! consents, the callback redeems the code, and the link's client starts
//! at once (no restart); Leave library tells the owner (`node.leaving`),
//! which revokes the node. Also the "I have an invite" tab, and a clerk
//! join (the owner's database is simulated: its config names Postgres
//! while the store is SQLite, since no Postgres runs in tests).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::{header, StatusCode};
use axum::routing::{get, post};
use bookclerk_library::control_plane::{
    load_cluster_link, load_node, load_node_credential, mint_enrollment_link, EnrollmentActor,
};
use bookclerk_library::{LibraryStore, UserRole};
use serde_json::{json, Value};

use crate::api::AppState;
use crate::auth::UserCaller;
use crate::node_api::branch::tests::{call, routes, LOOPBACK};

/// Tests have no database guests installed: treat the library's as present.
pub(super) static ASSUME_DATABASE_GUESTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Serves `app` on a loopback port, as the daemon's listener does.
async fn serve(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    let app = app.layer(axum::Extension(crate::node_api::transport::LocalAddr(
        local,
    )));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    local
}

/// An owner library served on loopback, with one member. Answers its state,
/// store, canonical origin and the member.
async fn owner(files: &tempfile::TempDir) -> (Arc<AppState>, LibraryStore, String, i64) {
    owner_with(files, "").await
}

/// [`owner`] with `config_toml` as the library's config.
async fn owner_with(
    files: &tempfile::TempDir,
    config_toml: &str,
) -> (Arc<AppState>, LibraryStore, String, i64) {
    let (state, store) = crate::node_api::tests::test_state_with(files, config_toml).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    state.config.write().await.integrations.public_origin =
        Some(format!("http://127.0.0.1:{}", addr.port()));
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
    (
        state,
        store,
        format!("http://localhost:{}", addr.port()),
        member.id,
    )
}

/// The joining daemon's operator routes (the auth layer is the daemon's).
fn host_routes(state: &Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/api/config/join", get(super::get_join_status))
        .route("/api/config/join/inspect", post(super::post_join_inspect))
        .route("/api/config/join/start", post(super::post_join_start))
        .route("/api/config/join/callback", post(super::post_join_callback))
        .route("/api/config/join/invite", post(super::post_join_invite))
        .route("/api/config/join/leave", post(super::post_join_leave))
        .with_state(Arc::clone(state))
}

/// Waits up to 10 s for `check`.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The owner's user consents to the authorize URL's request; answers the
/// callback's query pairs.
async fn consent(
    owner: &Arc<AppState>,
    member: i64,
    authorize_url: &str,
    approve: bool,
) -> std::collections::HashMap<String, String> {
    consent_with(owner, member, authorize_url, approve, json!({})).await
}

/// [`consent`] with `extra` fields in the body (`connection`, `step_up`).
async fn consent_with(
    owner: &Arc<AppState>,
    member: i64,
    authorize_url: &str,
    approve: bool,
    extra: Value,
) -> std::collections::HashMap<String, String> {
    let url = url::Url::parse(authorize_url).unwrap();
    assert_eq!(url.path(), "/oidc/authorize");
    let mut body = serde_json::Map::new();
    for (name, value) in url.query_pairs() {
        body.insert(name.into_owned(), json!(value.into_owned()));
    }
    body.insert("approve".into(), json!(approve));
    for (name, value) in extra.as_object().unwrap() {
        body.insert(name.clone(), value.clone());
    }
    let app = routes(owner, UserCaller::User { user_id: member }, LOOPBACK);
    let (status, _, answer) = call(
        &app,
        "POST",
        "/api/auth/branch-consent",
        Some(Value::Object(body)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let redirect = url::Url::parse(answer["redirect"].as_str().unwrap()).unwrap();
    assert_eq!(redirect.path(), "/join/callback");
    redirect.query_pairs().into_owned().collect()
}

#[tokio::test]
async fn a_sign_in_joins_starts_the_client_and_leave_tells_the_owner() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let (owner_state, owner_store, origin, member) = owner(&owner_files).await;
    let host_files = tempfile::tempdir().unwrap();
    let (host_state, host_store) = crate::node_api::tests::test_state(&host_files).await;
    // The supervisor a join wakes, as the daemon runs it.
    super::super::spawn_cluster_links(Arc::clone(&host_state));
    let host = host_routes(&host_state);

    let (status, _, fresh) = call(&host, "GET", "/api/config/join", None).await;
    assert_eq!(status, StatusCode::OK, "{fresh}");
    assert_eq!(fresh["fresh"], true);

    // The address must be the library's own origin.
    let port = origin.rsplit(':').next().unwrap();
    let (status, _, mismatch) = call(
        &host,
        "POST",
        "/api/config/join/inspect",
        Some(json!({ "address": format!("http://127.0.0.1:{port}") })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{mismatch}");
    assert_eq!(mismatch["origin"], origin.as_str());
    let (status, _, inspected) = call(
        &host,
        "POST",
        "/api/config/join/inspect",
        Some(json!({ "address": origin })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{inspected}");
    assert_eq!(inspected["library"]["needs_fingerprint"], false);
    assert_eq!(inspected["library"]["enrollment"], true);

    // A denied sign-in uses up its pending join.
    let start = json!({ "address": origin, "branch_name": "Den PC" });
    let (status, headers, started) =
        call(&host, "POST", "/api/config/join/start", Some(start.clone())).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let back = consent(
        &owner_state,
        member,
        started["authorize_url"].as_str().unwrap(),
        false,
    )
    .await;
    assert_eq!(back["error"], "access_denied");
    let callback = json!({ "state": back["state"], "error": "access_denied" });
    let (status, _, denied) = call(
        &host,
        "POST",
        "/api/config/join/callback",
        Some(callback.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let (status, _, _) = call(&host, "POST", "/api/config/join/callback", Some(callback)).await;
    assert_eq!(status, StatusCode::GONE);

    // Approve: the callback redeems and the client starts at once.
    let (_, _, started) = call(&host, "POST", "/api/config/join/start", Some(start)).await;
    let authorize_url = started["authorize_url"].as_str().unwrap();
    assert!(
        !authorize_url.contains("code_verifier"),
        "the verifier never leaves the daemon: {authorize_url}"
    );
    let back = consent(&owner_state, member, authorize_url, true).await;
    let callback = json!({ "code": back["code"], "state": back["state"] });
    let (status, _, joined) = call(
        &host,
        "POST",
        "/api/config/join/callback",
        Some(callback.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["running"], true);
    let cluster_id = joined["cluster"]["cluster_id"]
        .as_str()
        .unwrap()
        .to_string();
    let node_id = joined["cluster"]["node_id"].as_str().unwrap().to_string();
    assert!(super::super::is_link_running(&cluster_id));
    let node = load_node(&owner_store, &node_id).await.unwrap();
    assert_eq!(node.owner_user_id, Some(member));
    eventually("the branch to connect", || async {
        load_node_credential(&owner_store, &node_id)
            .await
            .unwrap()
            .is_some_and(|credential| credential.last_connected_at.is_some())
    })
    .await;
    // The state is single use.
    let (status, _, _) = call(&host, "POST", "/api/config/join/callback", Some(callback)).await;
    assert_eq!(status, StatusCode::GONE);
    let (_, _, status_now) = call(&host, "GET", "/api/config/join", None).await;
    assert_eq!(status_now["fresh"], false);
    assert_eq!(status_now["links"][0]["running"], true);

    // Leave library: the owner hears node.leaving and revokes the node.
    let (status, _, left) = call(
        &host,
        "POST",
        "/api/config/join/leave",
        Some(json!({ "cluster_id": cluster_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{left}");
    assert_eq!(left["owner_notified"], true);
    let credential = load_node_credential(&owner_store, &node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        credential.revoked_at.is_some(),
        "the owner revoked the node"
    );
    let link = load_cluster_link(&host_store, &cluster_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!link.is_active());
    eventually("the client to stop", || async {
        !super::super::is_link_running(&cluster_id)
    })
    .await;
    let (status, _, _) = call(
        &host,
        "POST",
        "/api/config/join/leave",
        Some(json!({ "cluster_id": "no-such-cluster" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_invite_joins_and_starts_at_once_and_a_closed_library_refuses() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let (owner_state, owner_store, origin, member) = owner(&owner_files).await;
    let host_files = tempfile::tempdir().unwrap();
    let (host_state, _host_store) = crate::node_api::tests::test_state(&host_files).await;
    // The supervisor a join wakes, as the daemon runs it.
    super::super::spawn_cluster_links(Arc::clone(&host_state));
    let host = host_routes(&host_state);

    let (status, _, refused) = call(
        &host,
        "POST",
        "/api/config/join/invite",
        Some(json!({ "link": "not a link" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "invalid_invite");

    let minted = mint_enrollment_link(
        &owner_store,
        &EnrollmentActor::Operator { id: "test".into() },
        member,
        "",
        None,
    )
    .await
    .unwrap();
    let (status, _, joined) = call(
        &host,
        "POST",
        "/api/config/join/invite",
        Some(json!({ "link": minted.url(&origin, None).as_str() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["running"], true);
    let cluster_id = joined["cluster"]["cluster_id"].as_str().unwrap();
    let node_id = joined["cluster"]["node_id"].as_str().unwrap();
    eventually("the branch to connect", || async {
        load_node_credential(&owner_store, node_id)
            .await
            .unwrap()
            .is_some_and(|credential| credential.last_connected_at.is_some())
    })
    .await;
    let (status, _, left) = call(
        &host,
        "POST",
        "/api/config/join/leave",
        Some(json!({ "cluster_id": cluster_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{left}");
    assert_eq!(left["owner_notified"], true);

    // "Let new branches join" off: the wizard stops before any sign-in.
    let operator = routes(&owner_state, UserCaller::Operator, LOOPBACK);
    let (status, _, _) = call(
        &operator,
        "PUT",
        "/api/config/branches",
        Some(json!({ "enrollment": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, closed) = call(
        &host,
        "POST",
        "/api/config/join/start",
        Some(json!({ "address": origin })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{closed}");
    assert_eq!(closed["error"], "enrollment_disabled");
}

#[tokio::test]
async fn an_empty_install_joins_as_a_clerk_and_takes_the_librarys_bootstrap() {
    use crate::node_api::branch::clerk_tests::{owner_with_totp, totp, POSTGRES_CONFIG};
    let _dek = crate::auth::tests::process_dek_lock().await;
    ASSUME_DATABASE_GUESTS.store(true, std::sync::atomic::Ordering::SeqCst);
    let owner_files = tempfile::tempdir().unwrap();
    let (owner_state, owner_store, origin, _member) =
        owner_with(&owner_files, POSTGRES_CONFIG).await;

    // A server that already holds a user can only join as a branch.
    let busy_files = tempfile::tempdir().unwrap();
    let (busy_state, busy_store) = crate::node_api::tests::test_state(&busy_files).await;
    busy_store
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .unwrap();
    let busy = host_routes(&busy_state);
    let (_, _, status_busy) = call(&busy, "GET", "/api/config/join", None).await;
    assert_eq!(status_busy["clerk_eligible"], false, "{status_busy}");
    let (status, _, refused) = call(
        &busy,
        "POST",
        "/api/config/join/start",
        Some(json!({ "address": origin, "clerk": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "not_empty");

    let host_files = tempfile::tempdir().unwrap();
    let (host_state, _host_store) = crate::node_api::tests::test_state(&host_files).await;
    let host = host_routes(&host_state);
    let (_, _, status_now) = call(&host, "GET", "/api/config/join", None).await;
    assert_eq!(status_now["clerk_eligible"], true, "{status_now}");
    let master_key = bookclerk_library::master_key_path(host_files.path());
    let before = std::fs::read(&master_key).ok();
    let config_file = host_state.config.read().await.paths().config_file.clone();
    // One process holds one DEK, and each test state above cached its own:
    // the one cached last is the library's from here on.
    let dek = bookclerk_library::require_master_key(None).unwrap();
    let (owner_user, secret) = owner_with_totp(&owner_store).await;

    // Approved as a branch, a code cannot be redeemed as a clerk: the
    // callback's `connection` is only a hint, the code carries the choice.
    let start = json!({ "address": origin, "branch_name": "Rack 2", "clerk": true });
    let (status, _, started) =
        call(&host, "POST", "/api/config/join/start", Some(start.clone())).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["clerk_offered"], true);
    let authorize_url = started["authorize_url"].as_str().unwrap();
    assert!(authorize_url.contains("clerk_offer=1"), "{authorize_url}");
    let back = consent(&owner_state, owner_user, authorize_url, true).await;
    assert!(!back.contains_key("connection"));
    let (status, _, tampered) = call(
        &host,
        "POST",
        "/api/config/join/callback",
        Some(json!({ "code": back["code"], "state": back["state"], "connection": "clerk" })),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{tampered}");
    assert_eq!(
        std::fs::read(&master_key).ok(),
        before,
        "master.key untouched"
    );
    assert!(!host_files
        .path()
        .join(bookclerk_node_client::CLERK_POSTGRES_URL_FILE)
        .exists());

    // Approved as a clerk with a fresh code: the bootstrap is applied.
    let (_, _, started) = call(&host, "POST", "/api/config/join/start", Some(start)).await;
    let back = consent_with(
        &owner_state,
        owner_user,
        started["authorize_url"].as_str().unwrap(),
        true,
        json!({ "connection": "clerk", "step_up": totp(&secret) }),
    )
    .await;
    assert_eq!(back["connection"], "clerk");
    let (status, _, joined) = call(
        &host,
        "POST",
        "/api/config/join/callback",
        Some(json!({ "code": back["code"], "state": back["state"], "connection": "clerk" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["connection"], "clerk");
    assert_eq!(joined["database"], "postgres");
    assert_eq!(joined["restarting"], true);
    assert!(!joined.to_string().contains("pg-secret"), "{joined}");
    let node_id = joined["node_id"].as_str().unwrap().to_string();
    let cluster_id = joined["cluster_id"].as_str().unwrap().to_string();

    // The library recorded an owner node; the joiner holds its bootstrap.
    let node = load_node(&owner_store, &node_id).await.unwrap();
    assert_eq!(node.kind, bookclerk_library::control_plane::NodeKind::Owner);
    assert!(load_node_credential(&owner_store, &node_id)
        .await
        .unwrap()
        .is_none());
    let mut want = b"BCK1".to_vec();
    want.extend_from_slice(dek.as_bytes());
    assert_eq!(
        std::fs::read(&master_key).unwrap(),
        want,
        "the library's DEK"
    );
    let identity: Value = serde_json::from_slice(
        &std::fs::read(host_files.path().join("node-identity.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(identity["node_id"], node_id.as_str());
    assert_eq!(identity["cluster_id"], cluster_id.as_str());
    let written = bookclerk_config::Config::from_toml_file(&config_file).unwrap();
    assert_eq!(written.database.plugin, "postgres");
    assert_eq!(written.database.postgres.url, None);
    let url_file = written.database.postgres.url_file.clone().unwrap();
    assert!(url_file.is_absolute());
    assert_eq!(
        std::fs::read_to_string(&url_file).unwrap(),
        "postgres://lib:pg-secret@db.example/library"
    );
    assert!(!std::fs::read_to_string(&config_file)
        .unwrap()
        .contains("pg-secret"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&url_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let actions: Vec<String> = owner_store
        .list_security_audit_events(50)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.action)
        .collect();
    assert!(
        actions.iter().any(|a| a == "clerk_authorize"),
        "{actions:?}"
    );
    assert!(actions.iter().any(|a| a == "clerk_enrolled"), "{actions:?}");
    ASSUME_DATABASE_GUESTS.store(false, std::sync::atomic::Ordering::SeqCst);
}
