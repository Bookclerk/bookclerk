//! Joining as a clerk, owner side: the consent page offers the choice only
//! to an Owner, clerk needs a shareable database and a fresh second factor,
//! the code is bound to the connection type chosen, and the redeem answer's
//! bootstrap opens only with the node's box key.

use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::routing::{get, post};
use base64::Engine as _;
use bookclerk_library::control_plane::{load_node, load_node_credential, NodeKind};
use bookclerk_library::{LibraryStore, UserRole};
use bookclerk_node_proto::{
    open_clerk_bootstrap, pkce_challenge, ClerkDatabase, BRANCH_CLIENT_ID, BRANCH_SCOPE,
};
use serde_json::{json, Value};

use super::tests::{call, LOOPBACK};
use crate::api::AppState;
use crate::auth::UserCaller;

/// Where the joining install wants the code.
const REDIRECT: &str = "http://127.0.0.1:41234/join/callback";

/// The library's database: PostgreSQL, so a clerk may share it.
pub(crate) const POSTGRES_CONFIG: &str =
    "[database]\nplugin = \"postgres\"\n\n[database.postgres]\nurl = \"postgres://lib:pg-secret@db.example/library\"\n";

/// The node's X25519 box secret in these tests.
const NODE_BOX_SECRET: [u8; 32] = [11u8; 32];

/// The consent and redeem routes for `caller` from `peer` on `local`.
fn routes(state: &Arc<AppState>, caller: UserCaller, (peer, local): (&str, &str)) -> axum::Router {
    let peer: std::net::SocketAddr = peer.parse().unwrap();
    let local: std::net::SocketAddr = local.parse().unwrap();
    axum::Router::new()
        .route("/api/node/v1/enroll/oauth", post(super::post_enroll_oauth))
        .route(
            "/api/auth/branch-consent",
            get(super::get_branch_consent).post(super::post_branch_consent),
        )
        .route(
            "/api/auth/branch-consent/step-up",
            post(super::post_branch_consent_step_up),
        )
        .layer(axum::Extension(caller))
        .layer(axum::Extension(super::super::transport::LocalAddr(local)))
        .layer(axum::extract::connect_info::MockConnectInfo(peer))
        .with_state(Arc::clone(state))
}

/// A PKCE verifier.
fn verifier(tag: char) -> String {
    std::iter::repeat_n(tag, 48).collect()
}

/// The authorization request, offering a clerk when `offer`.
fn params(verifier: &str, offer: bool) -> Value {
    let mut body = json!({
        "client_id": BRANCH_CLIENT_ID,
        "redirect_uri": REDIRECT,
        "response_type": "code",
        "scope": BRANCH_SCOPE,
        "state": "state-1",
        "code_challenge": pkce_challenge(verifier),
        "code_challenge_method": "S256",
        "branch_name": "Spare NAS",
    });
    if offer {
        body["clerk_offer"] = json!("1");
    }
    body
}

/// The request as a query string.
fn query(verifier: &str, offer: bool) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in params(verifier, offer).as_object().unwrap() {
        out.append_pair(name, value.as_str().unwrap());
    }
    out.finish()
}

/// A consent answer; `connection` and `step_up` as given.
async fn consent(
    app: &axum::Router,
    verifier: &str,
    offer: bool,
    connection: Option<&str>,
    step_up: Option<Value>,
) -> (StatusCode, Value) {
    let mut body = params(verifier, offer);
    body["approve"] = json!(true);
    if let Some(connection) = connection {
        body["connection"] = json!(connection);
    }
    if let Some(step_up) = step_up {
        body["step_up"] = step_up;
    }
    let (status, _, answer) = call(app, "POST", "/api/auth/branch-consent", Some(body)).await;
    (status, answer)
}

/// The callback query of a consent answer.
fn callback(answer: &Value) -> std::collections::HashMap<String, String> {
    url::Url::parse(answer["redirect"].as_str().unwrap())
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// The redeem body for `code` as `connection`, with the test node keys.
fn redeem(code: &str, verifier: &str, connection: Option<&str>) -> Value {
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let mut body = json!({
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": REDIRECT,
        "sign_pk": b64(&bookclerk_node_proto::sign_public_key(&[10u8; 32])),
        "box_pk": b64(&bookclerk_node_proto::box_public_key(&NODE_BOX_SECRET)),
        "branch_name": "Spare NAS",
        "software_version": "1.0.0",
    });
    if let Some(connection) = connection {
        body["connection"] = json!(connection);
    }
    body
}

/// An Owner with a confirmed authenticator; answers the user id and secret.
pub(crate) async fn owner_with_totp(store: &LibraryStore) -> (i64, String) {
    let owner = store
        .create_user(UserRole::Owner, Some("Morgan"), None)
        .await
        .unwrap();
    let secret = totp_rs::Secret::generate().to_base32();
    store
        .confirm_totp_enrollment(owner.id, &secret)
        .await
        .unwrap();
    (owner.id, secret)
}

/// A TOTP step-up for `secret`.
pub(crate) fn totp(secret: &str) -> Value {
    json!({ "method": "totp", "code": crate::totp::current_test_code(secret) })
}

/// Recent audit actions.
async fn audit_actions(store: &LibraryStore) -> Vec<String> {
    store
        .list_security_audit_events(50)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.action)
        .collect()
}

#[tokio::test]
async fn an_owner_with_a_fresh_second_factor_lets_a_clerk_join_with_a_sealed_bootstrap() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::super::tests::test_state_with(&files, POSTGRES_CONFIG).await;
    // The test state locks out after one failure; allow the one wrong code below.
    *state.auth.write().await = Arc::new(crate::auth::OperatorAuthState::new(
        "test-token".into(),
        1,
        false,
        5,
        60,
    ));
    let (owner, secret) = owner_with_totp(&store).await;
    let app = routes(&state, UserCaller::User { user_id: owner }, LOOPBACK);

    // The consent page offers the choice to the Owner, branch by default.
    let v = verifier('a');
    let (status, _, view) = call(
        &app,
        "GET",
        &format!("/api/auth/branch-consent?{}", query(&v, true)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["connection"]["choice"], true);
    assert_eq!(view["connection"]["default"], "branch");
    assert_eq!(view["connection"]["clerk_available"], true, "{view}");
    assert_eq!(view["connection"]["step_up"], json!(["totp"]));

    // Clerk needs the step-up, and a code that verifies.
    let (status, answer) = consent(&app, &v, true, Some("clerk"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "step_up_required");
    let (status, answer) = consent(
        &app,
        &v,
        true,
        Some("clerk"),
        Some(json!({ "method": "totp", "code": "000000" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "step_up_failed");
    let (status, answer) = consent(&app, &v, true, Some("clerk"), Some(totp(&secret))).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let pairs = callback(&answer);
    assert_eq!(pairs["connection"], "clerk");
    let code = pairs["code"].clone();

    let (status, headers, enrolled) = call(
        &app,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, Some("clerk"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{enrolled}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(enrolled["connection"], "clerk");
    let node_id = enrolled["node_id"].as_str().unwrap().to_string();
    let cluster_id = enrolled["cluster_id"].as_str().unwrap().to_string();
    let sealed = enrolled["bootstrap"].as_str().unwrap().to_string();
    assert!(!sealed.contains("pg-secret"));
    assert!(!enrolled.to_string().contains("pg-secret"));

    // The library records an owner node, not a branch, with no socket key.
    let node = load_node(&store, &node_id).await.unwrap();
    assert_eq!(node.kind, NodeKind::Owner);
    assert_eq!(node.owner_user_id, Some(owner));
    assert!(load_node_credential(&store, &node_id)
        .await
        .unwrap()
        .is_none());

    // Only the node's box key opens the bootstrap: the database and the DEK.
    let opened = open_clerk_bootstrap(&NODE_BOX_SECRET, &cluster_id, &node_id, &sealed).unwrap();
    match &opened.database {
        ClerkDatabase::Postgres { url } => {
            assert_eq!(url.expose(), b"postgres://lib:pg-secret@db.example/library");
        }
        other => panic!("unexpected database {other:?}"),
    }
    let dek = bookclerk_library::require_master_key(Some(files.path())).unwrap();
    assert_eq!(opened.dek.expose(), dek.as_bytes());
    assert!(open_clerk_bootstrap(&[12u8; 32], &cluster_id, &node_id, &sealed).is_err());

    // A lost answer: the retry replays the same node with a fresh box.
    let (status, _, again) = call(
        &app,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, Some("clerk"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["node_id"], node_id.as_str());
    // ... but never as a branch.
    let (status, _, refused) = call(
        &app,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, None)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");

    let actions = audit_actions(&store).await;
    for action in [
        "clerk_authorize",
        "clerk_enrolled",
        "clerk_authorize_refused",
    ] {
        assert!(actions.iter().any(|a| a == action), "{action}: {actions:?}");
    }
}

#[tokio::test]
async fn a_code_is_bound_to_the_connection_type_chosen_at_consent() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::super::tests::test_state_with(&files, POSTGRES_CONFIG).await;
    let (owner, secret) = owner_with_totp(&store).await;
    let app = routes(&state, UserCaller::User { user_id: owner }, LOOPBACK);
    let refused = |(status, _, body): (StatusCode, axum::http::HeaderMap, Value)| {
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"], "enrollment_refused");
        assert!(body.get("bootstrap").is_none());
    };

    // A branch approval redeemed as a clerk: refused, and the code is spent.
    let v = verifier('b');
    let (status, answer) = consent(&app, &v, true, None, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let pairs = callback(&answer);
    assert!(!pairs.contains_key("connection"));
    let branch_code = pairs["code"].clone();
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem(&branch_code, &v, Some("clerk"))),
        )
        .await,
    );
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem(&branch_code, &v, None)),
        )
        .await,
    );

    // A clerk approval redeemed as a branch: refused too.
    let (status, answer) = consent(&app, &v, true, Some("clerk"), Some(totp(&secret))).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let clerk_code = callback(&answer)["code"].clone();
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem(&clerk_code, &v, None)),
        )
        .await,
    );

    // A branch redeemed as a branch, then retried as a clerk.
    let v = verifier('c');
    let (_, answer) = consent(&app, &v, true, Some("branch"), None).await;
    let code = callback(&answer)["code"].clone();
    let (status, _, enrolled) = call(
        &app,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, None)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{enrolled}");
    assert!(enrolled.get("bootstrap").is_none());
    let node = load_node(&store, enrolled["node_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(node.kind, NodeKind::Host);
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem(&code, &v, Some("clerk"))),
        )
        .await,
    );
}

#[tokio::test]
async fn clerk_is_refused_to_non_owners_without_a_shareable_database_or_second_factor() {
    let _dek = crate::auth::tests::process_dek_lock().await;

    // SQLite: shown disabled with the reason, and refused.
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::super::tests::test_state(&files).await;
    let (owner, secret) = owner_with_totp(&store).await;
    let app = routes(&state, UserCaller::User { user_id: owner }, LOOPBACK);
    let v = verifier('d');
    let (_, _, view) = call(
        &app,
        "GET",
        &format!("/api/auth/branch-consent?{}", query(&v, true)),
        None,
    )
    .await;
    assert_eq!(view["connection"]["choice"], true);
    assert_eq!(view["connection"]["clerk_available"], false);
    assert!(
        view["connection"]["clerk_reason"]
            .as_str()
            .unwrap()
            .contains("SQLite"),
        "{view}"
    );
    let (status, answer) = consent(&app, &v, true, Some("clerk"), Some(totp(&secret))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"], "database_not_shareable");

    // PostgreSQL from here on.
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::super::tests::test_state_with(&files, POSTGRES_CONFIG).await;
    let (owner, secret) = owner_with_totp(&store).await;

    // Anyone but an Owner gets branch only: no choice, and clerk refused.
    let member = store
        .create_user(UserRole::Administrator, Some("Riley"), None)
        .await
        .unwrap();
    let as_member = routes(&state, UserCaller::User { user_id: member.id }, LOOPBACK);
    let (_, _, view) = call(
        &as_member,
        "GET",
        &format!("/api/auth/branch-consent?{}", query(&v, true)),
        None,
    )
    .await;
    assert_eq!(view["connection"]["choice"], false, "{view}");
    let (status, answer) = consent(&as_member, &v, true, Some("clerk"), Some(totp(&secret))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "owner_required");
    let (status, _, answer) =
        call(&as_member, "POST", "/api/auth/branch-consent/step-up", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");

    // The joining install did not offer a clerk (it is not empty).
    let app = routes(&state, UserCaller::User { user_id: owner }, LOOPBACK);
    let (_, _, view) = call(
        &app,
        "GET",
        &format!("/api/auth/branch-consent?{}", query(&v, false)),
        None,
    )
    .await;
    assert_eq!(view["connection"]["clerk_available"], false, "{view}");
    let (status, answer) = consent(&app, &v, false, Some("clerk"), Some(totp(&secret))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(answer["error"], "clerk_not_offered");

    // Plain HTTP from the LAN: no clerk consent.
    let lan = routes(
        &state,
        UserCaller::User { user_id: owner },
        ("192.168.1.20:5000", "0.0.0.0:8787"),
    );
    let (status, answer) = consent(&lan, &v, true, Some("clerk"), Some(totp(&secret))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "transport_insecure");

    // An Owner with neither an authenticator nor a passkey.
    let bare = store
        .create_user(UserRole::Owner, Some("Sam"), None)
        .await
        .unwrap();
    let as_bare = routes(&state, UserCaller::User { user_id: bare.id }, LOOPBACK);
    let (_, _, view) = call(
        &as_bare,
        "GET",
        &format!("/api/auth/branch-consent?{}", query(&v, true)),
        None,
    )
    .await;
    assert_eq!(view["connection"]["clerk_available"], false);
    assert!(view["connection"]["clerk_reason"]
        .as_str()
        .unwrap()
        .contains("second factor"));
    let (status, answer) = consent(&as_bare, &v, true, Some("clerk"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "step_up_unavailable");
    let (status, _, answer) =
        call(&as_bare, "POST", "/api/auth/branch-consent/step-up", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    assert_eq!(answer["error"], "no_passkey");

    // An Owner demoted between consent and redeem: refused at redeem.
    let (status, answer) = consent(&app, &v, true, Some("clerk"), Some(totp(&secret))).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let code = callback(&answer)["code"].clone();
    store
        .set_user_role(owner, UserRole::Administrator)
        .await
        .unwrap();
    let (status, _, refused) = call(
        &app,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, Some("clerk"))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert!(refused.get("bootstrap").is_none());
}
