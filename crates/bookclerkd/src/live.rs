//! The SPA's live socket, `GET /api/live` (H-4 design, step 10).
//!
//! The SPA used to poll its status, jobs, nodes and consent routes on
//! timers. It now holds one WebSocket here and re-reads a route only when
//! the daemon says its topic changed. The socket carries topic names and
//! nothing else: no row data, no ids, no secrets. Everything the SPA shows
//! still comes from the ordinary authorized routes.
//!
//! **Admission.** In this order, each failure answering before the
//! upgrade:
//!
//! 1. `Origin` must match exactly ([`origin_allowed`]): the configured
//!    `integrations.public_origin` when it is set; otherwise
//!    `http://<Host>` for a loopback `Host` with no forwarding headers
//!    (the daemon's own page, or the Vite dev server proxying to it).
//!    Anything else, or no `Origin` at all, is 403 `origin_mismatch`. This
//!    holds with `auth.enabled = false` too, so no other site's page can
//!    open the socket (cross-site WebSocket hijacking).
//! 2. The caller must be signed in, exactly as
//!    [`crate::auth::require_operator_or_user_auth`] admits: the operator
//!    (token, session, or auth disabled) or a signed-in user.
//!
//! **Topics.** The operator gets every [`LiveTopic`]. An Owner or
//! Administrator gets `library`, `jobs`, `deliveries`, `nodes` and
//! `users`; any other user gets `library` and `nodes` (the routes behind
//! them are scoped to the caller). `config`, `consents` and `clusters`
//! are operator only.
//!
//! **Frames** (server to client, JSON text):
//!
//! | Frame | When |
//! |---|---|
//! | `{"v":1,"type":"hello","topics":[...]}` | once, after the upgrade |
//! | `{"v":1,"type":"changed","topics":[...]}` | a topic's revision moved |
//!
//! Changes are found by reading [`live_revisions`] every
//! [`POLL_INTERVAL`] (the library database is shared by every owner node,
//! so this sees changes made on any of them) and by in-process pokes
//! ([`poke`]) for immediate updates. The first read, the baseline, is
//! taken before `hello` is sent, so a write made once the client has
//! `hello` is always reported. The client sends nothing; a binary
//! frame closes the socket (1003), and text is ignored. The session is
//! re-checked every [`REAUTH_INTERVAL`]: a signed-out, revoked or demoted
//! caller loses the socket (close 4401) or topics. The daemon pings every
//! 20 s (browsers answer with a pong); 60 s without a frame from the
//! client closes it.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts as _, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{changed_topics, live_revisions, LiveRevisions, LiveTopic};
use bookclerk_library::{LibraryStore, UserRole};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::json;
use tokio::sync::{broadcast, Mutex, RwLock};

use crate::api::AppState;
use crate::auth::{resolve_user_caller, UserCaller, UserCallerRefusal};

/// Path of the socket.
pub const LIVE_PATH: &str = "/api/live";

/// Subprotocol a client may ask for (accepted, not required).
pub const LIVE_SUBPROTOCOL: &str = "bookclerk-live.v1";

/// How often revisions are read.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How often the caller's session is checked again.
pub const REAUTH_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(30)
};

/// How long a re-check that was refused waits before it checks again.
const REAUTH_RETRY_DELAY: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(1)
};

/// How often the server pings.
const PING_INTERVAL: Duration = Duration::from_secs(20);

/// Silence from the client after which the socket closes.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest frame accepted from the client (it sends nothing of use).
const MAX_CLIENT_FRAME: usize = 4 * 1024;

/// Close code: the session is no longer valid.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

/// In-process pokes: topics that changed in this process just now.
static POKES: LazyLock<broadcast::Sender<LiveTopic>> = LazyLock::new(|| broadcast::channel(64).0);

/// Tells every open socket in this process that `topic` changed, without
/// waiting for the next revision read.
pub fn poke(topic: LiveTopic) {
    let _ = POKES.send(topic);
}

/// True when `host` (a `Host` header value) names a loopback host.
fn loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split_once(']').map_or("", |(inside, _)| inside)
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The one value of a header, when it appears exactly once as ASCII.
fn single(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok().map(str::trim).filter(|v| !v.is_empty())
}

/// True when the request's `Origin` is exactly the origin this socket
/// serves: `public_origin` when configured, else `http://<Host>` for a
/// loopback `Host` and no forwarding headers.
#[must_use]
pub fn origin_allowed(headers: &HeaderMap, public_origin: Option<&str>) -> bool {
    let Some(origin) = single(headers, header::ORIGIN) else {
        return false;
    };
    if let Some(configured) = crate::origin::configured_public_origin(public_origin) {
        // Browsers send the serialized origin; anything else is not it.
        return bookclerk_node_proto::canonical_origin(&configured)
            .is_some_and(|expected| origin.eq_ignore_ascii_case(&expected));
    }
    let forwarded = headers.keys().any(|name| {
        let n = name.as_str();
        n == "forwarded" || n == "via" || n == "x-real-ip" || n.starts_with("x-forwarded-")
    });
    if forwarded {
        return false;
    }
    let Some(host) = single(headers, header::HOST) else {
        return false;
    };
    loopback_host(host) && origin.eq_ignore_ascii_case(&format!("http://{host}"))
}

/// The topics `caller` may hear about; `None` when the user's role could
/// not be read (a re-check then keeps the topics it had, and a new socket
/// takes [`member_topics`]).
async fn topics_for(state: &AppState, caller: UserCaller) -> Option<BTreeSet<LiveTopic>> {
    match caller {
        UserCaller::Operator => Some(LiveTopic::ALL.into_iter().collect()),
        UserCaller::User { user_id } => {
            let library = state.library_snapshot().await;
            let role = library.get_user(user_id).await.ok()?.map(|user| user.role);
            let mut topics = member_topics();
            if matches!(role, Some(UserRole::Owner | UserRole::Administrator)) {
                topics.extend([LiveTopic::Jobs, LiveTopic::Deliveries, LiveTopic::Users]);
            }
            Some(topics)
        }
    }
}

/// The topics every signed-in user may hear about.
fn member_topics() -> BTreeSet<LiveTopic> {
    BTreeSet::from([LiveTopic::Library, LiveTopic::Nodes])
}

/// A refusal before the upgrade.
fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// `GET /api/live`.
pub async fn get_live(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    let public_origin = state.config.read().await.integrations.public_origin.clone();
    if !origin_allowed(&parts.headers, public_origin.as_deref()) {
        tracing::warn!("refused a live socket from another origin");
        return refusal(StatusCode::FORBIDDEN, "origin_mismatch");
    }
    let caller = match resolve_user_caller(&state, &parts.headers, LIVE_PATH).await {
        Ok(caller) => caller,
        Err(UserCallerRefusal::Status(status)) => return refusal(status, "unauthorized"),
        Err(UserCallerRefusal::Response(denied)) => return denied,
    };
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let topics = topics_for(&state, caller)
        .await
        .unwrap_or_else(member_topics);
    // The session headers stay in memory to re-check the caller.
    let headers = parts.headers.clone();
    upgrade
        .protocols([LIVE_SUBPROTOCOL])
        .max_message_size(MAX_CLIENT_FRAME)
        .max_frame_size(MAX_CLIENT_FRAME)
        .on_upgrade(move |socket| run(state, socket, headers, topics))
}

/// A frame naming `topics`.
fn frame(kind: &str, topics: &BTreeSet<LiveTopic>) -> Message {
    let names: Vec<&str> = topics.iter().map(|topic| topic.as_str()).collect();
    Message::Text(
        json!({ "v": 1, "type": kind, "topics": names })
            .to_string()
            .into(),
    )
}

/// How long one revision read serves every socket over the same library:
/// under [`POLL_INTERVAL`], so each socket's tick still sees a read newer
/// than its last, while the tables are read at most twice per interval
/// however many sockets are open.
const SHARED_READ_FRESH: Duration = Duration::from_millis(POLL_INTERVAL.as_millis() as u64 / 2);

/// The last revision read over one library handle, shared by its sockets.
#[derive(Default)]
struct SharedRead {
    /// When the last read finished and what it found (`None`: it failed).
    last: Option<(tokio::time::Instant, Option<LiveRevisions>)>,
    /// True while reads fail, so the failure is logged once per streak.
    failing: bool,
}

/// Shared reads keyed by the address of the library handle they read. The
/// weak handle keeps that address from being reused while the entry lives,
/// and an entry whose handle is gone is dropped.
type SharedReads = HashMap<usize, (Weak<RwLock<LibraryStore>>, Arc<Mutex<SharedRead>>)>;

/// Every [`SharedRead`] in this process.
static SHARED_READS: LazyLock<std::sync::Mutex<SharedReads>> = LazyLock::new(Default::default);

/// The [`SharedRead`] for `state`'s library handle.
fn shared_read(state: &AppState) -> Arc<Mutex<SharedRead>> {
    let key = Arc::as_ptr(&state.library) as usize;
    let mut reads = SHARED_READS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reads.retain(|_, (library, _)| library.strong_count() > 0);
    Arc::clone(
        &reads
            .entry(key)
            .or_insert_with(|| (Arc::downgrade(&state.library), Arc::default()))
            .1,
    )
}

/// Reads revisions, or reuses a read another socket over the same library
/// made within [`SHARED_READ_FRESH`]; logs once per failure streak when the
/// read fails. A reused read is older than a fresh one would be, so it can
/// only report a change again, never absorb one.
async fn read_revisions(state: &AppState) -> Option<LiveRevisions> {
    let shared = shared_read(state);
    let mut read = shared.lock().await;
    if let Some((at, revisions)) = &read.last {
        if at.elapsed() < SHARED_READ_FRESH {
            return revisions.clone();
        }
    }
    let library = state.library_snapshot().await;
    let revisions = match live_revisions(&library).await {
        Ok(revisions) => {
            read.failing = false;
            Some(revisions)
        }
        Err(err) => {
            if !read.failing {
                tracing::warn!(error = %err, "live socket could not read revisions");
            }
            read.failing = true;
            None
        }
    };
    read.last = Some((tokio::time::Instant::now(), revisions.clone()));
    revisions
}

/// Serves one socket until it closes.
async fn run(
    state: Arc<AppState>,
    socket: WebSocket,
    headers: HeaderMap,
    mut topics: BTreeSet<LiveTopic>,
) {
    let (mut tx, mut rx) = socket.split();
    // Baseline and pokes before `hello`: a write that lands after the
    // client hears `hello` (and stops its fallback polling) is then always
    // reported, never absorbed into the baseline.
    let mut pokes = POKES.subscribe();
    let mut seen = read_revisions(&state).await;
    if tx.send(frame("hello", &topics)).await.is_err() {
        return;
    }
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;
    let mut reauth = tokio::time::interval(REAUTH_INTERVAL);
    reauth.tick().await;
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    let mut last_heard = tokio::time::Instant::now();
    let close = |code: u16, reason: &'static str| {
        Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))
    };
    loop {
        let changed: BTreeSet<LiveTopic> = tokio::select! {
            _ = poll.tick() => {
                let Some(now) = read_revisions(&state).await else {
                    continue;
                };
                // Without a baseline (its read failed) nothing is known to
                // be current: every topic is reported once.
                let changed = changed_topics(seen.as_ref().unwrap_or(&Vec::new()), &now)
                    .into_iter()
                    .collect();
                seen = Some(now);
                changed
            }
            poked = pokes.recv() => match poked {
                Ok(topic) => BTreeSet::from([topic]),
                // Missed pokes: the next revision read catches up.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = reauth.tick() => {
                // A refusal is checked once more before the socket closes:
                // a session lookup that timed out reads as signed out, and
                // a client told so waits out its longest backoff.
                let mut caller = resolve_user_caller(&state, &headers, LIVE_PATH).await;
                if caller.is_err() {
                    tokio::time::sleep(REAUTH_RETRY_DELAY).await;
                    caller = resolve_user_caller(&state, &headers, LIVE_PATH).await;
                }
                match caller {
                    Ok(caller) => {
                        if let Some(now) = topics_for(&state, caller).await {
                            topics = now;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(close(CLOSE_UNAUTHORIZED, "signed out")).await;
                        return;
                    }
                }
                continue;
            }
            _ = ping.tick() => {
                if last_heard.elapsed() >= IDLE_TIMEOUT {
                    let _ = tx.send(close(1000, "idle")).await;
                    return;
                }
                if tx.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
                continue;
            }
            incoming = rx.next() => {
                match incoming {
                    Some(Ok(Message::Binary(_))) => {
                        let _ = tx.send(close(1003, "binary frames are not accepted")).await;
                        return;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => {
                        last_heard = tokio::time::Instant::now();
                        continue;
                    }
                }
            }
        };
        let visible: BTreeSet<LiveTopic> = changed.intersection(&topics).copied().collect();
        if visible.is_empty() {
            continue;
        }
        if tx.send(frame("changed", &visible)).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
