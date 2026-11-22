//! Listening routes of the player API: play sessions, audio streams and
//! offline downloads, progress, bookmarks and the patron's listening
//! history.
//!
//! Every stream and session goes through [`stream_plan`], the one place
//! the playback core decides what the patron may play here: anyone who
//! may see a book may stream it. Saving files for offline listening is
//! what the Download member control gates, as Audiobookshelf's `download`
//! permission does, so only the file download route adds
//! [`may_save_offline`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_library::control_plane::{
    load_or_create_node_identity, plan_playback, user_may, LiveTopic, MemberControl, PlaybackPlan,
};
use bookclerk_library::{
    Bookmark, LibraryStore, NewPlaybackSession, PlaybackEvent, PlaybackMs, PlaybackProgress,
    PlaybackProgressEdit, PlaybackSession as StoredSession, PlaybackSync, PlayerItem, PlayerKind,
    WorkScope,
};
use bookclerk_player_api::ids::{file_ino, is_entity_id, parse_file_ino, WHOLE_LIBRARY_ID};
use bookclerk_player_api::request::{
    parse_bookmark_request, parse_bookmark_time, parse_detail_query, parse_local_session,
    parse_local_sessions, parse_play_request, parse_progress_batch, parse_progress_patch,
    parse_sync_request, LocalSession, ProgressPatch, MAX_BATCH_BODY_BYTES, MAX_BODY_BYTES,
};
use bookclerk_player_api::time::ms_to_secs;
use bookclerk_player_api::tracks::{track_durations_ms, track_starts_ms, TrackSize};
use bookclerk_player_api::wire::{
    AudioFile, AudioTrack, Bookmark as BookmarkWire, FileMetadata, ItemListening, LibraryFile,
    LibraryItems, ListeningStats, MediaProgress, PlaybackSession, SessionDevice, SessionPage,
    SyncResult, SyncResults, MEDIA_TYPE_BOOK,
};
use bookclerk_player_api::COMPATIBLE_SERVER_VERSION;

use super::catalog::{item_wire, metadata_wire, visible_item, visible_items};
use super::sign_in::read_body;
use super::{ok, Patron, PlayerError, PlayerResult};
use crate::api::AppState;
use crate::node_api::copies::serve_file;
use crate::node_api::uploads::owner_destinations;
use crate::playback::{playable_here, track_content_type, STREAM_IDLE_SECS};

/// What the file download route answers when the Download member control
/// is off for the patron.
const DOWNLOADS_OFF: &str = "Downloads are turned off for this patron";

/// Most progress rows the in-progress list reads.
const MAX_IN_PROGRESS: u64 = 100;

/// Most sessions the listening stats read.
const MAX_STATS_SESSIONS: u64 = 1000;

/// Sessions per page of the listening history when the app names none.
const DEFAULT_SESSIONS_PER_PAGE: u64 = 10;

/// Sessions the listening stats list as recent.
const RECENT_SESSIONS: usize = 10;

/// The name a session stores when the app reported no device name.
const DEFAULT_PLAYER_NAME: &str = "Player app";

/// `Play method` apps read: 0 is direct play (no transcoding).
const DIRECT_PLAY: u8 = 0;

/// What the playback core lets the patron play of an edition here.
///
/// The one decision behind every play session, track list and stream of
/// the player API: the playback core's [`plan_playback`] (the copy rule
/// and an acquired primary audio). It never reads the Download control.
///
/// # Errors
///
/// A 500 when the library cannot be read.
async fn stream_plan(
    patron: &Patron,
    edition_id: &str,
    route: &str,
) -> PlayerResult<Option<PlaybackPlan>> {
    plan_playback(&patron.library, patron.user_id(), edition_id)
        .await
        .map_err(|err| PlayerError::failed(route, &err))
}

/// Whether the patron may save files for offline listening: the Download
/// member control.
///
/// # Errors
///
/// A 500 when the controls cannot be read.
async fn may_save_offline(patron: &Patron, route: &str) -> PlayerResult<bool> {
    user_may(&patron.library, patron.user_id(), MemberControl::Download)
        .await
        .map_err(|err| PlayerError::failed(route, &err))
}

/// One track of an item as this node can stream it.
#[derive(Debug, Clone)]
pub(crate) struct PlannedTrack {
    /// The file's name (the last part of its storage key).
    filename: String,
    /// Its media type.
    mime_type: String,
    /// Its size in bytes.
    size: u64,
    /// Where it starts in the book, in milliseconds.
    start_ms: u64,
    /// Its length in milliseconds.
    duration_ms: u64,
    /// Its average bitrate in kilobits per second, once probed.
    bitrate_kbps: Option<u32>,
}

/// An item's tracks on one of this node's destinations.
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    /// The destination the tracks are read from.
    destination: String,
    /// The digest of the edition's file rows there.
    digest: String,
    /// The tracks, in play order.
    pub(crate) tracks: Vec<PlannedTrack>,
}

impl Planned {
    /// The whole length in milliseconds.
    pub(crate) fn duration_ms(&self) -> u64 {
        self.tracks.iter().map(|t| t.duration_ms).sum()
    }

    /// Bytes in all the tracks.
    pub(crate) fn size(&self) -> u64 {
        self.tracks.iter().map(|t| t.size).sum()
    }
}

/// The tracks the patron may stream of `item` from this node, timed.
///
/// # Returns
///
/// `None` when there is nothing the patron may play here.
///
/// # Errors
///
/// A 500 when the library cannot be read, a 503 when this node's storage
/// cannot be asked.
pub(crate) async fn plan_tracks(
    state: &AppState,
    patron: &Patron,
    item: &PlayerItem,
    route: &str,
) -> PlayerResult<Option<Planned>> {
    let Some(plan) = stream_plan(patron, &item.edition_id, route).await? else {
        return Ok(None);
    };
    let destinations = owner_destinations(state).await.map_err(|err| {
        tracing::warn!(error = %err, route, "no output destination to play from");
        PlayerError::unavailable()
    })?;
    let Some((destination, tracks)) = playable_here(&plan, &destinations) else {
        return Ok(None);
    };
    let Some(digest) = plan.digest(&destination.id) else {
        return Ok(None);
    };
    let keys: Vec<String> = tracks.iter().map(|t| t.key.clone()).collect();
    let probes = destination.backend.head_many(&keys).await.map_err(|err| {
        tracing::warn!(error = %err, route, destination = %destination.id, "track sizes could not be read");
        PlayerError::unavailable()
    })?;
    let sizes: Vec<TrackSize> = tracks
        .iter()
        .zip(probes.iter().chain(std::iter::repeat(&None)))
        .map(|(track, probe)| TrackSize {
            bytes: probe.as_ref().map_or(0, |p| p.size),
            bitrate_kbps: track.bitrate_kbps,
        })
        .collect();
    let runtime_ms = item.runtime_minutes.map(|m| m.saturating_mul(60_000));
    let durations = track_durations_ms(&sizes, runtime_ms);
    let starts = track_starts_ms(&durations);
    let planned = tracks
        .iter()
        .enumerate()
        .map(|(i, track)| PlannedTrack {
            filename: track
                .key
                .rsplit('/')
                .next()
                .unwrap_or(track.key.as_str())
                .to_owned(),
            mime_type: track_content_type(track)
                .to_str()
                .unwrap_or("application/octet-stream")
                .to_owned(),
            size: sizes[i].bytes,
            start_ms: starts[i],
            duration_ms: durations[i],
            bitrate_kbps: track.bitrate_kbps,
        })
        .collect();
    Ok(Some(Planned {
        destination: destination.id.clone(),
        digest: digest.to_owned(),
        tracks: planned,
    }))
}

/// A file's name, size and times as apps read them.
fn file_metadata(track: &PlannedTrack, at_ms: i64) -> FileMetadata {
    let ext = track
        .filename
        .rsplit_once('.')
        .map(|(_, ext)| format!(".{}", ext.to_ascii_lowercase()))
        .unwrap_or_default();
    FileMetadata {
        filename: track.filename.clone(),
        ext,
        path: track.filename.clone(),
        rel_path: track.filename.clone(),
        size: track.size,
        mtime_ms: at_ms,
        ctime_ms: at_ms,
        birthtime_ms: at_ms,
    }
}

/// The track list apps play from, with each track's stream path.
pub(crate) fn tracks_wire(edition_id: &str, planned: &Planned, at_ms: i64) -> Vec<AudioTrack> {
    planned
        .tracks
        .iter()
        .enumerate()
        .map(|(i, track)| AudioTrack {
            index: u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1),
            start_offset: ms_to_secs(track.start_ms),
            duration: ms_to_secs(track.duration_ms),
            title: track.filename.clone(),
            content_url: format!("/api/items/{edition_id}/file/{}", file_ino(i)),
            mime_type: track.mime_type.clone(),
            codec: None,
            metadata: Some(file_metadata(track, at_ms)),
        })
        .collect()
}

/// The audio files of an expanded item.
pub(crate) fn audio_files_wire(planned: &Planned, at_ms: i64) -> Vec<AudioFile> {
    planned
        .tracks
        .iter()
        .enumerate()
        .map(|(i, track)| AudioFile {
            index: u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1),
            ino: file_ino(i),
            metadata: file_metadata(track, at_ms),
            added_at: at_ms,
            updated_at: at_ms,
            track_num_from_meta: None,
            disc_num_from_meta: None,
            track_num_from_filename: None,
            disc_num_from_filename: None,
            manually_verified: false,
            exclude: false,
            error: None,
            format: None,
            duration: ms_to_secs(track.duration_ms),
            bit_rate: track.bitrate_kbps.map(|kbps| u64::from(kbps) * 1000),
            language: None,
            codec: None,
            time_base: None,
            channels: None,
            channel_layout: None,
            chapters: Vec::new(),
            embedded_cover_art: None,
            meta_tags: bookclerk_player_api::wire::MetaTags::default(),
            mime_type: track.mime_type.clone(),
        })
        .collect()
}

/// The files of an expanded item.
pub(crate) fn library_files_wire(planned: &Planned, at_ms: i64) -> Vec<LibraryFile> {
    planned
        .tracks
        .iter()
        .enumerate()
        .map(|(i, track)| LibraryFile {
            ino: file_ino(i),
            metadata: file_metadata(track, at_ms),
            added_at: at_ms,
            updated_at: at_ms,
            file_type: "audio",
        })
        .collect()
}

/// A progress row as apps read it.
pub(crate) fn progress_wire(user_id: i64, progress: PlaybackProgress) -> MediaProgress {
    let duration_ms = progress.duration.map_or(0, PlaybackMs::get);
    let position_ms = progress.position.get();
    let share = if progress.is_finished {
        1.0
    } else if duration_ms > 0 {
        (ms_to_secs(position_ms) / ms_to_secs(duration_ms)).min(1.0)
    } else {
        0.0
    };
    MediaProgress {
        id: progress.edition_id.clone(),
        user_id: user_id.to_string(),
        library_item_id: progress.edition_id.clone(),
        episode_id: None,
        media_item_id: progress.edition_id,
        media_item_type: MEDIA_TYPE_BOOK,
        duration: ms_to_secs(duration_ms),
        progress: share,
        current_time: ms_to_secs(position_ms),
        is_finished: progress.is_finished,
        hide_from_continue_listening: false,
        ebook_location: None,
        ebook_progress: None,
        last_update: progress.updated_at.timestamp_millis(),
        started_at: progress.started_at.timestamp_millis(),
        finished_at: progress.finished_at.map(|at| at.timestamp_millis()),
    }
}

/// A bookmark as apps read it.
pub(crate) fn bookmark_wire(bookmark: Bookmark) -> BookmarkWire {
    BookmarkWire {
        library_item_id: bookmark.edition_id,
        title: bookmark.title,
        time: ms_to_secs(bookmark.position.get()),
        created_at: bookmark.created_at.timestamp_millis(),
    }
}

/// A timeline length an app sent, or a 400.
fn timeline(ms: u64) -> PlayerResult<PlaybackMs> {
    PlaybackMs::new(ms).ok_or_else(PlayerError::bad_request)
}

/// A stored session as apps read it, with the item's metadata when it is
/// still listed.
fn session_wire(
    patron: &Patron,
    session: &StoredSession,
    item: Option<&PlayerItem>,
    planned: Option<&Planned>,
) -> PlaybackSession {
    let metadata = item.map(metadata_wire);
    let duration_ms = planned
        .map(Planned::duration_ms)
        .or_else(|| {
            item.and_then(|i| i.runtime_minutes)
                .map(|m| m.saturating_mul(60_000))
        })
        .unwrap_or(0);
    let now_ms = chrono::Utc::now().timestamp_millis();
    PlaybackSession {
        id: session.id.clone(),
        user_id: patron.user_id().to_string(),
        library_id: WHOLE_LIBRARY_ID.to_owned(),
        library_item_id: session.edition_id.clone(),
        book_id: session.edition_id.clone(),
        episode_id: None,
        media_type: MEDIA_TYPE_BOOK,
        display_title: item.map(|i| i.title.clone()).unwrap_or_default(),
        display_author: metadata
            .as_ref()
            .map(|m| m.author_name.clone())
            .unwrap_or_default(),
        media_metadata: metadata
            .unwrap_or_else(|| super::catalog::empty_metadata(&session.edition_id)),
        chapters: Vec::new(),
        cover_path: item.map(|i| format!("/api/items/{}/cover", i.edition_id)),
        duration: ms_to_secs(duration_ms),
        play_method: DIRECT_PLAY,
        media_player: session.client_name.clone(),
        device_info: SessionDevice {
            device_id: patron.card_id.to_string(),
            device_name: Some(session.client_name.clone()),
            client_name: Some(session.client_name.clone()),
        },
        server_version: COMPATIBLE_SERVER_VERSION,
        date: session.started_at.format("%Y-%m-%d").to_string(),
        day_of_week: session.started_at.format("%A").to_string(),
        time_listening: ms_to_secs(session.listened.get()),
        start_time: ms_to_secs(session.start_position.get()),
        current_time: ms_to_secs(session.position.get()),
        started_at: session.started_at.timestamp_millis(),
        updated_at: session.updated_at.timestamp_millis(),
        audio_tracks: planned
            .map(|p| tracks_wire(&session.edition_id, p, now_ms))
            .unwrap_or_default(),
        library_item: item.map(|i| item_wire(i, None, planned)),
    }
}

/// Opens a play session on an item.
#[utoipa::path(
    post,
    path = "/player/api/items/{id}/play",
    params(("id" = String, Path, description = "Item id.")),
    request_body(content = super::bodies::PlayBody, description = "The device and what the app asks for; tracks always play directly."),
    responses(
        (status = 200, description = "The session, its tracks and where to start.", body = PlaybackSession),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such item, or nothing of it this patron may play here.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read or written.", body = String, content_type = "text/plain"),
        (status = 503, description = "Storage could not be asked; retry.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_play(
    State(state): State<Arc<AppState>>,
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> PlayerResult<Json<PlaybackSession>> {
    const ROUTE: &str = "POST /player/api/items/{id}/play";
    let body = read_body(body, MAX_BODY_BYTES).await?;
    let play = if body.is_empty() {
        parse_play_request(b"{}")
    } else {
        parse_play_request(&body)
    }
    .map_err(|_| PlayerError::bad_request())?;
    let item = visible_item(&patron, &id, ROUTE).await?;
    let not_playable = || PlayerError::new(StatusCode::NOT_FOUND, "Nothing to play");
    let planned = plan_tracks(&state, &patron, &item, ROUTE)
        .await?
        .ok_or_else(not_playable)?;
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let this_node = load_or_create_node_identity(&files_dir).map_err(|err| {
        tracing::warn!(error = %err, route = ROUTE, "no node identity to play from");
        PlayerError::unavailable()
    })?;
    let name = if play.device.name.is_empty() {
        DEFAULT_PLAYER_NAME
    } else {
        play.device.name.as_str()
    };
    let session = patron
        .library
        .open_playback_session(NewPlaybackSession {
            user_id: patron.user_id(),
            edition_id: &item.edition_id,
            serving_node_id: &this_node.node_id,
            destination: &planned.destination,
            files_digest: &planned.digest,
            client_kind: PlayerKind::App,
            client_name: name,
        })
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    let progress = patron
        .library
        .playback_progress(patron.user_id(), &item.edition_id)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    if let Some(progress) = &progress {
        patron
            .library
            .publish_playback_event(
                PlaybackEvent::Started,
                progress,
                Some(&item.work_id),
                Some(&session.id),
            )
            .await;
    }
    crate::live::poke(LiveTopic::Playback);
    tracing::info!(session_id = %session.id, edition_id = %item.edition_id, "a player app opened a play session");
    Ok(Json(session_wire(
        &patron,
        &session,
        Some(&item),
        Some(&planned),
    )))
}

/// Streams one audio file of an item, checked afresh on every request.
///
/// # Errors
///
/// A 404 when the item or file is unknown or not playable here, a 500 or
/// 503 when the library or storage cannot be read.
async fn stream(
    state: &AppState,
    patron: &Patron,
    id: &str,
    ino: &str,
    headers: &HeaderMap,
    route: &str,
) -> PlayerResult<(Response, String)> {
    let index = parse_file_ino(ino).ok_or_else(PlayerError::not_found)?;
    let item = visible_item(patron, id, route).await?;
    let plan = stream_plan(patron, &item.edition_id, route)
        .await?
        .ok_or_else(PlayerError::not_found)?;
    let destinations = owner_destinations(state).await.map_err(|err| {
        tracing::warn!(error = %err, route, "no output destination to play from");
        PlayerError::unavailable()
    })?;
    let (destination, tracks) =
        playable_here(&plan, &destinations).ok_or_else(PlayerError::not_found)?;
    let track = tracks.get(index).ok_or_else(PlayerError::not_found)?;
    let filename = track
        .key
        .rsplit('/')
        .next()
        .unwrap_or(track.key.as_str())
        .to_owned();
    let response = serve_file(
        destination.backend.as_ref(),
        &track.key,
        headers.get(header::RANGE),
        track_content_type(track),
    )
    .await;
    Ok((response, filename))
}

/// Streams one audio file of an item.
#[utoipa::path(
    get,
    path = "/player/api/items/{id}/file/{ino}",
    params(
        ("id" = String, Path, description = "Item id."),
        ("ino" = String, Path, description = "The file's `ino` from the item's tracks."),
        ("Range" = Option<String>, Header, description = "One byte range."),
    ),
    responses(
        (status = 200, description = "The whole file.", content_type = "audio/mp4"),
        (status = 206, description = "The requested byte range.", content_type = "audio/mp4"),
        (status = 404, description = "No such item or file, or nothing of it this patron may play here.", body = String, content_type = "text/plain"),
        (status = 416, description = "The range starts past the end of the file; `Content-Range` states the size.", body = crate::http_error::ErrorMessage, example = json!({"error": "range_not_satisfiable"})),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
        (status = 503, description = "Storage could not be asked; retry.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_item_file(
    State(state): State<Arc<AppState>>,
    Extension(patron): Extension<Patron>,
    AxumPath((id, ino)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> PlayerResult<Response> {
    const ROUTE: &str = "GET /player/api/items/{id}/file/{ino}";
    stream(&state, &patron, &id, &ino, &headers, ROUTE)
        .await
        .map(|(response, _)| response)
}

/// Downloads one audio file of an item for offline listening.
#[utoipa::path(
    get,
    path = "/player/api/items/{id}/file/{ino}/download",
    params(
        ("id" = String, Path, description = "Item id."),
        ("ino" = String, Path, description = "The file's `ino` from the item's audio files."),
        ("Range" = Option<String>, Header, description = "One byte range, to resume."),
    ),
    responses(
        (status = 200, description = "The whole file, as an attachment.", content_type = "audio/mp4"),
        (status = 206, description = "The requested byte range.", content_type = "audio/mp4"),
        (status = 403, description = "The patron may not save files offline.", body = String, content_type = "text/plain", example = json!(DOWNLOADS_OFF)),
        (status = 404, description = "No such item or file, or nothing of it this patron may play here.", body = String, content_type = "text/plain"),
        (status = 416, description = "The range starts past the end of the file; `Content-Range` states the size.", body = crate::http_error::ErrorMessage, example = json!({"error": "range_not_satisfiable"})),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
        (status = 503, description = "Storage could not be asked; retry.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_item_file_download(
    State(state): State<Arc<AppState>>,
    Extension(patron): Extension<Patron>,
    AxumPath((id, ino)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> PlayerResult<Response> {
    const ROUTE: &str = "GET /player/api/items/{id}/file/{ino}/download";
    if !may_save_offline(&patron, ROUTE).await? {
        return Err(PlayerError::new(StatusCode::FORBIDDEN, DOWNLOADS_OFF));
    }
    let (mut response, filename) = stream(&state, &patron, &id, &ino, &headers, ROUTE).await?;
    if response.status().is_success() {
        let safe: String = filename
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, value);
        }
    }
    Ok(response)
}

/// The patron's own open session `id`, or a 404.
///
/// # Errors
///
/// A 404 for a malformed, unknown or foreign id, a 500 when the library
/// cannot be read.
async fn own_session(patron: &Patron, id: &str, route: &str) -> PlayerResult<StoredSession> {
    if !is_entity_id(id) {
        return Err(PlayerError::not_found());
    }
    patron
        .library
        .playback_session(id)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?
        .filter(|session| session.user_id == patron.user_id())
        .ok_or_else(PlayerError::not_found)
}

/// Records one report of a session and publishes what it changed.
///
/// # Errors
///
/// A 400 for a malformed body, a 404 for a session not the patron's, a
/// 409 for a closed or idle one, a 500 when the library cannot be written.
async fn report(
    patron: &Patron,
    id: &str,
    body: &[u8],
    close: bool,
    route: &str,
) -> PlayerResult<Response> {
    let session = own_session(patron, id, route).await?;
    let idle = chrono::Utc::now().signed_duration_since(session.updated_at);
    if !close && idle.num_seconds() > STREAM_IDLE_SECS {
        return Err(PlayerError::new(StatusCode::CONFLICT, "Session ended"));
    }
    let sync = if body.is_empty() {
        PlaybackSync {
            position: session.position,
            listened: PlaybackMs::ZERO,
            duration: None,
            close,
        }
    } else {
        let request = parse_sync_request(body).map_err(|_| PlayerError::bad_request())?;
        PlaybackSync {
            position: match request.position_ms {
                Some(ms) => timeline(ms)?,
                None => session.position,
            },
            listened: timeline(request.listened_ms)?,
            duration: request.duration_ms.map(timeline).transpose()?,
            close,
        }
    };
    let work_id = patron
        .library
        .work_of_edition(&session.edition_id)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?;
    let Some(change) = patron
        .library
        .sync_playback_session(&session, work_id.as_deref(), &sync)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?
    else {
        // A close of a session already closed is answered as done.
        return if close {
            Ok(ok())
        } else {
            Err(PlayerError::new(StatusCode::CONFLICT, "Session ended"))
        };
    };
    if close {
        patron
            .library
            .publish_playback_event(
                PlaybackEvent::Progress,
                &change.progress,
                work_id.as_deref(),
                Some(&session.id),
            )
            .await;
    }
    if change.became_finished {
        patron
            .library
            .publish_playback_event(
                PlaybackEvent::Finished,
                &change.progress,
                work_id.as_deref(),
                Some(&session.id),
            )
            .await;
    }
    crate::live::poke(LiveTopic::Playback);
    Ok(ok())
}

/// Reports where the patron is in a playing session.
#[utoipa::path(
    post,
    path = "/player/api/session/{id}/sync",
    operation_id = "player_session_sync",
    params(("id" = String, Path, description = "Session id from the play answer.")),
    request_body(content = super::bodies::SyncBody, description = "Where the patron is, in seconds."),
    responses(
        (status = 200, description = "`OK`.", body = String, content_type = "text/plain"),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such session of the patron's.", body = String, content_type = "text/plain"),
        (status = 409, description = "The session ended; open a new one.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_session_sync(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> PlayerResult<Response> {
    const ROUTE: &str = "POST /player/api/session/{id}/sync";
    let body = read_body(body, MAX_BODY_BYTES).await?;
    report(&patron, &id, &body, false, ROUTE).await
}

/// Sends a session's last report, if any, and ends it.
#[utoipa::path(
    post,
    path = "/player/api/session/{id}/close",
    operation_id = "player_session_close",
    params(("id" = String, Path, description = "Session id from the play answer.")),
    request_body(content = super::bodies::SyncBody, description = "Optionally where the patron is, in seconds; empty keeps the last sync."),
    responses(
        (status = 200, description = "`OK`; the session is closed.", body = String, content_type = "text/plain"),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such session of the patron's.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_session_close(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> PlayerResult<Response> {
    const ROUTE: &str = "POST /player/api/session/{id}/close";
    let body = read_body(body, MAX_BODY_BYTES).await?;
    report(&patron, &id, &body, true, ROUTE).await
}

/// Applies one offline session: its position becomes the patron's
/// progress when it is newer than what the library holds.
///
/// # Errors
///
/// A 404 for an item the patron cannot see, a 500 when the library cannot
/// be read or written.
async fn apply_local(patron: &Patron, local: &LocalSession, route: &str) -> PlayerResult<()> {
    let item = visible_item(patron, &local.item_id, route).await?;
    let current = patron
        .library
        .playback_progress(patron.user_id(), &item.edition_id)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?;
    let newer = match (&current, local.updated_at_ms) {
        (None, _) | (Some(_), None) => true,
        (Some(current), Some(at)) => at > current.updated_at.timestamp_millis(),
    };
    if !newer {
        return Ok(());
    }
    let edit = PlaybackProgressEdit {
        position: Some(timeline(local.position_ms)?),
        duration: local.duration_ms.map(timeline).transpose()?,
        finished: None,
    };
    let change = patron
        .library
        .set_playback_progress(
            patron.user_id(),
            &item.edition_id,
            Some(&item.work_id),
            &edit,
        )
        .await
        .map_err(|err| PlayerError::failed(route, &err))?;
    patron
        .library
        .publish_playback_event(
            PlaybackEvent::Progress,
            &change.progress,
            Some(&item.work_id),
            None,
        )
        .await;
    if change.became_finished {
        patron
            .library
            .publish_playback_event(
                PlaybackEvent::Finished,
                &change.progress,
                Some(&item.work_id),
                None,
            )
            .await;
    }
    Ok(())
}

/// Uploads one session an app played offline.
#[utoipa::path(
    post,
    path = "/player/api/session/local",
    request_body(content = super::bodies::LocalSessionBody, description = "An offline session."),
    responses(
        (status = 200, description = "`OK`.", body = String, content_type = "text/plain"),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such item.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_local_session(
    Extension(patron): Extension<Patron>,
    body: Body,
) -> PlayerResult<Response> {
    const ROUTE: &str = "POST /player/api/session/local";
    let body = read_body(body, MAX_BODY_BYTES).await?;
    let local = parse_local_session(&body).map_err(|_| PlayerError::bad_request())?;
    apply_local(&patron, &local, ROUTE).await?;
    crate::live::poke(LiveTopic::Playback);
    Ok(ok())
}

/// Uploads every session an app played offline.
#[utoipa::path(
    post,
    path = "/player/api/session/local-all",
    request_body(content = super::bodies::LocalSessionsBody, description = "Offline sessions, each as `POST /player/api/session/local` takes one."),
    responses(
        (status = 200, description = "Each session's outcome.", body = SyncResults),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_local_sessions(
    Extension(patron): Extension<Patron>,
    body: Body,
) -> PlayerResult<Json<SyncResults>> {
    const ROUTE: &str = "POST /player/api/session/local-all";
    let body = read_body(body, MAX_BATCH_BODY_BYTES).await?;
    let sessions = parse_local_sessions(&body).map_err(|_| PlayerError::bad_request())?;
    let mut results = Vec::with_capacity(sessions.len());
    for session in sessions {
        let outcome = match session {
            Ok(local) => {
                let id = local.id.clone();
                match apply_local(&patron, &local, ROUTE).await {
                    Ok(()) => SyncResult {
                        id,
                        success: true,
                        error: None,
                    },
                    Err(err) => SyncResult {
                        id,
                        success: false,
                        error: Some(
                            err.status()
                                .canonical_reason()
                                .unwrap_or("Failed")
                                .to_owned(),
                        ),
                    },
                }
            }
            Err((id, err)) => SyncResult {
                id: id.unwrap_or_default(),
                success: false,
                error: Some(err.to_string()),
            },
        };
        results.push(outcome);
    }
    crate::live::poke(LiveTopic::Playback);
    Ok(Json(SyncResults { results }))
}

/// Reads the patron's progress on an item.
#[utoipa::path(
    get,
    path = "/player/api/me/progress/{id}",
    params(("id" = String, Path, description = "Item id.")),
    responses(
        (status = 200, description = "The progress.", body = MediaProgress),
        (status = 404, description = "No progress on that item.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_progress(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Json<MediaProgress>> {
    const ROUTE: &str = "GET /player/api/me/progress/{id}";
    let item = visible_item(&patron, &id, ROUTE).await?;
    let progress = patron
        .library
        .playback_progress(patron.user_id(), &item.edition_id)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?
        .ok_or_else(PlayerError::not_found)?;
    Ok(Json(progress_wire(patron.user_id(), progress)))
}

/// Sets the patron's progress on one item.
///
/// # Errors
///
/// A 404 for an item the patron cannot see, a 400 for a length out of
/// range, a 500 when the library cannot be written.
async fn apply_patch(
    patron: &Patron,
    id: &str,
    patch: &ProgressPatch,
    route: &str,
) -> PlayerResult<()> {
    let item = visible_item(patron, id, route).await?;
    if patch.position_ms.is_none() && patch.duration_ms.is_none() && patch.is_finished.is_none() {
        // Only `hideFromContinueListening`, which the library does not keep.
        return Ok(());
    }
    let edit = PlaybackProgressEdit {
        position: patch.position_ms.map(timeline).transpose()?,
        duration: patch.duration_ms.map(timeline).transpose()?,
        finished: patch.is_finished,
    };
    let change = patron
        .library
        .set_playback_progress(
            patron.user_id(),
            &item.edition_id,
            Some(&item.work_id),
            &edit,
        )
        .await
        .map_err(|err| PlayerError::failed(route, &err))?;
    patron
        .library
        .publish_playback_event(
            PlaybackEvent::Progress,
            &change.progress,
            Some(&item.work_id),
            None,
        )
        .await;
    if change.became_finished {
        patron
            .library
            .publish_playback_event(
                PlaybackEvent::Finished,
                &change.progress,
                Some(&item.work_id),
                None,
            )
            .await;
    }
    Ok(())
}

/// Sets the patron's progress on an item: a seek, Mark finished or Start
/// over.
#[utoipa::path(
    patch,
    path = "/player/api/me/progress/{id}",
    params(("id" = String, Path, description = "Item id.")),
    request_body(content = super::bodies::ProgressBody, description = "The fields to change."),
    responses(
        (status = 200, description = "`OK`.", body = String, content_type = "text/plain"),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such item.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn patch_progress(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> PlayerResult<Response> {
    const ROUTE: &str = "PATCH /player/api/me/progress/{id}";
    let body = read_body(body, MAX_BODY_BYTES).await?;
    let patch = parse_progress_patch(&body).map_err(|_| PlayerError::bad_request())?;
    apply_patch(&patron, &id, &patch, ROUTE).await?;
    crate::live::poke(LiveTopic::Playback);
    Ok(ok())
}

/// Clears the patron's progress on an item.
#[utoipa::path(
    delete,
    path = "/player/api/me/progress/{id}",
    operation_id = "player_delete_progress",
    params(("id" = String, Path, description = "Item id.")),
    responses(
        (status = 200, description = "`OK`.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such item.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn delete_progress(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Response> {
    const ROUTE: &str = "DELETE /player/api/me/progress/{id}";
    let item = visible_item(&patron, &id, ROUTE).await?;
    patron
        .library
        .delete_playback_progress(patron.user_id(), &item.edition_id)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    crate::live::poke(LiveTopic::Playback);
    Ok(ok())
}

/// Sets the patron's progress on several items.
#[utoipa::path(
    patch,
    path = "/player/api/me/progress/batch/update",
    request_body(content = Vec<super::bodies::ProgressBatchEntry>, description = "One change per item."),
    responses(
        (status = 200, description = "`OK`.", body = String, content_type = "text/plain"),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 404, description = "An item is unknown.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn patch_progress_batch(
    Extension(patron): Extension<Patron>,
    body: Body,
) -> PlayerResult<Response> {
    const ROUTE: &str = "PATCH /player/api/me/progress/batch/update";
    let body = read_body(body, MAX_BATCH_BODY_BYTES).await?;
    let patches = parse_progress_batch(&body).map_err(|_| PlayerError::bad_request())?;
    for (id, patch) in &patches {
        apply_patch(&patron, id, patch, ROUTE).await?;
    }
    crate::live::poke(LiveTopic::Playback);
    Ok(ok())
}

/// The editions among `editions` that `scope` shows: every one when the
/// scope hides nothing. Progress and bookmarks only name editions, but a
/// patron's apps still learn only of the books their scope shows them.
///
/// # Errors
///
/// Returns the read error; callers fail closed on it.
pub(crate) async fn editions_in_scope(
    library: &LibraryStore,
    scope: Option<&WorkScope>,
    editions: Vec<String>,
) -> bookclerk_library::Result<HashSet<String>> {
    let mut shown = HashSet::new();
    for edition in editions {
        if !shown.contains(&edition) && library.edition_in_scope(&edition, scope).await? {
            shown.insert(edition);
        }
    }
    Ok(shown)
}

/// The patron's progress rows by edition, newest first.
///
/// # Errors
///
/// A 500 when the library cannot be read.
pub(crate) async fn progress_by_edition(
    patron: &Patron,
    include_finished: bool,
    cap: u64,
    route: &str,
) -> PlayerResult<Vec<PlaybackProgress>> {
    let (rows, _) = patron
        .library
        .list_playback_progress(patron.user_id(), include_finished, None, cap)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?;
    Ok(rows)
}

/// Lists the items the patron is part way through, most recent first.
#[utoipa::path(
    get,
    path = "/player/api/me/items-in-progress",
    responses(
        (status = 200, description = "The items, each with the patron's progress.", body = LibraryItems),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_items_in_progress(
    Extension(patron): Extension<Patron>,
) -> PlayerResult<Json<LibraryItems>> {
    const ROUTE: &str = "GET /player/api/me/items-in-progress";
    let rows = progress_by_edition(&patron, false, MAX_IN_PROGRESS, ROUTE).await?;
    let ids: Vec<String> = rows.iter().map(|p| p.edition_id.clone()).collect();
    let items: HashMap<String, PlayerItem> = visible_items(&patron, &ids, ROUTE)
        .await?
        .into_iter()
        .map(|item| (item.edition_id.clone(), item))
        .collect();
    let library_items = rows
        .into_iter()
        .filter_map(|progress| {
            let item = items.get(&progress.edition_id)?;
            Some(item_wire(
                item,
                Some(progress_wire(patron.user_id(), progress)),
                None,
            ))
        })
        .collect();
    Ok(Json(LibraryItems { library_items }))
}

/// The items of `sessions` the patron may still see, by edition.
///
/// # Errors
///
/// A 500 when the library cannot be read.
async fn session_items(
    patron: &Patron,
    sessions: &[StoredSession],
    route: &str,
) -> PlayerResult<HashMap<String, PlayerItem>> {
    let mut ids: Vec<String> = sessions.iter().map(|s| s.edition_id.clone()).collect();
    ids.sort();
    ids.dedup();
    Ok(visible_items(patron, &ids, route)
        .await?
        .into_iter()
        .map(|item| (item.edition_id.clone(), item))
        .collect())
}

/// Lists the patron's play sessions, newest first.
#[utoipa::path(
    get,
    path = "/player/api/me/listening-sessions",
    params(
        ("itemsPerPage" = Option<u64>, Query, description = "Sessions per page, default 10."),
        ("page" = Option<u64>, Query, description = "Page number, from 0."),
    ),
    responses(
        (status = 200, description = "A page of sessions.", body = SessionPage),
        (status = 400, description = "A query value is out of range.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_listening_sessions(
    Extension(patron): Extension<Patron>,
    uri: Uri,
) -> PlayerResult<Json<SessionPage>> {
    const ROUTE: &str = "GET /player/api/me/listening-sessions";
    let query = parse_detail_query(uri.query().unwrap_or_default())
        .map_err(|_| PlayerError::bad_request())?;
    let per_page = query.limit.unwrap_or(DEFAULT_SESSIONS_PER_PAGE).max(1);
    let (sessions, total) = patron
        .library
        .list_playback_sessions(
            patron.user_id(),
            query.page.saturating_mul(per_page),
            per_page,
        )
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    let items = session_items(&patron, &sessions, ROUTE).await?;
    // A session of an item the patron may no longer see keeps its place
    // in the count but shows no title.
    let sessions = sessions
        .iter()
        .map(|session| session_wire(&patron, session, items.get(&session.edition_id), None))
        .collect();
    Ok(Json(SessionPage {
        total,
        num_pages: total.div_ceil(per_page),
        page: query.page,
        items_per_page: per_page,
        sessions,
    }))
}

/// Sums the patron's listening, by item and by day.
#[utoipa::path(
    get,
    path = "/player/api/me/listening-stats",
    responses(
        (status = 200, description = "Listening totals over the patron's latest 1000 sessions.", body = ListeningStats),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_listening_stats(
    Extension(patron): Extension<Patron>,
) -> PlayerResult<Json<ListeningStats>> {
    const ROUTE: &str = "GET /player/api/me/listening-stats";
    let (sessions, _) = patron
        .library
        .list_playback_sessions(patron.user_id(), 0, MAX_STATS_SESSIONS)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    let items = session_items(&patron, &sessions, ROUTE).await?;
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut total_ms = 0u64;
    let mut today_ms = 0u64;
    let mut by_item: BTreeMap<String, u64> = BTreeMap::new();
    let mut days: BTreeMap<String, u64> = BTreeMap::new();
    let mut weekdays: BTreeMap<String, u64> = BTreeMap::new();
    for session in &sessions {
        let listened = session.listened.get();
        total_ms = total_ms.saturating_add(listened);
        let day = session.started_at.format("%Y-%m-%d").to_string();
        if day == today {
            today_ms = today_ms.saturating_add(listened);
        }
        *days.entry(day).or_default() += listened;
        *weekdays
            .entry(session.started_at.format("%A").to_string())
            .or_default() += listened;
        if items.contains_key(&session.edition_id) {
            *by_item.entry(session.edition_id.clone()).or_default() += listened;
        }
    }
    let to_secs = |map: BTreeMap<String, u64>| -> BTreeMap<String, f64> {
        map.into_iter().map(|(k, ms)| (k, ms_to_secs(ms))).collect()
    };
    let item_stats = by_item
        .into_iter()
        .filter_map(|(id, ms)| {
            let item = items.get(&id)?;
            Some((
                id.clone(),
                ItemListening {
                    id,
                    time_listening: ms_to_secs(ms),
                    media_metadata: metadata_wire(item),
                },
            ))
        })
        .collect();
    let recent_sessions = sessions
        .iter()
        .take(RECENT_SESSIONS)
        .map(|session| session_wire(&patron, session, items.get(&session.edition_id), None))
        .collect();
    Ok(Json(ListeningStats {
        total_time: ms_to_secs(total_ms),
        items: item_stats,
        days: to_secs(days),
        day_of_week: to_secs(weekdays),
        today: ms_to_secs(today_ms),
        recent_sessions,
    }))
}

/// Adds a bookmark to an item.
#[utoipa::path(
    post,
    path = "/player/api/me/item/{id}/bookmark",
    params(("id" = String, Path, description = "Item id.")),
    request_body(content = super::bodies::BookmarkBody, description = "The place, in seconds, and a title."),
    responses(
        (status = 200, description = "The bookmark.", body = BookmarkWire),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such item.", body = String, content_type = "text/plain"),
        (status = 409, description = "The patron has as many bookmarks as the library keeps.", body = String, content_type = "text/plain", example = json!("Too many bookmarks")),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_bookmark(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> PlayerResult<Json<BookmarkWire>> {
    const ROUTE: &str = "POST /player/api/me/item/{id}/bookmark";
    let body = read_body(body, MAX_BODY_BYTES).await?;
    let request = parse_bookmark_request(&body).map_err(|_| PlayerError::bad_request())?;
    let item = visible_item(&patron, &id, ROUTE).await?;
    let bookmark = patron
        .library
        .add_bookmark(
            patron.user_id(),
            &item.edition_id,
            timeline(request.position_ms)?,
            &request.title,
        )
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?
        .ok_or_else(|| PlayerError::new(StatusCode::CONFLICT, "Too many bookmarks"))?;
    crate::live::poke(LiveTopic::Playback);
    Ok(Json(bookmark_wire(bookmark)))
}

/// The patron's bookmark at `position_ms` in an item.
///
/// # Errors
///
/// A 404 when there is none, a 500 when the library cannot be read.
async fn bookmark_at(
    patron: &Patron,
    item: &PlayerItem,
    position_ms: u64,
    route: &str,
) -> PlayerResult<Bookmark> {
    patron
        .library
        .list_bookmarks(patron.user_id(), &item.edition_id)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?
        .into_iter()
        .find(|bookmark| bookmark.position.get() == position_ms)
        .ok_or_else(PlayerError::not_found)
}

/// Renames the bookmark at a time in an item.
#[utoipa::path(
    patch,
    path = "/player/api/me/item/{id}/bookmark",
    params(("id" = String, Path, description = "Item id.")),
    request_body(content = super::bodies::BookmarkBody, description = "Which bookmark, by its place in seconds, and its new title."),
    responses(
        (status = 200, description = "The bookmark.", body = BookmarkWire),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such item or bookmark.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn patch_bookmark(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    body: Body,
) -> PlayerResult<Json<BookmarkWire>> {
    const ROUTE: &str = "PATCH /player/api/me/item/{id}/bookmark";
    let body = read_body(body, MAX_BODY_BYTES).await?;
    let request = parse_bookmark_request(&body).map_err(|_| PlayerError::bad_request())?;
    let item = visible_item(&patron, &id, ROUTE).await?;
    let existing = bookmark_at(&patron, &item, request.position_ms, ROUTE).await?;
    let bookmark = patron
        .library
        .edit_bookmark(patron.user_id(), &existing.id, None, Some(&request.title))
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?
        .ok_or_else(PlayerError::not_found)?;
    crate::live::poke(LiveTopic::Playback);
    Ok(Json(bookmark_wire(bookmark)))
}

/// Removes the bookmark at a time in an item.
#[utoipa::path(
    delete,
    path = "/player/api/me/item/{id}/bookmark/{time}",
    operation_id = "player_delete_bookmark",
    params(
        ("id" = String, Path, description = "Item id."),
        ("time" = String, Path, description = "The bookmark's time in seconds."),
    ),
    responses(
        (status = 200, description = "`OK`.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such item or bookmark.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn delete_bookmark(
    Extension(patron): Extension<Patron>,
    AxumPath((id, time)): AxumPath<(String, String)>,
) -> PlayerResult<Response> {
    const ROUTE: &str = "DELETE /player/api/me/item/{id}/bookmark/{time}";
    let position_ms = parse_bookmark_time(&time).ok_or_else(PlayerError::not_found)?;
    let item = visible_item(&patron, &id, ROUTE).await?;
    let existing = bookmark_at(&patron, &item, position_ms, ROUTE).await?;
    patron
        .library
        .delete_bookmark(patron.user_id(), &existing.id)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    crate::live::poke(LiveTopic::Playback);
    Ok(ok().into_response())
}
