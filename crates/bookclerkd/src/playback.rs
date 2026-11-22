//! The built-in player's routes (`docs/adr/playback.md`): what a signed-in
//! person may play, their play sessions, the audio itself, their progress
//! and their bookmarks.
//!
//! | Route | What it does |
//! |---|---|
//! | `GET /api/library/editions/{id}/playback` | Whether this node can play the edition for the caller, its tracks, and the caller's progress and bookmarks |
//! | `POST /api/playback/sessions` | Opens a play session: the tracks with their stream paths, and where to start |
//! | `POST /api/playback/sessions/{id}/sync` | Reports the position while playing |
//! | `POST /api/playback/sessions/{id}/close` | The last report; the session ends |
//! | `GET /api/playback/sessions/{id}/tracks/{n}` | One track's bytes, whole or by `Range` |
//! | `GET /api/playback/progress` | The caller's progress, most recent first (Continue listening) |
//! | `PUT`, `DELETE /api/playback/progress/{edition_id}` | Sets or forgets the caller's progress outright |
//! | `GET`, `POST /api/library/editions/{id}/bookmarks` | The caller's bookmarks in an edition |
//! | `PUT`, `DELETE /api/playback/bookmarks/{id}` | Changes or removes one |
//!
//! Everything here belongs to the signed-in library user: the operator
//! token has no user and is answered 403. An edition outside the caller's
//! patron scope reads as unknown (404), exactly as on the library's pages.
//!
//! **One check.** Whether a person may play an edition, and which files
//! that means, is decided in one place
//! ([`bookclerk_library::control_plane::plan_playback`], the rule a copy
//! grant uses). The manifest, a new session and every stream request run
//! it, so a right that ends stops the next request rather than waiting for
//! a session to close.
//!
//! **Streams.** A track request must come from the session's own person,
//! on the node that opened the session, while the session is open and was
//! last reported within [`STREAM_IDLE_SECS`]; the edition's files must
//! still be the ones the session listed. Any of these failing is 409
//! `session_ended`: the player opens a new session, which then gives the
//! real answer. Bytes are streamed from the destination, never staged
//! here, by the same code a copy download uses
//! ([`crate::node_api::copies::serve_file`]).
//!
//! The session cookie is the credential on every route, the stream route
//! included, so an `<audio>` element on the app's own origin plays with no
//! token in a URL.

use std::sync::Arc;

use axum::extract::{FromRequest, Path as AxumPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    authorize_playback, load_or_create_node_identity, plan_playback, PlaybackPlan, PlaybackTrack,
};
use bookclerk_library::{
    parse_progress_cursor, parse_track_path, progress_cursor_token, Bookmark, BookmarkEditBody,
    EntityKind, LibraryError, LibraryStore, NewBookmarkBody, NewPlaybackSession, OpenSessionBody,
    PlaybackChange, PlaybackEdition, PlaybackEvent, PlaybackMs, PlaybackProgress, PlaybackSession,
    PlaybackSyncBody, ProgressEditBody, MAX_PLAYBACK_BODY_BYTES, MAX_PLAYBACK_SYNC_BYTES,
    MAX_PLAYBACK_TRACKS,
};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::http_error::ErrorMessage;
use crate::library_works::{caller_scope, caller_user_id, read_failed, require_in_scope};
use crate::node_api::copies::serve_file;
use crate::node_api::uploads::{owner_destinations, OwnerDestination};

/// Seconds after its last report that a session still streams. A player
/// reports every few seconds while it plays; one silent for a day opens a
/// new session to go on.
pub(crate) const STREAM_IDLE_SECS: i64 = 24 * 60 * 60;

/// Default page size of `GET /api/playback/progress`.
const DEFAULT_PROGRESS_LIMIT: u64 = 20;

/// Largest page size of `GET /api/playback/progress`.
const MAX_PROGRESS_LIMIT: u64 = 100;

/// An error answer of these routes: status and JSON message.
type PlaybackError = (StatusCode, Json<ErrorMessage>);

/// A JSON request body of at most `MAX` bytes: the length limit of the
/// sans-IO parser the fuzz target drives, applied before the same JSON
/// reading, so the route accepts no body the parser refuses for its size.
///
/// A longer body is answered `413`; everything else is [`Json`]'s own
/// answer (`415` without the JSON content type, `400` for bytes that are
/// not JSON, `422` for JSON of another shape).
pub(crate) struct BoundedJson<T, const MAX: usize>(
    /// The body, read from JSON.
    pub(crate) T,
);

impl<S, T, const MAX: usize> FromRequest<S> for BoundedJson<T, MAX>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (parts, body) = request.into_parts();
        // One byte past the limit is enough to know the body is too long.
        let bytes = axum::body::to_bytes(body, MAX.saturating_add(1))
            .await
            .map_err(|_| too_long(MAX))?;
        if bytes.len() > MAX {
            return Err(too_long(MAX));
        }
        let request = Request::from_parts(parts, axum::body::Body::from(bytes));
        match Json::<T>::from_request(request, state).await {
            Ok(Json(body)) => Ok(Self(body)),
            Err(rejection) => Err(rejection.into_response()),
        }
    }
}

/// The 413 a body over `max` bytes gets.
fn too_long(max: usize) -> Response {
    refuse(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!("the body is at most {max} bytes"),
    )
    .into_response()
}

/// An error answer with `status` and `message`.
fn refuse(status: StatusCode, message: impl Into<String>) -> PlaybackError {
    (status, Json(ErrorMessage::new(message.into())))
}

/// The 404 an unknown edition gets, and one outside the caller's scope.
fn no_edition(id: &str) -> PlaybackError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorMessage::new(LibraryError::not_found(
            EntityKind::Edition,
            id,
        ))),
    )
}

/// Logs a failed write and returns the 500 answer.
fn write_failed(route: &str, err: &LibraryError) -> PlaybackError {
    tracing::error!(error = %err, route, "playback write failed");
    refuse(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the listening state could not be written",
    )
}

/// The signed-in library user, or the 403 a session without one gets.
///
/// In the desktop window ([`AppState::in_window`]) the listener is the
/// patron the window is signed in as, whoever the request acts as: while
/// that patron is elevated to the operator, or impersonating someone from
/// the elevation, their own listening state stays theirs
/// (`elevated_from_user_id`). Before the library has an Owner the window
/// is the operator token and has nothing to play as. A daemon with a
/// listener plays as the caller, so impersonation there plays as the
/// person impersonated, as every other per-person route does.
///
/// # Errors
///
/// A 403 for the operator token, an operator session nobody elevated
/// from (in the window), or a session linked to no library user.
async fn listener(state: &AppState, headers: &HeaderMap) -> Result<i64, PlaybackError> {
    let no_user = || {
        refuse(
            StatusCode::FORBIDDEN,
            "this session has no library user to keep listening state for",
        )
    };
    if state.in_window.get().is_some() {
        // The window plays as its own patron even while elevated or
        // impersonating: the listening state belongs to the person at
        // this computer, who started the elevation.
        let auth = state.auth_snapshot().await;
        if let Some(op) = crate::auth::resolve_operator_session(state, &auth, headers).await {
            return op.elevated_from_user_id.ok_or_else(no_user);
        }
    }
    caller_user_id(state, headers).await.ok_or_else(no_user)
}

/// The edition `id` when it exists and the caller's patron scope shows
/// its work; the same 404 otherwise.
///
/// # Errors
///
/// A 404 for an unknown or hidden edition, a 401 when the session ended,
/// a 500 when the library cannot be read.
async fn edition_in_scope(
    state: &AppState,
    library: &LibraryStore,
    headers: &HeaderMap,
    id: &str,
    route: &str,
) -> Result<PlaybackEdition, PlaybackError> {
    let edition = library
        .playback_edition(id)
        .await
        .map_err(|err| read_failed(route, &err))?
        .ok_or_else(|| no_edition(id))?;
    let scope = caller_scope(state, library, headers, route).await?;
    // An edition between two works has no work to check a scope against:
    // only a caller no scope limits may see it.
    match (&edition.work_id, scope.as_ref()) {
        (Some(work_id), scope) => {
            require_in_scope(library, scope, work_id, EntityKind::Edition, id, route).await?;
        }
        (None, Some(_)) => return Err(no_edition(id)),
        (None, None) => {}
    }
    Ok(edition)
}

/// The first of this node's destinations, in its own order, that `plan`
/// lists tracks on.
pub(crate) fn playable_here<'a>(
    plan: &'a PlaybackPlan,
    destinations: &'a [OwnerDestination],
) -> Option<(&'a OwnerDestination, &'a [PlaybackTrack])> {
    destinations.iter().find_map(|destination| {
        plan.destinations
            .get(&destination.id)
            .filter(|tracks| !tracks.is_empty() && tracks.len() <= MAX_PLAYBACK_TRACKS)
            .map(|tracks| (destination, tracks.as_slice()))
    })
}

/// The media types a track is stated as; the stream route's document
/// lists exactly these.
const TRACK_MEDIA_TYPES: [&str; 7] = [
    "audio/mp4",
    "audio/mpeg",
    "audio/aac",
    "audio/flac",
    "audio/ogg",
    "audio/wav",
    "application/octet-stream",
];

/// The media type to state for a track: the probe's when it is one of
/// [`TRACK_MEDIA_TYPES`], else one read from the key's extension, else
/// `application/octet-stream`. Never a value taken from stored data as
/// is.
pub(crate) fn track_content_type(track: &PlaybackTrack) -> HeaderValue {
    let probed = track.content_type.as_deref().and_then(|probed| {
        let essence = probed.split(';').next().unwrap_or_default().trim();
        TRACK_MEDIA_TYPES
            .iter()
            .find(|known| known.eq_ignore_ascii_case(essence))
    });
    if let Some(known) = probed {
        return HeaderValue::from_static(known);
    }
    let extension = track
        .key
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    HeaderValue::from_static(match extension.as_str() {
        "m4b" | "m4a" | "mp4" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    })
}

/// One person's progress in one edition.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ProgressJson {
    /// The edition.
    #[schema(example = "5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d")]
    edition_id: String,
    /// The edition's work, when known: what a Continue listening shelf
    /// reads the title and cover from.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    work_id: Option<String>,
    /// Position on the edition's timeline, in milliseconds.
    #[schema(example = 754_000)]
    position_ms: u64,
    /// The edition's runtime in milliseconds as the last player reported
    /// it; null until one has.
    #[schema(example = 41_400_000)]
    duration_ms: Option<u64>,
    /// True once the caller reached the end of this edition.
    #[schema(example = false)]
    is_finished: bool,
    /// When the caller first played it (RFC 3339).
    #[schema(example = "2026-11-19T08:15:00+00:00")]
    started_at: String,
    /// When the caller last reached the end (RFC 3339).
    #[schema(example = json!(null))]
    finished_at: Option<String>,
    /// When the progress last changed (RFC 3339).
    #[schema(example = "2026-11-19T09:02:41+00:00")]
    updated_at: String,
}

/// The wire form of a progress row.
fn progress_json(progress: PlaybackProgress, work_id: Option<String>) -> ProgressJson {
    ProgressJson {
        edition_id: progress.edition_id,
        work_id,
        position_ms: progress.position.get(),
        duration_ms: progress.duration.map(PlaybackMs::get),
        is_finished: progress.is_finished,
        started_at: progress.started_at.to_rfc3339(),
        finished_at: progress.finished_at.map(|at| at.to_rfc3339()),
        updated_at: progress.updated_at.to_rfc3339(),
    }
}

/// A place the caller marked in an edition.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct BookmarkJson {
    /// Bookmark id.
    #[schema(example = "9a1b2c3d-4e5f-4a6b-8c7d-0e1f2a3b4c5d")]
    id: String,
    /// The edition it is in.
    #[schema(example = "5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d")]
    edition_id: String,
    /// The marked place, in milliseconds from the edition's start.
    #[schema(example = 754_000)]
    position_ms: u64,
    /// The caller's note; may be empty.
    #[schema(example = "The storm")]
    title: String,
    /// When it was made (RFC 3339).
    #[schema(example = "2026-11-19T09:02:41+00:00")]
    created_at: String,
}

/// The wire form of a bookmark.
fn bookmark_json(bookmark: Bookmark) -> BookmarkJson {
    BookmarkJson {
        id: bookmark.id,
        edition_id: bookmark.edition_id,
        position_ms: bookmark.position.get(),
        title: bookmark.title,
        created_at: bookmark.created_at.to_rfc3339(),
    }
}

/// One audio file of an edition, as a player sees it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct TrackJson {
    /// The track's place in play order, from 0.
    #[schema(example = 0)]
    index: usize,
    /// Path to stream it from, on this origin; only in a session's answer.
    #[schema(example = "/api/playback/sessions/7c9e6679-7425-40de-944b-e07fc1f90ae7/tracks/0")]
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// Media type the stream states, such as `audio/mp4`.
    #[schema(example = "audio/mp4")]
    content_type: String,
    /// Average bitrate in kilobits per second, once probed.
    #[schema(example = 125)]
    bitrate_kbps: Option<u32>,
    /// True for a lossless encoding.
    #[schema(example = false)]
    lossless: bool,
}

/// The wire form of `tracks`, with stream paths when `session_id` is
/// given.
fn tracks_json(tracks: &[PlaybackTrack], session_id: Option<&str>) -> Vec<TrackJson> {
    tracks
        .iter()
        .enumerate()
        .map(|(index, track)| TrackJson {
            index,
            path: session_id.map(|id| format!("/api/playback/sessions/{id}/tracks/{index}")),
            content_type: track_content_type(track)
                .to_str()
                .unwrap_or("application/octet-stream")
                .to_string(),
            bitrate_kbps: track.bitrate_kbps,
            lossless: track.lossless,
        })
        .collect()
}

/// `GET /api/library/editions/{id}/playback` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PlaybackManifestJson {
    /// The edition.
    #[schema(example = "5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d")]
    edition_id: String,
    /// Its work.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    work_id: Option<String>,
    /// The edition's display title.
    #[schema(example = "The Left Hand of Darkness")]
    title: String,
    /// True when this node holds a copy the caller may play. False when
    /// the library has not acquired the edition, no destination of this
    /// node holds it, or the caller may not take it.
    #[schema(example = true)]
    playable: bool,
    /// Runtime in milliseconds: the last player's report when the caller
    /// has one, else the catalog's, to the minute; null when neither is
    /// known.
    #[schema(example = 41_400_000)]
    duration_ms: Option<u64>,
    /// The tracks in play order; empty when not playable. A session's
    /// answer adds the stream paths.
    tracks: Vec<TrackJson>,
    /// The caller's progress; null before they first play it.
    progress: Option<ProgressJson>,
    /// The caller's bookmarks, in timeline order.
    bookmarks: Vec<BookmarkJson>,
    /// The chapters the edition plays, in pre-order, each ending where the
    /// next starts and the last at `duration_ms`; empty when it has none.
    chapters: Vec<bookclerk_library::ChapterSpan>,
}

/// Reads whether and what the caller may play of an edition, with their
/// own progress and bookmarks.
#[utoipa::path(
    get,
    path = "/api/library/editions/{id}/playback",
    params(("id" = String, Path, description = "Edition id (an `id` in a work's `editions`).")),
    responses(
        (status = 200, description = "What the caller may play, and where they are.", body = PlaybackManifestJson),
        (status = 401, description = "The session ended.", body = ErrorMessage, example = json!({"error": "sign in again"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d not found"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn get_edition_playback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<PlaybackManifestJson>, PlaybackError> {
    const ROUTE: &str = "GET /api/library/editions/{id}/playback";
    let user_id = listener(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let edition = edition_in_scope(&state, &library, &headers, &id, ROUTE).await?;
    let plan = plan_playback(&library, user_id, &edition.edition_id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?;
    // The copy this node plays: its destination and its tracks' keys.
    let (tracks, copy) = match plan {
        Some(plan) => match owner_destinations(&state).await {
            Ok(destinations) => playable_here(&plan, &destinations)
                .map(|(destination, tracks)| {
                    let keys: Vec<String> = tracks.iter().map(|track| track.key.clone()).collect();
                    (
                        tracks_json(tracks, None),
                        Some((destination.id.clone(), keys)),
                    )
                })
                .unwrap_or_default(),
            Err(err) => {
                tracing::warn!(error = %err, route = ROUTE, "no output destination to play from");
                (Vec::new(), None)
            }
        },
        None => (Vec::new(), None),
    };
    let progress = library
        .playback_progress(user_id, &edition.edition_id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?;
    let bookmarks = library
        .list_bookmarks(user_id, &edition.edition_id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?;
    let duration = progress
        .as_ref()
        .and_then(|progress| progress.duration)
        .or(edition.runtime);
    let chapters = match &copy {
        Some((destination, keys)) => {
            library
                .chapters_for_copy(&edition.edition_id, destination, keys)
                .await
        }
        None => library.edition_chapters(&edition.edition_id).await,
    }
    .map_err(|err| read_failed(ROUTE, &err))?
    .map(|resolved| {
        let end = duration.map(PlaybackMs::get).or(resolved.runtime_ms);
        resolved.list.spans(end)
    })
    .unwrap_or_default();
    Ok(Json(PlaybackManifestJson {
        chapters,
        playable: !tracks.is_empty(),
        duration_ms: duration.map(PlaybackMs::get),
        tracks,
        progress: progress.map(|progress| progress_json(progress, edition.work_id.clone())),
        bookmarks: bookmarks.into_iter().map(bookmark_json).collect(),
        edition_id: edition.edition_id,
        work_id: edition.work_id,
        title: edition.title,
    }))
}

/// `POST /api/playback/sessions` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct SessionJson {
    /// The session's id, for its sync, close and track routes.
    #[schema(example = "7c9e6679-7425-40de-944b-e07fc1f90ae7")]
    session_id: String,
    /// The edition being played.
    #[schema(example = "5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d")]
    edition_id: String,
    /// Its work.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    work_id: Option<String>,
    /// Where to start, in milliseconds: the caller's stored position, or
    /// 0 for a first listen or an edition they had finished.
    #[schema(example = 754_000)]
    position_ms: u64,
    /// Runtime in milliseconds when known (see the manifest).
    #[schema(example = 41_400_000)]
    duration_ms: Option<u64>,
    /// The tracks in play order, each with the path to stream it from.
    tracks: Vec<TrackJson>,
    /// Seconds without a report after which the session stops streaming
    /// and the player opens a new one.
    #[schema(example = 86_400)]
    idle_secs: i64,
}

/// Opens a play session for the caller.
#[utoipa::path(
    post,
    path = "/api/playback/sessions",
    request_body = OpenSessionBody,
    responses(
        (status = 200, description = "The session, its tracks and where to start.", body = SessionJson),
        (status = 400, description = "The edition id is empty or over 128 bytes.", body = ErrorMessage, example = json!({"error": "edition_id must be 1 to 128 bytes"})),
        (status = 413, description = "The body is over 4096 bytes.", body = ErrorMessage, example = json!({"error": "the body is at most 4096 bytes"})),
        (status = 401, description = "The session ended.", body = ErrorMessage, example = json!({"error": "sign in again"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such edition (`edition ... not found`), or no copy of it on this node that the caller may play (`not_playable`).", body = ErrorMessage, examples(("unknown" = (summary = "No such edition", value = json!({"error": "edition 5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d not found"}))), ("not_playable" = (summary = "Nothing to play here", value = json!({"error": "not_playable"}))))),
        (status = 500, description = "The library could not be read or written.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
        (status = 503, description = "This node's storage could not be asked; retry (`unavailable`).", body = ErrorMessage, example = json!({"error": "unavailable"})),
    ),
)]
pub(crate) async fn post_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    BoundedJson(body): BoundedJson<OpenSessionBody, MAX_PLAYBACK_BODY_BYTES>,
) -> Result<Json<SessionJson>, PlaybackError> {
    const ROUTE: &str = "POST /api/playback/sessions";
    // The same check the fuzz target drives (`parse_open_session_request`).
    let open = checked(body.validate(), ROUTE)?;
    let user_id = listener(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let edition = edition_in_scope(&state, &library, &headers, &open.edition_id, ROUTE).await?;
    let not_playable = || refuse(StatusCode::NOT_FOUND, "not_playable");
    let unavailable = || refuse(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    let plan = plan_playback(&library, user_id, &edition.edition_id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?
        .ok_or_else(not_playable)?;
    let destinations = owner_destinations(&state).await.map_err(|err| {
        tracing::warn!(error = %err, route = ROUTE, "no output destination to play from");
        unavailable()
    })?;
    let (destination, tracks) = playable_here(&plan, &destinations).ok_or_else(not_playable)?;
    let digest = plan
        .digest(&destination.id)
        .ok_or_else(not_playable)?
        .to_string();
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let this_node = load_or_create_node_identity(&files_dir).map_err(|err| {
        tracing::warn!(error = %err, route = ROUTE, "no node identity to play from");
        unavailable()
    })?;
    let session = library
        .open_playback_session(NewPlaybackSession {
            user_id,
            edition_id: &edition.edition_id,
            serving_node_id: &this_node.node_id,
            destination: &destination.id,
            files_digest: &digest,
            client_kind: open.player,
            client_name: &open.player_name,
        })
        .await
        .map_err(|err| write_failed(ROUTE, &err))?;
    let progress = library
        .playback_progress(user_id, &edition.edition_id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?;
    if let Some(progress) = &progress {
        library
            .publish_playback_event(
                PlaybackEvent::Started,
                progress,
                edition.work_id.as_deref(),
                Some(&session.id),
            )
            .await;
    }
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Playback);
    tracing::info!(session_id = %session.id, edition_id = %edition.edition_id, "opened a play session");
    let duration = progress
        .and_then(|progress| progress.duration)
        .or(edition.runtime);
    Ok(Json(SessionJson {
        tracks: tracks_json(tracks, Some(&session.id)),
        session_id: session.id,
        edition_id: edition.edition_id,
        work_id: edition.work_id,
        position_ms: session.start_position.get(),
        duration_ms: duration.map(PlaybackMs::get),
        idle_secs: STREAM_IDLE_SECS,
    }))
}

/// `POST /api/playback/sessions/{id}/sync` and `.../close` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct SyncJson {
    /// The caller's progress after the report.
    progress: ProgressJson,
    /// True when this report is the one that reached the edition's end:
    /// the edition and its work are now finished for the caller.
    #[schema(example = false)]
    finished_now: bool,
}

/// The caller's own session `id`, or the 404 an unknown one gets (and
/// another person's, which reads the same).
///
/// # Errors
///
/// A 404 for a malformed, unknown or foreign id, a 500 when the library
/// cannot be read.
async fn own_session(
    library: &LibraryStore,
    user_id: i64,
    id: &str,
    route: &str,
) -> Result<PlaybackSession, PlaybackError> {
    let unknown = || refuse(StatusCode::NOT_FOUND, "unknown_session");
    let path = parse_track_path(id, "0").ok_or_else(unknown)?;
    library
        .playback_session(&path.session_id)
        .await
        .map_err(|err| read_failed(route, &err))?
        .filter(|session| session.user_id == user_id)
        .ok_or_else(unknown)
}

/// Records one report of a session and publishes what it changed.
///
/// # Errors
///
/// A 400 for a length out of range, a 404 for a session that is not the
/// caller's, a 409 `session_ended` for one already closed, a 500 when the
/// library cannot be read or written.
async fn report(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    body: PlaybackSyncBody,
    close: bool,
    route: &str,
) -> Result<Json<SyncJson>, PlaybackError> {
    // The same check the fuzz target drives (`parse_playback_sync_request`).
    let sync = checked(body.validate(close), route)?;
    let user_id = listener(state, headers).await?;
    let library = state.library_snapshot().await;
    let session = own_session(&library, user_id, id, route).await?;
    let work_id = library
        .work_of_edition(&session.edition_id)
        .await
        .map_err(|err| read_failed(route, &err))?;
    let PlaybackChange {
        progress,
        became_finished,
    } = library
        .sync_playback_session(&session, work_id.as_deref(), &sync)
        .await
        .map_err(|err| write_failed(route, &err))?
        .ok_or_else(|| refuse(StatusCode::CONFLICT, "session_ended"))?;
    if close {
        library
            .publish_playback_event(
                PlaybackEvent::Progress,
                &progress,
                work_id.as_deref(),
                Some(&session.id),
            )
            .await;
    }
    if became_finished {
        library
            .publish_playback_event(
                PlaybackEvent::Finished,
                &progress,
                work_id.as_deref(),
                Some(&session.id),
            )
            .await;
    }
    if close || became_finished {
        crate::live::poke(bookclerk_library::control_plane::LiveTopic::Playback);
    }
    Ok(Json(SyncJson {
        progress: progress_json(progress, work_id),
        finished_now: became_finished,
    }))
}

/// Reports where the caller is in a playing session.
///
/// Players report every few seconds while playing, and on pause and seek.
#[utoipa::path(
    post,
    path = "/api/playback/sessions/{id}/sync",
    params(("id" = String, Path, description = "Session id from `POST /api/playback/sessions`.")),
    request_body = PlaybackSyncBody,
    responses(
        (status = 200, description = "The caller's progress after the report.", body = SyncJson),
        (status = 400, description = "A length is past sixty days.", body = ErrorMessage, example = json!({"error": "position_ms must be at most 5184000000"})),
        (status = 413, description = "The body is over 1024 bytes.", body = ErrorMessage, example = json!({"error": "the body is at most 1024 bytes"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such session of the caller's (`unknown_session`).", body = ErrorMessage, example = json!({"error": "unknown_session"})),
        (status = 409, description = "The session was already closed (`session_ended`); open a new one.", body = ErrorMessage, example = json!({"error": "session_ended"})),
        (status = 500, description = "The library could not be read or written.", body = ErrorMessage, example = json!({"error": "the listening state could not be written"})),
    ),
)]
pub(crate) async fn post_session_sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    BoundedJson(body): BoundedJson<PlaybackSyncBody, MAX_PLAYBACK_SYNC_BYTES>,
) -> Result<Json<SyncJson>, PlaybackError> {
    const ROUTE: &str = "POST /api/playback/sessions/{id}/sync";
    report(&state, &headers, &id, body, false, ROUTE).await
}

/// Sends a session's last report and ends it.
#[utoipa::path(
    post,
    path = "/api/playback/sessions/{id}/close",
    params(("id" = String, Path, description = "Session id from `POST /api/playback/sessions`.")),
    request_body = PlaybackSyncBody,
    responses(
        (status = 200, description = "The caller's progress after the report; the session is closed.", body = SyncJson),
        (status = 400, description = "A length is past sixty days.", body = ErrorMessage, example = json!({"error": "position_ms must be at most 5184000000"})),
        (status = 413, description = "The body is over 1024 bytes.", body = ErrorMessage, example = json!({"error": "the body is at most 1024 bytes"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such session of the caller's (`unknown_session`).", body = ErrorMessage, example = json!({"error": "unknown_session"})),
        (status = 409, description = "The session was already closed (`session_ended`).", body = ErrorMessage, example = json!({"error": "session_ended"})),
        (status = 500, description = "The library could not be read or written.", body = ErrorMessage, example = json!({"error": "the listening state could not be written"})),
    ),
)]
pub(crate) async fn post_session_close(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    BoundedJson(body): BoundedJson<PlaybackSyncBody, MAX_PLAYBACK_SYNC_BYTES>,
) -> Result<Json<SyncJson>, PlaybackError> {
    const ROUTE: &str = "POST /api/playback/sessions/{id}/close";
    report(&state, &headers, &id, body, true, ROUTE).await
}

/// Streams one track of a play session.
#[utoipa::path(
    get,
    path = "/api/playback/sessions/{id}/tracks/{index}",
    params(
        ("id" = String, Path, description = "Session id from `POST /api/playback/sessions`."),
        ("index" = u32, Path, description = "The track's `index` in the session's `tracks`."),
        ("Range" = Option<String>, Header, description = "One byte range (`bytes=A-B`, `bytes=A-` or `bytes=-N`); anything else reads the whole file."),
    ),
    responses(
        (status = 200, description = "The whole track.", content(
            (crate::openapi::BinaryBody = "audio/mp4"),
            (crate::openapi::BinaryBody = "audio/mpeg"),
            (crate::openapi::BinaryBody = "audio/aac"),
            (crate::openapi::BinaryBody = "audio/flac"),
            (crate::openapi::BinaryBody = "audio/ogg"),
            (crate::openapi::BinaryBody = "audio/wav"),
            (crate::openapi::BinaryBody = "application/octet-stream"),
        )),
        (status = 206, description = "The requested byte range, with `Content-Range`.", content(
            (crate::openapi::BinaryBody = "audio/mp4"),
            (crate::openapi::BinaryBody = "audio/mpeg"),
            (crate::openapi::BinaryBody = "audio/aac"),
            (crate::openapi::BinaryBody = "audio/flac"),
            (crate::openapi::BinaryBody = "audio/ogg"),
            (crate::openapi::BinaryBody = "audio/wav"),
            (crate::openapi::BinaryBody = "application/octet-stream"),
        )),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such session of the caller's (`unknown_session`), or the index names no track of it (`unknown_track`).", body = ErrorMessage, example = json!({"error": "unknown_track"})),
        (status = 409, description = "The session no longer streams (`session_ended`): it was closed, sat a day without a report, the caller may no longer play the edition, or the edition's files changed. Open a new session.", body = ErrorMessage, example = json!({"error": "session_ended"})),
        (status = 416, description = "The range starts at or past the end of the file (`range_not_satisfiable`); `Content-Range` states the size.", body = ErrorMessage, example = json!({"error": "range_not_satisfiable"})),
        (status = 421, description = "Another node opened this session (`misdirected`).", body = ErrorMessage, example = json!({"error": "misdirected"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
        (status = 503, description = "Storage is unavailable; retry (`unavailable`).", body = ErrorMessage, example = json!({"error": "unavailable"})),
    ),
)]
pub(crate) async fn get_session_track(
    State(state): State<Arc<AppState>>,
    AxumPath((id, index)): AxumPath<(String, String)>,
    request: Request,
) -> Result<Response, PlaybackError> {
    const ROUTE: &str = "GET /api/playback/sessions/{id}/tracks/{index}";
    let headers = request.headers();
    let user_id = listener(&state, headers).await?;
    let path = parse_track_path(&id, &index)
        .ok_or_else(|| refuse(StatusCode::NOT_FOUND, "unknown_track"))?;
    let library = state.library_snapshot().await;
    let session = own_session(&library, user_id, &path.session_id, ROUTE).await?;
    let ended = || refuse(StatusCode::CONFLICT, "session_ended");
    let unavailable = || refuse(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    let idle = chrono::Utc::now().signed_duration_since(session.updated_at);
    if session.closed_at.is_some() || idle.num_seconds() > STREAM_IDLE_SECS {
        return Err(ended());
    }
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let this_node = load_or_create_node_identity(&files_dir)
        .ok()
        .map(|identity| identity.node_id);
    if this_node.as_deref() != Some(session.serving_node_id.as_str()) {
        return Err(refuse(StatusCode::MISDIRECTED_REQUEST, "misdirected"));
    }
    // The person's scope and right to the edition, read afresh.
    match edition_in_scope(&state, &library, headers, &session.edition_id, ROUTE).await {
        Ok(_) => {}
        Err((StatusCode::NOT_FOUND, _)) => return Err(ended()),
        Err(other) => return Err(other),
    }
    let tracks = authorize_playback(
        &library,
        user_id,
        &session.edition_id,
        &session.destination,
        &session.files_digest,
    )
    .await
    .map_err(|err| read_failed(ROUTE, &err))?
    .ok_or_else(ended)?;
    let track = tracks
        .get(path.index)
        .ok_or_else(|| refuse(StatusCode::NOT_FOUND, "unknown_track"))?;
    let destination = owner_destinations(&state)
        .await
        .map_err(|err| {
            tracing::warn!(session_id = %session.id, error = %err, "no output destination to play from");
            unavailable()
        })?
        .into_iter()
        .find(|destination| destination.id == session.destination)
        .ok_or_else(unavailable)?;
    Ok(serve_file(
        destination.backend.as_ref(),
        &track.key,
        headers.get(header::RANGE),
        track_content_type(track),
    )
    .await)
}

/// Query string of `GET /api/playback/progress`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ProgressQuery {
    /// Rows per page: 1 to 100, default 20.
    #[param(example = 20)]
    limit: Option<u64>,
    /// `next_cursor` of the page before.
    cursor: Option<String>,
    /// True also lists editions the caller finished; default false
    /// (Continue listening).
    #[param(example = false)]
    include_finished: Option<bool>,
}

/// `GET /api/playback/progress` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ProgressPage {
    /// The caller's progress rows, most recently played first. Editions
    /// their patron scope no longer shows are left out, so a page can be
    /// shorter than `limit` with more to come.
    items: Vec<ProgressJson>,
    /// Cursor of the next page; absent on the last.
    #[schema(example = "MjAyNi0xMS0xOVQwOTowMjo0MSswMDowMHw1ZDJj")]
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// Lists the caller's progress, most recently played first.
#[utoipa::path(
    get,
    path = "/api/playback/progress",
    params(ProgressQuery),
    responses(
        (status = 200, description = "A page of the caller's progress.", body = ProgressPage),
        (status = 400, description = "The cursor is not one this route returned.", body = ErrorMessage, example = json!({"error": "invalid page cursor"})),
        (status = 401, description = "The session ended.", body = ErrorMessage, example = json!({"error": "sign in again"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn list_progress(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ProgressQuery>,
) -> Result<Json<ProgressPage>, PlaybackError> {
    const ROUTE: &str = "GET /api/playback/progress";
    let user_id = listener(&state, &headers).await?;
    let after = match query.cursor.as_deref() {
        Some(token) => Some(
            parse_progress_cursor(token)
                .ok_or_else(|| refuse(StatusCode::BAD_REQUEST, "invalid page cursor"))?,
        ),
        None => None,
    };
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PROGRESS_LIMIT)
        .clamp(1, MAX_PROGRESS_LIMIT);
    let library = state.library_snapshot().await;
    let scope = caller_scope(&state, &library, &headers, ROUTE).await?;
    let (rows, next) = library
        .list_playback_progress(
            user_id,
            query.include_finished.unwrap_or(false),
            after.as_ref(),
            limit,
        )
        .await
        .map_err(|err| read_failed(ROUTE, &err))?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let work_id = library
            .work_of_edition(&row.edition_id)
            .await
            .map_err(|err| read_failed(ROUTE, &err))?;
        let shown = match (&work_id, scope.as_ref()) {
            (_, None) => true,
            (Some(work_id), Some(scope)) => library
                .work_in_scope(work_id, scope)
                .await
                .map_err(|err| read_failed(ROUTE, &err))?,
            (None, Some(_)) => false,
        };
        if shown {
            items.push(progress_json(row, work_id));
        }
    }
    Ok(Json(ProgressPage {
        items,
        next_cursor: next.as_ref().map(progress_cursor_token),
    }))
}

/// A body the library's playback parsers checked, or the 400 (for what
/// the body says) or 500 it is refused with.
fn checked<T>(checked: Result<T, LibraryError>, route: &str) -> Result<T, PlaybackError> {
    match checked {
        Ok(value) => Ok(value),
        Err(LibraryError::Invalid(message)) => Err(refuse(StatusCode::BAD_REQUEST, message)),
        Err(err) => Err(write_failed(route, &err)),
    }
}

/// Sets the caller's progress in an edition outright: Mark finished,
/// Start over, or a place chosen with no session open.
#[utoipa::path(
    put,
    path = "/api/playback/progress/{edition_id}",
    params(("edition_id" = String, Path, description = "Edition id.")),
    request_body = ProgressEditBody,
    responses(
        (status = 200, description = "The caller's progress after the change.", body = SyncJson),
        (status = 400, description = "A length is past sixty days.", body = ErrorMessage, example = json!({"error": "position_ms must be at most 5184000000"})),
        (status = 413, description = "The body is over 4096 bytes.", body = ErrorMessage, example = json!({"error": "the body is at most 4096 bytes"})),
        (status = 401, description = "The session ended.", body = ErrorMessage, example = json!({"error": "sign in again"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d not found"})),
        (status = 500, description = "The library could not be read or written.", body = ErrorMessage, example = json!({"error": "the listening state could not be written"})),
    ),
)]
pub(crate) async fn put_progress(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(edition_id): AxumPath<String>,
    BoundedJson(body): BoundedJson<ProgressEditBody, MAX_PLAYBACK_BODY_BYTES>,
) -> Result<Json<SyncJson>, PlaybackError> {
    const ROUTE: &str = "PUT /api/playback/progress/{edition_id}";
    // The same check the fuzz target drives (`parse_progress_edit_request`).
    let edit = checked(body.validate(), ROUTE)?;
    let user_id = listener(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let edition = edition_in_scope(&state, &library, &headers, &edition_id, ROUTE).await?;
    let PlaybackChange {
        progress,
        became_finished,
    } = library
        .set_playback_progress(
            user_id,
            &edition.edition_id,
            edition.work_id.as_deref(),
            &edit,
        )
        .await
        .map_err(|err| write_failed(ROUTE, &err))?;
    // Every outright write is a progress event; one that finishes the
    // edition is a finished event as well, as a finishing close is.
    library
        .publish_playback_event(
            PlaybackEvent::Progress,
            &progress,
            edition.work_id.as_deref(),
            None,
        )
        .await;
    if became_finished {
        library
            .publish_playback_event(
                PlaybackEvent::Finished,
                &progress,
                edition.work_id.as_deref(),
                None,
            )
            .await;
    }
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Playback);
    Ok(Json(SyncJson {
        progress: progress_json(progress, edition.work_id),
        finished_now: became_finished,
    }))
}

/// Forgets the caller's progress in an edition. The work's finished flag
/// is left as it is.
#[utoipa::path(
    delete,
    path = "/api/playback/progress/{edition_id}",
    params(("edition_id" = String, Path, description = "Edition id.")),
    responses(
        (status = 204, description = "The caller has no progress in the edition now."),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 500, description = "The library could not be written.", body = ErrorMessage, example = json!({"error": "the listening state could not be written"})),
    ),
)]
pub(crate) async fn delete_progress(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(edition_id): AxumPath<String>,
) -> Result<StatusCode, PlaybackError> {
    const ROUTE: &str = "DELETE /api/playback/progress/{edition_id}";
    let user_id = listener(&state, &headers).await?;
    let library = state.library_snapshot().await;
    // The caller's own row, by its key: nothing of the edition is shown,
    // so a row for an edition since hidden can still be removed.
    library
        .delete_playback_progress(user_id, &edition_id)
        .await
        .map_err(|err| write_failed(ROUTE, &err))?;
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Playback);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/library/editions/{id}/bookmarks` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct BookmarksJson {
    /// The caller's bookmarks in the edition, in timeline order.
    bookmarks: Vec<BookmarkJson>,
}

/// Lists the caller's bookmarks in an edition.
#[utoipa::path(
    get,
    path = "/api/library/editions/{id}/bookmarks",
    params(("id" = String, Path, description = "Edition id.")),
    responses(
        (status = 200, description = "The caller's bookmarks, in timeline order.", body = BookmarksJson),
        (status = 401, description = "The session ended.", body = ErrorMessage, example = json!({"error": "sign in again"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d not found"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn list_edition_bookmarks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<BookmarksJson>, PlaybackError> {
    const ROUTE: &str = "GET /api/library/editions/{id}/bookmarks";
    let user_id = listener(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let edition = edition_in_scope(&state, &library, &headers, &id, ROUTE).await?;
    let bookmarks = library
        .list_bookmarks(user_id, &edition.edition_id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?;
    Ok(Json(BookmarksJson {
        bookmarks: bookmarks.into_iter().map(bookmark_json).collect(),
    }))
}

/// Adds a bookmark of the caller's in an edition.
#[utoipa::path(
    post,
    path = "/api/library/editions/{id}/bookmarks",
    params(("id" = String, Path, description = "Edition id.")),
    request_body = NewBookmarkBody,
    responses(
        (status = 200, description = "The new bookmark.", body = BookmarkJson),
        (status = 400, description = "The position is past sixty days, or the title is too long or has control characters.", body = ErrorMessage, example = json!({"error": "title must be at most 200 bytes"})),
        (status = 413, description = "The body is over 4096 bytes.", body = ErrorMessage, example = json!({"error": "the body is at most 4096 bytes"})),
        (status = 401, description = "The session ended.", body = ErrorMessage, example = json!({"error": "sign in again"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d not found"})),
        (status = 409, description = "The caller already keeps 200 bookmarks in the edition.", body = ErrorMessage, example = json!({"error": "an edition takes at most 200 bookmarks"})),
        (status = 500, description = "The library could not be read or written.", body = ErrorMessage, example = json!({"error": "the listening state could not be written"})),
    ),
)]
pub(crate) async fn post_edition_bookmark(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    BoundedJson(body): BoundedJson<NewBookmarkBody, MAX_PLAYBACK_BODY_BYTES>,
) -> Result<Json<BookmarkJson>, PlaybackError> {
    const ROUTE: &str = "POST /api/library/editions/{id}/bookmarks";
    // The same check the fuzz target drives (`parse_new_bookmark_request`).
    let new = checked(body.validate(), ROUTE)?;
    let user_id = listener(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let edition = edition_in_scope(&state, &library, &headers, &id, ROUTE).await?;
    let bookmark = library
        .add_bookmark(user_id, &edition.edition_id, new.position, &new.title)
        .await
        .map_err(|err| write_failed(ROUTE, &err))?
        .ok_or_else(|| {
            refuse(
                StatusCode::CONFLICT,
                format!(
                    "an edition takes at most {} bookmarks",
                    bookclerk_library::MAX_BOOKMARKS_PER_EDITION
                ),
            )
        })?;
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Playback);
    Ok(Json(bookmark_json(bookmark)))
}

/// The 404 a bookmark that is not the caller's gets.
fn no_bookmark() -> PlaybackError {
    refuse(StatusCode::NOT_FOUND, "unknown_bookmark")
}

/// Changes one of the caller's bookmarks.
#[utoipa::path(
    put,
    path = "/api/playback/bookmarks/{id}",
    params(("id" = String, Path, description = "Bookmark id.")),
    request_body = BookmarkEditBody,
    responses(
        (status = 200, description = "The bookmark after the change.", body = BookmarkJson),
        (status = 400, description = "The position is past sixty days, or the title is too long or has control characters.", body = ErrorMessage, example = json!({"error": "title must be at most 200 bytes"})),
        (status = 413, description = "The body is over 4096 bytes.", body = ErrorMessage, example = json!({"error": "the body is at most 4096 bytes"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such bookmark of the caller's (`unknown_bookmark`).", body = ErrorMessage, example = json!({"error": "unknown_bookmark"})),
        (status = 500, description = "The library could not be written.", body = ErrorMessage, example = json!({"error": "the listening state could not be written"})),
    ),
)]
pub(crate) async fn put_bookmark(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    BoundedJson(body): BoundedJson<BookmarkEditBody, MAX_PLAYBACK_BODY_BYTES>,
) -> Result<Json<BookmarkJson>, PlaybackError> {
    const ROUTE: &str = "PUT /api/playback/bookmarks/{id}";
    // The same check the fuzz target drives (`parse_bookmark_edit_request`).
    let edit = checked(body.validate(), ROUTE)?;
    let user_id = listener(&state, &headers).await?;
    let path = parse_track_path(&id, "0").ok_or_else(no_bookmark)?;
    let library = state.library_snapshot().await;
    let bookmark = library
        .edit_bookmark(
            user_id,
            &path.session_id,
            edit.position,
            edit.title.as_deref(),
        )
        .await
        .map_err(|err| write_failed(ROUTE, &err))?
        .ok_or_else(no_bookmark)?;
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Playback);
    Ok(Json(bookmark_json(bookmark)))
}

/// Removes one of the caller's bookmarks.
#[utoipa::path(
    delete,
    path = "/api/playback/bookmarks/{id}",
    params(("id" = String, Path, description = "Bookmark id.")),
    responses(
        (status = 204, description = "The bookmark is gone."),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such bookmark of the caller's (`unknown_bookmark`).", body = ErrorMessage, example = json!({"error": "unknown_bookmark"})),
        (status = 500, description = "The library could not be written.", body = ErrorMessage, example = json!({"error": "the listening state could not be written"})),
    ),
)]
pub(crate) async fn delete_bookmark(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, PlaybackError> {
    const ROUTE: &str = "DELETE /api/playback/bookmarks/{id}";
    let user_id = listener(&state, &headers).await?;
    let path = parse_track_path(&id, "0").ok_or_else(no_bookmark)?;
    let library = state.library_snapshot().await;
    if !library
        .delete_bookmark(user_id, &path.session_id)
        .await
        .map_err(|err| write_failed(ROUTE, &err))?
    {
        return Err(no_bookmark());
    }
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Playback);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
#[path = "playback_tests.rs"]
mod tests;
