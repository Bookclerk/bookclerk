//! The clerk socket, `GET /api/clerk/v1/connect`: a clerk's live link to
//! this library, held with a library card (`bookclerk_node_proto`'s
//! `clerk_socket` module has the frames).
//!
//! **Admission.** In this order, each failure answering before the
//! upgrade:
//!
//! 1. The node API transport policy ([`super::check_transport`]): TLS
//!    except on loopback, and only where the node API is mounted.
//! 2. The request must carry a library card,
//!    `Authorization: Bookclerk-Clerk <token>`. On the main listener
//!    [`crate::clerk_token::hold_clerk_token`] has already taken it out
//!    of the headers, so it is read from the held token; the TLS-only node
//!    listener has no such layer, so there it is read from the header.
//!    Anything else, a browser cookie or the operator's token included, is
//!    401 `unauthorized`.
//! 3. The card must resolve to a person ([`crate::auth::resolve_card`]):
//!    401 `unauthorized` when it does not, 503 `session_check_unavailable`
//!    when the library did not answer.
//! 4. The request must offer [`CLERK_SUBPROTOCOL`]: 400
//!    `subprotocol_required`.
//! 5. At most [`MAX_CLERK_SOCKETS`] clerk sockets are open at once: 503
//!    `clerk_sockets_busy` with `Retry-After`.
//!
//! **Serving.** The socket sends `hello` with every [`ClerkTopic`], then
//! `changed` whenever the matching live topic moves, read exactly as the
//! SPA's live socket reads them (the shared revision read every
//! [`crate::live::POLL_INTERVAL`] and in-process pokes). It answers each
//! `books` frame, in order, with `books.page` from
//! [`person_books_page`] for the card's person, or `error`. Nothing on it
//! claims work, names a node, or reads another person's books.
//!
//! The card is checked again every [`crate::live::REAUTH_INTERVAL`]: an
//! ended, expired or disabled card, or one that now names another person,
//! closes the socket with [`CLOSE_UNAUTHORIZED`]. The daemon pings every
//! 20 s; 60 s without a frame from the clerk closes it. Binary frames and
//! frames over [`MAX_CLERK_FRAME_BYTES`] close it too.

use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts as _, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{changed_topics, person_books_page, LiveTopic};
use bookclerk_node_proto::{
    parse_clerk_frame, ClerkFrame, ClerkSocketError, ClerkTopic, LibraryFrame, CLERK_CONNECT_PATH,
    CLERK_SUBPROTOCOL, MAX_CLERK_FRAME_BYTES,
};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::json;
use tokio::sync::{broadcast, OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

use crate::api::AppState;
use crate::auth::{resolve_card, CardLookup};
use crate::clerk_token::{clerk_token_from_headers, HeldClerkToken};
use crate::live::{
    read_revisions, subscribe_pokes, CLOSE_UNAUTHORIZED, IDLE_TIMEOUT, PING_INTERVAL,
    POLL_INTERVAL, REAUTH_INTERVAL, REAUTH_RETRY_DELAY,
};

/// Most clerk sockets open at once in this process.
pub const MAX_CLERK_SOCKETS: usize = 512;

/// Permits for open clerk sockets.
static SOCKETS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CLERK_SOCKETS)));

/// The client topic a live topic is reported as, or `None` for a topic a
/// clerk does not hear about.
///
/// # Arguments
///
/// * `topic` - A topic the live revisions or a poke named.
fn clerk_topic(topic: LiveTopic) -> Option<ClerkTopic> {
    match topic {
        LiveTopic::Library => Some(ClerkTopic::Library),
        LiveTopic::Discover => Some(ClerkTopic::Discover),
        LiveTopic::Jobs
        | LiveTopic::Deliveries
        | LiveTopic::Nodes
        | LiveTopic::Config
        | LiveTopic::Consents
        | LiveTopic::Clusters
        | LiveTopic::Users => None,
    }
}

/// True when the request offers [`CLERK_SUBPROTOCOL`].
fn offers_subprotocol(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|proto| proto.trim() == CLERK_SUBPROTOCOL)
}

/// A refusal before the upgrade.
fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// `GET /api/clerk/v1/connect`: upgrade to the clerk socket after the
/// transport check and the library card.
#[utoipa::path(
    get,
    path = "/api/clerk/v1/connect",
    security(("clerkToken" = [])),
    responses(
        (status = 101, description = "Switching to the clerk socket: `hello` and `changed` frames name the topics that changed, and each `books` frame is answered with one page of the card's person's books (`books.page`)."),
        (status = 400, description = "The request does not offer the `bookclerk-clerk.v1` subprotocol (`subprotocol_required`)."),
        (status = 401, description = "No library card, or the card is not signed in (`unauthorized`)."),
        (status = 403, description = "The transport is not allowed (`transport_insecure`)."),
        (status = 404, description = "The node API is not mounted here (`node_api_unmounted`)."),
        (status = 503, description = "The card could not be checked yet (`session_check_unavailable`), or too many clerk sockets are open (`clerk_sockets_busy`); retry after `Retry-After` seconds.", headers(("Retry-After" = u64, description = "Seconds to wait before retrying."))),
    ),
)]
pub async fn get_clerk_connect(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    if let Err(refused) = super::check_transport(&state, &mut parts, "clerk_socket").await {
        return refused;
    }
    let token = parts
        .extensions
        .get::<HeldClerkToken>()
        .map(|held| held.token().to_owned())
        .or_else(|| clerk_token_from_headers(&parts.headers).map(str::to_owned))
        .map(Zeroizing::new);
    let Some(token) = token else {
        return refusal(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let person = match resolve_card(&state, &token, CLERK_CONNECT_PATH).await {
        CardLookup::Person(user_id) => user_id,
        CardLookup::SignedOut => return refusal(StatusCode::UNAUTHORIZED, "unauthorized"),
        CardLookup::Unavailable => return crate::auth::session_check_unavailable(),
        CardLookup::Refused(denied) => return denied,
    };
    if !offers_subprotocol(&parts.headers) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "subprotocol_required", "subprotocol": CLERK_SUBPROTOCOL })),
        )
            .into_response();
    }
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(permit) = Arc::clone(&SOCKETS).try_acquire_owned() else {
        tracing::warn!("clerk socket refused: too many open");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "10")],
            Json(json!({ "error": "clerk_sockets_busy" })),
        )
            .into_response();
    };
    upgrade
        .protocols([CLERK_SUBPROTOCOL])
        .max_message_size(MAX_CLERK_FRAME_BYTES)
        .max_frame_size(MAX_CLERK_FRAME_BYTES)
        .on_upgrade(move |socket| run(state, socket, token, person, permit))
}

/// A text frame carrying `frame`.
fn text(frame: &LibraryFrame) -> Message {
    Message::Text(frame.encode().into())
}

/// A close frame.
fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

/// The answer to one clerk frame.
///
/// # Arguments
///
/// * `state` - The library to read.
/// * `person` - The card's person.
/// * `frame` - The frame's bytes.
async fn answer(state: &AppState, person: i64, frame: &[u8]) -> LibraryFrame {
    let Some(ClerkFrame::Books { after }) = parse_clerk_frame(frame) else {
        return LibraryFrame::Error {
            code: ClerkSocketError::Invalid,
        };
    };
    let library = state.library_snapshot().await;
    match person_books_page(&library, person, after).await {
        Ok((works, next)) => LibraryFrame::BooksPage { after, works, next },
        Err(err) => {
            tracing::warn!(error = %err, "clerk socket could not read a page of books");
            LibraryFrame::Error {
                code: ClerkSocketError::Unavailable,
            }
        }
    }
}

/// Serves one socket until it closes. `_permit` holds its place under
/// [`MAX_CLERK_SOCKETS`].
async fn run(
    state: Arc<AppState>,
    socket: WebSocket,
    token: Zeroizing<String>,
    person: i64,
    _permit: OwnedSemaphorePermit,
) {
    let (mut tx, mut rx) = socket.split();
    // Baseline and pokes before `hello`, as on the live socket: a change
    // made once the clerk has `hello` is always reported.
    let mut pokes = subscribe_pokes();
    let mut seen = read_revisions(&state).await;
    let hello = LibraryFrame::Hello {
        topics: ClerkTopic::ALL.into_iter().collect(),
    };
    if tx.send(text(&hello)).await.is_err() {
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
    loop {
        let changed: BTreeSet<LiveTopic> = tokio::select! {
            _ = poll.tick() => {
                let Some(now) = read_revisions(&state).await else {
                    continue;
                };
                let changed = changed_topics(seen.as_ref().unwrap_or(&Vec::new()), &now)
                    .into_iter()
                    .collect();
                seen = Some(now);
                changed
            }
            poked = pokes.recv() => match poked {
                Ok(topic) => BTreeSet::from([topic]),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = reauth.tick() => {
                // A refusal is checked once more before the socket closes,
                // as on the live socket.
                let mut card = resolve_card(&state, &token, CLERK_CONNECT_PATH).await;
                if !matches!(card, CardLookup::Person(_) | CardLookup::Unavailable) {
                    tokio::time::sleep(REAUTH_RETRY_DELAY).await;
                    card = resolve_card(&state, &token, CLERK_CONNECT_PATH).await;
                }
                match card {
                    CardLookup::Person(user_id) if user_id == person => {}
                    // The library could not say: keep the socket and check
                    // again on the next tick.
                    CardLookup::Unavailable => {}
                    CardLookup::Person(_) | CardLookup::SignedOut | CardLookup::Refused(_) => {
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
                    Some(Ok(Message::Text(frame))) => {
                        last_heard = tokio::time::Instant::now();
                        let reply = answer(&state, person, frame.as_bytes()).await;
                        if tx.send(text(&reply)).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let _ = tx.send(close(1003, "binary frames are not accepted")).await;
                        return;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        last_heard = tokio::time::Instant::now();
                        continue;
                    }
                }
            }
        };
        let topics: BTreeSet<ClerkTopic> = changed.into_iter().filter_map(clerk_topic).collect();
        if topics.is_empty() {
            continue;
        }
        if tx
            .send(text(&LibraryFrame::Changed { topics }))
            .await
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
#[path = "clerk_socket_tests.rs"]
mod tests;
