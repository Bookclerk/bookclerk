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
//! this node, each under that destination's own key from the manifest (the
//! host named the files with each destination's options from
//! `acquire.targets`, as a local acquire would). Files are accepted only
//! while this node has exactly the destinations it had at grant (otherwise
//! 422 `destinations_changed`, the upload is aborted, and the title fails
//! on the host); a title's commit re-checks the set the same way. While a
//! file streams, its size and SHA-256 are checked against the manifest
//! and the job's fence is re-checked about once a second. Each destination
//! sees the end of the stream only after the whole body matched and the
//! fence was still live; a size or digest mismatch, a lost fence, or a
//! broken connection instead ends every destination's stream with an
//! error, so each discards what it wrote (a local destination removes its
//! temp file, S3 aborts its multipart upload). The commit then marks the
//! title acquired in one fenced batch ([`commit_upload`]). Only the job's
//! closing upload may set `finish`, and the owner completes the job only
//! once no title of it is left unacquired (else 422 `titles_remaining`).
//!
//! An upload aborted after some of its files arrived (its fence lost, its
//! commit refused for a lost fence, the destinations changed, a re-sent
//! title superseding it, or the job finished without it, its closing
//! commit included) has every copy this node received deleted again
//! ([`discard_received`]); a file that finished streaming just as its
//! upload was aborted is deleted at once. A copy is deleted only while its
//! key is still recorded as that upload's (`title_files`): a later upload
//! of the title or an acquire that wrote the key since keeps its file,
//! however late the stale cleanup runs.
//! Files go straight to the destinations, so deletion is the rollback. An
//! upload received by another owner node is that node's to clean: this
//! node only logs it.
//!
//! The token is accepted for an hour past its latest extension: each
//! accepted file and each renewal of the job's lease extend it, and only
//! while the fence is live.
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
    abort_job_uploads, abort_upload, authorize_upload, commit_upload, load_or_create_node_identity,
    record_upload_file, upload_fence_live, upload_owns_key, JobClaimLimits, UploadCommit,
    UploadFinish, UploadSession,
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
    /// Its id in plans and manifests ([`destination_id`]).
    pub id: String,
    /// Its storage backend (a guest's session, or a test stand-in).
    pub backend: Box<dyn StorageBackend>,
    /// Its naming and packaging options.
    pub options: DownloadOptions,
}

/// The id of the destination of `kind`: its lowercase kind name (a node
/// deploys at most one destination per kind).
pub(crate) fn destination_id(kind: bookclerk_config::OutputBackendKind) -> String {
    bookclerk_acquire::destination_id(kind).to_string()
}

#[cfg(test)]
mod test_destinations {
    //! Local-filesystem stand-ins for the destination guests, by owner
    //! files dir, for tests that cannot spawn guests.

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};

    use bookclerk_config::{DestinationNaming, OutputBackendKind};

    /// One stand-in destination: its kind, root and naming overrides.
    #[derive(Clone)]
    pub(crate) struct TestDestination {
        /// The kind it stands in for.
        pub kind: OutputBackendKind,
        /// Where its files go.
        pub root: PathBuf,
        /// Its naming overrides.
        pub naming: DestinationNaming,
    }

    /// Destinations by files dir, the primary first.
    static ROOTS: LazyLock<Mutex<HashMap<PathBuf, Vec<TestDestination>>>> =
        LazyLock::new(Default::default);

    /// Makes `root` the one local destination of the owner at `files_dir`.
    pub(crate) fn set(files_dir: &Path, root: &Path) {
        set_many(
            files_dir,
            vec![TestDestination {
                kind: OutputBackendKind::Local,
                root: root.to_path_buf(),
                naming: DestinationNaming::default(),
            }],
        );
    }

    /// Makes `destinations` (the primary first) the owner's at `files_dir`.
    pub(crate) fn set_many(files_dir: &Path, destinations: Vec<TestDestination>) {
        ROOTS
            .lock()
            .unwrap()
            .insert(files_dir.to_path_buf(), destinations);
    }

    /// The stand-ins of the owner at `files_dir`.
    pub(super) fn get(files_dir: &Path) -> Option<Vec<TestDestination>> {
        ROOTS.lock().unwrap().get(files_dir).cloned()
    }
}

#[cfg(test)]
pub(crate) use test_destinations::{
    set as set_test_destination, set_many as set_test_destinations, TestDestination,
};

/// Every output destination deployed on this node, the primary first.
///
/// # Errors
///
/// Returns an error when none is deployed or two of one kind are.
pub(crate) async fn owner_destinations(state: &AppState) -> anyhow::Result<Vec<OwnerDestination>> {
    let cfg = state.config.read().await.clone();
    #[cfg(test)]
    if let Some(stand_ins) = test_destinations::get(&cfg.paths().files_dir) {
        let mut out = Vec::with_capacity(stand_ins.len());
        for stand_in in stand_ins {
            out.push(OwnerDestination {
                id: destination_id(stand_in.kind),
                backend: Box::new(bookclerk_storage::LocalFsBackend::new(stand_in.root)?),
                options: DownloadOptions::for_destination(
                    &cfg,
                    stand_in.kind,
                    &stand_in.naming,
                    "",
                ),
            });
        }
        return Ok(out);
    }
    let destinations =
        bookclerk_plugin_host::build_acquire_destinations(&cfg, &*state.destinations.read().await)
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(destinations
        .items
        .into_iter()
        .map(|item| OwnerDestination {
            id: destination_id(item.kind),
            backend: item.backend,
            options: item.options,
        })
        .collect())
}

/// The ids of `destinations`, in order (the primary first).
pub(crate) fn destination_ids(destinations: &[OwnerDestination]) -> Vec<String> {
    destinations.iter().map(|d| d.id.clone()).collect()
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

/// Deletes `key` from `destination` for the aborted `session`, but only
/// while the key is still that upload's own ([`upload_owns_key`]): a later
/// upload of the title, an acquire on this node, or another title may have
/// written it since (a stale cleanup running after the job's permits were
/// given back), and that file is left alone. Best effort; a failure is
/// logged, and an unreadable record keeps the file.
async fn discard_copy(
    library: &LibraryStore,
    session: &UploadSession,
    destination: &OwnerDestination,
    key: &str,
) {
    match upload_owns_key(library, session, &destination.id, key).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(upload_id = %session.upload_id, destination = %destination.id, "an aborted upload's file was written again since; left in place");
            return;
        }
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, destination = %destination.id, error = %err, "could not check who wrote an aborted upload's file; left in place");
            return;
        }
    }
    if let Err(err) = destination.backend.delete(key).await {
        tracing::warn!(upload_id = %session.upload_id, destination = %destination.id, error = %err, "could not delete an aborted upload's file");
    }
}

/// Deletes the copies of declared file `index` of `session` from
/// `destinations` that are still the upload's own ([`discard_copy`]).
async fn discard_file(
    library: &LibraryStore,
    destinations: &[OwnerDestination],
    session: &UploadSession,
    index: usize,
) {
    let Some(file) = session.files.get(index) else {
        return;
    };
    for copy in &file.copies {
        let Some(destination) = destinations.iter().find(|d| d.id == copy.destination) else {
            continue;
        };
        discard_copy(library, session, destination, &copy.key).await;
    }
}

/// Deletes every copy an aborted upload had received from this node's
/// destinations, so an abort leaves no partial title behind; a copy whose
/// key was written again since is left alone ([`discard_copy`]). An upload
/// another owner node received is left to that node (logged).
pub(crate) async fn discard_received(
    state: &AppState,
    library: &LibraryStore,
    session: &UploadSession,
) {
    if session.received.is_empty() {
        return;
    }
    let files_dir = state.config.read().await.paths().files_dir.clone();
    if this_node(&files_dir).as_deref() != Some(session.receiving_node_id.as_str()) {
        tracing::warn!(
            upload_id = %session.upload_id,
            job_id = %session.job_id,
            receiving_node_id = %session.receiving_node_id,
            "an aborted upload was received by another owner node; its files stay there"
        );
        return;
    }
    let destinations = match owner_destinations(state).await {
        Ok(destinations) => destinations,
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, error = %err, "could not open this node's destinations to delete an aborted upload's files");
            return;
        }
    };
    for index in &session.received {
        discard_file(library, &destinations, session, *index).await;
    }
    tracing::info!(
        upload_id = %session.upload_id,
        job_id = %session.job_id,
        files = session.received.len(),
        "deleted an aborted upload's files"
    );
}

/// Aborts `upload_id` and deletes what it had received.
async fn abort_and_discard(state: &AppState, library: &LibraryStore, upload_id: &str) {
    match abort_upload(library, upload_id).await {
        Ok(Some(session)) => discard_received(state, library, &session).await,
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(upload_id, error = %err, "could not abort an upload");
        }
    }
}

/// Aborts every open upload `node_id` holds under the job's lease
/// generation `generation` (the job finished or its fence was lost) and
/// deletes what each had received.
pub(crate) async fn discard_job_uploads(
    state: &AppState,
    library: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
) {
    match abort_job_uploads(library, node_id, job_id, generation).await {
        Ok(sessions) => {
            for session in &sessions {
                discard_received(state, library, session).await;
            }
        }
        Err(err) => {
            tracing::warn!(job_id, fence = generation, error = %err, "could not abort a job's uploads");
        }
    }
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

/// Streams `body` into each destination the file is written to, under
/// that destination's key; answers once every one has the whole, verified
/// file, or every one discarded it.
async fn tee(
    library: &LibraryStore,
    session: &UploadSession,
    index: usize,
    body: Body,
    destinations: Vec<OwnerDestination>,
) -> Result<(), StreamFailure> {
    let file = &session.files[index];
    let mut by_id: std::collections::HashMap<String, OwnerDestination> = destinations
        .into_iter()
        .map(|destination| (destination.id.clone(), destination))
        .collect();
    // The caller checked the destination set against the grant's; a copy
    // naming one this node lacks fails closed before anything is opened.
    if file
        .copies
        .iter()
        .any(|copy| !by_id.contains_key(&copy.destination))
    {
        return Err(StreamFailure::Mismatch);
    }
    let mut senders = Vec::with_capacity(file.copies.len());
    let mut writes = Vec::with_capacity(file.copies.len());
    for copy in &file.copies {
        let Some(destination) = by_id.remove(&copy.destination) else {
            continue;
        };
        let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
        senders.push(tx);
        let key = copy.key.clone();
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
            (destination, key, result)
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
            Ok((destination, key, Ok(_))) => written.push((destination, key)),
            Ok((_, _, Err(err))) => {
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
            // A destination that already finished keeps no partial title,
            // unless the key was claimed again meanwhile.
            for (destination, key) in written {
                discard_copy(library, session, &destination, &key).await;
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
            abort_and_discard(&state, &library, &session.upload_id).await;
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
    if destination_ids(&destinations) != session.destinations {
        // The host named the files for the destinations it was planned
        // with; writing them to a changed set would mis-name or skip one.
        tracing::warn!(upload_id = %session.upload_id, "this owner node's destinations changed since the upload was granted; aborted");
        abort_and_discard(&state, &library, &session.upload_id).await;
        return refused("destinations_changed");
    }
    let count = destinations.len();
    let ids = destination_ids(&destinations);
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
            Ok(false) => {
                // Aborted while this file streamed: its copies go too.
                tracing::warn!(upload_id = %session.upload_id, index, "the upload was aborted while a file streamed; deleting it");
                match owner_destinations(&state).await {
                    Ok(destinations) if destination_ids(&destinations) == ids => {
                        discard_file(&library, &destinations, &session, index).await;
                    }
                    _ => {
                        tracing::warn!(upload_id = %session.upload_id, index, "could not reopen this node's destinations to delete an aborted file")
                    }
                }
                lost()
            }
            Err(err) => {
                tracing::warn!(upload_id = %session.upload_id, error = %err, "could not record an uploaded file");
                unavailable()
            }
        },
        Err(StreamFailure::Lost) => {
            tracing::warn!(upload_id = %session.upload_id, job_id = %session.job_id, index, "the job's fence was lost mid-upload; aborted");
            abort_and_discard(&state, &library, &session.upload_id).await;
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
    // A title's files reached only the destinations it was granted for;
    // one deployed or undeployed since its last file would be left without
    // the title once it is marked acquired (later plans skip it). The
    // closing upload carries no files, and an already committed title is
    // answered as committed again.
    if !session.book_uuid.is_empty() && session.state != "committed" {
        match owner_destinations(&state).await {
            Ok(destinations) if destination_ids(&destinations) == session.destinations => {}
            Ok(destinations) if !destinations.is_empty() => {
                tracing::warn!(upload_id = %session.upload_id, "this owner node's destinations changed before the upload's commit; aborted");
                abort_and_discard(&state, &library, &session.upload_id).await;
                return refused("destinations_changed");
            }
            Ok(_) | Err(_) => {
                tracing::warn!(upload_id = %session.upload_id, "no output destination is deployed on this owner node");
                return unavailable();
            }
        }
    }
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
    let (limits, download_episodes) = {
        let cfg = state.config.read().await;
        (
            JobClaimLimits {
                class: cfg.jobs.concurrency.network.max(1),
                plugin_instance: cfg.jobs.concurrency.plugin_instance.max(1),
            },
            cfg.library.download_episodes,
        )
    };
    // The host's `finish` only asks; the commit itself checks that this is
    // the closing upload and that no title of the job remains.
    let finish = commit.finish.then_some(UploadFinish {
        detail,
        download_episodes,
    });
    match commit_upload(&library, &session.upload_id, finish, limits).await {
        Ok(UploadCommit::Committed { job_completed }) => {
            if job_completed {
                // The job is finished: an upload of it still open (a title
                // re-sent but never committed) can no longer commit, so it
                // is aborted and what it received is deleted.
                discard_job_uploads(
                    &state,
                    &library,
                    &session.node_id,
                    &session.job_id,
                    session.generation,
                )
                .await;
            }
            answer(StatusCode::OK, "committed", None, job_completed)
        }
        Ok(UploadCommit::Lost) => {
            abort_and_discard(&state, &library, &session.upload_id).await;
            lost()
        }
        Ok(UploadCommit::Refused(reason)) => refused(reason),
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, error = %err, "upload commit failed");
            unavailable()
        }
    }
}
