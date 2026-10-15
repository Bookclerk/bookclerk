//! The host-node socket: `GET /api/node/v1/connect` (H-4 design, section 3).
//!
//! The route sits outside the cookie and operator auth layers and is never
//! disabled by `auth.enabled = false`: the node's enrolled Ed25519 key is
//! the credential. Order of checks:
//!
//! 1. [`super::transport::classify_transport`], before the upgrade.
//! 2. The client must offer the [`SUBPROTOCOL`] subprotocol.
//! 3. hello → challenge → auth. The owner always sends a challenge, so an
//!    unknown node id and a bad signature look the same from outside; both
//!    close with [`CLOSE_AUTH_FAILED`]. A valid signature from a revoked
//!    credential closes with [`CLOSE_REVOKED`]. Each failure writes a
//!    `security_audit_events` row.
//!
//! After `welcome`, the session pings every [`PING_INTERVAL_SECS`], closes
//! after [`IDLE_TIMEOUT_SECS`] without an inbound frame, re-reads the
//! credential on every ping tick, and closes with [`CLOSE_REVOKED`] as soon
//! as a `node.revoked` wake arrives ([`close_node_sessions`]). Binary frames
//! and malformed frames close with [`CLOSE_PROTOCOL`]. Work frames (claim,
//! renew, kv, upload grants) are later steps; until then every request
//! frame is answered with `error {code: "unsupported"}`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    load_cluster_row, load_node_credential, record_node_connected,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{
    canonical_origin, encode_nonce, verify_auth, AuthContext, Body, Frame, CLOSE_AUTH_FAILED,
    CLOSE_PROTOCOL, CLOSE_REVOKED, CLOSE_SUPERSEDED, HANDSHAKE_TIMEOUT_SECS, IDLE_TIMEOUT_SECS,
    MAX_FRAME_BYTES, NONCE_LEN, PING_INTERVAL_SECS, SUBPROTOCOL,
};
use rand::RngCore as _;
use serde_json::json;
use tokio::sync::watch;

use crate::api::AppState;

/// Longest node id or incarnation a hello may carry.
const MAX_ID_BYTES: usize = 128;

/// One live, authenticated host connection.
struct SessionSlot {
    /// Distinguishes this connection from a later one for the same node.
    serial: u64,
    /// Asks the connection to close with a code.
    close: watch::Sender<Option<u16>>,
}

/// Live host connections on this owner node, by node id.
static SESSIONS: LazyLock<Mutex<HashMap<String, SessionSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Serial for the next registered connection.
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

/// The session table, recovering from a poisoned lock (entries stay
/// consistent: each operation is a single insert or remove).
fn sessions() -> std::sync::MutexGuard<'static, HashMap<String, SessionSlot>> {
    SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Asks the live connection of `node_id`, if this node holds one, to close
/// with `code`. Called for `node.revoked` wakes.
pub fn close_node_sessions(node_id: &str, code: u16) {
    if let Some(slot) = sessions().get(node_id) {
        let _ = slot.close.send(Some(code));
    }
}

/// Registers an authenticated connection; an older one for the same node
/// is asked to close with [`CLOSE_SUPERSEDED`].
fn register(node_id: &str) -> (u64, watch::Receiver<Option<u16>>) {
    let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
    let (close, receiver) = watch::channel(None);
    if let Some(previous) = sessions().insert(node_id.to_string(), SessionSlot { serial, close }) {
        let _ = previous.close.send(Some(CLOSE_SUPERSEDED));
    }
    (serial, receiver)
}

/// Removes a connection's slot unless a newer connection replaced it.
fn unregister(node_id: &str, serial: u64) {
    let mut table = sessions();
    if table.get(node_id).is_some_and(|slot| slot.serial == serial) {
        table.remove(node_id);
    }
}

/// True when the request offers the node subprotocol.
fn offers_subprotocol(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|proto| proto.trim() == SUBPROTOCOL)
}

/// `GET /api/node/v1/connect`: upgrade to the host-node socket after the
/// transport check. 403 `transport_insecure` for a refused connection,
/// 400 `subprotocol_required` without [`SUBPROTOCOL`].
pub async fn get_connect(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    if let Err(refused) = super::check_transport(&state, &mut parts, "connect").await {
        return refused;
    }
    if !offers_subprotocol(&parts.headers) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "subprotocol_required", "subprotocol": SUBPROTOCOL })),
        )
            .into_response();
    }
    let peer = <ConnectInfo<SocketAddr> as FromRequestParts<Arc<AppState>>>::from_request_parts(
        &mut parts, &state,
    )
    .await
    .ok()
    .map(|ConnectInfo(addr)| addr);
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade
        .protocols([SUBPROTOCOL])
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| run(state, socket, peer))
}

/// A refused handshake.
struct Refusal {
    /// Close code to send.
    code: u16,
    /// Stable reason for the audit row and logs.
    reason: &'static str,
    /// Node id the host claimed, once it is well-formed.
    node_id: Option<String>,
}

impl Refusal {
    /// A [`CLOSE_AUTH_FAILED`] refusal.
    fn auth(reason: &'static str, node_id: Option<&str>) -> Self {
        Self {
            code: CLOSE_AUTH_FAILED,
            reason,
            node_id: node_id.map(str::to_string),
        }
    }
}

/// An authenticated host.
struct Authed {
    /// Node id the signature proved.
    node_id: String,
}

/// Runs one connection: the handshake, then the session.
async fn run(state: Arc<AppState>, mut socket: WebSocket, peer: Option<SocketAddr>) {
    let library = state.library_snapshot().await;
    let owner_origin = {
        let cfg = state.config.read().await;
        canonical_origin(&crate::origin::effective_origin_from_config(&cfg, None))
    };
    let authed = match owner_origin {
        Some(origin) => handshake(&library, &mut socket, &origin).await,
        None => Err(Refusal::auth("owner_origin_unavailable", None)),
    };
    let authed = match authed {
        Ok(authed) => authed,
        Err(refusal) => {
            refuse(&library, socket, peer, refusal).await;
            return;
        }
    };
    let (serial, close) = register(&authed.node_id);
    tracing::info!(node_id = %authed.node_id, ?peer, "host node connected");
    session(&library, &mut socket, &authed.node_id, close).await;
    unregister(&authed.node_id, serial);
    tracing::info!(node_id = %authed.node_id, "host node disconnected");
}

/// Closes a refused connection and records why.
async fn refuse(
    library: &LibraryStore,
    mut socket: WebSocket,
    peer: Option<SocketAddr>,
    refusal: Refusal,
) {
    let action = if refusal.code == CLOSE_REVOKED {
        "node_auth_revoked"
    } else {
        "node_auth_failed"
    };
    tracing::warn!(
        node_id = refusal.node_id.as_deref().unwrap_or(""),
        ?peer,
        reason = refusal.reason,
        "host node handshake refused"
    );
    let actor = refusal
        .node_id
        .as_deref()
        .map_or_else(|| "node:unknown".to_string(), |id| format!("node:{id}"));
    let detail = json!({
        "reason": refusal.reason,
        "node_id": refusal.node_id,
        "peer": peer.map(|addr| addr.ip().to_string()),
    })
    .to_string();
    if let Err(err) = library
        .insert_security_audit_event(&actor, action, Some(&detail))
        .await
    {
        tracing::warn!(error = %err, "could not record node handshake refusal");
    }
    close_with(&mut socket, refusal.code, refusal.reason).await;
}

/// Sends a close frame; the peer may already be gone.
async fn close_with(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// Sends one frame.
async fn send_frame(socket: &mut WebSocket, frame: &Frame) -> bool {
    socket
        .send(Message::Text(frame.to_text().into()))
        .await
        .is_ok()
}

/// The next text frame within the handshake timeout, skipping control
/// frames. `None` on timeout, close, binary, or a malformed frame.
async fn next_handshake_frame(socket: &mut WebSocket) -> Option<Frame> {
    let deadline = Instant::now() + Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);
    loop {
        let message = tokio::time::timeout_at(deadline.into(), socket.recv())
            .await
            .ok()??
            .ok()?;
        match message {
            Message::Text(text) => return Frame::parse(text.as_str()).ok(),
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Binary(_) | Message::Close(_) => return None,
        }
    }
}

/// True for a node id or incarnation the owner will look up and log.
fn is_plain_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_ID_BYTES
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// hello → challenge → auth.
async fn handshake(
    library: &LibraryStore,
    socket: &mut WebSocket,
    owner_origin: &str,
) -> Result<Authed, Refusal> {
    let Some(hello) = next_handshake_frame(socket).await else {
        return Err(Refusal::auth("malformed_hello", None));
    };
    let Body::Hello {
        node_id,
        incarnation,
        ..
    } = hello.body
    else {
        return Err(Refusal::auth("expected_hello", None));
    };
    if !is_plain_id(&node_id) {
        return Err(Refusal::auth("malformed_hello", None));
    }
    if !is_plain_id(&incarnation) {
        return Err(Refusal::auth("malformed_hello", Some(&node_id)));
    }
    let cluster_id = match load_cluster_row(library).await {
        Ok(Some(row)) => row.cluster_id,
        Ok(None) | Err(_) => return Err(Refusal::auth("cluster_unavailable", Some(&node_id))),
    };
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let challenge = Frame::new(
        hello.id,
        Body::Challenge {
            nonce: encode_nonce(&nonce),
            cluster_id: cluster_id.clone(),
        },
    );
    if !send_frame(socket, &challenge).await {
        return Err(Refusal::auth("closed", Some(&node_id)));
    }
    let Some(Frame {
        body: Body::Auth { sig },
        ..
    }) = next_handshake_frame(socket).await
    else {
        return Err(Refusal::auth("malformed_auth", Some(&node_id)));
    };
    // Read the credential only now, so an unknown node and a bad signature
    // take the same path from the host's side.
    let credential = match load_node_credential(library, &node_id).await {
        Ok(Some(credential)) => credential,
        Ok(None) => return Err(Refusal::auth("unknown_node", Some(&node_id))),
        Err(_) => return Err(Refusal::auth("credential_unavailable", Some(&node_id))),
    };
    let ctx = AuthContext {
        cluster_id: &cluster_id,
        node_id: &node_id,
        incarnation: &incarnation,
        nonce: &nonce,
        owner_origin,
    };
    if verify_auth(&credential.sign_public_key, &ctx, &sig).is_err() {
        return Err(Refusal::auth("bad_signature", Some(&node_id)));
    }
    let revoked = Refusal {
        code: CLOSE_REVOKED,
        reason: "revoked",
        node_id: Some(node_id.clone()),
    };
    if credential.revoked_at.is_some() {
        return Err(revoked);
    }
    // Stamps only an unrevoked credential, so a revoke mid-handshake wins.
    match record_node_connected(library, &node_id).await {
        Ok(true) => {}
        Ok(false) => return Err(revoked),
        Err(_) => return Err(Refusal::auth("credential_unavailable", Some(&node_id))),
    }
    let welcome = Frame::new(
        "",
        Body::Welcome {
            node_id: node_id.clone(),
            cluster_id,
        },
    );
    if !send_frame(socket, &welcome).await {
        return Err(Refusal::auth("closed", Some(&node_id)));
    }
    Ok(Authed { node_id })
}

/// True when the credential is gone or revoked. A failed read keeps the
/// connection; the next tick reads again.
async fn credential_revoked(library: &LibraryStore, node_id: &str) -> bool {
    match load_node_credential(library, node_id).await {
        Ok(Some(credential)) => credential.revoked_at.is_some(),
        Ok(None) => true,
        Err(err) => {
            tracing::warn!(node_id, error = %err, "host credential re-check failed");
            false
        }
    }
}

/// The authenticated session loop.
async fn session(
    library: &LibraryStore,
    socket: &mut WebSocket,
    node_id: &str,
    mut close: watch::Receiver<Option<u16>>,
) {
    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.reset();
    let idle = Duration::from_secs(IDLE_TIMEOUT_SECS);
    let mut last_inbound = Instant::now();
    loop {
        tokio::select! {
            changed = close.changed() => {
                let code = if changed.is_ok() { *close.borrow() } else { None };
                let code = code.unwrap_or(CLOSE_REVOKED);
                let reason = if code == CLOSE_SUPERSEDED { "superseded" } else { "revoked" };
                tracing::info!(node_id, code, "closing host node connection");
                close_with(socket, code, reason).await;
                return;
            }
            _ = ping.tick() => {
                if credential_revoked(library, node_id).await {
                    close_with(socket, CLOSE_REVOKED, "revoked").await;
                    return;
                }
                if last_inbound.elapsed() >= idle {
                    close_with(socket, 1000, "idle").await;
                    return;
                }
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
            message = socket.recv() => {
                let Some(Ok(message)) = message else { return };
                last_inbound = Instant::now();
                match message {
                    Message::Text(text) => {
                        let Ok(frame) = Frame::parse(text.as_str()) else {
                            close_with(socket, CLOSE_PROTOCOL, "malformed frame").await;
                            return;
                        };
                        match frame.body {
                            Body::Hello { .. } | Body::Auth { .. } => {
                                close_with(socket, CLOSE_PROTOCOL, "handshake already done").await;
                                return;
                            }
                            // Never answer an error, so two peers cannot loop.
                            Body::Error { .. } => continue,
                            _ => {}
                        }
                        let answer = Frame::new(
                            frame.id,
                            Body::Error {
                                code: "unsupported".into(),
                                message: "this owner does not serve that frame type yet".into(),
                            },
                        );
                        if !send_frame(socket, &answer).await {
                            return;
                        }
                    }
                    Message::Binary(_) => {
                        close_with(socket, CLOSE_PROTOCOL, "binary frames are refused").await;
                        return;
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => return,
                }
            }
        }
    }
}
