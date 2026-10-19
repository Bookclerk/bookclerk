//! The node API TLS listener: ACME failure, self-signed pinning, and an
//! ACME issuance against Pebble when one is configured.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tower::ServiceExt as _;

use super::{node_api_mounted, NodeApiGate, NodeApiStatus};
use crate::auth::UserCaller;

/// `POST /api/node/v1/enroll` through the daemon's full router, from a
/// loopback peer on a loopback listener, with `gate` attached when given.
async fn enroll_status(
    state: &Arc<crate::api::AppState>,
    gate: Option<NodeApiGate>,
) -> (StatusCode, Value) {
    let local: SocketAddr = "127.0.0.1:8787".parse().unwrap();
    let mut app = crate::api::router(Arc::clone(state), None)
        .layer(axum::Extension(super::super::transport::LocalAddr(local)))
        .layer(axum::extract::connect_info::MockConnectInfo(
            "127.0.0.1:5000".parse::<SocketAddr>().unwrap(),
        ));
    if let Some(gate) = gate {
        app = app.layer(axum::Extension(gate));
    }
    let request = Request::builder()
        .method("POST")
        .uri("/api/node/v1/enroll")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "link_id": "x", "secret": "y", "sign_pk": "z", "box_pk": "w" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[test]
fn tls_modes_need_a_ready_gate() {
    use bookclerk_config::NodeApiMode;
    for mode in [NodeApiMode::Loopback, NodeApiMode::Proxy] {
        assert!(node_api_mounted(mode, None));
        let started = NodeApiGate::fixed(mode, NodeApiStatus::Ready);
        assert!(node_api_mounted(mode, Some(&started)));
    }
    for mode in [NodeApiMode::Acme, NodeApiMode::SelfSigned] {
        let ready = NodeApiGate::fixed(mode, NodeApiStatus::Ready);
        let failed = NodeApiGate::fixed(mode, NodeApiStatus::Failed);
        assert!(!node_api_mounted(mode, None), "no gate fails closed");
        assert!(!node_api_mounted(mode, Some(&failed)));
        assert!(node_api_mounted(mode, Some(&ready)));
    }
}

#[test]
fn a_reloaded_node_api_mode_stays_unmounted_until_restart() {
    use bookclerk_config::NodeApiMode;
    let modes = [
        NodeApiMode::Loopback,
        NodeApiMode::Proxy,
        NodeApiMode::Acme,
        NodeApiMode::SelfSigned,
    ];
    for started in modes {
        let gate = NodeApiGate::fixed(started, NodeApiStatus::Ready);
        for live in modes.into_iter().filter(|live| *live != started) {
            assert!(
                !node_api_mounted(live, Some(&gate)),
                "started {started:?}, reloaded to {live:?}"
            );
        }
    }
}

#[tokio::test]
async fn acme_failure_keeps_the_node_api_unmounted() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    // Nothing listens on the discard port, so every ACME order fails.
    let (state, _store) = super::super::tests::test_state_with(
        &files,
        r#"
[integrations]
public_origin = "https://owner.example"

[daemon.node_api]
mode = "acme"
acme_contact = "mailto:ops@example.com"
tls_listen = "127.0.0.1:0"
acme_directory = "https://127.0.0.1:9/directory"
"#,
    )
    .await;
    let cfg = state.config.read().await.clone();
    let started = super::start(&state, &cfg, files.path()).await;
    let mut gate = started.gate.clone();
    let settled = tokio::time::timeout(Duration::from_secs(30), gate.settled())
        .await
        .expect("ACME reports a failure");
    assert_eq!(settled, NodeApiStatus::Failed);
    let local = started.local.expect("the listener is bound for challenges");

    // Every listener refuses the node API, loopback included.
    for gate in [Some(gate), None] {
        let (status, body) = enroll_status(&state, gate).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], "node_api_unmounted");
    }
    // The TLS listener has no certificate to offer.
    let config = ClientConfig::builder_with_provider(super::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(local).await.unwrap();
    let handshake = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("owner.example").unwrap(), tcp)
        .await;
    assert!(handshake.is_err());

    // The ACME cache is private to the daemon's user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(files.path().join(bookclerk_config::NODE_API_ACME_DIR))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

/// Sends a bare HTTP/1.1 request over `tls` and answers the status code.
async fn http_status<S>(tls: &mut S, path: &str) -> u16
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), tls.read_to_end(&mut response)).await;
    let text = String::from_utf8_lossy(&response);
    text.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"))
}

#[tokio::test]
async fn a_stalled_server_bounds_finished_handshakes() {
    use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio_rustls::rustls::ServerConfig;

    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = generated.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
    let mut server = ServerConfig::builder_with_provider(super::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], PrivateKeyDer::Pkcs8(key))
        .unwrap();
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let client = ClientConfig::builder_with_provider(super::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));

    // One slot in flight, one in the queue, and a server that never accepts.
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = tcp.local_addr().unwrap();
    let (handshakes, mut accepted) = tokio::sync::mpsc::channel(1);
    tokio::spawn(super::accept_loop(
        tcp,
        Arc::new(server),
        None,
        handshakes,
        1,
    ));
    let handshake = || async {
        let tcp = tokio::net::TcpStream::connect(local).await.unwrap();
        connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
    };
    // Retries while the previous connection still holds the slot.
    let handshake_eventually = || async {
        for _ in 0..100 {
            if let Ok(tls) = handshake().await {
                return tls;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("handshake never succeeded");
    };

    let _queued = handshake_eventually().await;
    let _waiting = handshake_eventually().await;
    // The waiting connection keeps its slot: the next one is shed, not
    // handshaken into an unbounded backlog.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(handshake().await.is_err(), "a third connection was let in");

    // Once the server takes one, the waiter moves up and the slot frees.
    let _ = accepted.recv().await.unwrap();
    let _after = handshake_eventually().await;
}

#[tokio::test]
async fn self_signed_serves_the_node_api_and_pins_links() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::super::tests::test_state_with(
        &files,
        "[daemon.node_api]\nmode = \"self_signed\"\ntls_listen = \"127.0.0.1:0\"\n",
    )
    .await;
    let cfg = state.config.read().await.clone();
    let started = super::start(&state, &cfg, files.path()).await;
    assert_eq!(started.gate.status(), NodeApiStatus::Ready);
    let local = started.local.unwrap();
    let pin = bookclerk_config::read_node_api_spki_pin(files.path()).expect("pin written");

    // A client that trusts exactly the pinned certificate reaches the node
    // API, which sees the self-signed listener (and wants the subprotocol).
    let dir = files
        .path()
        .join(bookclerk_config::NODE_API_SELF_SIGNED_DIR);
    let cert = CertificateDer::from(std::fs::read(dir.join("cert.der")).unwrap());
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let config = ClientConfig::builder_with_provider(super::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(local).await.unwrap();
    let mut tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    assert_eq!(http_status(&mut tls, "/api/node/v1/connect").await, 400);
    // Only node API routes are served there.
    let tcp = tokio::net::TcpStream::connect(local).await.unwrap();
    let mut tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    assert_eq!(http_status(&mut tls, "/api/status").await, 404);

    // The key and certificate are private and survive a restart.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let key_mode = std::fs::metadata(dir.join("key.der"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(key_mode & 0o777, 0o600);
    }
    let again = super::load_or_create_self_signed(&cfg, files.path(), local).unwrap();
    assert_eq!(again.pin, pin);

    // Minted links carry the pin as the token's fourth part.
    let user = store
        .create_user(bookclerk_library::UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
    let invites = super::super::tests::invite_routes(&state, UserCaller::User { user_id: user.id });
    let (status, minted) =
        super::super::tests::send(&invites, "POST", "/api/config/node-invites", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let url = minted["url"].as_str().unwrap();
    assert!(url.ends_with(&format!(".{pin}")), "{url}");
    let token = bookclerk_library::control_plane::parse_enrollment_token(url).unwrap();
    assert_eq!(token.spki_pin.as_deref(), Some(pin.as_str()));
}

/// Issues a certificate from a Pebble ACME server when
/// `BOOKCLERK_TEST_ACME_DIRECTORY` (its directory URL) and
/// `BOOKCLERK_TEST_ACME_CA` (the PEM root of its HTTPS listener) are set.
/// Run Pebble with `PEBBLE_VA_ALWAYS_VALID=1`: this test checks the
/// account, order, and cache path, not challenge reachability.
#[tokio::test]
async fn acme_issues_from_pebble_when_configured() {
    let (Ok(directory), Ok(ca)) = (
        std::env::var("BOOKCLERK_TEST_ACME_DIRECTORY"),
        std::env::var("BOOKCLERK_TEST_ACME_CA"),
    ) else {
        eprintln!("skipping: BOOKCLERK_TEST_ACME_DIRECTORY / BOOKCLERK_TEST_ACME_CA unset");
        return;
    };
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, _store) = super::super::tests::test_state_with(
        &files,
        &format!(
            r#"
[integrations]
public_origin = "https://owner.bookclerk.test"

[daemon.node_api]
mode = "acme"
acme_contact = "mailto:ops@bookclerk.test"
tls_listen = "127.0.0.1:0"
acme_directory = "{directory}"
acme_directory_ca = "{ca}"
"#
        ),
    )
    .await;
    let cfg = state.config.read().await.clone();
    let started = super::start(&state, &cfg, files.path()).await;
    let mut gate = started.gate.clone();
    // Pebble's order may fail once before succeeding; wait for Ready.
    tokio::time::timeout(
        Duration::from_secs(120),
        gate.status
            .wait_for(|status| *status == NodeApiStatus::Ready),
    )
    .await
    .expect("Pebble issued a certificate")
    .unwrap();
    let (status, body) = enroll_status(&state, Some(gate)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "mounted: {body}");
    // The cache write follows the deploy.
    let cache_dir = files.path().join(bookclerk_config::NODE_API_ACME_DIR);
    let mut cached = false;
    for _ in 0..100 {
        cached = std::fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("cached_cert_")
            });
        if cached {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(cached, "the certificate is cached for restarts");
}

#[tokio::test]
async fn the_node_router_times_out_a_stalled_enroll_body() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, _store) = super::super::tests::test_state_with(&files, "").await;
    // The routes exactly as the TLS-only listener serves them.
    let app = super::super::node_router::<()>(Arc::clone(&state))
        .layer(axum::Extension(super::super::transport::LocalAddr(
            "127.0.0.1:8787".parse().unwrap(),
        )))
        .layer(axum::extract::connect_info::MockConnectInfo(
            "127.0.0.1:5000".parse::<SocketAddr>().unwrap(),
        ));
    // A body that never finishes.
    let body = Body::from_stream(futures::stream::pending::<
        Result<axum::body::Bytes, std::io::Error>,
    >());
    let request = Request::builder()
        .method("POST")
        .uri("/api/node/v1/enroll")
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(30), app.oneshot(request))
        .await
        .expect("the API deadline ends the request")
        .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
}
