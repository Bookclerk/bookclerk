//! The owner's upload endpoint (H-4 design, section 6; decision 4).
//!
//! A host that ran an acquire streams each title's files here, one HTTP
//! request per declared file, after `upload.grant` opened the upload on
//! the node socket:
//!
//! | Request | Answer |
//! |---|---|
//! | `PUT /api/node/v1/uploads/{upload_id}/files/{n}` | 200 `received` |
//! | `POST /api/node/v1/uploads/{upload_id}/commit` | 200 `committed` |
//!
//! Both carry `Authorization: Bookclerk-Upload <token>`, the token the grant
//! sealed to the host's box key. The route sits outside the cookie and
//! operator auth layers (the token is the credential) and runs the
//! transport policy first. Refusals: 401 `unauthorized` (no such upload,
//! wrong or expired token), 421 `misdirected` (another owner node received
//! the grant), 409 `lost` (the job's fence is gone), 422 `refused`.
//!
//! A file is never staged on the owner. Its body is read once and teed,
//! chunk by chunk through bounded channels, into
//! [`StorageBackend::put_stream`] of every output destination deployed on
//! this node, while its size and SHA-256 are checked against the manifest
//! and the job's fence is re-checked about once a second. Each destination
//! sees the end of the stream only after the whole body matched and the
//! fence was still live; a size or digest mismatch, a lost fence, or a
//! broken connection instead ends every destination's stream with an
//! error, so each discards what it wrote (a local destination removes its
//! temp file, S3 aborts its multipart upload). The commit then marks the
//! title acquired in one fenced batch ([`commit_upload`]).
//!
//! The token never reaches a log: request tracing logs method and path
//! only, and these handlers log upload and job ids.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    abort_upload, authorize_upload, commit_upload, load_or_create_node_identity,
    record_upload_file, upload_fence_live, JobClaimLimits, UploadCommit, UploadSession,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{UploadAnswer, UploadCommitRequest, UPLOAD_AUTH_SCHEME};
use bookclerk_source::DownloadOptions;
use bookclerk_storage::{ObjectMeta, StorageBackend};
use futures::StreamExt as _;
use sha2::Digest as _;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{mpsc, watch};

use super::check_transport;
use crate::api::AppState;

/// Chunks buffered per destination before the reader waits.
const CHANNEL_CHUNKS: usize = 8;

/// How often the job's fence is re-checked while a file streams.
const FENCE_CHECK_EVERY: Duration = Duration::from_secs(1);

/// Largest commit body.
const MAX_COMMIT_BODY: usize = 16 * 1024;

/// Longest job outcome text a commit records.
const MAX_DETAIL_BYTES: usize = 4 * 1024;

/// One output destination deployed on this owner node.
pub(crate) struct OwnerDestination {
    /// Its storage backend (a guest's session, or a test stand-in).
    pub backend: Box<dyn StorageBackend>,
    /// Its naming and packaging options.
    pub options: DownloadOptions,
}

#[cfg(test)]
mod test_destinations {
    //! Local stand-ins for the `local` destination guest, by owner files
    //! dir, for tests that cannot spawn guests.

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};

    /// Roots by files dir.
    static ROOTS: LazyLock<Mutex<HashMap<PathBuf, PathBuf>>> = LazyLock::new(Default::default);

    /// Makes `root` the one local destination of the owner at `files_dir`.
    pub(crate) fn set(files_dir: &Path, root: &Path) {
        ROOTS
            .lock()
            .unwrap()
            .insert(files_dir.to_path_buf(), root.to_path_buf());
    }

    /// The stand-in root of the owner at `files_dir`.
    pub(super) fn get(files_dir: &Path) -> Option<PathBuf> {
        ROOTS.lock().unwrap().get(files_dir).cloned()
    }
}

#[cfg(test)]
pub(crate) use test_destinations::set as set_test_destination;

/// Every output destination deployed on this node, the primary first.
///
/// # Errors
///
/// Returns an error when none is deployed or two of one kind are.
pub(crate) async fn owner_destinations(state: &AppState) -> anyhow::Result<Vec<OwnerDestination>> {
    let cfg = state.config.read().await.clone();
    #[cfg(test)]
    if let Some(root) = test_destinations::get(&cfg.paths().files_dir) {
        let options = DownloadOptions::for_destination(
            &cfg,
            bookclerk_config::OutputBackendKind::Local,
            &Default::default(),
            "",
        );
        return Ok(vec![OwnerDestination {
            backend: Box::new(bookclerk_storage::LocalFsBackend::new(root)?),
            options,
        }]);
    }
    let destinations =
        bookclerk_plugin_host::build_acquire_destinations(&cfg, &*state.destinations.read().await)
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(destinations
        .items
        .into_iter()
        .map(|item| OwnerDestination {
            backend: item.backend,
            options: item.options,
        })
        .collect())
}

/// An answer with `status`.
fn answer(code: StatusCode, status: &str, reason: Option<&str>, job_completed: bool) -> Response {
    (
        code,
        Json(UploadAnswer {
            status: status.into(),
            reason: reason.map(str::to_string),
            job_completed,
        }),
    )
        .into_response()
}

/// 401 `unauthorized`.
fn unauthorized() -> Response {
    answer(StatusCode::UNAUTHORIZED, "unauthorized", None, false)
}

/// 409 `lost`.
fn lost() -> Response {
    answer(StatusCode::CONFLICT, "lost", None, false)
}

/// 422 `refused`.
fn refused(reason: &str) -> Response {
    answer(
        StatusCode::UNPROCESSABLE_ENTITY,
        "refused",
        Some(reason),
        false,
    )
}

/// 503 `unavailable`.
fn unavailable() -> Response {
    answer(StatusCode::SERVICE_UNAVAILABLE, "unavailable", None, false)
}

/// The bearer token of an upload request.
fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme == UPLOAD_AUTH_SCHEME && !token.is_empty()).then_some(token.trim())
}

/// This owner node's id.
fn this_node(state_files: &std::path::Path) -> Option<String> {
    load_or_create_node_identity(state_files)
        .ok()
        .map(|identity| identity.node_id)
}

/// The upload a request may act on, or the refusal to answer with.
async fn authorized(
    state: &Arc<AppState>,
    library: &LibraryStore,
    parts: &mut axum::http::request::Parts,
    upload_id: &str,
    allow_committed: bool,
) -> Result<UploadSession, Response> {
    check_transport(state, parts, "upload").await?;
    if upload_id.is_empty() || upload_id.len() > 64 {
        return Err(unauthorized());
    }
    let Some(token) = bearer(&parts.headers) else {
        return Err(unauthorized());
    };
    let session = match authorize_upload(library, upload_id, token, allow_committed).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(unauthorized()),
        Err(err) => {
            tracing::warn!(upload_id, error = %err, "upload lookup failed");
            return Err(unavailable());
        }
    };
    let files_dir = state.config.read().await.paths().files_dir.clone();
    if this_node(&files_dir).as_deref() != Some(session.receiving_node_id.as_str()) {
        return Err(answer(
            StatusCode::MISDIRECTED_REQUEST,
            "misdirected",
            None,
            false,
        ));
    }
    Ok(session)
}

/// A destination's view of the body: the chunks the tee sends, then end
/// of stream, or the error that aborts the write.
struct ChannelReader {
    /// Chunks from the tee.
    rx: mpsc::Receiver<io::Result<Bytes>>,
    /// The unread rest of the current chunk.
    pending: Bytes,
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if !self.pending.is_empty() {
                let n = out.remaining().min(self.pending.len());
                let chunk = self.pending.split_to(n);
                out.put_slice(&chunk);
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => self.pending = chunk,
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Why a streamed file was not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFailure {
    /// The job's fence was lost.
    Lost,
    /// The body does not match the manifest.
    Mismatch,
    /// The connection broke, or a destination failed.
    Broken,
}

/// Re-checks the upload's fence every [`FENCE_CHECK_EVERY`]; sends `true`
/// once it is lost.
async fn watch_fence(library: LibraryStore, session: UploadSession, lost: watch::Sender<bool>) {
    let mut every = tokio::time::interval(FENCE_CHECK_EVERY);
    every.tick().await;
    loop {
        every.tick().await;
        // A read error is treated as lost: fail closed.
        if !upload_fence_live(&library, &session).await.unwrap_or(false) {
            let _ = lost.send(true);
            return;
        }
    }
}

/// Streams `body` into every destination under `key`; answers once every
/// destination has the whole, verified file, or every one discarded it.
async fn tee(
    library: &LibraryStore,
    session: &UploadSession,
    index: usize,
    body: Body,
    destinations: Vec<OwnerDestination>,
) -> Result<(), StreamFailure> {
    let file = &session.files[index];
    let mut senders = Vec::with_capacity(destinations.len());
    let mut writes = Vec::with_capacity(destinations.len());
    for destination in destinations {
        let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
        senders.push(tx);
        let key = file.key.clone();
        let meta = ObjectMeta {
            content_type: file.content_type.clone(),
            content_length: Some(file.size),
            sha256_hex: Some(file.sha256.clone()),
            ..ObjectMeta::default()
        };
        writes.push(tokio::spawn(async move {
            let reader = ChannelReader {
                rx,
                pending: Bytes::new(),
            };
            let result = destination
                .backend
                .put_stream(&key, Box::pin(reader), meta)
                .await;
            (destination, result)
        }));
    }
    let (lost_tx, mut lost_rx) = watch::channel(false);
    let watcher = tokio::spawn(watch_fence(library.clone(), session.clone(), lost_tx));
    let mut stream = body.into_data_stream();
    let mut hasher = sha2::Sha256::new();
    let mut seen = 0u64;
    let mut failure = None;
    loop {
        let next = tokio::select! {
            next = stream.next() => next,
            changed = lost_rx.changed() => {
                if changed.is_err() || *lost_rx.borrow() {
                    failure = Some(StreamFailure::Lost);
                    break;
                }
                continue;
            }
        };
        match next {
            None => break,
            Some(Err(_)) => {
                failure = Some(StreamFailure::Broken);
                break;
            }
            Some(Ok(chunk)) => {
                seen = seen.saturating_add(chunk.len() as u64);
                if seen > file.size {
                    failure = Some(StreamFailure::Mismatch);
                    break;
                }
                hasher.update(&chunk);
                for tx in &senders {
                    if tx.send(Ok(chunk.clone())).await.is_err() {
                        failure = Some(StreamFailure::Broken);
                    }
                }
                if failure.is_some() {
                    break;
                }
            }
        }
    }
    watcher.abort();
    if failure.is_none() {
        let digest = hex_lower(&hasher.finalize());
        if seen != file.size || digest != file.sha256 {
            failure = Some(StreamFailure::Mismatch);
        } else if *lost_rx.borrow() || !upload_fence_live(library, session).await.unwrap_or(false) {
            failure = Some(StreamFailure::Lost);
        }
    }
    if failure.is_some() {
        for tx in &senders {
            let _ = tx
                .send(Err(io::Error::other("the upload was aborted")))
                .await;
        }
    }
    drop(senders);
    let mut written = Vec::new();
    for write in writes {
        match write.await {
            Ok((destination, Ok(_))) => written.push(destination),
            Ok((_, Err(err))) => {
                if failure.is_none() {
                    tracing::warn!(upload_id = %session.upload_id, index, error = %err, "a destination refused an uploaded file");
                    failure = Some(StreamFailure::Broken);
                }
            }
            Err(_) => failure = failure.or(Some(StreamFailure::Broken)),
        }
    }
    match failure {
        None => Ok(()),
        Some(failure) => {
            // A destination that already finished keeps no partial title.
            for destination in written {
                let _ = destination.backend.delete(&file.key).await;
            }
            Err(failure)
        }
    }
}

/// Lowercase hex of a digest.
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `PUT /api/node/v1/uploads/{upload_id}/files/{index}`.
pub async fn put_file(
    State(state): State<Arc<AppState>>,
    Path((upload_id, index)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let library = state.library_snapshot().await;
    let session = match authorized(&state, &library, &mut parts, &upload_id, false).await {
        Ok(session) => session,
        Err(refusal) => return refusal,
    };
    let Some(index) = index
        .parse::<usize>()
        .ok()
        .filter(|index| *index < session.files.len())
    else {
        return refused("unknown_file");
    };
    if session.received.contains(&index) {
        return answer(StatusCode::OK, "received", None, false);
    }
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared != Some(session.files[index].size) {
        return refused("size_mismatch");
    }
    match upload_fence_live(&library, &session).await {
        Ok(true) => {}
        Ok(false) => {
            let _ = abort_upload(&library, &session.upload_id).await;
            return lost();
        }
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, error = %err, "upload fence check failed");
            return unavailable();
        }
    }
    let destinations = match owner_destinations(&state).await {
        Ok(destinations) if !destinations.is_empty() => destinations,
        Ok(_) | Err(_) => {
            tracing::warn!(upload_id = %session.upload_id, "no output destination is deployed on this owner node");
            return unavailable();
        }
    };
    let count = destinations.len();
    match tee(&library, &session, index, body, destinations).await {
        Ok(()) => match record_upload_file(&library, &session.upload_id, index).await {
            Ok(true) => {
                tracing::info!(
                    upload_id = %session.upload_id,
                    job_id = %session.job_id,
                    index,
                    destinations = count,
                    "a host's uploaded file reached every destination"
                );
                answer(StatusCode::OK, "received", None, false)
            }
            Ok(false) => lost(),
            Err(err) => {
                tracing::warn!(upload_id = %session.upload_id, error = %err, "could not record an uploaded file");
                unavailable()
            }
        },
        Err(StreamFailure::Lost) => {
            tracing::warn!(upload_id = %session.upload_id, job_id = %session.job_id, index, "the job's fence was lost mid-upload; aborted");
            let _ = abort_upload(&library, &session.upload_id).await;
            lost()
        }
        Err(StreamFailure::Mismatch) => {
            tracing::warn!(upload_id = %session.upload_id, index, "an uploaded file does not match its manifest; discarded");
            refused("content_mismatch")
        }
        Err(StreamFailure::Broken) => unavailable(),
    }
}

/// `POST /api/node/v1/uploads/{upload_id}/commit`.
pub async fn post_commit(
    State(state): State<Arc<AppState>>,
    Path(upload_id): Path<String>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let library = state.library_snapshot().await;
    let session = match authorized(&state, &library, &mut parts, &upload_id, true).await {
        Ok(session) => session,
        Err(refusal) => return refusal,
    };
    let Ok(bytes) = axum::body::to_bytes(body, MAX_COMMIT_BODY).await else {
        return refused("invalid_body");
    };
    let Ok(commit) = serde_json::from_slice::<UploadCommitRequest>(&bytes) else {
        return refused("invalid_body");
    };
    let detail = commit.detail.as_deref().unwrap_or("");
    let detail = if detail.len() > MAX_DETAIL_BYTES {
        let mut end = MAX_DETAIL_BYTES;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        &detail[..end]
    } else {
        detail
    };
    let limits = {
        let cfg = state.config.read().await;
        JobClaimLimits {
            class: cfg.jobs.concurrency.network.max(1),
            plugin_instance: cfg.jobs.concurrency.plugin_instance.max(1),
        }
    };
    let finish = commit.finish.then_some(detail);
    match commit_upload(&library, &session.upload_id, finish, limits).await {
        Ok(UploadCommit::Committed { job_completed }) => {
            answer(StatusCode::OK, "committed", None, job_completed)
        }
        Ok(UploadCommit::Lost) => lost(),
        Ok(UploadCommit::Refused(reason)) => refused(reason),
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, error = %err, "upload commit failed");
            unavailable()
        }
    }
}
