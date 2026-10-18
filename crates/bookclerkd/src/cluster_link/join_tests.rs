//! Join a library from the web UI, end to end in one process: the joining
//! daemon reads the owner's `info`, starts a sign-in, the owner's user
//! consents, the callback redeems the code, and the link's client starts
//! at once (no restart); Leave library tells the owner (`node.leaving`),
//! which revokes the node. Also the "I have an invite" tab, and who may
//! change Settings → Library's sharing settings (the operator or an Owner).

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
    let (state, store) = crate::node_api::tests::test_state(files).await;
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
    host_routes_as(state, UserCaller::Operator)
}

/// The joining daemon's routes as `caller` reaches them (the sharing
/// routes read the caller the daemon's auth layer puts in the request).
fn host_routes_as(state: &Arc<AppState>, caller: UserCaller) -> axum::Router {
    axum::Router::new()
        .route("/api/config/join", get(super::get_join_status))
        .route("/api/config/join/inspect", post(super::post_join_inspect))
        .route("/api/config/join/start", post(super::post_join_start))
        .route("/api/config/join/callback", post(super::post_join_callback))
        .route("/api/config/join/invite", post(super::post_join_invite))
        .route("/api/config/join/leave", post(super::post_join_leave))
        .route(
            "/api/config/join/sharing",
            get(super::get_join_sharing).post(super::post_join_sharing),
        )
        .layer(axum::Extension(caller))
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
    let url = url::Url::parse(authorize_url).unwrap();
    assert_eq!(url.path(), "/oidc/authorize");
    let mut body = serde_json::Map::new();
    for (name, value) in url.query_pairs() {
        body.insert(name.into_owned(), json!(value.into_owned()));
    }
    body.insert("approve".into(), json!(approve));
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
    assert_eq!(status_now["links"][0]["share_books"], true);
    assert_eq!(status_now["links"][0]["include_branches"], true);
    eventually("the branch to report its sharing settings", || async {
        load_node(&owner_store, &node_id).await.unwrap().share_books == Some(true)
    })
    .await;

    // Settings → Library: the settings reach the library at once.
    let (status, _, shared) = call(
        &host,
        "POST",
        "/api/config/join/sharing",
        Some(json!({ "cluster_id": cluster_id, "share_books": false, "include_branches": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{shared}");
    assert_eq!(shared["cluster"]["share_books"], false);
    eventually("the library to see sharing off", || async {
        load_node(&owner_store, &node_id).await.unwrap().share_books == Some(false)
    })
    .await;
    let (status, _, _) = call(
        &host,
        "POST",
        "/api/config/join/sharing",
        Some(json!({ "cluster_id": "no-such-cluster", "share_books": true, "include_branches": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An Owner of this branch may change them too, and the change reaches
    // the running link; an Administrator or a Member may not.
    let mut users = Vec::new();
    for role in [UserRole::Owner, UserRole::Administrator, UserRole::Member] {
        let user = host_store
            .create_user(role, Some("Sam"), None)
            .await
            .unwrap();
        users.push(user.id);
    }
    let (owner_user, admin_user, member_user) = (users[0], users[1], users[2]);
    for refused in [admin_user, member_user] {
        let app = host_routes_as(&host_state, UserCaller::User { user_id: refused });
        let (status, _, _) = call(&app, "GET", "/api/config/join/sharing", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = call(
            &app,
            "POST",
            "/api/config/join/sharing",
            Some(
                json!({ "cluster_id": cluster_id, "share_books": true, "include_branches": false }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let unchanged = load_cluster_link(&host_store, &cluster_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!unchanged.share_books && unchanged.include_branches);
    let as_owner = host_routes_as(
        &host_state,
        UserCaller::User {
            user_id: owner_user,
        },
    );
    let (status, _, listed) = call(&as_owner, "GET", "/api/config/join/sharing", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["clusters"][0]["cluster_id"], cluster_id.as_str());
    assert_eq!(listed["clusters"][0]["share_books"], false);
    let (status, _, shared) = call(
        &as_owner,
        "POST",
        "/api/config/join/sharing",
        Some(json!({ "cluster_id": cluster_id, "share_books": true, "include_branches": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{shared}");
    assert_eq!(shared["cluster"]["share_books"], true);
    assert_eq!(shared["cluster"]["include_branches"], false);
    assert!(super::super::is_link_running(&cluster_id));
    eventually("the library to see the Owner's settings", || async {
        let node = load_node(&owner_store, &node_id).await.unwrap();
        node.share_books == Some(true) && node.include_branches == Some(false)
    })
    .await;
    // The operator still may.
    let (status, _, listed) = call(&host, "GET", "/api/config/join/sharing", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["clusters"][0]["include_branches"], false);

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
