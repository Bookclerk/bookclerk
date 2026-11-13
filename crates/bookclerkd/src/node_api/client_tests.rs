//! Client sign-in, library side: the consent API, the `/oidc/authorize`
//! hand-off, the token redeem and its refusals, and where the session
//! token is accepted once issued.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::{get, post};
use bookclerk_library::{hash_token, PortalSessionKind, UserRole};
use bookclerk_node_proto::{
    pkce_challenge, BRANCH_CLIENT_ID, CLIENT_CLIENT_ID, CLIENT_SCOPE, CLIENT_TOKEN_LEN,
};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;

use super::super::branch::tests::{call, LOOPBACK};
use crate::api::AppState;
use crate::auth::UserCaller;

/// Where the client install wants the code.
const REDIRECT: &str = "http://127.0.0.1:41923/client/callback";

/// A LAN peer over plain HTTP.
const LAN: (&str, &str) = ("192.168.1.20:5000", "0.0.0.0:8787");

/// The client sign-in routes as the daemon mounts them, for `caller` seen
/// from `peer` on a listener bound to `local`.
fn routes(state: &Arc<AppState>, caller: UserCaller, (peer, local): (&str, &str)) -> axum::Router {
    let peer: std::net::SocketAddr = peer.parse().unwrap();
    let local: std::net::SocketAddr = local.parse().unwrap();
    axum::Router::new()
        .route("/api/client/v1/token", post(super::post_client_token))
        .route(
            "/api/auth/client-consent",
            get(super::get_client_consent).post(super::post_client_consent),
        )
        .layer(axum::Extension(caller))
        .layer(axum::Extension(super::super::transport::LocalAddr(local)))
        .layer(axum::extract::connect_info::MockConnectInfo(peer))
        .with_state(Arc::clone(state))
}

/// A PKCE verifier (43+ unreserved characters).
fn verifier(tag: char) -> String {
    std::iter::repeat_n(tag, 48).collect()
}

/// The authorization request for `verifier`, as the consent page relays it.
fn params(verifier: &str) -> Value {
    json!({
        "client_id": CLIENT_CLIENT_ID,
        "redirect_uri": REDIRECT,
        "response_type": "code",
        "scope": CLIENT_SCOPE,
        "state": "state-1",
        "code_challenge": pkce_challenge(verifier),
        "code_challenge_method": "S256",
        "client_name": "Riley's laptop",
    })
}

/// A request as a query string.
fn query(params: &Value) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in params.as_object().unwrap() {
        out.append_pair(name, value.as_str().unwrap());
    }
    out.finish()
}

/// Approves the request for `verifier` as `app`'s user; answers the code.
async fn approve(app: &axum::Router, verifier: &str) -> String {
    let mut body = params(verifier);
    body["approve"] = json!(true);
    let (status, headers, answer) = call(app, "POST", "/api/auth/client-consent", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let redirect = url::Url::parse(answer["redirect"].as_str().unwrap()).unwrap();
    assert!(redirect.as_str().starts_with(REDIRECT), "{redirect}");
    let pairs: std::collections::HashMap<_, _> = redirect.query_pairs().into_owned().collect();
    assert_eq!(pairs["state"], "state-1");
    pairs["code"].clone()
}

/// The token body for `code`.
fn redeem(code: &str, verifier: &str) -> Value {
    json!({ "code": code, "code_verifier": verifier, "redirect_uri": REDIRECT })
}

/// A library with one active member.
async fn library(
    files: &tempfile::TempDir,
) -> (Arc<AppState>, bookclerk_library::LibraryStore, i64) {
    let (state, store) = super::super::tests::test_state(files).await;
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
    (state, store, member.id)
}

#[tokio::test]
async fn a_consented_code_redeems_once_for_a_labelled_client_session() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, member) = library(&files).await;
    let app = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);

    let (status, _, page) = call(
        &app,
        "GET",
        &format!(
            "/api/auth/client-consent?{}",
            query(&params(&verifier('a')))
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["client_name"], "Riley's laptop");
    assert_eq!(page["library_name"], "localhost");
    assert_eq!(page["redirect_origin"], "http://127.0.0.1:41923");
    assert_eq!(page["user"]["id"], member);

    let verifier = verifier('b');
    let code = approve(&app, &verifier).await;
    let (status, _, answer) = call(
        &app,
        "POST",
        "/api/client/v1/token",
        Some(redeem(&code, &verifier)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let token = answer["token"].as_str().unwrap();
    assert_eq!(token.len(), CLIENT_TOKEN_LEN);
    assert_eq!(answer["user"]["id"], member);
    assert_eq!(answer["user"]["display_name"], "Riley");
    assert_eq!(answer["idle_secs"], 90 * 24 * 3600);
    let cluster = bookclerk_library::control_plane::load_cluster_row(&store)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer["cluster_id"], cluster.cluster_id);

    // The session is a client session, labelled with the consented name,
    // and resolves only as one.
    let identity = store
        .get_client_session_identity(&hash_token(token), chrono::Duration::days(90))
        .await
        .unwrap()
        .expect("a client session");
    assert_eq!(identity.user_id, Some(member));
    assert!(store
        .get_portal_session_identity(&hash_token(token))
        .await
        .unwrap()
        .is_none());
    let rows = store
        .list_portal_session_records_for_identity(identity.id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, PortalSessionKind::Client);
    assert_eq!(rows[0].client_label.as_deref(), Some("Riley's laptop"));

    // A code redeems once.
    let (status, _, refused) = call(
        &app,
        "POST",
        "/api/client/v1/token",
        Some(redeem(&code, &verifier)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "sign_in_refused");
}

#[tokio::test]
async fn a_mismatched_exchange_spends_the_code() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, _store, member) = library(&files).await;
    let app = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);

    let good = verifier('c');
    let code = approve(&app, &good).await;
    let (status, _, refused) = call(
        &app,
        "POST",
        "/api/client/v1/token",
        Some(redeem(&code, &verifier('d'))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "sign_in_refused");
    let (status, _, _) = call(
        &app,
        "POST",
        "/api/client/v1/token",
        Some(redeem(&code, &good)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a refused exchange spends the code"
    );

    let code = approve(&app, &good).await;
    let mut wrong_redirect = redeem(&code, &good);
    wrong_redirect["redirect_uri"] = json!("http://127.0.0.1:41923/elsewhere");
    let (status, _, _) = call(&app, "POST", "/api/client/v1/token", Some(wrong_redirect)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, _) = call(
        &app,
        "POST",
        "/api/client/v1/token",
        Some(redeem("", &good)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, refused) = call(
        &app,
        "POST",
        "/api/client/v1/token",
        Some(json!({"code": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

#[tokio::test]
async fn another_clients_code_never_redeems_here() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, member) = library(&files).await;
    let app = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);

    let verifier = verifier('e');
    let code = "branch-code-that-is-long-enough-to-look-real";
    store
        .insert_oidc_auth_code(
            &hash_token(code),
            BRANCH_CLIENT_ID,
            member,
            REDIRECT,
            &pkce_challenge(&verifier),
            "S256",
            CLIENT_SCOPE,
            chrono::Utc::now() + chrono::Duration::seconds(60),
        )
        .await
        .unwrap();
    let (status, _, refused) = call(
        &app,
        "POST",
        "/api/client/v1/token",
        Some(redeem(code, &verifier)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "sign_in_refused");
}

#[tokio::test]
async fn consent_refuses_malformed_requests_and_plain_http_from_afar() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, _store, member) = library(&files).await;
    let app = routes(&state, UserCaller::User { user_id: member }, LOOPBACK);

    let good = params(&verifier('f'));
    for (field, value, detail) in [
        ("client_id", "bookclerk-branch", "unknown client"),
        ("scope", "node:enroll", "scope must be library:member"),
        (
            "code_challenge_method",
            "plain",
            "an S256 code challenge is required",
        ),
        (
            "code_challenge",
            "short",
            "an S256 code challenge is required",
        ),
        (
            "redirect_uri",
            "http://192.168.1.5/client/callback",
            "the redirect URI must be a loopback or https URL",
        ),
        (
            "client_name",
            "",
            "the client needs a name of 1 to 64 characters",
        ),
    ] {
        let mut bad = good.clone();
        bad[field] = json!(value);
        let (status, _, refused) = call(
            &app,
            "GET",
            &format!("/api/auth/client-consent?{}", query(&bad)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {refused}");
        assert_eq!(refused["detail"], detail, "{field}");
    }

    // Denying never issues a code.
    let mut deny = good.clone();
    deny["approve"] = json!(false);
    let (status, _, answer) = call(&app, "POST", "/api/auth/client-consent", Some(deny)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let redirect = answer["redirect"].as_str().unwrap();
    assert!(redirect.contains("error=access_denied"), "{redirect}");
    assert!(!redirect.contains("code="), "{redirect}");

    // Approving from another machine over plain HTTP is refused.
    let lan = routes(&state, UserCaller::User { user_id: member }, LAN);
    let mut body = good;
    body["approve"] = json!(true);
    let (status, _, refused) = call(&lan, "POST", "/api/auth/client-consent", Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "transport_insecure");

    // The operator alone is not a user to sign in as.
    let operator = routes(&state, UserCaller::Operator, LOOPBACK);
    let (status, _, refused) = call(
        &operator,
        "GET",
        &format!(
            "/api/auth/client-consent?{}",
            query(&params(&verifier('g')))
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "user_required");
}

#[tokio::test]
async fn authorize_hands_a_client_request_to_its_consent_page() {
    let (_state, app, _store) = crate::auth::tests::phase2_harness("operator-token").await;
    let q = query(&params(&verifier('h')));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/oidc/authorize?{q}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("/client/approve?{q}").as_str()
    );

    let mut bad = params(&verifier('h'));
    bad["scope"] = json!("openid");
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/oidc/authorize?{}", query(&bad)))
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_token_endpoint_never_redeems_a_client_code() {
    let (_state, app, _store) = crate::auth::tests::phase2_harness("operator-token").await;
    let form = format!(
        "grant_type=authorization_code&code=abc&redirect_uri={}&client_id={CLIENT_CLIENT_ID}&code_verifier={}",
        url::form_urlencoded::byte_serialize(REDIRECT.as_bytes()).collect::<String>(),
        verifier('i'),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oidc/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::ACCEPT, "application/json")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// The full router as a listener bound to `local` serves it, seen from
/// `peer`.
fn served(app: axum::Router, (peer, local): (&str, &str)) -> axum::Router {
    let peer: std::net::SocketAddr = peer.parse().unwrap();
    let local: std::net::SocketAddr = local.parse().unwrap();
    app.layer(axum::Extension(super::super::transport::LocalAddr(local)))
        .layer(axum::extract::connect_info::MockConnectInfo(peer))
}

/// Sends `method uri` with one extra header; answers status and body.
async fn with_header(
    app: &axum::Router,
    method: &str,
    uri: &str,
    header: (header::HeaderName, String),
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header.0, header.1)
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A client session for the harness's member; answers its token.
async fn client_token(store: &bookclerk_library::LibraryStore) -> String {
    let identity = store
        .get_portal_identity("test", "member-ext")
        .await
        .unwrap()
        .expect("member identity");
    let token = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx".to_owned();
    store
        .insert_client_session(
            &hash_token(&token),
            identity.id,
            chrono::Utc::now() + chrono::Duration::days(1),
            "Riley's laptop",
        )
        .await
        .unwrap();
    token
}

#[tokio::test]
async fn a_client_token_opens_the_library_routes_and_nothing_else() {
    let (_state, app, store) = crate::auth::tests::phase2_harness("operator-token").await;
    let token = client_token(&store).await;
    let app = served(app, LOOPBACK);
    let card = (header::AUTHORIZATION, format!("Bookclerk-Client {token}"));

    let (status, body) = with_header(&app, "GET", "/api/library/books", card.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = with_header(&app, "GET", "/api/auth/sessions", card.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sessions = body["sessions"].as_array().unwrap();
    let current: Vec<_> = sessions
        .iter()
        .filter(|s| s["is_current"] == true)
        .collect();
    assert_eq!(current.len(), 1, "{body}");
    assert_eq!(current[0]["kind"], "portal");
    assert_eq!(current[0]["channel"], "client");
    assert_eq!(current[0]["client_label"], "Riley's laptop");

    // Administration and routes that check a session themselves never see
    // the token.
    for (method, uri) in [
        ("GET", "/api/config/branches"),
        ("GET", "/api/config/node-invites"),
        ("GET", "/api/users"),
        ("GET", "/api/jobs"),
        ("GET", "/api/config/plugins"),
    ] {
        let (status, body) = with_header(&app, method, uri, card.clone()).await;
        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
            "{method} {uri}: {status} {body}"
        );
    }
    let (status, body) = with_header(&app, "GET", "/api/auth/me", card.clone()).await;
    assert_ne!(body["authenticated"], true, "{status} {body}");

    // The token is never a cookie, and a browser session is never a client
    // token.
    let cookie = (
        header::COOKIE,
        format!("{}={token}", crate::auth::PORTAL_SESSION_COOKIE),
    );
    let (status, _) = with_header(&app, "GET", "/api/library/books", cookie).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let browser = crate::auth::tests::portal_cookie_for(&store, "test", "member-ext").await;
    let browser_raw = browser.split_once('=').unwrap().1.to_owned();
    let (status, _) = with_header(
        &app,
        "GET",
        "/api/library/books",
        (
            header::AUTHORIZATION,
            format!("Bookclerk-Client {browser_raw}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Plain HTTP from another machine is refused before the session is read.
    let lan = served(
        crate::auth::tests::phase2_harness("operator-token").await.1,
        LAN,
    );
    let (status, body) = with_header(&lan, "GET", "/api/library/books", card.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "transport_insecure");
    // In the standard error body the library routes declare.
    assert_eq!(body["status"], 403);
    assert!(body["message"].is_string(), "{body}");

    // Signing out ends the session.
    let (status, _) = with_header(&app, "POST", "/api/auth/logout", card.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = with_header(&app, "GET", "/api/library/books", card).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[test]
fn the_built_in_client_id_cannot_be_registered() {
    assert!(crate::oidc::validate_client_id(CLIENT_CLIENT_ID).is_err());
    assert!(crate::oidc::validate_client_id(&format!(" {CLIENT_CLIENT_ID} ")).is_err());
    assert!(crate::oidc::validate_client_id("bookclerk-clients").is_ok());
}
