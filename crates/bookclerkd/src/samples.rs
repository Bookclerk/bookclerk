//! Book samples: a few minutes of a book to hear from its details, which
//! never count as listening (`docs/adr/book-samples.md`).
//!
//! | Route | What it does |
//! |---|---|
//! | `GET /api/library/editions/{id}/sample` | A sample cut from an edition this node holds |
//! | `POST /api/discover/title-sample` | Which sample a title's details can play: the held edition's path when this node can cut one, else a storefront's, else none |
//! | `GET /api/samples/{token}` | That storefront sample's audio |
//!
//! - **Never listening.** No route here opens a play session, reads or
//!   writes progress, or publishes an event, so a sample moves no position,
//!   marks nothing finished and feeds nothing Discover learns taste from.
//! - **One encoding.** Both kinds of sample are cut and encoded by the
//!   media worker ([`bookclerk_media::MediaPool::cut_sample`]), jailed to
//!   no path and no network: mono MP3 at 64 kbit/s. The bytes served are
//!   never the stored file's or the store's own bytes.
//! - **Own samples.** An edition's sample starts at its second chapter when
//!   its first is shorter than [`OPENING_MAX_MS`] (opening credits, a
//!   store's announcement), else at its start, and runs
//!   [`SAMPLE_LENGTH_MS`]. Anyone who may see the edition may hear it, as
//!   with streaming; the worker reads the file from the destination one
//!   block at a time.
//! - **Store samples.** A storefront declares a sample on its catalog
//!   record (`CatalogHit.sampleUrl`). The title-sample route asks the
//!   stores that list a title, signs the URL into a same-origin path with
//!   this node's own key (as cover paths are signed), and only a path this
//!   node issued is fetched. The first `covers` plugin downloads it with
//!   `media = audio` (public `https` hosts only, at most
//!   [`bookclerk_plugin_abi::MAX_COVER_BYTES`]); the host makes no request
//!   of its own. A store sample is cut to at most
//!   [`bookclerk_media::pcm_wire::MAX_SAMPLE_MS`].
//! - **Cache.** Encoded samples are kept in `cache/samples/`, named by a
//!   digest of what they were cut from (for an edition, the files the
//!   playback plan lists), so a changed file gets a new sample. The least
//!   recently served are removed once the folder passes
//!   [`CACHE_BUDGET_BYTES`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_cover::token::{self, is_coverable, TokenKey};
use bookclerk_library::control_plane::plan_playback;
use bookclerk_library::COPY_RUNTIME_TOLERANCE_MS;
use bookclerk_media::{ByteSpan, MediaError, RangeSource};
use bookclerk_node_proto::{parse_copy_range, CopyRange};
use bookclerk_storage::{ByteRange, StorageBackend};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::api::AppState;
use crate::http_error::ErrorMessage;
use crate::library_works::read_failed;
use crate::node_api::uploads::owner_destinations;
use crate::playback::{edition_in_scope, listener, playable_here};

/// Where the API mounts store sample paths.
pub(crate) const ROUTE_PREFIX: &str = "/api/samples";

/// How long an edition's sample runs: five minutes, as stores' samples
/// usually do.
pub(crate) const SAMPLE_LENGTH_MS: u64 = 5 * 60 * 1000;

/// A first chapter shorter than this (three minutes) is taken for opening
/// credits, and an edition's sample starts at the second chapter instead.
pub(crate) const OPENING_MAX_MS: u64 = 3 * 60 * 1000;

/// Bytes `cache/samples/` may hold before the least recently served
/// samples are removed (256 MiB, about a hundred samples).
pub(crate) const CACHE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// File holding this node's sample token key inside the cache directory.
const KEY_FILE: &str = "token.key";

/// Extension of a cached sample.
const SAMPLE_EXTENSION: &str = "mp3";

/// The browser cache header of an edition's sample: the person's right to
/// it can end at any time, so no browser keeps it.
const EDITION_CACHE_CONTROL: &str = "no-store";

/// The browser cache header of a store sample: a path always names the
/// same sample.
const STORE_CACHE_CONTROL: &str = "private, max-age=86400";

/// An error answer of these routes: status and JSON message.
type SampleError = (StatusCode, Json<ErrorMessage>);

/// The error answer `status` with `message`.
fn refuse(status: StatusCode, message: &str) -> SampleError {
    (status, Json(ErrorMessage::new(message)))
}

/// Why a sample could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SampleFailure {
    /// Nothing on this node can make it right now: no confined worker, no
    /// `covers` plugin, or storage that did not answer.
    Unavailable(String),
    /// The audio could not be fetched or decoded.
    Refused(String),
}

impl SampleFailure {
    /// The route's answer for this failure (logged here, never sent).
    fn answer(&self, route: &str) -> SampleError {
        match self {
            Self::Unavailable(detail) => {
                tracing::warn!(route, detail = %detail, "no sample could be made");
                refuse(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
            }
            Self::Refused(detail) => {
                tracing::info!(route, detail = %detail, "sample refused");
                refuse(StatusCode::UNPROCESSABLE_ENTITY, "no_sample")
            }
        }
    }
}

impl From<MediaError> for SampleFailure {
    fn from(err: MediaError) -> Self {
        match err {
            MediaError::Native(_) | MediaError::Mp4(_) | MediaError::Tags(_) => {
                Self::Refused(err.to_string())
            }
            MediaError::Worker { .. }
            | MediaError::NotIsolated { .. }
            | MediaError::InputMissing(_)
            | MediaError::OutputMissing(_)
            | MediaError::Cover(_)
            | MediaError::Archive(_)
            | MediaError::Io(_)
            | MediaError::Other(_) => Self::Unavailable(err.to_string()),
        }
    }
}

/// The request deadline of the sample routes: the media worker's own limit
/// ([`bookclerk_media::SAMPLE_TIMEOUT`]) plus time for the download before
/// it and the cache write after it.
pub(crate) const ROUTE_BUDGET: std::time::Duration =
    bookclerk_media::SAMPLE_TIMEOUT.saturating_add(std::time::Duration::from_secs(30));

/// Whether `path` is one of the sample routes that cut audio
/// (`/api/samples/{token}` or `/api/library/editions/{id}/sample`).
///
/// # Arguments
///
/// * `path` - A request path, without its query.
pub(crate) fn is_sample_path(path: &str) -> bool {
    if let Some(token) = path
        .strip_prefix(ROUTE_PREFIX)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        return !token.is_empty() && !token.contains('/');
    }
    path.strip_prefix("/api/library/editions/")
        .and_then(|rest| rest.strip_suffix("/sample"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/// Signs store sample URLs into same-origin paths and keeps encoded
/// samples.
pub(crate) struct SampleService {
    /// `cache/samples`, or `None` for a service that keeps nothing.
    dir: Option<PathBuf>,
    /// Signs and opens store sample tokens; never leaves this node.
    key: TokenKey,
    /// One lock per sample being made, so concurrent requests for it wait
    /// for one worker instead of starting several.
    flights: Arc<Flights>,
    /// Held while a sample is renamed into the cache and the cache trimmed,
    /// so two writes never trim from the same stale listing.
    publishing: Arc<Mutex<()>>,
    /// Bytes the cache may hold.
    budget: u64,
}

impl SampleService {
    /// A service caching under `dir`, with the token key kept there.
    ///
    /// # Errors
    ///
    /// The I/O error when the directory or the key cannot be created or
    /// read.
    pub(crate) fn open(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let key = crate::covers::load_or_create_key(&dir.join(KEY_FILE))?;
        Ok(Self {
            dir: Some(dir),
            key,
            flights: Arc::default(),
            publishing: Arc::default(),
            budget: CACHE_BUDGET_BYTES,
        })
    }

    /// A service that signs paths and makes samples but keeps none. Used
    /// when the cache directory cannot be created, and by tests.
    pub(crate) fn detached() -> Self {
        Self {
            dir: None,
            key: crate::covers::random_key(),
            flights: Arc::default(),
            publishing: Arc::default(),
            budget: CACHE_BUDGET_BYTES,
        }
    }

    /// The same-origin path of a store's sample URL.
    ///
    /// # Returns
    ///
    /// `None` for anything but an absolute `https` URL with a host, or one
    /// over 2048 bytes.
    pub(crate) fn path_for(&self, url: &str) -> Option<String> {
        let url = url.trim();
        if !is_coverable(url) {
            return None;
        }
        let token = token::sign(&self.key, url)?;
        Some(format!("{ROUTE_PREFIX}/{token}"))
    }

    /// The store sample URL a token names, when this node issued it.
    pub(crate) fn open_token(&self, token: &str) -> Option<String> {
        token::open(&self.key, token)
    }

    /// The sample `cache_key` names: the cached copy, or a new one from
    /// `make`, kept for next time.
    ///
    /// # Errors
    ///
    /// What `make` returns.
    pub(crate) async fn sample<F, Fut>(
        &self,
        cache_key: &str,
        make: F,
    ) -> Result<Vec<u8>, SampleFailure>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>, SampleFailure>>,
    {
        let Some(dir) = self.dir.as_ref() else {
            return make().await;
        };
        let name = format!(
            "{}.{SAMPLE_EXTENSION}",
            hex::encode(Sha256::digest(cache_key.as_bytes()))
        );
        let path = dir.join(&name);
        if let Some(bytes) = read_cached(&path).await {
            return Ok(bytes);
        }
        let flight = self.board(&name).await;
        // Another request may have made it while this one waited.
        if let Some(bytes) = read_cached(&path).await {
            return Ok(bytes);
        }
        let made = make().await;
        if let Ok(bytes) = &made {
            // The write owns the sample's lock until the file is in place
            // (or the write failed), even if this request is cancelled
            // meanwhile, so a waiter never cuts the same sample again.
            keep(
                dir,
                &name,
                bytes,
                self.budget,
                Arc::clone(&self.publishing),
                flight,
            )
            .await;
        }
        made
    }

    /// This request's share of the lock of `name`, registering the lock
    /// when no other request holds it.
    fn ticket(&self, name: &str) -> FlightTicket {
        let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        let lock = Arc::clone(flights.entry(name.to_owned()).or_default());
        FlightTicket {
            flights: Arc::clone(&self.flights),
            name: name.to_owned(),
            lock,
        }
    }

    /// Takes a share of the lock of `name` and waits until this request
    /// holds it.
    async fn board(&self, name: &str) -> Flight {
        let ticket = self.ticket(name);
        let guard = Arc::clone(&ticket.lock).lock_owned().await;
        Flight {
            _guard: guard,
            _ticket: ticket,
        }
    }
}

/// The locks of samples being made, by cache file name.
type Flights = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// A request holding a sample's lock. It owns everything it needs, so the
/// cache write can carry it onto its blocking task and release it there.
struct Flight {
    /// The held lock. Declared first, so it drops before the ticket and
    /// the ticket sees only the shares still outstanding.
    _guard: tokio::sync::OwnedMutexGuard<()>,
    /// This request's share, which removes the lock once it is the last.
    _ticket: FlightTicket,
}

/// One request's share of a sample's lock. Dropping it, when the request
/// finishes or is cancelled while it waits, releases the share and removes
/// the lock once no request holds one.
struct FlightTicket {
    /// The service's locks by sample name.
    flights: Arc<Flights>,
    /// The sample's cache file name.
    name: String,
    /// This request's clone of the lock.
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for FlightTicket {
    /// Releases the share under the map's lock. Clones are taken and
    /// released only under it, so the count read here is exact: two
    /// requests ending together see each other, and the last one removes
    /// the entry, only if the map still holds this same lock.
    fn drop(&mut self) {
        let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        let lock = std::mem::replace(&mut self.lock, Arc::new(tokio::sync::Mutex::new(())));
        // The map's reference plus this request's.
        let last = Arc::strong_count(&lock) <= 2;
        if last
            && flights
                .get(&self.name)
                .is_some_and(|kept| Arc::ptr_eq(kept, &lock))
        {
            flights.remove(&self.name);
        }
        drop(lock);
        drop(flights);
    }
}

/// Writes a sample into the cache (a temporary file renamed into place),
/// then trims the cache to `budget` bytes. The work runs on its own
/// blocking task, so a request dropped mid-write still renames or removes
/// its temporary file. Failures are logged: the sample is served either
/// way.
///
/// # Arguments
///
/// * `publishing` - Held around the rename and the trim, so concurrent
///   writes of different samples trim one after another, each from a
///   fresh listing.
/// * `flight` - The sample's own lock and this request's share of it,
///   released only once the file is in place or the write failed; a
///   request waiting on it then finds the file or makes it itself, and the
///   last share removes the lock.
async fn keep(
    dir: &std::path::Path,
    name: &str,
    bytes: &[u8],
    budget: u64,
    publishing: Arc<Mutex<()>>,
    flight: Flight,
) {
    let dir = dir.to_path_buf();
    let name = name.to_owned();
    let bytes = bytes.to_vec();
    let _ = tokio::task::spawn_blocking(move || {
        let _flight = flight;
        let staged = dir.join(format!(".{name}.{:016x}", rand::random::<u64>()));
        if let Err(err) = std::fs::write(&staged, &bytes) {
            let _ = std::fs::remove_file(&staged);
            tracing::warn!(error = %err, "could not keep a sample");
            return;
        }
        let _publishing = publishing.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(err) = std::fs::rename(&staged, dir.join(&name)) {
            let _ = std::fs::remove_file(&staged);
            tracing::warn!(error = %err, "could not keep a sample");
            return;
        }
        trim(&dir, budget);
    })
    .await;
}

/// Reads a cached sample and marks it served (its modification time), or
/// `None` when there is none.
async fn read_cached(path: &std::path::Path) -> Option<Vec<u8>> {
    let bytes = tokio::fs::read(path).await.ok()?;
    let touched = path.to_path_buf();
    // Best effort: an entry not marked is only evicted sooner.
    let _ = tokio::task::spawn_blocking(move || {
        std::fs::File::options()
            .write(true)
            .open(&touched)
            .and_then(|file| file.set_modified(SystemTime::now()))
    })
    .await;
    Some(bytes)
}

/// Removes the least recently served samples in `dir` until the rest fit
/// `budget` bytes. Files that are not samples are left alone.
fn trim(dir: &std::path::Path, budget: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut samples: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.path().extension().and_then(|ext| ext.to_str()) == Some(SAMPLE_EXTENSION)
                && !entry.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = samples.iter().map(|(_, len, _)| len).sum();
    samples.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in samples {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// A file on a destination, read one range at a time for the media worker.
struct StoredFile {
    /// The destination holding it.
    backend: Box<dyn StorageBackend>,
    /// Its storage key there.
    key: String,
}

impl RangeSource for StoredFile {
    async fn read_range(&self, span: ByteSpan) -> std::io::Result<Vec<u8>> {
        let (_, reader) = self
            .backend
            .get_stream(
                &self.key,
                Some(ByteRange {
                    offset: span.offset,
                    length: Some(span.len),
                }),
            )
            .await
            .map_err(std::io::Error::other)?;
        let wanted = usize::try_from(span.len).map_err(std::io::Error::other)?;
        let mut bytes = Vec::with_capacity(wanted);
        // One byte past the span shows a destination that ignored the
        // range, without reading the rest of the file.
        reader
            .take(span.len.saturating_add(1))
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() != wanted {
            return Err(std::io::Error::other(format!(
                "the destination answered {} bytes for a {wanted}-byte range",
                bytes.len()
            )));
        }
        Ok(bytes)
    }
}

/// A file held in memory (a downloaded store sample).
struct Downloaded(Vec<u8>);

impl RangeSource for Downloaded {
    async fn read_range(&self, span: ByteSpan) -> std::io::Result<Vec<u8>> {
        let from = usize::try_from(span.offset).map_err(std::io::Error::other)?;
        let len = usize::try_from(span.len).map_err(std::io::Error::other)?;
        from.checked_add(len)
            .and_then(|to| self.0.get(from..to))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| std::io::Error::other("span outside the download"))
    }
}

/// The starts of an edition's top-level chapters, in order. Nested
/// chapters are left out, so a part's first sub-chapter is not taken for
/// the end of an opening.
pub(crate) fn top_level_starts(chapters: &[bookclerk_library::Chapter]) -> Vec<u64> {
    chapters
        .iter()
        .filter(|chapter| chapter.depth == 0)
        .map(|chapter| chapter.start_ms)
        .collect()
}

/// Whether a chapter list measured on a `listed_ms` timeline fits a copy
/// running `own_ms`: within [`COPY_RUNTIME_TOLERANCE_MS`], or either time
/// unknown (as the player assumes). A copy off the list's timeline that
/// carries no chapters of its own is sampled from its start.
pub(crate) fn on_copy_timeline(listed_ms: Option<u64>, own_ms: Option<u64>) -> bool {
    match (listed_ms, own_ms) {
        (Some(listed), Some(own)) => listed.abs_diff(own) <= COPY_RUNTIME_TOLERANCE_MS,
        (None, _) | (_, None) => true,
    }
}

/// Where an edition's sample starts: the second chapter when the first is
/// shorter than [`OPENING_MAX_MS`], else the start.
///
/// # Arguments
///
/// * `chapter_starts` - Top-level chapter starts in milliseconds, in order
///   ([`top_level_starts`]).
pub(crate) fn sample_start(chapter_starts: &[u64]) -> u64 {
    match chapter_starts {
        [first, second, ..] if second.saturating_sub(*first) < OPENING_MAX_MS => *second,
        _ => 0,
    }
}

/// An MP3 sample as a response: whole, or the one byte range a `Range`
/// header asks for.
fn mp3_response(bytes: Vec<u8>, range: Option<&HeaderValue>, cache: &'static str) -> Response {
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let range = range
        .and_then(|value| value.to_str().ok())
        .map_or(CopyRange::Whole, |value| parse_copy_range(value, size));
    let (status, body, content_range) = match range {
        CopyRange::Whole => (StatusCode::OK, bytes, None),
        CopyRange::Part { offset, length } => {
            let part = usize::try_from(offset)
                .ok()
                .zip(usize::try_from(length).ok())
                .and_then(|(from, len)| bytes.get(from..from.checked_add(len)?))
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            let last = offset + length.saturating_sub(1);
            (
                StatusCode::PARTIAL_CONTENT,
                part,
                Some(format!("bytes {offset}-{last}/{size}")),
            )
        }
        CopyRange::Unsatisfiable => {
            let mut response =
                refuse(StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable").into_response();
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            return response;
        }
    };
    let length = body.len();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    if let Some(value) = content_range.and_then(|text| HeaderValue::from_str(&text).ok()) {
        headers.insert(header::CONTENT_RANGE, value);
    }
    response
}

/// Plays a few minutes of an edition this node holds.
#[utoipa::path(
    get,
    path = "/api/library/editions/{id}/sample",
    params(
        ("id" = String, Path, description = "Edition id (an `id` in a work's `editions`)."),
        ("Range" = Option<String>, Header, description = "One byte range (`bytes=A-B`, `bytes=A-` or `bytes=-N`); anything else reads the whole sample."),
    ),
    responses(
        (status = 200, description = "The sample: mono MP3, five minutes or the rest of the first file.", body = crate::openapi::BinaryBody, content_type = "audio/mpeg"),
        (status = 206, description = "The requested byte range, with `Content-Range`.", body = crate::openapi::BinaryBody, content_type = "audio/mpeg"),
        (status = 401, description = "The session ended.", body = ErrorMessage, example = json!({"error": "sign in again"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep listening state for"})),
        (status = 404, description = "No such edition (`edition … not found`), or this node holds none of its audio (`no_sample`).", body = ErrorMessage, example = json!({"error": "no_sample"})),
        (status = 416, description = "The range starts at or past the end of the sample (`range_not_satisfiable`); `Content-Range` states the size.", body = ErrorMessage, example = json!({"error": "range_not_satisfiable"})),
        (status = 422, description = "The edition's audio could not be decoded (`no_sample`).", body = ErrorMessage, example = json!({"error": "no_sample"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
        (status = 503, description = "No confined media worker or storage is available; retry (`unavailable`).", body = ErrorMessage, example = json!({"error": "unavailable"})),
    ),
)]
pub(crate) async fn get_edition_sample(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, SampleError> {
    const ROUTE: &str = "GET /api/library/editions/{id}/sample";
    let user_id = listener(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let edition = edition_in_scope(&state, &library, &headers, &id, ROUTE).await?;
    let no_sample = || refuse(StatusCode::NOT_FOUND, "no_sample");
    let plan = plan_playback(&library, user_id, &edition.edition_id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?
        .ok_or_else(no_sample)?;
    let destinations = owner_destinations(&state).await.map_err(|err| {
        tracing::warn!(error = %err, route = ROUTE, "no output destination to sample from");
        refuse(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    })?;
    let (destination_id, track_key, single_file) = {
        let (destination, tracks) = playable_here(&plan, &destinations).ok_or_else(no_sample)?;
        let first = tracks.first().ok_or_else(no_sample)?;
        (destination.id.clone(), first.key.clone(), tracks.len() == 1)
    };
    // Chapter starts are on the copy's timeline, which is the first
    // file's only when there is one file. The chapters are the ones the
    // player shows for this same copy: the edition's list, or the copy's
    // own when its running time is off the list's timeline. A copy off
    // the timeline with no chapters of its own starts at zero.
    let start_ms = if single_file {
        let chapters = library
            .chapters_for_copy(
                &edition.edition_id,
                &destination_id,
                std::slice::from_ref(&track_key),
            )
            .await
            .map_err(|err| read_failed(ROUTE, &err))?;
        let own_ms = library
            .edition_audio_files(&edition.edition_id)
            .await
            .map_err(|err| read_failed(ROUTE, &err))?
            .into_iter()
            .find(|file| file.destination == destination_id && file.storage_key == track_key)
            .and_then(|file| file.stream.duration_ms);
        let starts: Vec<u64> = chapters
            .filter(|found| on_copy_timeline(found.runtime_ms, own_ms))
            .map(|found| top_level_starts(found.list.chapters()))
            .unwrap_or_default();
        sample_start(&starts)
    } else {
        0
    };
    let digest = plan.digest(&destination_id).unwrap_or_default().to_owned();
    let cache_key = format!(
        "edition\n{}\n{destination_id}\n{digest}\n{track_key}\n{start_ms}\n{SAMPLE_LENGTH_MS}",
        edition.edition_id
    );
    let backend = destinations
        .into_iter()
        .find(|destination| destination.id == destination_id)
        .map(|destination| destination.backend)
        .ok_or_else(no_sample)?;
    let bytes = state
        .samples
        .sample(&cache_key, || cut_stored(backend, track_key, start_ms))
        .await
        .map_err(|failure| failure.answer(ROUTE))?;
    Ok(mp3_response(
        bytes,
        headers.get(header::RANGE),
        EDITION_CACHE_CONTROL,
    ))
}

/// Cuts an edition's sample from its file on a destination.
async fn cut_stored(
    backend: Box<dyn StorageBackend>,
    key: String,
    start_ms: u64,
) -> Result<Vec<u8>, SampleFailure> {
    let size = match backend.head(&key).await {
        Ok(Some(probe)) => probe.size,
        Ok(None) => return Err(SampleFailure::Unavailable("the file is not stored".into())),
        Err(err) => return Err(SampleFailure::Unavailable(err.to_string())),
    };
    let sample = bookclerk_media::pool()
        .cut_sample(
            size,
            start_ms,
            SAMPLE_LENGTH_MS,
            StoredFile { backend, key },
        )
        .await?;
    Ok(sample.mp3)
}

/// `POST /api/discover/title-sample` request: the title as title-meta
/// sees it, and the edition this library holds of it, when it holds one.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct TitleSampleQuery {
    /// The title, its identifiers and the stores that list it.
    #[serde(flatten)]
    title: bookclerk_discover::TitleMetaQuery,
    /// The library edition the caller would hear a sample of (an `id` in a
    /// work's `editions`); the stores are asked only when this node cannot
    /// cut a sample from it.
    #[serde(default)]
    #[schema(example = "5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d")]
    edition_id: Option<String>,
}

/// `POST /api/discover/title-sample` answer. At most one path is given;
/// none means no sample can be played here, so the app shows no sample
/// control.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct TitleSampleJson {
    /// Same-origin path of a sample cut from the held edition
    /// (`GET /api/library/editions/{id}/sample`); present when the caller
    /// may hear that edition and this node reaches its audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "/api/library/editions/5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d/sample")]
    edition_sample_path: Option<String>,
    /// Same-origin path of a storefront's sample of the title, to play
    /// with `GET`; absent when an edition sample is offered instead, or no
    /// store that lists the title offers one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(
        example = "/api/samples/aHR0cHM6Ly9zYW1wbGVzLmV4YW1wbGUvYS5tcDM.q2fW1n4bJm0Rg9xV3kTzPA"
    )]
    sample_path: Option<String>,
}

/// The edition sample path for edition `id`, when the caller may hear it
/// and this node can cut a sample from it: the playback plan's rights
/// check passes and one of this node's destinations holds its audio. Cuts
/// nothing.
async fn edition_sample_here(state: &AppState, headers: &HeaderMap, id: &str) -> Option<String> {
    const ROUTE: &str = "POST /api/discover/title-sample";
    let user_id = listener(state, headers).await.ok()?;
    let library = state.library_snapshot().await;
    let edition = edition_in_scope(state, &library, headers, id, ROUTE)
        .await
        .ok()?;
    let plan = plan_playback(&library, user_id, &edition.edition_id)
        .await
        .ok()??;
    let destinations = owner_destinations(state).await.ok()?;
    playable_here(&plan, &destinations)?;
    Some(format!(
        "/api/library/editions/{}/sample",
        edition.edition_id
    ))
}

/// Finds the sample a title's details can play: one cut from the held
/// edition when this node can, else a storefront's.
#[utoipa::path(
    post,
    path = "/api/discover/title-sample",
    request_body = TitleSampleQuery,
    responses(
        (status = 200, description = "The path of the sample to play: the held edition's, else a store's; neither when no sample can be played here (no confined media worker, or no source offers one).", body = TitleSampleJson),
        (status = 400, description = "The query is malformed or too large.", body = String, content_type = "text/plain", example = json!("title, asin, or isbn is required")),
    ),
)]
pub(crate) async fn post_title_sample(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<TitleSampleQuery>,
) -> Result<Json<TitleSampleJson>, (StatusCode, String)> {
    crate::api::validate_title_meta_query(&body.title)?;
    let none = TitleSampleJson {
        edition_sample_path: None,
        sample_path: None,
    };
    // Every sample is cut by the confined worker: without one, none plays.
    if !bookclerk_media::pool().is_isolated() {
        return Ok(Json(none));
    }
    if let Some(id) = body.edition_id.as_deref().map(str::trim) {
        if let Some(path) = edition_sample_here(&state, &headers, id).await {
            // A held book's own sample never calls out to a store.
            return Ok(Json(TitleSampleJson {
                edition_sample_path: Some(path),
                sample_path: None,
            }));
        }
    }
    let sources = state.sources.read().await;
    // Only a URL this node can sign is worth taking: a store whose record
    // carries another kind gives way to the next store's.
    let url = bookclerk_discover::resolve_title_sample(&body.title, &sources, is_coverable).await;
    Ok(Json(TitleSampleJson {
        sample_path: url.and_then(|url| state.samples.path_for(&url)),
        ..none
    }))
}

/// Plays a storefront's sample of a title.
#[utoipa::path(
    get,
    path = "/api/samples/{token}",
    params(
        ("token" = String, Path, description = "Opaque token from a `sample_path`."),
        ("Range" = Option<String>, Header, description = "One byte range (`bytes=A-B`, `bytes=A-` or `bytes=-N`); anything else reads the whole sample."),
    ),
    responses(
        (status = 200, description = "The sample, re-encoded: mono MP3.", body = crate::openapi::BinaryBody, content_type = "audio/mpeg"),
        (status = 206, description = "The requested byte range, with `Content-Range`.", body = crate::openapi::BinaryBody, content_type = "audio/mpeg"),
        (status = 404, description = "The token was not issued by this node (`unknown_sample`).", body = ErrorMessage, example = json!({"error": "unknown_sample"})),
        (status = 416, description = "The range starts at or past the end of the sample (`range_not_satisfiable`); `Content-Range` states the size.", body = ErrorMessage, example = json!({"error": "range_not_satisfiable"})),
        (status = 422, description = "The store's sample could not be downloaded or decoded (`no_sample`).", body = ErrorMessage, example = json!({"error": "no_sample"})),
        (status = 503, description = "No `covers` plugin or confined media worker is available; retry (`unavailable`).", body = ErrorMessage, example = json!({"error": "unavailable"})),
    ),
)]
pub(crate) async fn get_store_sample(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> Result<Response, SampleError> {
    const ROUTE: &str = "GET /api/samples/{token}";
    let url = state
        .samples
        .open_token(&token)
        .ok_or_else(|| refuse(StatusCode::NOT_FOUND, "unknown_sample"))?;
    let cache_key = format!("store\n{url}");
    let integrations = Arc::clone(&state.integrations);
    let bytes = state
        .samples
        .sample(&cache_key, || async move {
            let downloaded = download(&integrations, url).await?;
            let size = u64::try_from(downloaded.len()).unwrap_or(u64::MAX);
            let sample = bookclerk_media::pool()
                .cut_sample(
                    size,
                    0,
                    bookclerk_media::pcm_wire::MAX_SAMPLE_MS,
                    Downloaded(downloaded),
                )
                .await?;
            Ok(sample.mp3)
        })
        .await
        .map_err(|failure| failure.answer(ROUTE))?;
    Ok(mp3_response(
        bytes,
        headers.get(header::RANGE),
        STORE_CACHE_CONTROL,
    ))
}

/// Downloads a store's sample through the deployed `covers` plugins, in
/// order, until one hands back a body: a plugin that refuses audio, fails,
/// or answers without a body passes the download to the next.
///
/// # Errors
///
/// [`SampleFailure::Unavailable`] when no plugin fetches remote media or
/// every call failed, [`SampleFailure::Refused`] when a plugin answered
/// and none returned the sample.
async fn download(
    integrations: &tokio::sync::RwLock<bookclerk_integrations::IntegrationRegistry>,
    url: String,
) -> Result<Vec<u8>, SampleFailure> {
    let fetchers = integrations.read().await.cover_fetchers();
    let mut failure = SampleFailure::Unavailable("no deployed plugin fetches remote media".into());
    for fetcher in fetchers {
        let params = bookclerk_plugin_abi::CoverFetchParams {
            urls: vec![url.clone()],
            max_bytes: bookclerk_plugin_abi::MAX_COVER_BYTES,
            conditions: Vec::new(),
            media: bookclerk_plugin_abi::RemoteMedia::Audio,
        };
        let answer = match fetcher.fetch_covers(params).await {
            Ok(fetched) => fetched.into_iter().next(),
            Err(err) => {
                // A refusal from an earlier plugin says more than a failed call.
                if matches!(failure, SampleFailure::Unavailable(_)) {
                    failure = SampleFailure::Unavailable(format!("sample fetch failed: {err}"));
                }
                continue;
            }
        };
        match answer.map(|answer| (answer.body, answer.error)) {
            Some((Some(bytes), _)) => return Ok(bytes),
            Some((None, Some(error))) => failure = SampleFailure::Refused(error),
            Some((None, None)) => failure = SampleFailure::Refused("no body".into()),
            None => failure = SampleFailure::Refused("no answer for the sample".into()),
        }
    }
    Err(failure)
}

#[cfg(test)]
#[path = "samples_tests.rs"]
mod tests;
