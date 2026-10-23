//! Join a library with a sign-in, owner side: `info`, the consent API, the
//! `/oidc/authorize` hand-off, and `enroll/oauth` (idempotent retries, one
//! node under concurrency, and every refusal), plus the branch invite's
//! `no-store` answer and Settings → Branches.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use base64::Engine as _;
use bookclerk_library::control_plane::{
    list_nodes, load_node, set_host_enrollment_enabled, ConfigActor, NodeKind,
};
use bookclerk_library::{UserRole, UserStatus};
use bookclerk_node_proto::{pkce_challenge, BRANCH_CLIENT_ID, BRANCH_SCOPE};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;

use crate::api::AppState;
use crate::auth::UserCaller;

/// Where the joining install wants the code.
const REDIRECT: &str = "http://127.0.0.1:41234/join/callback";

/// A loopback peer on a loopback listener.
pub(crate) const LOOPBACK: (&str, &str) = ("127.0.0.1:5000", "127.0.0.1:8787");

/// A LAN peer over plain HTTP.
const LAN: (&str, &str) = ("192.168.1.20:5000", "0.0.0.0:8787");

/// The branch routes as the daemon mounts them, for `caller` seen from
/// `peer` on a listener bound to `local`.
pub(crate) fn routes(
    state: &Arc<AppState>,
    caller: UserCaller,
    (peer, local): (&str, &str),
) -> axum::Router {
    let peer: std::net::SocketAddr = peer.parse().unwrap();
    let local: std::net::SocketAddr = local.parse().unwrap();
    axum::Router::new()
        .route("/api/node/v1/info", get(super::get_info))
        .route("/api/node/v1/enroll/oauth", post(super::post_enroll_oauth))
        .route(
            "/api/auth/branch-consent",
            get(super::get_branch_consent).post(super::post_branch_consent),
        )
        .route(
            "/api/config/branches",
            get(super::get_branches).put(super::put_branches),
        )
        .route(
            "/api/config/branches/titles/remove",
            post(super::post_remove_branch_titles),
        )
        .route(
            "/api/config/node-invites",
            post(super::super::post_node_invite),
        )
        .layer(axum::Extension(caller))
        .layer(axum::Extension(super::super::transport::LocalAddr(local)))
        .layer(axum::extract::connect_info::MockConnectInfo(peer))
        .with_state(Arc::clone(state))
}

/// Sends a JSON request; answers status, headers and body.
pub(crate) async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A base64url Ed25519 public key from a seed byte.
fn key(byte: u8) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(bookclerk_node_proto::sign_public_key(&[byte; 32]))
}

/// A base64url X25519 public key from a secret byte.
fn box_key(byte: u8) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(bookclerk_node_proto::box_public_key(&[byte; 32]))
}

/// A PKCE verifier (43+ unreserved characters).
fn verifier(tag: char) -> String {
    std::iter::repeat_n(tag, 48).collect()
}

/// The authorization request for `verifier`, as the consent page relays it.
fn params(verifier: &str) -> Value {
    json!({
        "client_id": BRANCH_CLIENT_ID,
        "redirect_uri": REDIRECT,
        "response_type": "code",
        "scope": BRANCH_SCOPE,
        "state": "state-1",
        "code_challenge": pkce_challenge(verifier),
        "code_challenge_method": "S256",
        "branch_name": "Den PC",
    })
}

/// The same request as a query string.
fn query(verifier: &str) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in params(verifier).as_object().unwrap() {
        out.append_pair(name, value.as_str().unwrap());
    }
    out.finish()
}

/// Approves the request for `verifier` as `app`'s user; answers the code.
async fn approve(app: &axum::Router, verifier: &str) -> String {
    let mut body = params(verifier);
    body["approve"] = json!(true);
    let (status, headers, answer) = call(app, "POST", "/api/auth/branch-consent", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let redirect = url::Url::parse(answer["redirect"].as_str().unwrap()).unwrap();
    assert!(redirect.as_str().starts_with(REDIRECT), "{redirect}");
    let pairs: std::collections::HashMap<_, _> = redirect.query_pairs().into_owned().collect();
    assert_eq!(pairs["state"], "state-1");
    pairs["code"].clone()
}

/// The `enroll/oauth` body for `code`.
fn redeem(code: &str, verifier: &str, keys: u8) -> Value {
    json!({
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": REDIRECT,
        "sign_pk": key(keys),
        "box_pk": box_key(keys + 1),
        "branch_name": "Den PC",
        "software_version": "1.0.0",
    })
}

/// An owner library with one active member.
async fn owner(files: &tempfile::TempDir) -> (Arc<AppState>, bookclerk_library::LibraryStore, i64) {
    let (state, store) = super::super::tests::test_state(files).await;
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
    (state, store, member.id)
}

#[tokio::test]
async fn info_describes_the_library_under_the_transport_policy() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, member) = owner(&files).await;
    let as_member = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);
    let (status, _, info) = call(&as_member, "GET", "/api/node/v1/info", None).await;
    assert_eq!(status, StatusCode::OK, "{info}");
    let cluster = bookclerk_library::control_plane::load_cluster_row(&store)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info["cluster_id"], cluster.cluster_id);
    assert_eq!(
        info["ancestry"],
        json!([cluster.cluster_id]),
        "a library that is nobody's branch is its own whole ancestry"
    );
    assert_eq!(info["transport"], "loopback");
    assert_eq!(info["enrollment"], true);
    assert!(info.get("spki_fingerprint").is_none_or(Value::is_null));
    assert_eq!(info["library_name"], "localhost");

    let lan = routes(&state, UserCaller::User { user_id: member }, LAN);
    let (status, _, refused) = call(&lan, "GET", "/api/node/v1/info", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "transport_insecure");
}

#[tokio::test]
async fn a_consented_code_enrolls_one_node_and_retries_replay() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, member) = owner(&files).await;
    let app = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);

    // The consent page's view of the request.
    let v = verifier('a');
    let (status, _, consent) = call(
        &app,
        "GET",
        &format!("/api/auth/branch-consent?{}", query(&v)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{consent}");
    assert_eq!(consent["branch_name"], "Den PC");
    assert_eq!(consent["library_name"], "localhost");
    assert_eq!(consent["redirect_origin"], "http://127.0.0.1:41234");
    assert_eq!(consent["user"]["id"], member);

    let code = approve(&app, &v).await;
    // The redeem body names the branch differently: the node is still
    // labelled with the name the user consented to.
    let mut renamed = redeem(&code, &v, 4);
    renamed["branch_name"] = json!("Not Den PC");
    let (status, _, enrolled) =
        call(&app, "POST", "/api/node/v1/enroll/oauth", Some(renamed)).await;
    assert_eq!(status, StatusCode::OK, "{enrolled}");
    let node_id = enrolled["node_id"].as_str().unwrap().to_string();
    let node = load_node(&store, &node_id).await.unwrap();
    assert_eq!(node.kind, NodeKind::Host);
    assert_eq!(node.owner_user_id, Some(member));
    {
        use bookclerk_library::entities::node_enrollment_links as links;
        use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter as _};
        let link = links::Entity::find()
            .filter(links::Column::RedeemedNodeId.eq(node_id.as_str()))
            .one(store.db())
            .await
            .unwrap()
            .expect("the redeemed link");
        assert_eq!(link.label, "Den PC");
    }

    // A retry of the same exchange (a lost answer) replays the same node.
    let (status, _, again) = call(
        &app,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, 4)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["node_id"], node_id.as_str());
    // The used code with other keys, or another verifier, is refused.
    for body in [redeem(&code, &v, 9), redeem(&code, &verifier('b'), 4)] {
        let (status, _, refused) =
            call(&app, "POST", "/api/node/v1/enroll/oauth", Some(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert_eq!(refused["error"], "enrollment_refused");
    }

    // Concurrent redeems of one fresh code make one node.
    let v = verifier('c');
    let code = approve(&app, &v).await;
    let before = list_nodes(&store).await.unwrap().len();
    let attempts = (0..4).map(|_| {
        let app = app.clone();
        let body = redeem(&code, &v, 20);
        async move { call(&app, "POST", "/api/node/v1/enroll/oauth", Some(body)).await }
    });
    let answers = futures::future::join_all(attempts).await;
    let ids: std::collections::BTreeSet<_> = answers
        .iter()
        .map(|(status, _, body)| {
            assert_eq!(*status, StatusCode::OK, "{body}");
            body["node_id"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(ids.len(), 1, "one node: {ids:?}");
    assert_eq!(list_nodes(&store).await.unwrap().len(), before + 1);
}

#[tokio::test]
async fn codes_are_refused_unless_every_check_passes() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, member) = owner(&files).await;
    let app = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);
    let refused = |answer: (StatusCode, HeaderMap, Value)| {
        assert_eq!(answer.0, StatusCode::FORBIDDEN, "{}", answer.2);
        assert_eq!(answer.2["error"], "enrollment_refused");
    };

    // A wrong verifier burns the code: the right one is refused after.
    let v = verifier('a');
    let code = approve(&app, &v).await;
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem(&code, &verifier('z'), 4)),
        )
        .await,
    );
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem(&code, &v, 4)),
        )
        .await,
    );

    // Another redirect URI than the one consented to.
    let code = approve(&app, &v).await;
    let mut body = redeem(&code, &v, 4);
    body["redirect_uri"] = json!("http://127.0.0.1:9/join/callback");
    refused(call(&app, "POST", "/api/node/v1/enroll/oauth", Some(body)).await);

    // An unknown code.
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem("made-up", &v, 4)),
        )
        .await,
    );

    // A code issued before enrollment was turned off.
    let code = approve(&app, &v).await;
    let operator = ConfigActor::Operator { id: "test".into() };
    set_host_enrollment_enabled(&store, &operator, false)
        .await
        .unwrap();
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem(&code, &v, 4)),
        )
        .await,
    );
    // ... and no new code while it is off.
    let mut body = params(&v);
    body["approve"] = json!(true);
    let (status, _, answer) = call(&app, "POST", "/api/auth/branch-consent", Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "enrollment_disabled");
    set_host_enrollment_enabled(&store, &operator, true)
        .await
        .unwrap();

    // A code whose user is disabled before it is redeemed; a disabled
    // user cannot consent either.
    let code = approve(&app, &v).await;
    store
        .set_user_status(member, UserStatus::Disabled)
        .await
        .unwrap();
    refused(
        call(
            &app,
            "POST",
            "/api/node/v1/enroll/oauth",
            Some(redeem(&code, &v, 4)),
        )
        .await,
    );
    let mut body = params(&v);
    body["approve"] = json!(true);
    let (status, _, answer) = call(&app, "POST", "/api/auth/branch-consent", Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "user_inactive");
    store
        .set_user_status(member, UserStatus::Active)
        .await
        .unwrap();

    // Plain HTTP from the LAN: neither consent nor redeem.
    let code = approve(&app, &v).await;
    let lan = routes(&state, UserCaller::User { user_id: member }, LAN);
    let (status, _, answer) = call(
        &lan,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, 4)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "transport_insecure");
    let mut body = params(&v);
    body["approve"] = json!(true);
    let (status, _, answer) = call(&lan, "POST", "/api/auth/branch-consent", Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "transport_insecure");
    // The code refused from the LAN is still good on loopback.
    let (status, _, answer) = call(
        &app,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, 4)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    // Malformed requests: a redirect that is neither loopback nor https,
    // plain PKCE, another scope, another client, no branch name.
    for (field, value) in [
        ("redirect_uri", json!("http://evil.example/join/callback")),
        ("code_challenge_method", json!("plain")),
        ("scope", json!("openid")),
        ("client_id", json!("someone-else")),
        ("branch_name", json!("")),
    ] {
        let mut body = params(&v);
        body[field] = value;
        body["approve"] = json!(true);
        let (status, _, answer) = call(&app, "POST", "/api/auth/branch-consent", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {answer}");
        assert_eq!(answer["error"], "invalid_request");
    }

    // The operator owns no node: a user must consent.
    let operator_app = routes(&state, UserCaller::Operator, LOOPBACK);
    let mut body = params(&v);
    body["approve"] = json!(true);
    let (status, _, answer) = call(
        &operator_app,
        "POST",
        "/api/auth/branch-consent",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"], "user_required");

    // Deny sends the browser back with access_denied and no code.
    let mut body = params(&v);
    body["approve"] = json!(false);
    let (status, _, answer) = call(&app, "POST", "/api/auth/branch-consent", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let redirect = answer["redirect"].as_str().unwrap();
    assert!(redirect.contains("error=access_denied"), "{redirect}");
    assert!(!redirect.contains("code="), "{redirect}");
}

#[tokio::test]
async fn oidc_authorize_hands_branch_requests_to_the_consent_page() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, _store, member) = owner(&files).await;
    let app = crate::api::router(Arc::clone(&state), None);
    let v = verifier('a');

    // The token endpoint never takes (or burns) a branch code.
    let consent = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);
    let code = approve(&consent, &v).await;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", &code)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair("client_id", BRANCH_CLIENT_ID)
        .append_pair("code_verifier", &v)
        .finish();
    let request = Request::builder()
        .method("POST")
        .uri("/oidc/token")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());
    let (status, _, enrolled) = call(
        &consent,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, 4)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{enrolled}");

    let raw = query(&v);
    let (status, headers, _) = call(&app, "GET", &format!("/oidc/authorize?{raw}"), None).await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(
        headers[header::LOCATION].to_str().unwrap(),
        format!("/join/approve?{raw}")
    );
    // A malformed branch request is answered here and never redirected.
    let bad = raw.replace("http%3A%2F%2F127.0.0.1", "http%3A%2F%2Fevil.example");
    assert_ne!(bad, raw);
    let (status, headers, _) = call(&app, "GET", &format!("/oidc/authorize?{bad}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(headers.get(header::LOCATION).is_none());
    // The built-in client id cannot be registered.
    assert!(crate::oidc::validate_client_id(BRANCH_CLIENT_ID).is_err());
    assert!(crate::oidc::validate_client_id(&format!(" {BRANCH_CLIENT_ID} ")).is_err());
}

#[tokio::test]
async fn branch_invites_are_never_cached_and_settings_need_an_admin() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, _store, member) = owner(&files).await;
    let as_member = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);
    let (status, headers, minted) = call(
        &as_member,
        "POST",
        "/api/config/node-invites",
        Some(json!({ "label": "den" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert!(minted["url"].as_str().unwrap().contains("#bcn1."));

    // Settings → Branches: the operator (or an Owner), never a member.
    let (status, _, _) = call(&as_member, "GET", "/api/config/branches", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let operator = routes(&state, UserCaller::Operator, LOOPBACK);
    let (status, _, branches) = call(&operator, "GET", "/api/config/branches", None).await;
    assert_eq!(status, StatusCode::OK, "{branches}");
    assert_eq!(branches["enrollment"], true);
    let (status, _, put) = call(
        &operator,
        "PUT",
        "/api/config/branches",
        Some(json!({ "enrollment": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let (_, _, info) = call(&operator, "GET", "/api/node/v1/info", None).await;
    assert_eq!(info["enrollment"], false);
}

/// Settings → Branches lists each branch with its owner, its sharing
/// settings as it reported them, and counts of the titles it shared; once
/// it stops sharing (or leaves), the owner removes those entries in bulk.
#[tokio::test]
async fn branches_show_their_sharing_and_the_owner_removes_what_a_branch_left() {
    use bookclerk_library::control_plane::{
        accept_catalog_batch, begin_catalog, record_node_left, CatalogAccept, CatalogBegin,
    };
    use bookclerk_node_proto::{CatalogFile, CatalogMetadata, CatalogTitle};

    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, member) = owner(&files).await;
    let app = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);
    let v = verifier('s');
    let code = approve(&app, &v).await;
    let (status, _, enrolled) = call(
        &app,
        "POST",
        "/api/node/v1/enroll/oauth",
        Some(redeem(&code, &v, 6)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{enrolled}");
    let node_id = enrolled["node_id"].as_str().unwrap().to_string();

    let operator = routes(&state, UserCaller::Operator, LOOPBACK);
    let (_, _, view) = call(&operator, "GET", "/api/config/branches", None).await;
    let branch = &view["branches"][0];
    assert_eq!(branch["node_id"], node_id.as_str());
    assert_eq!(branch["owner_name"], "Riley");
    assert_eq!(branch["joined"], true);
    assert!(branch["share_books"].is_null(), "not reported yet");

    let CatalogBegin::Opened(session) = begin_catalog(&store, &node_id, true, false).await.unwrap()
    else {
        panic!("a branch opens a catalog session");
    };
    let title = CatalogTitle {
        store: "libro".into(),
        product_id: "p1".into(),
        metadata: CatalogMetadata {
            title: "Title".into(),
            ..CatalogMetadata::default()
        },
        primary_key: "A/p1.m4b".into(),
        pdf_key: None,
        files: vec![CatalogFile {
            key: "A/p1.m4b".into(),
            size: 1,
            content_type: None,
        }],
    };
    assert!(matches!(
        accept_catalog_batch(&store, &node_id, session.fence, "c", true, &[title])
            .await
            .unwrap(),
        CatalogAccept::Accepted { upserted: 1, .. }
    ));
    let (_, _, view) = call(&operator, "GET", "/api/config/branches", None).await;
    let branch = &view["branches"][0];
    assert_eq!(branch["share_books"], true);
    assert_eq!(branch["include_branches"], false);
    assert_eq!(
        branch["titles"],
        json!({ "shared": 1, "delivered": 0, "detached": 0 })
    );

    // A member cannot remove entries; nothing is removed while shared.
    let (status, _, _) = call(
        &app,
        "POST",
        "/api/config/branches/titles/remove",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, _, removed) = call(
        &operator,
        "POST",
        "/api/config/branches/titles/remove",
        Some(json!({ "node_id": node_id })),
    )
    .await;
    assert_eq!(removed["removed"], 0);

    assert!(record_node_left(&store, &node_id).await.unwrap());
    let (_, _, view) = call(&operator, "GET", "/api/config/branches", None).await;
    let branch = &view["branches"][0];
    assert_eq!(branch["joined"], false);
    assert_eq!(
        branch["titles"],
        json!({ "shared": 0, "delivered": 0, "detached": 1 })
    );
    let (status, _, removed) = call(
        &operator,
        "POST",
        "/api/config/branches/titles/remove",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["removed"], 1);
}
