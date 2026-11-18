//! A reader takes the copy this library already holds of an edition.
//!
//! | Request | Answer |
//! |---|---|
//! | `copy.grant {store, marketplace?, product_id}` (node socket) | `copy.granted {copy_id, sealed_token, expires_in_secs, copy, files}` or `error {code: "refused" \| "not_found" \| "invalid" \| "unavailable"}` |
//! | `GET /api/copies/v1/{copy_id}/files/{n}` | 200 or 206 with the file's bytes |
//!
//! The grant names an edition by its store listing and no person: the
//! person is the asking node's owner. Whether they may have the copy is
//! decided in the library (`control_plane::plan_copy`), which answers "you
//! may not" and "there is no copy" alike as `not_found`. This node then
//! picks the first of its output destinations that holds the edition's
//! files, reads their sizes, and seals the token to the node's box key.
//!
//! The download carries `Authorization: Bookclerk-Copy <token>`. The route
//! sits outside the cookie and operator auth layers (the token is the
//! credential) and runs the transport policy first. Every request checks
//! the grant afresh (`control_plane::authorize_copy`): the token, the
//! branch's trust, the person, their right to the edition, and that the
//! files are still the ones granted. Refusals: 401 `unauthorized` (any of
//! those), 421 `misdirected` (another node granted the copy), 404
//! `unknown_file`, 416 for a range past the file's end, 503 `unavailable`.
//!
//! A file is streamed from the destination and never staged here. One
//! byte range is honoured (`bookclerk_node_proto::parse_copy_range`), so a
//! reader resumes a broken download.
//!
//! The token never reaches a log: request tracing logs method and path
//! only, and these handlers log copy and node ids.

use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    authorize_copy, load_or_create_node_identity, open_copy, plan_copy, CopyHolder, CopyListing,
    CopyPlanFile, CopyPlanned,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{
    copy_grant_problem, copy_granted_problem, parse_copy_range, seal_copy_token, Body as FrameBody,
    CopyFile, CopyGrantedFrame, CopyRange, Frame, COPY_AUTH_SCHEME, MAX_COPY_FILE_BYTES,
    MAX_FRAME_BYTES,
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

/// `copy.grant`.
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
    let plan = match plan_copy(library, holder, listing).await? {
        CopyPlanned::Refused => return Ok(error(id, "refused", "not_a_branch")),
        CopyPlanned::NotFound => return Ok(not_found(id)),
        CopyPlanned::Planned(plan) => plan,
    };
    let destinations = match owner_destinations(&host.state).await {
        Ok(destinations) => destinations,
        Err(err) => {
            tracing::warn!(node_id = %host.node_id, error = %err, "no output destination to read a copy from");
            return Ok(error(id, "unavailable", "storage is unavailable"));
        }
    };
    // The first destination, in this node's order, that holds every file.
    let mut chosen = None;
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
                tracing::warn!(node_id = %host.node_id, destination = %destination.id, error = %err, "could not size a copy's files");
            }
        }
    }
    let Some((destination, files)) = chosen else {
        return Ok(not_found(id));
    };
    let Some(opened) = open_copy(library, holder, &plan, destination, &host.owner_node_id).await?
    else {
        return Ok(not_found(id));
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
        copy: &plan.copy,
        files: &files,
    });
    let frame = Frame::new(
        id.clone(),
        FrameBody::CopyGranted {
            copy_id: opened.copy_id.clone(),
            sealed_token,
            expires_in_secs: opened.expires_in_secs,
            copy: plan.copy,
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
        ("copy_id" = String, Path, description = "Copy id from `copy.granted`."),
        ("index" = u32, Path, description = "The file's index in the grant's `files`."),
        ("Range" = Option<String>, Header, description = "One byte range (`bytes=A-B`, `bytes=A-` or `bytes=-N`); anything else reads the whole file."),
    ),
    responses(
        (status = 200, description = "The whole file.", body = crate::openapi::BinaryBody, content_type = "application/octet-stream"),
        (status = 206, description = "The requested byte range, with `Content-Range`.", body = crate::openapi::BinaryBody, content_type = "application/octet-stream"),
        (status = 401, description = "The copy token is missing, wrong or expired, or the grant no longer holds: the branch is no longer trusted, the person may no longer take the edition, or the library's files changed since the grant (`unauthorized`). Ask for a new grant.", body = crate::http_error::ErrorMessage, example = json!({"error": "unauthorized"})),
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
