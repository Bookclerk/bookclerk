//! The player API's live socket at `/player/socket.io`: Socket.IO over a
//! websocket only (Engine.IO 4, no long polling), as apps written for an
//! Audiobookshelf server open it.
//!
//! The socket opens without a card; the app sends `auth` with its access
//! token (or puts it in the connect packet), and the host answers `init`
//! or `auth_failed`. Once signed in, the socket sends the patron's
//! progress changes (`user_item_progress_updated`) and items newly added
//! to what they may see (`item_added`), read from the same revisions the
//! web app's live socket watches. The card is checked again every minute;
//! a card that ended closes the socket.
//!
//! Every frame an app sends goes through
//! [`bookclerk_player_api::socket::parse_client_frame`], the code the
//! `player_socket` fuzz target drives.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts as _, Request, State};
use axum::response::{IntoResponse, Response};
use bookclerk_library::control_plane::{changed_topics, LiveTopic};
use bookclerk_library::{WorkPageQuery, WorkSort};
use bookclerk_player_api::socket::{
    bare_event_frame, connect_error_frame, connect_frame, event_frame, open_frame,
    parse_client_frame, ClientEvent, ClientFrame, ENGINE_IO_REVISION, MAX_CLIENT_FRAME_BYTES,
    PING_FRAME, PING_INTERVAL_MS, PING_TIMEOUT_MS, PONG_FRAME,
};
use bookclerk_player_api::wire::{ProgressEvent, SocketAuthFailed, SocketInit};
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::broadcast;
use zeroize::Zeroizing;

use super::catalog::item_wire;
use super::listening::progress_wire;
use super::{resolve_card, username, CardRefusal, Patron, PlayerError};
use crate::api::AppState;
use crate::live::{read_revisions, subscribe_pokes, POLL_INTERVAL};

/// The socket's path without its trailing slash, for the second-factor
/// rule's exemptions.
const SOCKET_PATH: &str = "/player/socket.io";

/// How often a signed-in socket checks its card again.
const RECHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Progress rows read per change, newest first.
const PROGRESS_ROWS: u64 = 50;

/// Newest items read per library change.
const NEW_ITEMS: u64 = 20;

/// The message apps show when the token is refused.
const AUTH_FAILED: &str = "Invalid token";

/// Opens the live socket.
#[utoipa::path(
    get,
    path = "/player/socket.io/",
    params(
        ("EIO" = String, Query, description = "`4`."),
        ("transport" = String, Query, description = "`websocket`; long polling is not served."),
    ),
    responses(
        (status = 101, description = "Switching to the Socket.IO websocket."),
        (status = 400, description = "Not a websocket upgrade, another Engine.IO revision, or long polling.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_socket(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    let query = parts.uri.query().unwrap_or_default();
    let mut eio = None;
    let mut transport = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "EIO" => eio = Some(value.into_owned()),
            "transport" => transport = Some(value.into_owned()),
            _ => {}
        }
    }
    if eio.as_deref() != Some(ENGINE_IO_REVISION) || transport.as_deref() != Some("websocket") {
        return PlayerError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "Only Engine.IO 4 over a websocket is served",
        )
        .into_response();
    }
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade
        .max_message_size(MAX_CLIENT_FRAME_BYTES)
        .max_frame_size(MAX_CLIENT_FRAME_BYTES)
        .on_upgrade(move |socket| run(state, socket))
}

/// Opens the live socket for a client that drops the trailing slash; the
/// same socket as [`get_socket`].
#[utoipa::path(
    get,
    path = "/player/socket.io",
    params(
        ("EIO" = String, Query, description = "`4`."),
        ("transport" = String, Query, description = "`websocket`; long polling is not served."),
    ),
    responses(
        (status = 101, description = "Switching to the Socket.IO websocket."),
        (status = 400, description = "Not a websocket upgrade, another Engine.IO revision, or long polling.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_socket_without_slash(
    state: State<Arc<AppState>>,
    request: Request,
) -> Response {
    get_socket(state, request).await
}

/// A fresh, unguessable socket id.
fn socket_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// What a signed-in socket has already told the app.
struct Watermarks {
    /// The newest progress change sent, in epoch milliseconds.
    progress_ms: i64,
    /// The newest item addition sent, in epoch milliseconds.
    added_ms: i64,
}

/// The newest progress change and item addition the patron has now.
async fn watermarks(patron: &Patron) -> Watermarks {
    let progress_ms = patron
        .library
        .list_playback_progress(patron.user_id(), true, None, 1)
        .await
        .ok()
        .and_then(|(rows, _)| rows.first().map(|p| p.updated_at.timestamp_millis()))
        .unwrap_or(0);
    let added_ms = newest_items(patron)
        .await
        .first()
        .map_or(0, |item| item.added_at.timestamp_millis());
    Watermarks {
        progress_ms,
        added_ms,
    }
}

/// The patron's newest items, newest first; empty when the read fails.
async fn newest_items(patron: &Patron) -> Vec<bookclerk_library::PlayerItem> {
    let query = WorkPageQuery {
        sort: WorkSort::Added,
        media_kind: Some(bookclerk_library::MediaKind::Audio),
        status: Some(bookclerk_library::AcquireStatus::Acquired),
        scope: patron.scope.clone(),
        ..WorkPageQuery::default()
    };
    match patron
        .library
        .player_items_page(&query, 0, NEW_ITEMS, false)
        .await
    {
        Ok((items, _)) => items,
        Err(err) => {
            tracing::warn!(error = %err, "player socket could not read new items");
            Vec::new()
        }
    }
}

/// The frames for what changed since `marks`, advancing them.
async fn changes(patron: &Patron, topics: &[LiveTopic], marks: &mut Watermarks) -> Vec<String> {
    let mut frames = Vec::new();
    if topics.contains(&LiveTopic::Playback) {
        if let Ok((rows, _)) = patron
            .library
            .list_playback_progress(patron.user_id(), true, None, PROGRESS_ROWS)
            .await
        {
            let mut newest = marks.progress_ms;
            for progress in rows.into_iter().rev() {
                let at = progress.updated_at.timestamp_millis();
                if at <= marks.progress_ms {
                    continue;
                }
                newest = newest.max(at);
                let data = progress_wire(patron.user_id(), progress);
                frames.push(event_frame(
                    "user_item_progress_updated",
                    &ProgressEvent {
                        id: data.id.clone(),
                        session_id: None,
                        device_description: String::new(),
                        data,
                    },
                ));
            }
            marks.progress_ms = newest;
        }
    }
    if topics.contains(&LiveTopic::Library) {
        let mut newest = marks.added_ms;
        for item in newest_items(patron).await.into_iter().rev() {
            let at = item.added_at.timestamp_millis();
            if at <= marks.added_ms {
                continue;
            }
            newest = newest.max(at);
            frames.push(event_frame("item_added", &item_wire(&item, None, None)));
        }
        marks.added_ms = newest;
    }
    frames
}

/// Signs the socket in with `token`.
///
/// # Returns
///
/// The patron and the frame to send (`init`), or the refusal frame
/// (`auth_failed`).
async fn sign_in(state: &AppState, token: &str) -> Result<(Patron, String), String> {
    let token = Zeroizing::new(token.to_owned());
    match resolve_card(state, &token, SOCKET_PATH).await {
        Ok(patron) => {
            let frame = event_frame(
                "init",
                &SocketInit {
                    user_id: patron.user_id().to_string(),
                    username: username(&patron.user),
                },
            );
            Ok((patron, frame))
        }
        Err(_) => Err(event_frame(
            "auth_failed",
            &SocketAuthFailed {
                message: AUTH_FAILED,
            },
        )),
    }
}

/// Serves one socket until it closes.
async fn run(state: Arc<AppState>, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let sid = socket_id();
    if tx
        .send(Message::Text(open_frame(&sid).into()))
        .await
        .is_err()
    {
        return;
    }
    let mut patron: Option<Patron> = None;
    let mut marks = Watermarks {
        progress_ms: 0,
        added_ms: 0,
    };
    let mut pokes = subscribe_pokes();
    let mut seen = None;
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;
    let mut ping = tokio::time::interval(Duration::from_millis(PING_INTERVAL_MS));
    ping.tick().await;
    let mut recheck = tokio::time::interval(RECHECK_INTERVAL);
    recheck.tick().await;
    let mut last_pong = tokio::time::Instant::now();
    let timeout = Duration::from_millis(PING_INTERVAL_MS + PING_TIMEOUT_MS);
    loop {
        let mut out: Vec<String> = Vec::new();
        let mut topics: Vec<LiveTopic> = Vec::new();
        tokio::select! {
            frame = rx.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Binary(_))) | Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                };
                let Ok(frame) = parse_client_frame(text.as_str()) else {
                    return;
                };
                match frame {
                    ClientFrame::Close | ClientFrame::Disconnect => return,
                    ClientFrame::Ping => out.push(PONG_FRAME.to_owned()),
                    ClientFrame::Pong => last_pong = tokio::time::Instant::now(),
                    ClientFrame::Noop => {}
                    ClientFrame::Connect { default_namespace: false, .. } => {
                        out.push(connect_error_frame("Invalid namespace"));
                    }
                    ClientFrame::Connect { default_namespace: true, token } => {
                        out.push(connect_frame(&sid));
                        if let Some(token) = token {
                            match sign_in(&state, &token).await {
                                Ok((signed_in, frame)) => {
                                    marks = watermarks(&signed_in).await;
                                    seen = read_revisions(&state).await;
                                    patron = Some(signed_in);
                                    out.push(frame);
                                }
                                Err(frame) => out.push(frame),
                            }
                        }
                    }
                    ClientFrame::Event { event: ClientEvent::Auth(token), .. } => {
                        match sign_in(&state, &token).await {
                            Ok((signed_in, frame)) => {
                                marks = watermarks(&signed_in).await;
                                seen = read_revisions(&state).await;
                                patron = Some(signed_in);
                                out.push(frame);
                            }
                            Err(frame) => {
                                patron = None;
                                out.push(frame);
                            }
                        }
                    }
                    ClientFrame::Event { event: ClientEvent::Ping, .. } => {
                        out.push(bare_event_frame("pong"));
                    }
                    ClientFrame::Event { event: ClientEvent::Other(_), .. } => {}
                }
            }
            _ = ping.tick() => {
                if last_pong.elapsed() > timeout {
                    return;
                }
                out.push(PING_FRAME.to_owned());
            }
            _ = poll.tick(), if patron.is_some() => {
                if let Some(now) = read_revisions(&state).await {
                    topics = changed_topics(seen.as_ref().unwrap_or(&Vec::new()), &now);
                    seen = Some(now);
                }
            }
            poked = pokes.recv(), if patron.is_some() => match poked {
                Ok(topic) => topics.push(topic),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = recheck.tick(), if patron.is_some() => {
                let token = patron.as_ref().map(|p| p.token.clone());
                if let Some(token) = token {
                    match resolve_card(&state, token.as_str(), SOCKET_PATH).await {
                        Ok(fresh) => patron = Some(fresh),
                        // A check that could not run keeps the socket.
                        Err(CardRefusal::Unavailable) => {}
                        Err(CardRefusal::SignedOut | CardRefusal::EnrollFirst) => {
                            let _ = tx
                                .send(Message::Text(
                                    event_frame("auth_failed", &SocketAuthFailed { message: AUTH_FAILED }).into(),
                                ))
                                .await;
                            return;
                        }
                    }
                }
            }
        }
        if let Some(signed_in) = patron.as_ref() {
            if !topics.is_empty() {
                out.extend(changes(signed_in, &topics, &mut marks).await);
            }
        }
        for frame in out {
            if tx.send(Message::Text(frame.into())).await.is_err() {
                return;
            }
        }
    }
}
