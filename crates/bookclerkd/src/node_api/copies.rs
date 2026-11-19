//! A reader takes the copy this library already holds of an edition.
//!
//! | Request | Answer |
//! |---|---|
//! | `copy.grant {store, marketplace?, product_id}` (node socket) | `copy.granted {copy_id, sealed_token, expires_in_secs, copy, files}` or `error {code: "refused" \| "not_found" \| "invalid" \| "unavailable"}` |
//! | `copy.grant {store, marketplace?, product_id}` (clerk socket) | `copy.granted {store, marketplace?, product_id, copy_id, token, expires_in_secs, copy, files}` or `error {code: "not_found" \| "invalid" \| "unavailable"}` |
//! | `GET /api/copies/v1/{copy_id}/files/{n}` | 200 or 206 with the file's bytes |
//!
//! The grant names an edition by its store listing and no person: the
//! person is the asking node's owner, or the asking library card's. Whether they may have the copy is
//! decided in the library (`control_plane::plan_copy`), which answers "you
//! may not" and "there is no copy" alike as `not_found`. This node then
//! picks the first of its output destinations that holds the edition's
//! files, reads their sizes, and seals the token to the node's box key.
//! A clerk has no box key: its token is frame text on the card's own
//! connection (TLS, except on loopback, by the transport policy).
//!
//! The download carries `Authorization: Bookclerk-Copy <token>`. The route
//! sits outside the cookie and operator auth layers (the token is the
//! credential) and runs the transport policy first. Every request checks
//! the grant afresh (`control_plane::authorize_copy`): the token, the
//! branch's trust or that the card is still signed in, the person, their
//! right to the edition, and that the files are still the ones granted. Refusals: 401 `unauthorized` (any of
//! those), 421 `misdirected` (another node granted the copy), 404
//! `unknown_file`, 416 for a range past the file's end, 503 `unavailable`.
//!
//! A file is streamed from the destination and never staged here. One
//! byte range is honoured (`bookclerk_node_proto::parse_copy_range`), so a
//! reader resumes a broken download.
//!
//! The token never reaches a log: request tracing logs method and path
//! only, and these handlers log copy and node ids, never a card or its
//! hash.

use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    authorize_copy, load_or_create_node_identity, open_copy, plan_copy, CopyHolder, CopyListing,
    CopyPlanFile, CopyPlanned, OpenedCopy,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{
    copy_grant_problem, copy_granted_problem, parse_copy_range, seal_copy_token, Body as FrameBody,
    ClerkCopy, ClerkSocketError, CopyFile, CopyGrantedFrame, CopyRange, CopyToken, Frame,
    LibraryFrame, WindowCopy, COPY_AUTH_SCHEME, MAX_COPY_FILE_BYTES, MAX_FRAME_BYTES,
};
use bookclerk_storage::{ByteRange, StorageBackend};
use rand::RngCore as _;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use zeroize::Zeroize as _;

use super::check_transport;
use super::uploads::{owner_destinations, OwnerDestination};
use super::work::{error, HostContext};
use crate::api::AppState;
use crate::http_error::ErrorMessage;

/// Bytes read from the destination per chunk of the response body.
const CHUNK_BYTES: usize = 64 * 1024;

/// The files of `listed` with the sizes `destination` reports, or `None`
/// when one is missing there or larger than a frame may state.
async fn sized(
    destination: &dyn StorageBackend,
    listed: &[CopyPlanFile],
) -> anyhow::Result<Option<Vec<CopyFile>>> {
    let keys: Vec<String> = listed.iter().map(|file| file.key.clone()).collect();
    let probes = destination.head_many(&keys).await?;
    if probes.len() != listed.len() {
        return Ok(None);
    }
    let mut files = Vec::with_capacity(listed.len());
    for (file, probe) in listed.iter().zip(probes) {
        let Some(probe) = probe.filter(|probe| probe.size <= MAX_COPY_FILE_BYTES) else {
            return Ok(None);
        };
        files.push(CopyFile {
            size: probe.size,
            ..file.file.clone()
        });
    }
    Ok(Some(files))
}

/// What asking for a copy came to, before it is written as a frame of
/// the socket it was asked on.
pub(super) enum Granted {
    /// The holder is not one this library gives copies to.
    Refused,
    /// The person may not take the edition, the library holds no copy of
    /// it, or no destination of this node holds every file. One answer for
    /// all three.
    NotFound,
    /// This node's storage could not be read, or a destination that
    /// lists the files could not be asked for them; ask again later.
    Unavailable,
    /// The grant is recorded.
    Opened {
        /// The copy id and its token.
        opened: OpenedCopy,
        /// What the copy is.
        copy: WindowCopy,
        /// The files, with the sizes the destination reports.
        files: Vec<CopyFile>,
    },
}

/// Plans and records a grant of the library's copy of `listing`'s edition
/// to `holder`: the one path a node socket's and a clerk socket's
/// `copy.grant` share.
///
/// # Arguments
///
/// * `library` - The library snapshot the holder was checked in.
/// * `state` - The daemon, for this node's output destinations.
/// * `holder` - Who asks.
/// * `serving_node_id` - This node's id; the downloads are served here.
/// * `listing` - The store listing, already well formed.
///
/// # Errors
///
/// Returns the library's read or write error.
pub(super) async fn open_for(
    library: &LibraryStore,
    state: &AppState,
    holder: CopyHolder<'_>,
    serving_node_id: &str,
    listing: CopyListing<'_>,
) -> bookclerk_library::Result<Granted> {
    let plan = match plan_copy(library, holder, listing).await? {
        CopyPlanned::Refused => return Ok(Granted::Refused),
        CopyPlanned::NotFound => return Ok(Granted::NotFound),
        CopyPlanned::Planned(plan) => plan,
    };
    let destinations = match owner_destinations(state).await {
        Ok(destinations) => destinations,
        Err(err) => {
            tracing::warn!(error = %err, "no output destination to read a copy from");
            return Ok(Granted::Unavailable);
        }
    };
    // The first destination, in this node's order, that holds every file.
    let mut chosen = None;
    // A destination that could not be asked may hold the copy: "not
    // found" is only for destinations that answered.
    let mut unasked = false;
    for destination in &destinations {
        let Some(listed) = plan.destinations.get(&destination.id) else {
            continue;
        };
        match sized(destination.backend.as_ref(), listed).await {
            Ok(Some(files)) => {
                chosen = Some((destination.id.as_str(), files));
                break;
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(destination = %destination.id, error = %err, "could not size a copy's files");
                unasked = true;
            }
        }
    }
    let Some((destination, files)) = chosen else {
        return Ok(if unasked {
            Granted::Unavailable
        } else {
            Granted::NotFound
        });
    };
    let Some(opened) = open_copy(library, holder, &plan, destination, serving_node_id).await?
    else {
        return Ok(Granted::NotFound);
    };
    Ok(Granted::Opened {
        opened,
        copy: plan.copy,
        files,
    })
}

/// `copy.grant` on a node socket.
pub(super) async fn grant(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    listing: CopyListing<'_>,
) -> bookclerk_library::Result<Frame> {
    if copy_grant_problem(listing.store, listing.marketplace, listing.product_id).is_some() {
        return Ok(error(id, "invalid", "malformed copy.grant"));
    }
    let not_found = |id| error(id, "not_found", "no copy of that edition for this person");
    let holder = CopyHolder::Node(&host.node_id);
    let (opened, copy, files) =
        match open_for(library, &host.state, holder, &host.shared_node_id, listing).await? {
            Granted::Refused => return Ok(error(id, "refused", "not_a_branch")),
            Granted::NotFound => return Ok(not_found(id)),
            Granted::Unavailable => return Ok(error(id, "unavailable", "storage is unavailable")),
            Granted::Opened {
                opened,
                copy,
                files,
            } => (opened, copy, files),
        };
    let mut ephemeral = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut ephemeral);
    let sealed = seal_copy_token(
        &host.box_public_key,
        &ephemeral,
        &host.node_id,
        &opened.copy_id,
        opened.token.as_bytes(),
    );
    ephemeral.zeroize();
    let Ok(sealed_token) = sealed else {
        return Ok(error(
            id,
            "unavailable",
            "the copy token could not be sealed",
        ));
    };
    let problem = copy_granted_problem(&CopyGrantedFrame {
        copy_id: &opened.copy_id,
        sealed_token: &sealed_token,
        expires_in_secs: opened.expires_in_secs,
        copy: &copy,
        files: &files,
    });
    let frame = Frame::new(
        id.clone(),
        FrameBody::CopyGranted {
            copy_id: opened.copy_id.clone(),
            sealed_token,
            expires_in_secs: opened.expires_in_secs,
            copy,
            files,
        },
    );
    if problem.is_some() || frame.to_text().len() > MAX_FRAME_BYTES {
        tracing::warn!(node_id = %host.node_id, copy_id = %opened.copy_id, problem, "a copy's answer does not fit a frame; not granted");
        return Ok(not_found(id));
    }
    tracing::info!(node_id = %host.node_id, copy_id = %opened.copy_id, "granted a copy of an edition");
    Ok(frame)
}

/// `copy.grant` on a clerk socket: the same grant, for the card's person,
/// with the token as frame text (a clerk has no box key; the socket is
/// the card's own connection, TLS except on loopback).
///
/// # Arguments
///
/// * `library` - The library snapshot the card was just checked in.
/// * `state` - The daemon.
/// * `card` - The library card's token.
/// * `listing` - The store listing, already well formed.
///
/// # Returns
///
/// `copy.granted` with its grant, or the `error` frame. The caller holds
/// the grant until the frame is sent: the token stays in the redaction
/// registry for as long as the grant lives.
pub(super) async fn grant_to_card(
    library: &LibraryStore,
    state: &AppState,
    card: &str,
    listing: CopyListing<'_>,
) -> (LibraryFrame, Option<OpenedCopy>) {
    let failed = |code| (LibraryFrame::Error { code }, None);
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let Ok(this_node) = load_or_create_node_identity(&files_dir) else {
        tracing::warn!("no node identity to serve a copy from");
        return failed(ClerkSocketError::Unavailable);
    };
    let card_hash = bookclerk_library::hash_token(card);
    let holder = CopyHolder::Card(&card_hash);
    let (opened, copy, files) =
        match open_for(library, state, holder, &this_node.node_id, listing).await {
            // The card was live a moment ago; one that is not now reads
            // like an edition it may not take.
            Ok(Granted::Refused | Granted::NotFound) => return failed(ClerkSocketError::NotFound),
            Ok(Granted::Unavailable) => return failed(ClerkSocketError::Unavailable),
            Ok(Granted::Opened {
                opened,
                copy,
                files,
            }) => (opened, copy, files),
            Err(err) => {
                tracing::warn!(error = %err, "clerk socket could not grant a copy");
                return failed(ClerkSocketError::Unavailable);
            }
        };
    let granted = ClerkCopy {
        store: listing.store.to_owned(),
        marketplace: listing.marketplace.to_owned(),
        product_id: listing.product_id.to_owned(),
        copy_id: opened.copy_id.clone(),
        token: CopyToken::new(&opened.token),
        expires_in_secs: opened.expires_in_secs,
        copy,
        files,
    };
    let problem = granted.problem();
    let frame = LibraryFrame::CopyGranted(Box::new(granted));
    if problem.is_some() || frame.encode().len() > MAX_FRAME_BYTES {
        tracing::warn!(copy_id = %opened.copy_id, problem, "a copy's answer does not fit a frame; not granted");
        return failed(ClerkSocketError::NotFound);
    }
    tracing::info!(copy_id = %opened.copy_id, "granted a copy of an edition to a library card");
    (frame, Some(opened))
}

/// A JSON refusal.
fn refusal(code: StatusCode, error: &'static str) -> Response {
    (code, Json(ErrorMessage::new(error))).into_response()
}

/// The bearer token of a download request.
fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme == COPY_AUTH_SCHEME && !token.is_empty()).then_some(token.trim())
}

/// A response body that reads `reader` to its end, a chunk at a time.
fn body_of(reader: Pin<Box<dyn AsyncRead + Send>>) -> Body {
    Body::from_stream(futures::stream::unfold(Some(reader), |state| async move {
        let mut reader = state?;
        let mut chunk = vec![0u8; CHUNK_BYTES];
        match reader.read(&mut chunk).await {
            Ok(0) => None,
            Ok(read) => {
                chunk.truncate(read);
                Some((Ok::<_, std::io::Error>(Bytes::from(chunk)), Some(reader)))
            }
            // The connection ends short of the stated length, which the
            // reader sees as a failed download.
            Err(err) => Some((Err(err), None)),
        }
    }))
}

/// Streams one file of a granted copy.
#[utoipa::path(
    get,
    path = "/api/copies/v1/{copy_id}/files/{index}",
    security(("copyToken" = [])),
    params(
        ("copy_id" = String, Path, description = "Copy id from `copy.granted`, on the node socket or the clerk socket."),
        ("index" = u32, Path, description = "The file's index in the grant's `files`."),
        ("Range" = Option<String>, Header, description = "One byte range (`bytes=A-B`, `bytes=A-` or `bytes=-N`); anything else reads the whole file."),
    ),
    responses(
        (status = 200, description = "The whole file.", body = crate::openapi::BinaryBody, content_type = "application/octet-stream"),
        (status = 206, description = "The requested byte range, with `Content-Range`.", body = crate::openapi::BinaryBody, content_type = "application/octet-stream"),
        (status = 401, description = "The copy token is missing, wrong or expired, or the grant no longer holds: the branch is no longer trusted or the library card is signed out, the person may no longer take the edition, or the library's files changed since the grant (`unauthorized`). Ask for a new grant; a clerk whose library card is signed out signs in again first.", body = crate::http_error::ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The connection failed the transport policy (`transport_insecure`, with the check in `reason`).", body = crate::http_error::TransportRefused, example = json!({"error": "transport_insecure", "reason": "plain_http"})),
        (status = 404, description = "The index names no file of the copy (`unknown_file`), or the node API is not mounted on this listener (`node_api_unmounted`).", body = crate::http_error::ErrorMessage, example = json!({"error": "unknown_file"})),
        (status = 416, description = "The range starts at or past the end of the file (`range_not_satisfiable`); `Content-Range` states the size.", body = crate::http_error::ErrorMessage, example = json!({"error": "range_not_satisfiable"})),
        (status = 421, description = "Another node granted this copy (`misdirected`).", body = crate::http_error::ErrorMessage, example = json!({"error": "misdirected"})),
        (status = 503, description = "Storage or the grant record is unavailable; retry (`unavailable`).", body = crate::http_error::ErrorMessage, example = json!({"error": "unavailable"})),
    ),
)]
pub async fn get_file(
    State(state): State<Arc<AppState>>,
    Path((copy_id, index)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (mut parts, _body) = request.into_parts();
    if let Err(refused) = check_transport(&state, &mut parts, "copy").await {
        return refused;
    }
    let unauthorized = || refusal(StatusCode::UNAUTHORIZED, "unauthorized");
    let unavailable = || refusal(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    let Some(token) = bearer(&parts.headers) else {
        return unauthorized();
    };
    let library = state.library_snapshot().await;
    let download = match authorize_copy(&library, &copy_id, token).await {
        Ok(Some(download)) => download,
        Ok(None) => return unauthorized(),
        Err(err) => {
            tracing::warn!(copy_id, error = %err, "copy grant lookup failed");
            return unavailable();
        }
    };
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let this_node = load_or_create_node_identity(&files_dir)
        .ok()
        .map(|identity| identity.node_id);
    if this_node.as_deref() != Some(download.serving_node_id.as_str()) {
        return refusal(StatusCode::MISDIRECTED_REQUEST, "misdirected");
    }
    let Some(file) = index
        .parse::<usize>()
        .ok()
        .and_then(|index| download.files.get(index))
    else {
        return refusal(StatusCode::NOT_FOUND, "unknown_file");
    };
    let destination: OwnerDestination = match owner_destinations(&state).await {
        Ok(destinations) => {
            match destinations
                .into_iter()
                .find(|destination| destination.id == download.destination)
            {
                Some(destination) => destination,
                None => return unavailable(),
            }
        }
        Err(err) => {
            tracing::warn!(copy_id, error = %err, "no output destination to read a copy from");
            return unavailable();
        }
    };
    let size = match destination.backend.head(&file.key).await {
        Ok(Some(probe)) => probe.size,
        // The row says the file is there; the destination disagrees.
        Ok(None) => return unavailable(),
        Err(err) => {
            tracing::warn!(copy_id, error = %err, "could not size a copy's file");
            return unavailable();
        }
    };
    let range = parts
        .headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map_or(CopyRange::Whole, |value| parse_copy_range(value, size));
    let (status, read, length, content_range) = match range {
        CopyRange::Whole => (StatusCode::OK, None, size, None),
        CopyRange::Part { offset, length } => (
            StatusCode::PARTIAL_CONTENT,
            Some(ByteRange {
                offset,
                length: Some(length),
            }),
            length,
            Some(format!("bytes {offset}-{}/{size}", offset + (length - 1))),
        ),
        CopyRange::Unsatisfiable => {
            let mut response = refusal(StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable");
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            return response;
        }
    };
    let body = if length == 0 {
        Body::empty()
    } else {
        match destination.backend.get_stream(&file.key, read).await {
            Ok((_, reader)) => body_of(reader),
            Err(err) => {
                tracing::warn!(copy_id, error = %err, "could not read a copy's file");
                return unavailable();
            }
        }
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    if let Some(value) = content_range.and_then(|text| HeaderValue::from_str(&text).ok()) {
        headers.insert(header::CONTENT_RANGE, value);
    }
    response
}
