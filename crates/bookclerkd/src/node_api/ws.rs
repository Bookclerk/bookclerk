//! The host-node socket: `GET /api/node/v1/connect` (H-4 design, section 3).
//!
//! The route sits outside the cookie and operator auth layers and is never
//! disabled by `auth.enabled = false`: the node's enrolled Ed25519 key is
//! the credential. Order of checks:
//!
//! 1. [`super::transport::classify_transport`], before the upgrade.
//! 2. The client must offer the [`SUBPROTOCOL`] subprotocol.
//! 3. hello → challenge → auth. The owner always sends a challenge, so an
//!    unknown node id and a bad signature look the same from outside; every
//!    authentication failure closes with [`CLOSE_AUTH_FAILED`] and the one
//!    reason [`AUTH_FAILED_REASON`]. A valid signature from a revoked
//!    credential closes with [`CLOSE_REVOKED`]. Each failure writes a log
//!    line with the detailed reason and a `security_audit_events` row, at
//!    most [`AUDIT_ROWS_PER_KEY`] per peer, claimed node and reason, and
//!    [`AUDIT_ROWS_PER_WINDOW`] in all, per [`AUDIT_WINDOW`], so
//!    unauthenticated peers cannot grow the table without bound.
//!    The connection is registered for `node.revoked` wakes before its
//!    credential is stamped, so a revoke during the handshake still closes
//!    it.
//! 4. At most [`MAX_PENDING_HANDSHAKES`] upgraded connections may be in the
//!    handshake at once; each holds a permit until `welcome` or refusal,
//!    and an upgrade beyond that answers 503 `handshakes_busy`.
//!
//! After `welcome` (and `resumed`, when the hello listed fences the host
//! still holds; see [`super::work::resume`]), the session pings every
//! [`PING_INTERVAL_SECS`], closes after [`IDLE_TIMEOUT_SECS`] without an
//! inbound frame, re-reads the credential on every ping tick, and closes
//! with [`CLOSE_REVOKED`] as soon as a `node.revoked` wake arrives
//! ([`close_node_sessions`]). Binary frames and malformed frames close with
//! [`CLOSE_PROTOCOL`].
//!
//! Request frames are served by [`super::work`] (heartbeat, job claim,
//! renew, complete, fail, token write-back) and the host's `node.<id>`
//! queue is forwarded as `wake` frames by [`super::wakes`]. Everything the
//! owner sends goes through one bounded queue of [`SEND_QUEUE_FRAMES`]
//! frames drained by a writer task: wakes are only claimed when the queue
//! has room, and an answer that cannot be queued within
//! [`IDLE_TIMEOUT_SECS`] (the host stopped reading) ends the connection.
//! Nothing is released when a connection ends: job leases expire on their
//! own unless the host resumes them. The one exception is a `work` answer
//! produced after the connection was told to close (a newer connection
//! superseded it, or the node was revoked) or that cannot be queued: the
//! host never sees that job, so its claim is given back unspent at once.
//!
//! A newer connection for the same host shares the owner node's wake
//! catalog row with the one it supersedes. Promoting a welcomed connection
//! to live with enabling the row, and unregistering one with disabling it,
//! run under one lock, and only the connection still live disables the row,
//! so a superseded session ending late never switches off the live
//! session's wakes.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use bookclerk_library::control_plane::{
    load_cluster_row, load_node_credential, record_node_connected, record_node_left,
    JobClaimLimits, RemoteClaimSettings,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{
    box_public_key, canonical_origin, encode_nonce, verify_auth, AuthContext, Body, Frame,
    HeldFence, CLOSE_AUTH_FAILED, CLOSE_PROTOCOL, CLOSE_REVOKED, CLOSE_SUPERSEDED,
    HANDSHAKE_TIMEOUT_SECS, IDLE_TIMEOUT_SECS, MAX_FRAME_BYTES, NONCE_LEN, PING_INTERVAL_SECS,
    SEND_QUEUE_FRAMES, SUBPROTOCOL,
};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt as _, StreamExt as _};
use rand::{Rng as _, RngCore as _};
use serde_json::json;
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

use super::wakes::WakeForwarder;
use super::work::{self, HostContext};
use crate::api::AppState;

/// Upgraded connections allowed in the handshake at once, across all
/// peers. TLS acceptance and the API timeout both end at the upgrade, so
/// without this bound peers that never authenticate could hold sockets and
/// tasks faster than the per-frame timeout frees them.
const MAX_PENDING_HANDSHAKES: usize = 128;

/// Handshake refusal audit rows written per [`AUDIT_WINDOW`], across all
/// peers. Refusals past the budget are logged but not stored.
const AUDIT_ROWS_PER_WINDOW: u32 = 60;

/// Handshake refusal audit rows written per [`AUDIT_WINDOW`] for one peer,
/// claimed node id and reason.
const AUDIT_ROWS_PER_KEY: u32 = 5;

/// Window over which [`AUDIT_ROWS_PER_WINDOW`] and [`AUDIT_ROWS_PER_KEY`]
/// apply.
const AUDIT_WINDOW: Duration = Duration::from_secs(60);

/// Permits for connections still in the handshake.
static HANDSHAKES: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES)));

/// Decides which handshake refusals get an audit row.
#[derive(Default)]
struct AuditLimiter {
    /// Start of the current window; `None` before the first refusal.
    window_start: Option<Instant>,
    /// Rows written in the current window.
    written: u32,
    /// Rows written in the current window per peer, claimed node id and
    /// reason.
    seen: HashMap<AuditKey, u32>,
}

/// What [`AuditLimiter`] counts refusals by: peer address, claimed node id,
/// and reason.
type AuditKey = (Option<IpAddr>, Option<String>, &'static str);

impl AuditLimiter {
    /// True when a refusal with `key` at `now` should be stored. `seen`
    /// holds at most [`AUDIT_ROWS_PER_WINDOW`] entries.
    fn allow(&mut self, now: Instant, key: AuditKey) -> bool {
        if self
            .window_start
            .is_none_or(|start| now.duration_since(start) >= AUDIT_WINDOW)
        {
            self.window_start = Some(now);
            self.written = 0;
            self.seen.clear();
        }
        if self.written >= AUDIT_ROWS_PER_WINDOW {
            return false;
        }
        let count = self.seen.entry(key).or_default();
        if *count >= AUDIT_ROWS_PER_KEY {
            return false;
        }
        *count += 1;
        self.written += 1;
        true
    }
}

/// Audit budget for handshake refusals on this node.
static AUDIT_LIMITER: LazyLock<Mutex<AuditLimiter>> =
    LazyLock::new(|| Mutex::new(AuditLimiter::default()));

/// Longest node id or incarnation a hello may carry.
const MAX_ID_BYTES: usize = 128;

/// Close reason sent for every [`CLOSE_AUTH_FAILED`]; the detailed reason
/// stays in the audit row and the log, so a peer cannot tell an unknown node
/// id from a bad signature.
pub(super) const AUTH_FAILED_REASON: &str = "auth_failed";

/// One live, authenticated host connection.
struct SessionSlot {
    /// Distinguishes this connection from a later one for the same node.
    serial: u64,
    /// Asks the connection to close with a code.
    close: watch::Sender<Option<u16>>,
}

/// Host connections on this owner node, by node id.
#[derive(Default)]
struct SessionTable {
    /// The live connection of each node: it has been welcomed.
    live: HashMap<String, SessionSlot>,
    /// Connections still finishing the handshake. They receive
    /// `node.revoked` wakes but do not replace the live connection until
    /// they are welcomed, so a reconnect that fails late leaves it up.
    pending: HashMap<String, Vec<SessionSlot>>,
}

/// Host connections on this owner node.
static SESSIONS: LazyLock<Mutex<SessionTable>> =
    LazyLock::new(|| Mutex::new(SessionTable::default()));

/// Serial for the next registered connection.
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

/// Serializes promoting a connection to live with enabling its wake catalog
/// row, and unregistering one with disabling it (see the module docs).
static CATALOG_GATE: AsyncMutex<()> = AsyncMutex::const_new(());

/// The session table, recovering from a poisoned lock (entries stay
/// consistent: each operation is a single insert, move, or remove).
fn sessions() -> std::sync::MutexGuard<'static, SessionTable> {
    SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Asks every connection of `node_id` on this node, live or still in the
/// handshake, to close with `code`. Called for `node.revoked` wakes.
pub fn close_node_sessions(node_id: &str, code: u16) {
    let table = sessions();
    let live = table.live.get(node_id).into_iter();
    let pending = table.pending.get(node_id).into_iter().flatten();
    for slot in live.chain(pending) {
        let _ = slot.close.send(Some(code));
    }
}

/// True while this owner node holds a live connection of `node_id`.
#[cfg(test)]
pub(crate) fn node_session_live(node_id: &str) -> bool {
    sessions().live.contains_key(node_id)
}

/// Registers a connection that is still in the handshake. It does not
/// replace the node's live connection; see [`promote`].
pub(super) fn register_pending(node_id: &str) -> (u64, watch::Receiver<Option<u16>>) {
    let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
    let (close, receiver) = watch::channel(None);
    sessions()
        .pending
        .entry(node_id.to_string())
        .or_default()
        .push(SessionSlot { serial, close });
    (serial, receiver)
}

/// Makes a welcomed connection the node's live one; an older live
/// connection is asked to close with [`CLOSE_SUPERSEDED`]. Returns `false`,
/// leaving the live connection alone, when the pending slot is gone or
/// already holds a close request (a revoke that landed during the
/// handshake): a revoked connection never supersedes, so the live one keeps
/// the revoke code it was sent.
fn promote(node_id: &str, serial: u64) -> bool {
    let mut table = sessions();
    let Some(slot) = take_pending(&mut table, node_id, serial) else {
        return false;
    };
    if slot.close.borrow().is_some() {
        return false;
    }
    if let Some(previous) = table.live.insert(node_id.to_string(), slot) {
        let _ = previous.close.send(Some(CLOSE_SUPERSEDED));
    }
    true
}

/// Removes and returns the pending slot `serial` of `node_id`.
fn take_pending(table: &mut SessionTable, node_id: &str, serial: u64) -> Option<SessionSlot> {
    let slots = table.pending.get_mut(node_id)?;
    let index = slots.iter().position(|slot| slot.serial == serial)?;
    let slot = slots.swap_remove(index);
    if slots.is_empty() {
        table.pending.remove(node_id);
    }
    Some(slot)
}

/// Removes a connection's slot, pending or live, unless a newer connection
/// replaced it. Returns `true` when the live slot was this connection's.
fn unregister(node_id: &str, serial: u64) -> bool {
    let mut table = sessions();
    if take_pending(&mut table, node_id, serial).is_some() {
        return false;
    }
    if table
        .live
        .get(node_id)
        .is_some_and(|slot| slot.serial == serial)
    {
        table.live.remove(node_id);
        return true;
    }
    false
}

/// A registered connection: its serial, close signal, and wake forwarder.
pub(super) struct Registered {
    /// Serial of this connection.
    pub serial: u64,
    /// Close requests for this connection.
    pub close: watch::Receiver<Option<u16>>,
    /// Forwarder of the host's queue, when the host id names one.
    pub wakes: Option<WakeForwarder>,
}

/// Makes the welcomed pending connection `serial` of `node_id` (from
/// [`register_pending`], whose `close` receiver it hands back) the node's
/// live one with [`promote`] and enables this owner node's wake catalog row
/// for it, both under [`CATALOG_GATE`]. `None`, with nothing enabled, when
/// [`promote`] refuses (the connection was revoked during the handshake).
pub(super) async fn start_session(
    library: &LibraryStore,
    owner_node_id: &str,
    node_id: &str,
    serial: u64,
    close: watch::Receiver<Option<u16>>,
) -> Option<Registered> {
    let _gate = CATALOG_GATE.lock().await;
    if !promote(node_id, serial) {
        return None;
    }
    let wakes = WakeForwarder::new(owner_node_id, node_id, serial);
    if let Some(wakes) = wakes.as_ref() {
        wakes.heartbeat(library, true).await;
    }
    Some(Registered {
        serial,
        close,
        wakes,
    })
}

/// Unregisters connection `serial` of `node_id` and shuts its wake
/// forwarder down under [`CATALOG_GATE`]: unacked wakes are released, and
/// the catalog row is disabled only when no newer connection replaced this
/// one.
pub(super) async fn end_session(
    library: &LibraryStore,
    node_id: &str,
    serial: u64,
    wakes: Option<WakeForwarder>,
) {
    let _gate = CATALOG_GATE.lock().await;
    let current = unregister(node_id, serial);
    if let Some(wakes) = wakes {
        wakes.shutdown(library, current).await;
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
#[utoipa::path(
    get,
    path = "/api/node/v1/connect",
    responses(
        (status = 101, description = "Switching to the host-node WebSocket. The node's signature is checked in the first frames; a wrong one closes the socket."),
        (status = 400, description = "The request does not offer the node subprotocol (`subprotocol_required`)."),
        (status = 403, description = "The transport is not allowed."),
        (status = 404, description = "The node API is not mounted."),
        (status = 503, description = "Too many handshakes are in flight (`handshakes_busy`); retry after `Retry-After` seconds.", headers(("Retry-After" = u64, description = "Seconds to wait before retrying."))),
    ),
)]
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
    let Ok(permit) = Arc::clone(&HANDSHAKES).try_acquire_owned() else {
        tracing::warn!(
            ?peer,
            "node API upgrade refused: too many handshakes in flight"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "10")],
            Json(json!({ "error": "handshakes_busy" })),
        )
            .into_response();
    };
    upgrade
        .protocols([SUBPROTOCOL])
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| run(state, socket, peer, permit))
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
    /// Incarnation from the hello.
    incarnation: String,
    /// Leases the hello says the host still holds.
    resume: Vec<HeldFence>,
    /// The node's enrolled X25519 box key.
    box_public_key: Vec<u8>,
    /// This connection's owner box secret (its public key went out in
    /// `welcome`).
    owner_box_secret: Zeroizing<[u8; 32]>,
    /// This connection, promoted to live with its wake forwarder.
    registered: Registered,
}

/// Runs one connection: the handshake, then the session. `permit` is held
/// until the handshake ends.
async fn run(
    state: Arc<AppState>,
    mut socket: WebSocket,
    peer: Option<SocketAddr>,
    permit: OwnedSemaphorePermit,
) {
    let library = state.library_snapshot().await;
    let (owner_origin, settings, files_dir) = {
        let cfg = state.config.read().await;
        let settings = RemoteClaimSettings {
            lease_secs: cfg.jobs.lease_seconds,
            limits: JobClaimLimits {
                class: cfg.jobs.concurrency.network.max(1),
                plugin_instance: cfg.jobs.concurrency.plugin_instance.max(1),
            },
        };
        (
            canonical_origin(&crate::origin::effective_origin_from_config(&cfg, None)),
            settings,
            cfg.paths().files_dir.clone(),
        )
    };
    let owner_node_id = crate::node_queue::node_id(&files_dir).unwrap_or_default();
    let authed = match owner_origin {
        Some(origin) => handshake(&library, &mut socket, &origin, &owner_node_id).await,
        None => Err(Refusal::auth("owner_origin_unavailable", None)),
    };
    let authed = match authed {
        Ok(authed) => authed,
        Err(refusal) => {
            refuse(&library, socket, peer, refusal).await;
            return;
        }
    };
    drop(permit);
    let Registered {
        serial,
        close,
        mut wakes,
    } = authed.registered;
    tracing::info!(node_id = %authed.node_id, ?peer, "host node connected");
    // Uploads are received by this node; its own identity file names it
    // (the queue's process-wide id is the consumer's, not necessarily it).
    let receiving_node_id =
        bookclerk_library::control_plane::load_or_create_node_identity(&files_dir)
            .map(|identity| identity.node_id)
            .unwrap_or_default();
    let host = HostContext {
        node_id: authed.node_id.clone(),
        incarnation: authed.incarnation,
        box_public_key: authed.box_public_key,
        owner_box_secret: authed.owner_box_secret,
        settings,
        owner_node_id: receiving_node_id,
        state: Arc::clone(&state),
    };
    session(&library, socket, &host, authed.resume, &mut wakes, close).await;
    end_session(&library, &authed.node_id, serial, wakes).await;
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
    let store = AUDIT_LIMITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .allow(
            Instant::now(),
            (
                peer.map(|addr| addr.ip()),
                refusal.node_id.clone(),
                refusal.reason,
            ),
        );
    if !store {
        tracing::debug!("node handshake refusal not stored: audit budget spent");
    } else if let Err(err) = library
        .insert_security_audit_event(&actor, action, Some(&detail))
        .await
    {
        tracing::warn!(error = %err, "could not record node handshake refusal");
    }
    // The detailed reason is for the audit row and the log only.
    let reason = if refusal.code == CLOSE_AUTH_FAILED {
        AUTH_FAILED_REASON
    } else {
        refusal.reason
    };
    close_with(&mut socket, refusal.code, reason).await;
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
    owner_node_id: &str,
) -> Result<Authed, Refusal> {
    let Some(hello) = next_handshake_frame(socket).await else {
        return Err(Refusal::auth("malformed_hello", None));
    };
    let Body::Hello {
        node_id,
        incarnation,
        resume,
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
    let nonce: [u8; NONCE_LEN] = rand::rngs::OsRng.gen();
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
        id: auth_id,
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
    let (serial, close) = admit(&node_id, record_node_connected(library, &node_id)).await?;
    if close.borrow().is_some() {
        // Revoked while the stamp ran: no welcome.
        unregister(&node_id, serial);
        return Err(revoked);
    }
    // A fresh owner box key per connection: the host seals token
    // write-backs to it.
    let mut owner_box_secret = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(owner_box_secret.as_mut_slice());
    let box_key =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(box_public_key(&owner_box_secret));
    // The welcome answers the auth frame, so it echoes its id.
    let welcome = Frame::new(
        auth_id,
        Body::Welcome {
            node_id: node_id.clone(),
            cluster_id,
            box_key,
        },
    );
    if !send_frame(socket, &welcome).await {
        unregister(&node_id, serial);
        return Err(Refusal::auth("closed", Some(&node_id)));
    }
    let Some(registered) = start_session(library, owner_node_id, &node_id, serial, close).await
    else {
        return Err(revoked);
    };
    Ok(Authed {
        node_id,
        incarnation,
        resume,
        box_public_key: credential.box_public_key,
        owner_box_secret,
        registered,
    })
}

/// Registers `node_id`'s connection as pending, then awaits `stamp` (which
/// records the connection against an unrevoked credential). Registering
/// first means a `node.revoked` wake that lands while the stamp runs, or any
/// time after it, finds this connection; a revoke committed before the
/// stamp makes the stamp report `false`. Either way a revoked credential
/// never keeps a session. The node's live connection is replaced only by
/// [`promote`], after the welcome is sent, so a refused attempt leaves it
/// up.
async fn admit<F>(node_id: &str, stamp: F) -> Result<(u64, watch::Receiver<Option<u16>>), Refusal>
where
    F: std::future::Future<Output = bookclerk_library::Result<bool>>,
{
    let (serial, close) = register_pending(node_id);
    let refusal = match stamp.await {
        Ok(true) => return Ok((serial, close)),
        Ok(false) => Refusal {
            code: CLOSE_REVOKED,
            reason: "revoked",
            node_id: Some(node_id.to_string()),
        },
        Err(_) => Refusal::auth("credential_unavailable", Some(node_id)),
    };
    unregister(node_id, serial);
    Err(refusal)
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

/// Seconds between attempts to forward the host's queue.
const WAKE_POLL_SECS: u64 = 2;

/// Seconds the session waits for the writer to flush a close.
const CLOSE_FLUSH_SECS: u64 = 5;

/// A text message for `frame`.
fn text(frame: &Frame) -> Message {
    Message::Text(frame.to_text().into())
}

/// Queues `message`, waiting up to [`IDLE_TIMEOUT_SECS`] for room. `false`
/// when the host stopped reading or the writer is gone.
async fn enqueue(outbox: &mpsc::Sender<Message>, message: Message) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(IDLE_TIMEOUT_SECS), outbox.send(message)).await,
        Ok(Ok(()))
    )
}

/// Drains the send queue into the socket until a close is sent, the queue
/// closes, or a write fails.
///
/// The queue stays locked while the writer runs. When it ends, or is
/// aborted, the lock is released and the session takes back what is still
/// queued. A message whose write failed is not given back: part of it may
/// have reached the host, which then resumes its job on reconnect, so its
/// lease is left to run.
async fn writer(
    mut sink: SplitSink<WebSocket, Message>,
    queue: Arc<AsyncMutex<mpsc::Receiver<Message>>>,
) {
    let mut queue = queue.lock().await;
    while let Some(message) = queue.recv().await {
        let closing = matches!(message, Message::Close(_));
        if sink.send(message).await.is_err() {
            break;
        }
        if closing {
            break;
        }
    }
    // A later [`enqueue`] now fails, and its caller gives the frame back.
    queue.close();
    drop(queue);
    let _ = sink.close().await;
}

/// The authenticated session: a writer task behind the bounded send queue,
/// and the loop that reads frames, pings, and forwards wakes.
async fn session(
    library: &LibraryStore,
    socket: WebSocket,
    host: &HostContext,
    resume: Vec<HeldFence>,
    wakes: &mut Option<WakeForwarder>,
    close: watch::Receiver<Option<u16>>,
) {
    let (sink, stream) = socket.split();
    let (outbox, queue) = mpsc::channel(SEND_QUEUE_FRAMES);
    let queue = Arc::new(AsyncMutex::new(queue));
    let mut writer = tokio::spawn(writer(sink, Arc::clone(&queue)));
    if let Some((code, reason)) =
        session_loop(library, stream, &outbox, host, resume, wakes, close).await
    {
        tracing::info!(node_id = %host.node_id, code, reason, "closing host node connection");
        let close = Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }));
        let _ = tokio::time::timeout(Duration::from_secs(1), outbox.send(close)).await;
    }
    drop(outbox);
    let mut unsent = Vec::new();
    if tokio::time::timeout(Duration::from_secs(CLOSE_FLUSH_SECS), &mut writer)
        .await
        .is_err()
    {
        // The socket stopped taking writes. Aborting drops the writer's
        // lock on the queue; the frame it was writing stays leased, as a
        // failed write does.
        writer.abort();
        let _ = writer.await;
    }
    {
        let mut queue = queue.lock().await;
        queue.close();
        while let Ok(message) = queue.try_recv() {
            unsent.push(message);
        }
    }
    // A `work` answer queued but never written would leave its job held
    // until the lease expires: the host never learned of it, so it cannot
    // resume it.
    for message in unsent {
        if let Message::Text(text) = message {
            if let Ok(frame) = Frame::parse(&text) {
                work::release_unsent(library, host, &frame).await;
            }
        }
    }
}

/// Queues `answer` for the host. A `work` answer is given back instead
/// when the connection was already told to close (the close branch of the
/// session loop ends it next), and when it cannot be queued, since the
/// host would never learn of the job. Returns `false` when the host
/// stopped reading.
pub(super) async fn deliver(
    library: &LibraryStore,
    outbox: &mpsc::Sender<Message>,
    host: &HostContext,
    close: &watch::Receiver<Option<u16>>,
    answer: Frame,
) -> bool {
    if close.borrow().is_some() {
        work::release_unsent(library, host, &answer).await;
        return true;
    }
    if enqueue(outbox, text(&answer)).await {
        return true;
    }
    work::release_unsent(library, host, &answer).await;
    false
}

/// Reads and answers frames until the connection ends. Returns the close
/// code and reason to send, or `None` when the connection is already gone.
async fn session_loop(
    library: &LibraryStore,
    mut stream: SplitStream<WebSocket>,
    outbox: &mpsc::Sender<Message>,
    host: &HostContext,
    resume: Vec<HeldFence>,
    wakes: &mut Option<WakeForwarder>,
    mut close: watch::Receiver<Option<u16>>,
) -> Option<(u16, &'static str)> {
    let node_id = host.node_id.as_str();
    if !resume.is_empty() {
        let resumed = work::resume(library, host, resume).await;
        if !enqueue(outbox, text(&Frame::new("resume", resumed))).await {
            return None;
        }
    }
    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.reset();
    let mut forward = tokio::time::interval(Duration::from_secs(WAKE_POLL_SECS));
    forward.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let idle = Duration::from_secs(IDLE_TIMEOUT_SECS);
    let mut last_inbound = Instant::now();
    loop {
        tokio::select! {
            changed = close.changed() => {
                let code = if changed.is_ok() { *close.borrow() } else { None };
                let code = code.unwrap_or(CLOSE_REVOKED);
                let reason = if code == CLOSE_SUPERSEDED { "superseded" } else { "revoked" };
                return Some((code, reason));
            }
            _ = ping.tick() => {
                if credential_revoked(library, node_id).await {
                    return Some((CLOSE_REVOKED, "revoked"));
                }
                if last_inbound.elapsed() >= idle {
                    return Some((1000, "idle"));
                }
                // A full queue already keeps the host busy; skip the ping.
                if let Err(mpsc::error::TrySendError::Closed(_)) =
                    outbox.try_send(Message::Ping(Default::default()))
                {
                    return None;
                }
                // A closing session no longer owns the catalog row; a newer
                // connection may have registered and ended already. The
                // check runs under the gate `start_session` registers under,
                // so a successor either has not registered yet or has
                // already signalled this session to close.
                if let Some(wakes) = wakes.as_ref() {
                    let _gate = CATALOG_GATE.lock().await;
                    if close.borrow().is_none() {
                        wakes.heartbeat(library, true).await;
                    }
                }
            }
            _ = forward.tick() => {
                if let Some(wakes) = wakes.as_mut() {
                    wakes.forward(library, outbox).await;
                }
            }
            message = stream.next() => {
                let Some(Ok(message)) = message else { return None };
                last_inbound = Instant::now();
                match message {
                    Message::Text(raw) => {
                        let Ok(frame) = Frame::parse(raw.as_str()) else {
                            return Some((CLOSE_PROTOCOL, "malformed frame"));
                        };
                        match frame.body {
                            Body::Hello { .. } | Body::Auth { .. } => {
                                return Some((CLOSE_PROTOCOL, "handshake already done"));
                            }
                            // Never answer an error, so two peers cannot loop.
                            Body::Error { .. } => continue,
                            Body::Ack {} => {
                                if let Some(wakes) = wakes.as_mut() {
                                    wakes.ack(library, &frame.id).await;
                                }
                                continue;
                            }
                            // The host's operator chose Leave library: the
                            // node is gone now, not after missed heartbeats.
                            Body::NodeLeaving {} => {
                                let answer = match record_node_left(library, node_id).await {
                                    Ok(_) => Body::Accepted {},
                                    Err(err) => {
                                        tracing::warn!(node_id, error = %err, "could not record a leaving host node");
                                        Body::Error {
                                            code: "unavailable".into(),
                                            message: "the leave could not be recorded; retry".into(),
                                        }
                                    }
                                };
                                let left = matches!(answer, Body::Accepted {});
                                if !enqueue(outbox, text(&Frame::new(frame.id, answer))).await {
                                    return None;
                                }
                                if left {
                                    return Some((CLOSE_REVOKED, "left"));
                                }
                                continue;
                            }
                            _ => {}
                        }
                        let answer = work::answer(library, host, frame).await;
                        if !deliver(library, outbox, host, &close, answer).await {
                            tracing::warn!(node_id, "host node stopped reading; dropping the connection");
                            return None;
                        }
                    }
                    Message::Binary(_) => {
                        return Some((CLOSE_PROTOCOL, "binary frames are refused"));
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => return None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_node_proto::{CLOSE_REVOKED, CLOSE_SUPERSEDED};

    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;

    use super::{
        admit, close_node_sessions, promote, sessions, unregister, AuditLimiter,
        AUDIT_ROWS_PER_KEY, AUDIT_ROWS_PER_WINDOW, AUDIT_WINDOW,
    };

    #[test]
    fn handshake_refusal_audit_rows_are_bounded() {
        let mut limiter = AuditLimiter::default();
        let start = Instant::now();
        let peer = Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
        let key = |node: &str, reason| (peer, Some(node.to_string()), reason);
        // One peer, node and reason is stored a few times per window.
        for _ in 0..AUDIT_ROWS_PER_KEY {
            assert!(limiter.allow(start, key("node", "bad_signature")));
        }
        assert!(!limiter.allow(start, key("node", "bad_signature")));
        assert!(limiter.allow(start, key("node", "revoked")));
        // Across peers the window has a fixed budget.
        let mut stored = AUDIT_ROWS_PER_KEY + 1;
        for n in 0..=u8::MAX {
            let other = Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, n)));
            if limiter.allow(start, (other, None, "malformed_hello")) {
                stored += 1;
            }
        }
        assert_eq!(stored, AUDIT_ROWS_PER_WINDOW);
        // A new window starts over.
        assert!(limiter.allow(start + AUDIT_WINDOW, key("node", "bad_signature")));
    }

    #[tokio::test]
    async fn a_revoke_during_the_stamp_reaches_the_admitted_connection() {
        let node_id = "admit-revoked-mid-stamp";
        // The wake lands while the credential is being stamped.
        let (serial, close) = admit(node_id, async {
            close_node_sessions(node_id, CLOSE_REVOKED);
            Ok(true)
        })
        .await
        .unwrap_or_else(|_| panic!("admitted"));
        assert_eq!(*close.borrow(), Some(CLOSE_REVOKED));
        // A revoked connection is not promoted, and keeps its close request.
        assert!(!promote(node_id, serial));
        assert_eq!(*close.borrow(), Some(CLOSE_REVOKED));
        unregister(node_id, serial);
        assert!(!sessions().live.contains_key(node_id));
        assert!(!sessions().pending.contains_key(node_id));
    }

    #[tokio::test]
    async fn a_revoked_reconnect_does_not_supersede_the_live_connection() {
        let node_id = "admit-revoked-reconnect";
        let (live_serial, live) = admit(node_id, async { Ok(true) })
            .await
            .unwrap_or_else(|_| panic!("admitted"));
        assert!(promote(node_id, live_serial));
        // A reconnect is mid-handshake when the revoke wake closes both.
        let (serial, pending) = admit(node_id, async { Ok(true) })
            .await
            .unwrap_or_else(|_| panic!("admitted"));
        close_node_sessions(node_id, CLOSE_REVOKED);
        assert!(!promote(node_id, serial));
        assert_eq!(*pending.borrow(), Some(CLOSE_REVOKED));
        // The live connection keeps the revoke code, not superseded.
        assert_eq!(*live.borrow(), Some(CLOSE_REVOKED));
        assert!(sessions()
            .live
            .get(node_id)
            .is_some_and(|slot| slot.serial == live_serial));
        unregister(node_id, live_serial);
        assert!(!sessions().live.contains_key(node_id));
    }

    #[tokio::test]
    async fn a_refused_reconnect_leaves_the_live_connection_up() {
        let node_id = "admit-refused-reconnect";
        let (live_serial, live) = admit(node_id, async { Ok(true) })
            .await
            .unwrap_or_else(|_| panic!("admitted"));
        assert!(promote(node_id, live_serial));

        // A stamp that fails, or finds the credential revoked, refuses the
        // new attempt without touching the live connection.
        for stamp in [
            Ok(false),
            Err(bookclerk_library::LibraryError::Unavailable("db".into())),
        ] {
            let refusal = admit(node_id, async { stamp }).await.expect_err("refused");
            assert_ne!(refusal.code, CLOSE_SUPERSEDED);
            assert_eq!(*live.borrow(), None);
        }
        // So does a welcome that cannot be sent.
        let (serial, _) = admit(node_id, async { Ok(true) })
            .await
            .unwrap_or_else(|_| panic!("admitted"));
        unregister(node_id, serial);
        assert_eq!(*live.borrow(), None);
        assert!(!sessions().pending.contains_key(node_id));

        // A welcomed reconnect supersedes it.
        let (serial, _) = admit(node_id, async { Ok(true) })
            .await
            .unwrap_or_else(|_| panic!("admitted"));
        assert!(promote(node_id, serial));
        assert_eq!(*live.borrow(), Some(CLOSE_SUPERSEDED));
        unregister(node_id, serial);
        unregister(node_id, live_serial);
        assert!(!sessions().live.contains_key(node_id));
    }
}
