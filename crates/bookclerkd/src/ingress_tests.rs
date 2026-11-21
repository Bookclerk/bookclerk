//! The daemon's ingress host and the tunnel transport.

use std::net::Ipv4Addr;

use axum::extract::Request as AxumRequest;
use axum::routing::{get, post};
use bookclerk_library::control_plane::{activate_ingress, set_ingress_exposure};
use bookclerk_library::UserRole;
use tokio::io::AsyncReadExt as _;

use super::*;
use crate::auth::OperatorAuthState;
use crate::node_api::tests::test_state;

/// A public client address.
const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));

/// A request head as a relay would hand it over.
fn head(method: &str, target: &str, headers: &[(&str, &str)]) -> CheckedOriginRequest {
    CheckedOriginRequest {
        method: method.into(),
        target: target.into(),
        headers: headers
            .iter()
            .map(|(name, value)| OriginHeader {
                name: (*name).into(),
                value: (*value).into(),
            })
            .collect(),
        client: Some(CLIENT),
    }
}

/// Reads a whole body.
async fn read_all(body: Option<Pin<Box<dyn AsyncRead + Send>>>) -> String {
    let mut out = String::new();
    if let Some(mut body) = body {
        body.read_to_string(&mut out).await.unwrap();
    }
    out
}

/// A field's value in `headers`.
fn field<'a>(headers: &'a [OriginHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|field| field.name.eq_ignore_ascii_case(name))
        .map(|field| field.value.as_str())
}

#[test]
fn the_client_must_be_reported_and_public() {
    assert_eq!(tunnel_client(Some(CLIENT)).unwrap(), CLIENT);
    for refused in [
        None,
        Some("127.0.0.1".parse().unwrap()),
        Some("::1".parse().unwrap()),
        Some("::ffff:127.0.0.1".parse().unwrap()),
        Some("0.0.0.0".parse().unwrap()),
        Some("::".parse().unwrap()),
    ] {
        assert!(tunnel_client(refused).is_err(), "{refused:?}");
    }
    assert_eq!(
        tunnel_client(Some("::ffff:203.0.113.7".parse().unwrap())).unwrap(),
        CLIENT
    );
}

#[test]
fn the_host_is_the_public_origins_authority() {
    assert_eq!(
        public_authority(Some("https://lib.example.net")).as_deref(),
        Some("lib.example.net")
    );
    assert_eq!(
        public_authority(Some("https://lib.example.net:8443/")).as_deref(),
        Some("lib.example.net:8443")
    );
    assert_eq!(public_authority(Some("not a url")), None);
    assert_eq!(public_authority(None), None);
}

#[test]
fn operator_credentials_are_spotted() {
    let matches = |token: &str| token == "op-token";
    let carries = |headers: &[(&str, &str)]| {
        carries_operator_credential(&head("GET", "/", headers).headers, matches)
    };
    assert!(carries(&[("Authorization", "Bearer op-token")]));
    assert!(carries(&[("authorization", "bearer  op-token ")]));
    assert!(carries(&[(
        "Cookie",
        "theme=dark; bookclerk_operator_session=abc"
    )]));
    assert!(!carries(&[("Authorization", "Bearer library-card")]));
    assert!(!carries(&[("Authorization", "Bearer ")]));
    assert!(!carries(&[("Authorization", "Basic op-token")]));
    assert!(!carries(&[("Cookie", "bookclerk_portal_session=abc")]));
    assert!(!carries(&[("X-Note", "bookclerk_operator_session=abc")]));
}

#[test]
fn the_router_sees_the_public_host_and_the_reported_client() {
    let request = head(
        "POST",
        "/api/node/v1/info?x=1",
        &[
            ("Accept", "application/json"),
            ("Via", "1.1 edge"),
            ("X-Forwarded-Port", "443"),
            ("x-real-ip", "198.51.100.1"),
        ],
    );
    let http = tunnel_request(request, "lib.example.net", CLIENT, None).unwrap();
    assert_eq!(http.method(), Method::POST);
    assert_eq!(http.uri(), "/api/node/v1/info?x=1");
    assert_eq!(http.headers()[header::HOST], "lib.example.net");
    assert_eq!(http.headers()[header::ACCEPT], "application/json");
    for dropped in ["via", "x-forwarded-port", "x-real-ip"] {
        assert!(!http.headers().contains_key(dropped), "{dropped}");
    }
    assert_eq!(
        http.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .unwrap()
            .0,
        SocketAddr::new(CLIENT, 0)
    );
    assert_eq!(
        http.extensions().get::<ListenerKind>(),
        Some(&ListenerKind::Tunnel)
    );
    assert!(http
        .extensions()
        .get::<crate::node_api::transport::LocalAddr>()
        .is_none());
}

#[tokio::test]
async fn responses_drop_connection_fields_and_stream_the_body() {
    let response = Response::builder()
        .status(StatusCode::CREATED)
        .header("content-type", "text/plain")
        .header("connection", "close")
        .header("transfer-encoding", "chunked")
        .header(
            "content-disposition",
            HeaderValue::from_bytes("attachment; filename=\"caf\u{e9}\"".as_bytes()).unwrap(),
        )
        .body(Body::from("hello"))
        .unwrap();
    let answer = answer(&tokio::runtime::Handle::current(), response);
    assert_eq!(answer.status, 201);
    assert_eq!(field(&answer.headers, "content-type"), Some("text/plain"));
    for dropped in ["connection", "transfer-encoding", "content-disposition"] {
        assert_eq!(field(&answer.headers, dropped), None, "{dropped}");
    }
    assert_eq!(read_all(answer.body).await, "hello");

    let empty = answer_for_empty();
    assert!(empty.body.is_none());
}

/// The answer for a response with no body.
fn answer_for_empty() -> TunnelAnswer {
    let response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap();
    answer(&tokio::runtime::Handle::current(), response)
}

/// A library with an Owner, operator auth on and `instance` active.
async fn active_library(
    files: &tempfile::TempDir,
) -> (Arc<AppState>, DaemonIngress, PluginInstanceId) {
    let (state, store) = test_state(files).await;
    *state.auth.write().await = Arc::new(OperatorAuthState::new("op-token".into(), 1, true, 1, 1));
    store
        .create_user(UserRole::Owner, Some("Dana"), None)
        .await
        .unwrap();
    let instance = PluginInstanceId::mint();
    activate_ingress(&store, &ingress_actor(), &instance)
        .await
        .unwrap();
    let ingress = DaemonIngress::new(&state, tokio::runtime::Handle::current());
    (state, ingress, instance)
}

#[tokio::test]
async fn only_the_active_instance_is_admitted_once_an_owner_exists() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = test_state(&files).await;
    let ingress = DaemonIngress::new(&state, tokio::runtime::Handle::current());
    let instance = PluginInstanceId::mint();
    let id = instance.to_string();

    let err = ingress.admit(&id).await.unwrap_err();
    assert!(err.contains("not the library's active ingress"), "{err}");
    activate_ingress(&store, &ingress_actor(), &instance)
        .await
        .unwrap();
    let err = ingress.admit(&id).await.unwrap_err();
    assert!(err.contains("operator auth is off"), "{err}");
    *state.auth.write().await = Arc::new(OperatorAuthState::new("op-token".into(), 1, true, 1, 1));
    let err = ingress.admit(&id).await.unwrap_err();
    assert!(err.contains("no Owner account"), "{err}");
    store
        .create_user(UserRole::Owner, Some("Dana"), None)
        .await
        .unwrap();
    ingress.admit(&id).await.unwrap();
    assert!(ingress
        .admit(&PluginInstanceId::mint().to_string())
        .await
        .is_err());
}

#[tokio::test]
async fn the_reported_origin_serves_until_the_instance_stops() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, ingress, instance) = active_library(&files).await;
    let id = instance.to_string();
    let report = |origin: &str| IngressStarted {
        origin: origin.into(),
    };

    assert!(ingress
        .started(&id, report("http://a.example"))
        .await
        .is_err());
    ingress
        .started(&id, report("https://a.example"))
        .await
        .unwrap();
    assert_eq!(origin(&state).await.as_deref(), Some("https://a.example"));
    // A new name waits for the operator; the library keeps the old one.
    ingress
        .started(&id, report("https://b.example"))
        .await
        .unwrap();
    assert_eq!(origin(&state).await.as_deref(), Some("https://a.example"));
    let other = PluginInstanceId::mint().to_string();
    assert!(ingress
        .started(&other, report("https://c.example"))
        .await
        .is_err());

    ingress.stopped(&id).await;
    assert_eq!(origin(&state).await, None);
}

/// The public origin the daemon serves under now.
async fn origin(state: &AppState) -> Option<String> {
    state
        .config
        .read()
        .await
        .integrations
        .effective_public_origin()
        .map(str::to_owned)
}

/// A router standing in for the daemon's: an echo of what the router saw.
fn echo_router() -> axum::Router {
    async fn echo(request: AxumRequest) -> String {
        let host = request.headers()[header::HOST]
            .to_str()
            .unwrap()
            .to_string();
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip().to_string())
            .unwrap_or_default();
        let listener = request.extensions().get::<ListenerKind>().copied();
        let body = axum::body::to_bytes(request.into_body(), 1 << 20)
            .await
            .unwrap();
        format!(
            "{host} {peer} {listener:?} {}",
            String::from_utf8_lossy(&body)
        )
    }
    axum::Router::new()
        .route("/api/node/v1/echo", post(echo))
        .route("/api/users", get(|| async { "users" }))
        .route("/", get(|| async { "app" }))
}

#[tokio::test]
async fn tunnel_requests_reach_the_router_as_public_https() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, ingress, instance) = active_library(&files).await;
    let id = instance.to_string();
    let origin = ingress.origin();

    // No router yet: the library is starting.
    let err = origin
        .fetch(head("GET", "/", &[]), None)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Unavailable);
    ingress.set_router(echo_router());
    // No public origin yet.
    assert!(origin.fetch(head("GET", "/", &[]), None).await.is_err());
    ingress
        .started(
            &id,
            IngressStarted {
                origin: "https://lib.example.net".into(),
            },
        )
        .await
        .unwrap();

    let body: Pin<Box<dyn AsyncRead + Send>> = Box::pin(std::io::Cursor::new(b"ping".to_vec()));
    let reply = origin
        .fetch(
            head("POST", "/api/node/v1/echo", &[("Host", "evil.example")]),
            Some(body),
        )
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(
        read_all(reply.body).await,
        "lib.example.net 203.0.113.7 Some(Tunnel) ping"
    );

    // The branches exposure refuses the rest of the API, not the web app.
    let reply = origin
        .fetch(head("GET", "/api/users/", &[]), None)
        .await
        .unwrap();
    assert_eq!(reply.status, 403);
    assert!(read_all(reply.body).await.contains("not_exposed"));
    let reply = origin.fetch(head("GET", "/", &[]), None).await.unwrap();
    assert_eq!(read_all(reply.body).await, "app");

    // The whole-library exposure applies once the reconciler re-admits.
    let library = state.library_snapshot().await;
    set_ingress_exposure(&library, &ingress_actor(), IngressExposure::Library)
        .await
        .unwrap();
    ingress.admit(&id).await.unwrap();
    let reply = origin
        .fetch(head("GET", "/api/users", &[]), None)
        .await
        .unwrap();
    assert_eq!(read_all(reply.body).await, "users");

    // Operator paths and credentials never pass.
    let reply = origin
        .fetch(head("POST", "/api/auth/login", &[]), None)
        .await
        .unwrap();
    assert_eq!(reply.status, 403);
    assert!(read_all(reply.body).await.contains("operator_only"));
    let reply = origin
        .fetch(
            head("GET", "/api/users", &[("Authorization", "Bearer op-token")]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(reply.status, 403);

    // A loopback client is refused before anything runs.
    let mut local = head("GET", "/", &[]);
    local.client = Some("127.0.0.1".parse().unwrap());
    let err = origin.fetch(local, None).await.err().unwrap();
    assert_eq!(
        err.code,
        bookclerk_plugin_abi::PluginErrorCode::InvalidParams
    );
}

/// Sends one `POST /api/config/ingress`.
async fn change(state: &Arc<AppState>, body: IngressChange) -> Result<IngressStatus, StatusCode> {
    post_ingress(axum::extract::State(Arc::clone(state)), axum::Json(body))
        .await
        .map(|axum::Json(status)| status)
        .map_err(|response| response.status())
}

#[tokio::test]
async fn the_operator_turns_ingress_on_only_once_an_owner_exists() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = test_state(&files).await;
    *state.auth.write().await = Arc::new(OperatorAuthState::new("op-token".into(), 1, true, 1, 1));
    let instance = bookclerk_library::control_plane::create_plugin_instance(
        &store,
        &ingress_actor(),
        "relay",
        "",
    )
    .await
    .unwrap();
    let activate = |target: &str| IngressChange {
        activate: Some(target.into()),
        ..IngressChange::default()
    };

    let both = IngressChange {
        activate: Some("relay".into()),
        deactivate: true,
        ..IngressChange::default()
    };
    assert_eq!(
        change(&state, both).await.err(),
        Some(StatusCode::BAD_REQUEST)
    );
    assert_eq!(
        change(&state, activate("relay")).await.err(),
        Some(StatusCode::CONFLICT)
    );
    store
        .create_user(UserRole::Owner, Some("Dana"), None)
        .await
        .unwrap();
    assert_eq!(
        change(&state, activate("nothing")).await.err(),
        Some(StatusCode::BAD_REQUEST)
    );
    let on = change(&state, activate("relay")).await.unwrap();
    assert_eq!(on.active_instance, Some(instance.id.to_string()));
    assert_eq!(on.plugin_key.as_deref(), Some("relay"));
    assert_eq!(on.exposure, IngressExposure::Branches);
    assert_eq!(on.blocked, None);
    let by_id = change(&state, activate(instance.id.as_str()))
        .await
        .unwrap();
    assert_eq!(by_id.active_instance, on.active_instance);

    let library = change(
        &state,
        IngressChange {
            exposure: Some(IngressExposure::Library),
            ..IngressChange::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(library.exposure, IngressExposure::Library);
    let off = change(
        &state,
        IngressChange {
            deactivate: true,
            ..IngressChange::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(off.active_instance, None);
    let axum::Json(status) = get_ingress(axum::extract::State(Arc::clone(&state)))
        .await
        .unwrap();
    assert_eq!(status.active_instance, None);
    assert_eq!(status.exposure, IngressExposure::Library);
}

#[tokio::test]
async fn the_operator_accepts_a_new_name_and_the_daemon_serves_it() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, ingress, instance) = active_library(&files).await;
    let id = instance.to_string();
    let report = |origin: &str| IngressStarted {
        origin: origin.into(),
    };
    ingress
        .started(&id, report("https://a.example"))
        .await
        .unwrap();
    ingress
        .started(&id, report("https://b.example"))
        .await
        .unwrap();
    let axum::Json(pending) = get_ingress(axum::extract::State(Arc::clone(&state)))
        .await
        .unwrap();
    assert_eq!(
        pending.reported_origin.as_deref(),
        Some("https://b.example")
    );
    assert_eq!(pending.public_origin.as_deref(), Some("https://a.example"));
    assert!(!pending.operator_override);

    let accepted = change(
        &state,
        IngressChange {
            accept_reported_origin: true,
            ..IngressChange::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(accepted.origin.as_deref(), Some("https://b.example"));
    assert_eq!(accepted.reported_origin, None);
    assert_eq!(accepted.public_origin.as_deref(), Some("https://b.example"));

    // The operator's own `public_origin` wins and says so.
    state.config.write().await.integrations.public_origin = Some("https://mine.example".into());
    let unchanged = change(&state, IngressChange::default()).await.unwrap();
    assert!(unchanged.operator_override);
    assert_eq!(
        unchanged.public_origin.as_deref(),
        Some("https://mine.example")
    );
}
