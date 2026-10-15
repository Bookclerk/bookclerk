//! The host-node socket: handshake, revocation, and transport checks.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body as HttpBody;
use axum::http::{Request, StatusCode};
use bookclerk_library::control_plane::{
    load_node_credential, mint_enrollment_link, redeem_enrollment_link, revoke_node_credential,
    EnrollmentActor, NodeMessage, RedeemRequest,
};
use bookclerk_library::{LibraryStore, UserRole};
use bookclerk_node_proto::{
    box_public_key, canonical_origin, decode_nonce, sign_auth, sign_public_key, AuthContext, Body,
    Frame, SigningKey, CLOSE_AUTH_FAILED, CLOSE_PROTOCOL, CLOSE_REVOKED, SUBPROTOCOL,
};
use futures::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower::ServiceExt as _;

/// A host-side socket.
type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Signing seed of the enrolled test host.
const SEED: [u8; 32] = [11; 32];

/// Serves `app` on a loopback port, as the daemon's listeners do.
async fn serve(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    let app = app.layer(axum::Extension(super::transport::LocalAddr(local)));
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

/// Enrolls a host for a new member with keys from [`SEED`].
async fn enroll(store: &LibraryStore) -> String {
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
    let minted = mint_enrollment_link(
        store,
        &EnrollmentActor::Operator { id: "test".into() },
        member.id,
        "",
        None,
    )
    .await
    .unwrap();
    redeem_enrollment_link(
        store,
        &RedeemRequest {
            link_id: minted.link.link_id.clone(),
            secret: minted.secret.clone(),
            sign_public_key: sign_public_key(&SEED).to_vec(),
            box_public_key: box_public_key(&[12; 32]).to_vec(),
            software_version: "1.0.0".into(),
        },
    )
    .await
    .unwrap()
    .node_id
}

/// Dials the node socket, offering `protocol` when given.
async fn dial(
    addr: SocketAddr,
    protocol: Option<&str>,
) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{addr}/api/node/v1/connect")
        .into_client_request()
        .unwrap();
    if let Some(protocol) = protocol {
        request
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(client, _)| client)
}

/// The next frame, or the close code once the owner closes.
async fn next(client: &mut Client) -> Result<Frame, Option<u16>> {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(10), client.next())
            .await
            .expect("owner answered in time");
        match message {
            Some(Ok(WsMessage::Text(text))) => return Ok(Frame::parse(text.as_str()).unwrap()),
            Some(Ok(WsMessage::Close(frame))) => {
                return Err(frame.map(|frame| u16::from(frame.code)))
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Err(None),
        }
    }
}

/// Runs the host side of the handshake with `key`; returns what the owner
/// sent after `auth`.
async fn handshake(
    client: &mut Client,
    node_id: &str,
    key: &SigningKey,
    origin: &str,
) -> Result<Frame, Option<u16>> {
    let hello = Frame::new(
        "1",
        Body::Hello {
            node_id: node_id.into(),
            incarnation: "inc-1".into(),
            software_version: "1.0.0".into(),
            resume: Vec::new(),
        },
    );
    client
        .send(WsMessage::Text(hello.to_text().into()))
        .await
        .unwrap();
    let challenge = next(client).await?;
    assert_eq!(challenge.id, "1", "the challenge answers the hello");
    let Body::Challenge { nonce, cluster_id } = challenge.body else {
        panic!("expected a challenge: {challenge:?}");
    };
    let nonce = decode_nonce(&nonce).unwrap();
    let sig = sign_auth(
        key,
        &AuthContext {
            cluster_id: &cluster_id,
            node_id,
            incarnation: "inc-1",
            nonce: &nonce,
            owner_origin: origin,
        },
    );
    let auth = Frame::new("2", Body::Auth { sig });
    client
        .send(WsMessage::Text(auth.to_text().into()))
        .await
        .unwrap();
    next(client).await
}

/// The owner origin a host signs, as the test daemon derives it.
async fn owner_origin(state: &Arc<crate::api::AppState>) -> String {
    let cfg = state.config.read().await;
    canonical_origin(&crate::origin::effective_origin_from_config(&cfg, None)).unwrap()
}

/// `(action, reason)` of the audit rows recorded for `node_id`, oldest
/// first.
async fn audit_actions(store: &LibraryStore, node_id: &str) -> Vec<(String, String)> {
    let mut rows: Vec<_> = store
        .list_security_audit_events(50)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.actor == format!("node:{node_id}"))
        .map(|event| {
            let detail: serde_json::Value =
                serde_json::from_str(event.detail_json.as_deref().unwrap()).unwrap();
            (event.action, detail["reason"].as_str().unwrap().to_string())
        })
        .collect();
    rows.reverse();
    rows
}

#[tokio::test]
async fn node_auth_applies_with_operator_auth_disabled() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::tests::test_state(&files).await;
    // The test daemon runs with operator auth off; the full router still
    // demands the node's key.
    assert!(!state.auth_snapshot().await.enabled);
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let node_id = enroll(&store).await;
    let origin = owner_origin(&state).await;

    // Without the subprotocol there is no upgrade.
    match dial(addr, None).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        other => panic!("expected 400, got {other:?}"),
    }

    // Wrong key: 4401 and an audit row.
    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    let wrong = SigningKey::from_bytes(&[99; 32]);
    assert_eq!(
        handshake(&mut client, &node_id, &wrong, &origin)
            .await
            .unwrap_err(),
        Some(CLOSE_AUTH_FAILED)
    );
    // The right key signing for another origin (a relaying server).
    let key = SigningKey::from_bytes(&SEED);
    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    assert_eq!(
        handshake(&mut client, &node_id, &key, "https://relay.example")
            .await
            .unwrap_err(),
        Some(CLOSE_AUTH_FAILED)
    );
    // An unknown node looks the same from outside.
    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    assert_eq!(
        handshake(&mut client, "no-such-node", &key, &origin)
            .await
            .unwrap_err(),
        Some(CLOSE_AUTH_FAILED)
    );
    assert_eq!(
        audit_actions(&store, &node_id).await,
        vec![
            ("node_auth_failed".to_string(), "bad_signature".to_string()),
            ("node_auth_failed".to_string(), "bad_signature".to_string()),
        ]
    );
    assert_eq!(
        audit_actions(&store, "no-such-node").await,
        vec![("node_auth_failed".to_string(), "unknown_node".to_string())]
    );
    assert!(load_node_credential(&store, &node_id)
        .await
        .unwrap()
        .unwrap()
        .last_connected_at
        .is_none());

    // The right key is welcomed and stamps the credential.
    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    let welcome = handshake(&mut client, &node_id, &key, &origin)
        .await
        .unwrap();
    assert!(
        matches!(&welcome.body, Body::Welcome { node_id: id, .. } if *id == node_id),
        "{welcome:?}"
    );
    assert!(load_node_credential(&store, &node_id)
        .await
        .unwrap()
        .unwrap()
        .last_connected_at
        .is_some());
    // Work frames are not served yet.
    client
        .send(WsMessage::Text(
            r#"{"v":1,"id":"7","type":"claim","item":"job"}"#.into(),
        ))
        .await
        .unwrap();
    let answer = next(&mut client).await.unwrap();
    assert_eq!(answer.id, "7");
    assert!(matches!(answer.body, Body::Error { ref code, .. } if code == "unsupported"));
    // Bulk data travels over HTTP: a binary frame ends the session.
    client
        .send(WsMessage::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    assert_eq!(next(&mut client).await.unwrap_err(), Some(CLOSE_PROTOCOL));
}

#[tokio::test]
async fn a_revoked_credential_closes_with_4403() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::tests::test_state(&files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let node_id = enroll(&store).await;
    let origin = owner_origin(&state).await;
    let key = SigningKey::from_bytes(&SEED);

    let mut live = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    handshake(&mut live, &node_id, &key, &origin).await.unwrap();
    revoke_node_credential(
        &store,
        &EnrollmentActor::Operator { id: "test".into() },
        &node_id,
    )
    .await
    .unwrap();
    // The node.revoked wake closes the live socket.
    crate::node_queue::wake_for(&NodeMessage::NodeRevoked {
        node_id: node_id.clone(),
    });
    assert_eq!(next(&mut live).await.unwrap_err(), Some(CLOSE_REVOKED));

    // A fresh connection with the right key is refused the same way.
    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    assert_eq!(
        handshake(&mut client, &node_id, &key, &origin)
            .await
            .unwrap_err(),
        Some(CLOSE_REVOKED)
    );
    assert_eq!(
        audit_actions(&store, &node_id).await,
        vec![("node_auth_revoked".to_string(), "revoked".to_string())]
    );
}

#[tokio::test]
async fn the_transport_check_runs_before_the_upgrade() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, _store) = super::tests::test_state(&files).await;
    let peer: SocketAddr = "192.168.1.20:5000".parse().unwrap();
    let local: SocketAddr = "0.0.0.0:8787".parse().unwrap();
    let app = axum::Router::new()
        .route(
            "/api/node/v1/connect",
            axum::routing::get(super::ws::get_connect),
        )
        .layer(axum::Extension(super::transport::LocalAddr(local)))
        .layer(axum::extract::connect_info::MockConnectInfo(peer))
        .with_state(state);
    let request = Request::builder()
        .uri("/api/node/v1/connect")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-protocol", SUBPROTOCOL)
        .body(HttpBody::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"], "transport_insecure");
}
