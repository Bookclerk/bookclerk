//! The host-node socket: handshake, revocation, transport checks, work
//! frames, sealed secrets, resume, and wake forwarding.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body as HttpBody;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use bookclerk_library::control_plane::{
    load_node_credential, mint_enrollment_link, node_queue_name, publish_node_message,
    redeem_enrollment_link, revoke_node_credential, EnrollmentActor, NodeMessage, RedeemRequest,
};
use bookclerk_library::secrets::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret, upsert_secret,
};
use bookclerk_library::{
    EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, LibraryStore, UserRole,
};
use bookclerk_node_proto::{
    box_public_key, canonical_origin, decode_nonce, open_secrets, seal_secret_update, sign_auth,
    sign_public_key, AuthContext, Body, Frame, HeldFence, SecretRef, SigningKey, CLOSE_AUTH_FAILED,
    CLOSE_PROTOCOL, CLOSE_REVOKED, CLOSE_SUPERSEDED, ITEM_JOB, SUBPROTOCOL,
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

/// Box secret of the enrolled test host.
const BOX_SECRET: [u8; 32] = [12; 32];

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
    enroll_member(store).await.0
}

/// [`enroll`], also returning the member's user id.
async fn enroll_member(store: &LibraryStore) -> (String, i64) {
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
    let redeemed = redeem_enrollment_link(
        store,
        &RedeemRequest {
            link_id: minted.link.link_id.clone(),
            secret: minted.secret.clone(),
            sign_public_key: sign_public_key(&SEED).to_vec(),
            box_public_key: box_public_key(&BOX_SECRET).to_vec(),
            software_version: "1.0.0".into(),
        },
    )
    .await
    .unwrap();
    (redeemed.node_id, member.id)
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
    handshake_resuming(client, node_id, key, origin, Vec::new()).await
}

/// [`handshake`] with a hello that lists the fences the host still holds.
async fn handshake_resuming(
    client: &mut Client,
    node_id: &str,
    key: &SigningKey,
    origin: &str,
    resume: Vec<HeldFence>,
) -> Result<Frame, Option<u16>> {
    let hello = Frame::new(
        "1",
        Body::Hello {
            node_id: node_id.into(),
            incarnation: "inc-1".into(),
            software_version: "1.0.0".into(),
            resume,
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
    // Nothing to claim: `none`. Frame types not served yet: `unsupported`.
    client
        .send(WsMessage::Text(
            r#"{"v":1,"id":"7","type":"claim","item":"job"}"#.into(),
        ))
        .await
        .unwrap();
    let answer = next(&mut client).await.unwrap();
    assert_eq!(answer.id, "7");
    assert!(matches!(answer.body, Body::NoWork { .. }), "{answer:?}");
    client
        .send(WsMessage::Text(
            r#"{"v":1,"id":"8","type":"kv.get","key":"k"}"#.into(),
        ))
        .await
        .unwrap();
    let answer = next(&mut client).await.unwrap();
    assert_eq!(answer.id, "8");
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

/// Plugin key of the test store account.
const PLUGIN: &str = "path:file:///tmp/store#store";

/// Log output captured in memory (tracing events and bridged `log`
/// records, the way the daemon's subscriber sees them).
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    /// Everything logged so far.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Sends `frame` and returns the owner's answer.
async fn request(client: &mut Client, frame: Frame) -> Frame {
    client
        .send(WsMessage::Text(frame.to_text().into()))
        .await
        .unwrap();
    next(client).await.unwrap()
}

/// The stored value of `secret`, unsealed.
async fn stored(store: &LibraryStore, secret: &SecretRef) -> String {
    let record = get_secret(
        store.db(),
        &secret.kind,
        Some(&secret.provider),
        secret_account_type::INTEGRATION,
        Some(&secret.account_id),
        &secret.name,
    )
    .await
    .unwrap()
    .unwrap();
    String::from_utf8(unseal_secret(&record).unwrap()).unwrap()
}

/// The owner box key a `welcome` carries.
fn welcome_box_key(welcome: &Frame) -> Vec<u8> {
    let Body::Welcome { box_key, .. } = &welcome.body else {
        panic!("expected welcome: {welcome:?}");
    };
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(box_key)
        .unwrap()
}

/// A `secret.update` for `secret` sealed to `owner_box_key`.
fn secret_update(
    owner_box_key: &[u8],
    node_id: &str,
    job_id: &str,
    fence: i64,
    secret: &SecretRef,
    value: &[u8],
) -> Frame {
    Frame::new(
        "u",
        Body::SecretUpdate {
            job_id: job_id.into(),
            fence,
            secret_ref: secret.clone(),
            sealed_value: seal_secret_update(
                owner_box_key,
                &[21; 32],
                node_id,
                job_id,
                fence,
                secret,
                value,
            )
            .unwrap(),
        },
    )
}

/// A member's host claims its owner's job and gets the token sealed to its
/// box key; it writes a refreshed token back sealed to the owner; a fence
/// it still holds survives a reconnect, one reclaimed while it was away is
/// lost; and no token ever reaches the logs, even at TRACE with the
/// WebSocket library's frame tracing bridged in.
#[tokio::test]
async fn work_frames_seal_secrets_and_resume_refuses_lost_fences() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let _ = tracing_log::LogTracer::init();

    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::tests::test_state(&files).await;
    state.config.write().await.jobs.lease_seconds = 1;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let (node_id, member) = enroll_member(&store).await;
    let origin = owner_origin(&state).await;
    let key = SigningKey::from_bytes(&SEED);

    store
        .upsert_account(PLUGIN, "riley-acct", "us", None, true, "test")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(member, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, "riley-acct", "test")
        .await
        .unwrap();
    let token = SecretRef {
        kind: secret_kind::SOURCE_AUTH.into(),
        provider: PLUGIN.into(),
        account_id: "riley-acct".into(),
        name: "riley-acct.auth".into(),
    };
    let record = build_sealed_record(
        b"riley-token-v1",
        &token.kind,
        &token.provider,
        secret_account_type::INTEGRATION,
        &token.account_id,
        &token.name,
    )
    .unwrap();
    upsert_secret(store.db(), &record).await.unwrap();
    let job_id = match store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Scan,
            payload: JobPayload {
                account: Some("riley-acct".into()),
                ..Default::default()
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 256,
            run_after: None,
        })
        .await
        .unwrap()
    {
        EnqueueOutcome::Created { id } => id,
        other => panic!("expected a new job: {other:?}"),
    };

    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    let welcome = handshake(&mut client, &node_id, &key, &origin)
        .await
        .unwrap();
    let owner_box_key = welcome_box_key(&welcome);

    // Claim: the job, its fence, and the token sealed to the host's key.
    let work = request(
        &mut client,
        Frame::new(
            "c1",
            Body::Claim {
                item: ITEM_JOB.into(),
                class: None,
                filter: None,
            },
        ),
    )
    .await;
    let Body::Work {
        job,
        fence,
        sealed_secrets,
        ..
    } = work.body
    else {
        panic!("expected work: {work:?}");
    };
    assert_eq!(job.id, job_id);
    let sealed_secrets = sealed_secrets.expect("the job's token is handed");
    let secrets = open_secrets(&BOX_SECRET, &node_id, &job_id, fence, &sealed_secrets).unwrap();
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0].secret_ref, token);
    assert_eq!(secrets[0].value.expose(), b"riley-token-v1");
    assert!(
        open_secrets(&[13; 32], &node_id, &job_id, fence, &sealed_secrets).is_err(),
        "only the enrolled box key opens it"
    );

    // Write-back under the live fence is stored; under another fence it is
    // refused.
    let answer = request(
        &mut client,
        secret_update(
            &owner_box_key,
            &node_id,
            &job_id,
            fence,
            &token,
            b"riley-token-v2",
        ),
    )
    .await;
    assert!(matches!(answer.body, Body::Accepted {}), "{answer:?}");
    assert_eq!(stored(&store, &token).await, "riley-token-v2");
    let answer = request(
        &mut client,
        secret_update(
            &owner_box_key,
            &node_id,
            &job_id,
            fence + 1,
            &token,
            b"riley-token-v3",
        ),
    )
    .await;
    assert!(
        matches!(&answer.body, Body::Error { code, .. } if code == "refused"),
        "{answer:?}"
    );
    assert_eq!(stored(&store, &token).await, "riley-token-v2");

    let renewed = request(
        &mut client,
        Frame::new(
            "r1",
            Body::Renew {
                item: ITEM_JOB.into(),
                job_id: job_id.clone(),
                fence,
                progress: Some("halfway".into()),
            },
        ),
    )
    .await;
    assert!(
        matches!(renewed.body, Body::Renewed { cancel: false, .. }),
        "{renewed:?}"
    );
    let beat = request(
        &mut client,
        Frame::new(
            "h1",
            Body::Heartbeat {
                started_at: "2026-10-15T00:00:00Z".into(),
                interval_secs: 15,
                cpu_millicores: 2000,
                memory_bytes: Some(1 << 30),
                cpu_used_millicores: None,
                memory_available_bytes: None,
                files_dir_free_bytes: None,
            },
        ),
    )
    .await;
    assert!(matches!(beat.body, Body::Accepted {}), "{beat:?}");

    // Reconnect while the fence is still the job's: it is renewed.
    drop(client);
    let held = HeldFence {
        kind: ITEM_JOB.into(),
        id: job_id.clone(),
        fence,
    };
    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    handshake_resuming(&mut client, &node_id, &key, &origin, vec![held.clone()])
        .await
        .unwrap();
    let resumed = next(&mut client).await.unwrap();
    assert_eq!(
        resumed.body,
        Body::Resumed {
            renewed: vec![held.clone()],
            lost: Vec::new(),
            unchecked: Vec::new(),
            lease_secs: Some(1),
        }
    );

    // Away past the lease: the job is reclaimed, and the fence is lost.
    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(store.reclaim_expired_leases().await.unwrap(), 1);
    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    let welcome = handshake_resuming(&mut client, &node_id, &key, &origin, vec![held.clone()])
        .await
        .unwrap();
    let owner_box_key = welcome_box_key(&welcome);
    let resumed = next(&mut client).await.unwrap();
    assert_eq!(
        resumed.body,
        Body::Resumed {
            renewed: Vec::new(),
            lost: vec![held.clone()],
            unchecked: Vec::new(),
            lease_secs: None,
        }
    );
    let complete = request(
        &mut client,
        Frame::new(
            "f1",
            Body::Complete {
                job_id: job_id.clone(),
                fence,
                detail: None,
            },
        ),
    )
    .await;
    assert!(
        matches!(complete.body, Body::Lost { fence: lost, .. } if lost == fence),
        "{complete:?}"
    );
    // Nor can the lost fence write a token back.
    let refused = request(
        &mut client,
        secret_update(
            &owner_box_key,
            &node_id,
            &job_id,
            fence,
            &token,
            b"riley-token-v4",
        ),
    )
    .await;
    assert!(
        matches!(&refused.body, Body::Error { code, message } if code == "refused" && message == "fence_not_live"),
        "{refused:?}"
    );
    assert_eq!(stored(&store, &token).await, "riley-token-v2");
    drop(client);

    let logs = logs.text();
    assert!(
        logs.contains("job handed to host node"),
        "the capture sees the session's logs"
    );
    assert!(
        logs.contains("Received message"),
        "the capture sees the WebSocket library's frame tracing"
    );
    for value in [
        "riley-token-v1",
        "riley-token-v2",
        "riley-token-v3",
        "riley-token-v4",
    ] {
        assert!(!logs.contains(value), "{value} reached the logs");
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
        assert!(
            !logs.contains(&encoded),
            "{value} reached the logs as base64"
        );
    }
}

/// While a host's socket is live, its `node.<id>` queue is forwarded as
/// `wake` frames and each is acked only once the host answers.
#[tokio::test]
async fn the_hosts_node_queue_is_forwarded_and_acked_by_the_host() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::tests::test_state(&files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let node_id = enroll(&store).await;
    let origin = owner_origin(&state).await;
    let key = SigningKey::from_bytes(&SEED);
    let mut client = dial(addr, Some(SUBPROTOCOL)).await.unwrap();
    handshake(&mut client, &node_id, &key, &origin)
        .await
        .unwrap();
    // The session registers this owner node as the queue's consumer.
    let queue = node_queue_name(&node_id);
    let consumer = format!("node:{node_id}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !store
        .list_live_event_subscribers()
        .await
        .unwrap()
        .iter()
        .any(|row| row.enabled && row.plugin_id == consumer)
    {
        assert!(std::time::Instant::now() < deadline, "consumer registered");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    publish_node_message(
        &store,
        &node_id,
        &NodeMessage::DeploymentChanged {
            deployment_id: "dep-1".into(),
            plugin_instance_id: "inst-1".into(),
            desired: "present".into(),
            revision: 1,
        },
    )
    .await
    .unwrap();
    for event in store
        .list_domain_events_for_queue(&queue, 10)
        .await
        .unwrap()
    {
        store
            .dispatch_catalog_matches(&event, &uuid::Uuid::new_v4().to_string())
            .await
            .unwrap();
    }
    let wake = next(&mut client).await.unwrap();
    let Body::Wake { message } = &wake.body else {
        panic!("expected a wake: {wake:?}");
    };
    assert_eq!(message["type"], "deployment.changed");
    let running = store
        .list_event_deliveries_for_queue(&queue, Some("running"), 10)
        .await
        .unwrap();
    assert_eq!(running.len(), 1, "leased until the host acks");
    client
        .send(WsMessage::Text(
            Frame::new(wake.id.clone(), Body::Ack {}).to_text().into(),
        ))
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while store
        .list_event_deliveries_for_queue(&queue, Some("acked"), 10)
        .await
        .unwrap()
        .is_empty()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "acked after the host's ack"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Links a store account with a token to `member` and queues one scan for
/// it; returns the job id.
async fn member_scan(store: &LibraryStore, member: i64) -> String {
    store
        .upsert_account(PLUGIN, "riley-acct", "us", None, true, "test")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(member, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, "riley-acct", "test")
        .await
        .unwrap();
    let record = build_sealed_record(
        b"riley-token-v1",
        secret_kind::SOURCE_AUTH,
        PLUGIN,
        secret_account_type::INTEGRATION,
        "riley-acct",
        "riley-acct.auth",
    )
    .unwrap();
    upsert_secret(store.db(), &record).await.unwrap();
    match store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Scan,
            payload: JobPayload {
                account: Some("riley-acct".into()),
                ..Default::default()
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 256,
            run_after: None,
        })
        .await
        .unwrap()
    {
        EnqueueOutcome::Created { id } => id,
        other => panic!("expected a new job: {other:?}"),
    }
}

/// The work-frame context of the enrolled test host on `state`.
async fn host_context(
    state: &Arc<crate::api::AppState>,
    node_id: &str,
) -> super::work::HostContext {
    let cfg = state.config.read().await;
    super::work::HostContext {
        node_id: node_id.into(),
        incarnation: "inc-1".into(),
        box_public_key: box_public_key(&BOX_SECRET).to_vec(),
        owner_box_secret: zeroize::Zeroizing::new([3; 32]),
        settings: bookclerk_library::control_plane::RemoteClaimSettings {
            lease_secs: cfg.jobs.lease_seconds,
            limits: bookclerk_library::control_plane::JobClaimLimits {
                class: cfg.jobs.concurrency.network.max(1),
                plugin_instance: cfg.jobs.concurrency.plugin_instance.max(1),
            },
        },
        owner_node_id: "owner-node".into(),
        state: Arc::clone(state),
    }
}

/// A `claim` for a job.
fn claim_frame() -> Frame {
    Frame::new(
        "c",
        Body::Claim {
            item: ITEM_JOB.into(),
            class: None,
            filter: None,
        },
    )
}

/// A job claimed on a connection that was superseded (or whose answer
/// cannot be queued) is given back unspent, not left leased to a host that
/// never learns of it.
#[tokio::test]
async fn a_job_claimed_on_a_closing_connection_is_given_back() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::tests::test_state(&files).await;
    let (node_id, member) = enroll_member(&store).await;
    let job_id = member_scan(&store, member).await;
    let host = host_context(&state, &node_id).await;
    let unspent = |store: LibraryStore, job_id: String| async move {
        let job = store.get_job(&job_id).await.unwrap().unwrap();
        job.state == bookclerk_library::JobState::Pending && job.attempt_count == 0
    };

    // Superseded while the claim was served: nothing is queued.
    let (_closer, closing) = tokio::sync::watch::channel(Some(CLOSE_SUPERSEDED));
    let (outbox, mut queue) = tokio::sync::mpsc::channel(4);
    let work = super::work::answer(&store, &host, claim_frame()).await;
    assert!(matches!(work.body, Body::Work { .. }), "{work:?}");
    assert!(super::ws::deliver(&store, &outbox, &host, &closing, work).await);
    assert!(queue.try_recv().is_err(), "the work frame is not sent");
    assert!(unspent(store.clone(), job_id.clone()).await);

    // Live, but the writer is gone: the answer cannot be queued.
    let (_open, open) = tokio::sync::watch::channel(None);
    drop(queue);
    let work = super::work::answer(&store, &host, claim_frame()).await;
    assert!(matches!(work.body, Body::Work { .. }), "{work:?}");
    assert!(!super::ws::deliver(&store, &outbox, &host, &open, work).await);
    assert!(unspent(store.clone(), job_id.clone()).await);

    // Live and readable: the work goes out and the job stays claimed.
    let (outbox, mut queue) = tokio::sync::mpsc::channel(4);
    let work = super::work::answer(&store, &host, claim_frame()).await;
    assert!(super::ws::deliver(&store, &outbox, &host, &open, work).await);
    assert!(queue.try_recv().is_ok());
    assert!(!unspent(store.clone(), job_id.clone()).await);
}

/// A superseded session that ends after its successor registered leaves
/// the shared wake catalog row enabled; the last session disables it.
#[tokio::test]
async fn a_superseded_session_leaves_the_wake_catalog_enabled() {
    let files = tempfile::tempdir().unwrap();
    let (_state, store) = super::tests::test_state(&files).await;
    let node_id = uuid::Uuid::new_v4().to_string();
    let consumer = format!("node:{node_id}");
    let enabled = |store: LibraryStore, consumer: String| async move {
        store
            .list_live_event_subscribers()
            .await
            .unwrap()
            .iter()
            .any(|row| row.enabled && row.plugin_id == consumer)
    };

    let old = super::ws::start_session(&store, "owner-node", &node_id).await;
    assert!(enabled(store.clone(), consumer.clone()).await);
    let new = super::ws::start_session(&store, "owner-node", &node_id).await;
    assert_eq!(
        *old.close.borrow(),
        Some(CLOSE_SUPERSEDED),
        "the older session is asked to close"
    );
    super::ws::end_session(&store, &node_id, old.serial, old.wakes).await;
    assert!(
        enabled(store.clone(), consumer.clone()).await,
        "the live session keeps its wakes"
    );
    super::ws::end_session(&store, &node_id, new.serial, new.wakes).await;
    assert!(!enabled(store.clone(), consumer.clone()).await);
}

/// A fence whose renewal fails with a database error on resume is
/// reported unchecked, not lost, and a later renewal keeps it.
#[tokio::test]
async fn a_failed_check_on_resume_is_not_a_lost_fence() {
    use sea_orm::ConnectionTrait as _;

    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = super::tests::test_state(&files).await;
    let (node_id, member) = enroll_member(&store).await;
    let job_id = member_scan(&store, member).await;
    let host = host_context(&state, &node_id).await;
    let work = super::work::answer(&store, &host, claim_frame()).await;
    let Body::Work { fence, .. } = work.body else {
        panic!("expected work: {work:?}");
    };
    let held = HeldFence {
        kind: ITEM_JOB.into(),
        id: job_id.clone(),
        fence,
    };

    // The permit table is briefly unreadable.
    store
        .db()
        .execute_unprepared("ALTER TABLE cluster_leases RENAME TO cluster_leases_away")
        .await
        .unwrap();
    let resumed = super::work::resume(&store, &host, vec![held.clone()]).await;
    assert_eq!(
        resumed,
        Body::Resumed {
            renewed: Vec::new(),
            lost: Vec::new(),
            unchecked: vec![held.clone()],
            lease_secs: None,
        }
    );
    store
        .db()
        .execute_unprepared("ALTER TABLE cluster_leases_away RENAME TO cluster_leases")
        .await
        .unwrap();
    let resumed = super::work::resume(&store, &host, vec![held.clone()]).await;
    assert_eq!(
        resumed,
        Body::Resumed {
            renewed: vec![held],
            lost: Vec::new(),
            unchecked: Vec::new(),
            lease_secs: Some(host.settings.lease_secs),
        }
    );
}
