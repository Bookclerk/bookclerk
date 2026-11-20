//! The owner's upload endpoint (H-4 design, section 6; decision 4).
//!
//! A host that ran an acquire streams each title's files here, one HTTP
//! request per declared file (or, for a file larger than
//! [`bookclerk_node_proto::UPLOAD_PART_BYTES`], one per part, so a relay
//! that caps request bodies passes it), after `upload.grant` opened the
//! upload on the node socket:
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
//! wrong or expired token), 421 `misdirected` (another shared node received
//! the grant), 409 `lost` (the job's fence is gone), 422 `refused`.
//!
//! A file is never staged on the owner. Its body is read once and teed,
//! chunk by chunk through bounded channels, into
//! [`StorageBackend::put_stream`] of every destination the upload was
//! granted for (a title's target set: the destinations deployed on this
//! node whose filter takes it; every one for a catalog upload), each under
//! that destination's own key from the manifest (the host named the files
//! with each destination's options from `acquire.targets`, as a local
//! acquire would). Files are accepted only while the title's target set
//! here is exactly the one granted, so no destination was deployed or
//! undeployed and no filter changed meanwhile (otherwise 422
//! `destinations_changed`, the upload is aborted, and the title fails on
//! the host); a title's commit re-checks the set the same way. While a
//! file streams, its size and SHA-256 are checked against the manifest
//! and the job's fence is re-checked about once a second. Each destination
//! sees the end of the stream only after the whole body matched, the fence
//! was still live, and the upload was still open and still owned each of
//! the file's keys; a size or digest mismatch, a lost fence, a key taken
//! meanwhile, or a broken connection instead ends every destination's
//! stream with an error, so each discards what it wrote (a local destination removes its
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
//! key is still recorded as that upload's (`edition_files`): a later upload
//! of the title or an acquire that wrote the key since keeps its file,
//! however late the stale cleanup runs.
//! Files go straight to the destinations, so deletion is the rollback. An
//! upload received by another shared node is that node's to clean: this
//! node only logs it.
//!
//! The PUTs of one declared file run one at a time on this node
//! ([`lock_file`]): a host retrying a file whose first attempt is still
//! streaming waits for it, then is answered from what it recorded, so a
//! failing attempt's cleanup never deletes the copy a retry just wrote.
//!
//! A file sent in parts carries `Content-Range: bytes <first>-<last>/<size>`
//! on each, in order ([`bookclerk_node_proto::parse_upload_range`] accepts
//! exactly one spelling of a part inside the declared file, else 422
//! `invalid_range`). The first part opens the file's write in a task of
//! its own ([`PartStream`]); each part's request feeds it and answers 200
//! `partial` with the `offset` it needs next, and the last part ends the
//! stream and answers as a whole file does, so the parts are one write per
//! destination and the size, digest, fence and key checks above cover the
//! whole file.
//!
//! An interrupted file resumes from the last byte the write was handed: a
//! part whose request breaks keeps the write open, and an empty `PUT` with
//! `Content-Range: bytes */<size>` answers 200 `partial` with the offset
//! the owner needs (0 when no write is open here). A part that does not
//! start there gets 503 `unavailable`, reason `resume`, with that offset,
//! and the write is kept. A write left without a part for [`PART_IDLE`] is
//! discarded, as is everything when this node restarts; the host then
//! sends the file again from byte 0. A file sent whole (no range) is not
//! resumed: it starts again, discarding what an earlier attempt left open.
//!
//! The token is accepted for an hour past its latest extension: each
//! accepted file and each renewal of the job's lease extend it, and only
//! while the fence is live.
//!
//! The token never reaches a log: request tracing logs method and path
//! only, and these handlers log upload and job ids.

use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_config::{DestinationFilter, PlacementFacts};
use bookclerk_library::control_plane::{
    abort_job_uploads, abort_upload, authorize_upload, commit_upload, destination_filters,
    load_or_create_node_identity, load_upload, record_upload_file, upload_fence_live,
    upload_owns_key, JobClaimLimits, UploadCommit, UploadFinish, UploadSession,
};
use bookclerk_library::{AcquireTitle, LibraryStore};
use bookclerk_node_proto::{
    parse_upload_range, UploadAnswer, UploadCommitRequest, UploadPart, UploadRange,
    UPLOAD_AUTH_SCHEME,
};
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

/// Longest a PUT waits for another attempt at the same file to finish
/// before answering 503 ([`lock_file`]).
const FILE_LOCK_WAIT: Duration = Duration::from_secs(30);

/// Longest a failed file's teardown waits for each destination to
/// discard its write ([`finish_write`]).
const TEARDOWN_WAIT: Duration = Duration::from_secs(30);

/// Longest the owner keeps a file whose parts are arriving open while it
/// waits for the next part, or for a broken one to resume ([`PartStream`]);
/// past it the file's writes are discarded and the host sends the file
/// again from byte 0.
const PART_IDLE: Duration = Duration::from_secs(10 * 60);

/// Largest commit body.
const MAX_COMMIT_BODY: usize = 16 * 1024;

/// Longest job outcome text a commit records.
const MAX_DETAIL_BYTES: usize = 4 * 1024;

/// One output destination deployed on this shared node.
pub(crate) struct OwnerDestination {
    /// Its id in plans and manifests: its storage plugin's alias.
    pub id: String,
    /// Its storage backend (a guest's session, or a test stand-in).
    pub backend: Box<dyn StorageBackend>,
    /// Its naming and packaging options.
    pub options: DownloadOptions,
}

#[cfg(test)]
mod test_destinations {
    //! Local-filesystem stand-ins for the destination guests, by owner
    //! files dir, for tests that cannot spawn guests.

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};

    use bookclerk_config::{DestinationId, DestinationKeyStyle, DestinationNaming};

    /// One stand-in destination: its id, key style, root and naming
    /// overrides.
    #[derive(Clone)]
    pub(crate) struct TestDestination {
        /// The destination id it stands in for.
        pub id: DestinationId,
        /// How the destination it stands in for names keys.
        pub key_style: DestinationKeyStyle,
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
                id: DestinationId::new("local").unwrap(),
                key_style: DestinationKeyStyle::Filesystem,
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
/// Returns an error when none is deployed or two instances serve one id.
pub(crate) async fn owner_destinations(state: &AppState) -> anyhow::Result<Vec<OwnerDestination>> {
    let cfg = state.config.read().await.clone();
    #[cfg(test)]
    if let Some(stand_ins) = test_destinations::get(&cfg.paths().files_dir) {
        let mut out = Vec::with_capacity(stand_ins.len());
        for stand_in in stand_ins {
            out.push(OwnerDestination {
                id: stand_in.id.to_string(),
                backend: Box::new(bookclerk_storage::LocalFsBackend::new(stand_in.root)?),
                options: DownloadOptions::for_destination(
                    &cfg,
                    stand_in.key_style,
                    &stand_in.naming,
                    "",
                ),
            });
        }
        return Ok(out);
    }
    let destinations = bookclerk_plugin_host::build_acquire_destinations(
        &cfg,
        &*state.destinations.read().await,
        // Plans and uploads name every destination; no title is placed here.
        &std::collections::BTreeMap::new(),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(destinations
        .items
        .into_iter()
        .map(|item| OwnerDestination {
            id: item.id.to_string(),
            backend: item.backend,
            options: item.options,
        })
        .collect())
}

/// The ids of `destinations`, in order (the primary first).
pub(crate) fn destination_ids(destinations: &[OwnerDestination]) -> Vec<String> {
    destinations.iter().map(|d| d.id.clone()).collect()
}

/// The destinations an upload was granted for, taken from `destinations`
/// (this node's deployed ones) in the grant's order, to delete what a
/// failed file wrote on them.
///
/// # Arguments
///
/// * `destinations` - This node's deployed destinations now.
/// * `granted` - The ids the upload was granted for
///   (`UploadSession::destinations`).
///
/// # Returns
///
/// `None` when one of `granted` is no longer deployed here (or is named
/// twice).
pub(crate) fn granted_destinations(
    destinations: Vec<OwnerDestination>,
    granted: &[String],
) -> Option<Vec<OwnerDestination>> {
    let mut by_id: std::collections::HashMap<String, OwnerDestination> = destinations
        .into_iter()
        .map(|destination| (destination.id.clone(), destination))
        .collect();
    granted.iter().map(|id| by_id.remove(id)).collect()
}

/// The stored filters of `destinations`, by destination id
/// ([`destination_filters`]); a destination without one takes every title.
///
/// # Errors
///
/// Returns the read or parse error; the caller fails closed on it rather
/// than writing where a filter said not to.
pub(crate) async fn owner_filters(
    library: &LibraryStore,
    destinations: &[OwnerDestination],
) -> bookclerk_library::Result<BTreeMap<String, DestinationFilter>> {
    let ids = destination_ids(destinations);
    destination_filters(library, ids.iter().map(String::as_str)).await
}

/// True when the destination `id` takes the title `facts` describes under
/// `filters` (no stored filter takes every title).
pub(crate) fn takes(
    filters: &BTreeMap<String, DestinationFilter>,
    id: &str,
    facts: &PlacementFacts,
) -> bool {
    filters.get(id).is_none_or(|filter| filter.accepts(facts))
}

/// `destinations` narrowed to the title's target set: those whose filter
/// takes `title`, in order (the primary first), as a local acquire of it
/// would choose ([`crate::jobs::placement_facts`]).
///
/// # Errors
///
/// Returns the read error for the filters or the title's linked patrons.
pub(crate) async fn title_target_set(
    library: &LibraryStore,
    destinations: Vec<OwnerDestination>,
    title: &AcquireTitle,
) -> bookclerk_library::Result<Vec<OwnerDestination>> {
    let filters = owner_filters(library, &destinations).await?;
    let facts = crate::jobs::placement_facts(library, title).await?;
    Ok(destinations
        .into_iter()
        .filter(|destination| takes(&filters, &destination.id, &facts))
        .collect())
}

/// The destinations `session` must write now: the target set of its title
/// among `destinations`, or every one for a catalog upload or a title this
/// node no longer has (whose grant scoped nothing). The upload proceeds
/// only while these are exactly the ones it was granted for: a destination
/// deployed or undeployed since, or a filter edited since, would leave a
/// destination without the title once it is marked acquired, or write it
/// where a filter says not to.
///
/// # Errors
///
/// Returns the read error for the title, its filters or its patrons.
async fn session_target_set(
    library: &LibraryStore,
    destinations: Vec<OwnerDestination>,
    session: &UploadSession,
) -> bookclerk_library::Result<Vec<OwnerDestination>> {
    if session.holding_id.is_empty() || session.is_catalog() {
        return Ok(destinations);
    }
    match library
        .acquire_title_of_holding(&session.holding_id)
        .await?
    {
        Some(title) => title_target_set(library, destinations, &title).await,
        None => Ok(destinations),
    }
}

/// An answer with `status`.
fn answer(code: StatusCode, status: &str, reason: Option<&str>, job_completed: bool) -> Response {
    (
        code,
        Json(UploadAnswer {
            status: status.into(),
            reason: reason.map(str::to_string),
            offset: None,
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

/// This shared node's id.
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
/// logged, and an unreadable record keeps the file. Never call it while
/// holding the key's lock ([`lock_copies`]).
async fn discard_copy(
    library: &LibraryStore,
    session: &UploadSession,
    destination: &OwnerDestination,
    key: &str,
) {
    // Held from the ownership check through the delete, as a finishing
    // write holds it from its last check to its end ([`tee`]): a write of
    // the key by a newer upload cannot land in between and be deleted.
    let _lock = lock_named(copy_lock_name(&destination.id, key)).await;
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
            // The destination was undeployed since the copy was written;
            // its backend is gone, so the file stays where it is.
            tracing::warn!(upload_id = %session.upload_id, destination = %copy.destination, key = %copy.key, "an aborted upload's file is on a destination this node no longer has; left in place");
            continue;
        };
        discard_copy(library, session, destination, &copy.key).await;
    }
}

/// Deletes every copy an aborted upload had received from this node's
/// destinations, so an abort leaves no partial title behind; a copy whose
/// key was written again since is left alone ([`discard_copy`]). An upload
/// another shared node received is left to that node (logged).
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
            "an aborted upload was received by another shared node; its files stay there"
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

/// [`ChannelReader::state`]: the tee is still sending; a closed channel
/// now means the tee went away (it was dropped), and fails the write.
const STREAM_OPEN: u8 = 0;
/// [`ChannelReader::state`]: every final check passed; end of stream may
/// be handed over and the destination may commit the file.
const STREAM_FINISHED: u8 = 1;
/// [`ChannelReader::state`]: the write must fail at once, whatever is
/// still buffered.
const STREAM_ABORTED: u8 = 2;

/// A destination's view of the body: the chunks the tee sends, then end
/// of stream, or the error that aborts the write.
struct ChannelReader {
    /// Chunks from the tee.
    rx: mpsc::Receiver<io::Result<Bytes>>,
    /// The unread rest of the current chunk.
    pending: Bytes,
    /// Shared with the tee: [`STREAM_OPEN`], [`STREAM_FINISHED`] or
    /// [`STREAM_ABORTED`], set before the senders are dropped. Aborted is
    /// checked before any byte is handed over, so an abort never waits for
    /// room in a full channel; end of stream is handed over only once the
    /// tee marked the file finished, so a tee dropped mid-request (its
    /// handler cancelled) never lets a destination commit the file.
    state: Arc<AtomicU8>,
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.state.load(Ordering::Acquire) == STREAM_ABORTED {
                return Poll::Ready(Err(io::Error::other("the upload was aborted")));
            }
            if !self.pending.is_empty() {
                let n = out.remaining().min(self.pending.len());
                let chunk = self.pending.split_to(n);
                out.put_slice(&chunk);
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => self.pending = chunk,
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err)),
                Poll::Ready(None) => {
                    return Poll::Ready(if self.state.load(Ordering::Acquire) == STREAM_FINISHED {
                        Ok(())
                    } else {
                        Err(io::Error::other("the upload stopped before its end"))
                    });
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// The copies of one file a PUT has written, or is still writing, that are
/// not yet recorded as received. Unless [`Unrecorded::keep`] is called
/// (the file was recorded, or its copies were handled), dropping it, as
/// when the PUT's handler is cancelled mid-request, fails every write still
/// open and deletes every copy that reached its destination (each only
/// while the upload still owns its key, [`discard_copy`]), in a task of its
/// own: an unrecorded copy is invisible to every later cleanup.
struct Unrecorded {
    /// The library, for the ownership checks of the cleanup.
    library: LibraryStore,
    /// The upload the file belongs to.
    session: UploadSession,
    /// The writes' shared [`ChannelReader::state`].
    state: Arc<AtomicU8>,
    /// Writes not yet awaited; `None` once awaited.
    pending: Vec<Option<DestinationWrite>>,
    /// Copies that reached their destination.
    written: Vec<(OwnerDestination, String)>,
    /// The copies' locks ([`lock_copies`]) while the writes finish: a
    /// cancelled request hands them to the cleanup, which holds them until
    /// the pending writes resolve, so no newer upload of the keys can land
    /// before this one's late rename.
    copy_locks: Vec<NamedLock>,
    /// Set by [`Unrecorded::keep`]: nothing is rolled back.
    kept: bool,
}

impl Unrecorded {
    /// Gives up the rollback: the copies were recorded, or handled.
    fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for Unrecorded {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        // A write not yet told to finish fails instead.
        let _ = self.state.compare_exchange(
            STREAM_OPEN,
            STREAM_ABORTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let pending: Vec<DestinationWrite> = self.pending.drain(..).flatten().collect();
        let mut copies = std::mem::take(&mut self.written);
        let copy_locks = std::mem::take(&mut self.copy_locks);
        if pending.is_empty() && copies.is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(upload_id = %self.session.upload_id, "an unrecorded upload's files could not be cleaned up outside a runtime");
            return;
        };
        let library = self.library.clone();
        let session = self.session.clone();
        runtime.spawn(async move {
            for write in pending {
                if let Ok((destination, key, Ok(_))) = write.await {
                    copies.push((destination, key));
                }
            }
            // [`discard_copy`] takes the same locks.
            drop(copy_locks);
            tracing::warn!(upload_id = %session.upload_id, files = copies.len(), "an upload request ended before its file was recorded; deleting its copies");
            for (destination, key) in copies {
                discard_copy(&library, &session, &destination, &key).await;
            }
        });
    }
}

/// Why a streamed file was not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFailure {
    /// The job's fence was lost, or the upload was aborted (a newer grant
    /// of the title superseded it).
    Lost,
    /// The body does not match the manifest.
    Mismatch,
    /// A key the upload was granted is recorded for another title or
    /// writer since (a local acquire wrote it meanwhile).
    Taken,
    /// The connection broke, or a destination failed.
    Broken,
}

/// Re-checks every [`FENCE_CHECK_EVERY`] that the upload's fence is live
/// and the upload still open; sends `true` once either is not, so a stream
/// whose upload a newer grant superseded stops within about a second.
async fn watch_fence(library: LibraryStore, session: UploadSession, lost: watch::Sender<bool>) {
    let mut every = tokio::time::interval(FENCE_CHECK_EVERY);
    every.tick().await;
    loop {
        every.tick().await;
        // A read error proves nothing either way: check again on the next
        // tick. The checks at the end of the stream and the commit gate
        // still refuse a file whose fence cannot be read.
        let open = match load_upload(&library, &session.upload_id).await {
            Ok(Some(current)) => current.state == "open",
            Ok(None) => false,
            Err(_) => continue,
        };
        let live = match upload_fence_live(&library, &session).await {
            Ok(live) => live,
            Err(_) => continue,
        };
        if !open || !live {
            let _ = lost.send(true);
            return;
        }
    }
}

/// Sends `chunk` to one destination, waiting for room in its channel only
/// while the fence holds: a lost fence or aborted upload ends the wait.
async fn send_chunk(
    tx: &mpsc::Sender<io::Result<Bytes>>,
    chunk: Bytes,
    lost_rx: &mut watch::Receiver<bool>,
) -> Result<(), StreamFailure> {
    let send = tx.send(Ok(chunk));
    tokio::pin!(send);
    loop {
        tokio::select! {
            sent = &mut send => return sent.map_err(|_| StreamFailure::Broken),
            changed = lost_rx.changed() => {
                if changed.is_err() || *lost_rx.borrow() {
                    return Err(StreamFailure::Lost);
                }
            }
        }
    }
}

/// One spawned destination write: the destination, its key, and the
/// outcome of its `put_stream`.
type DestinationWrite = tokio::task::JoinHandle<(
    OwnerDestination,
    String,
    bookclerk_storage::Result<bookclerk_storage::PutStreamResult>,
)>;

/// Waits for one destination write, awaited in place so a cancelled
/// caller leaves the handle to [`Unrecorded`]'s cleanup. A failed file's
/// teardown waits at most [`TEARDOWN_WAIT`] (a destination that never
/// reads again cannot hold the request open forever) and then aborts the
/// task, dropping its write; `None` when the task ended without an
/// outcome.
async fn finish_write(
    write: &mut DestinationWrite,
    teardown: bool,
) -> Option<(
    OwnerDestination,
    String,
    bookclerk_storage::Result<bookclerk_storage::PutStreamResult>,
)> {
    if !teardown {
        return write.await.ok();
    }
    match tokio::time::timeout(TEARDOWN_WAIT, &mut *write).await {
        Ok(done) => done.ok(),
        Err(_) => {
            write.abort();
            None
        }
    }
}

/// Streams `body` into each destination the file is written to, under
/// that destination's key; answers once every one has the whole, verified
/// file (with the copies, to keep once recorded), or every one discarded
/// it. Dropped part way, it fails every open write and deletes what
/// already landed ([`Unrecorded`]).
async fn tee(
    library: &LibraryStore,
    session: &UploadSession,
    index: usize,
    body: Body,
    destinations: Vec<OwnerDestination>,
) -> Result<Unrecorded, StreamFailure> {
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
    // The grant claimed each key for this upload; one claimed since by
    // another writer (a local acquire records before it writes) is not
    // overwritten. Checked again before the end of the stream.
    still_writable(library, session, index).await?;
    let state = Arc::new(AtomicU8::new(STREAM_OPEN));
    let mut unrecorded = Unrecorded {
        library: library.clone(),
        session: session.clone(),
        state: Arc::clone(&state),
        pending: Vec::with_capacity(file.copies.len()),
        written: Vec::new(),
        copy_locks: Vec::new(),
        kept: false,
    };
    let mut senders = Vec::with_capacity(file.copies.len());
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
        let state = Arc::clone(&state);
        unrecorded.pending.push(Some(tokio::spawn(async move {
            let reader = ChannelReader {
                rx,
                pending: Bytes::new(),
                state,
            };
            let result = destination
                .backend
                .put_stream(&key, Box::pin(reader), meta)
                .await;
            (destination, key, result)
        })));
    }
    let (lost_tx, mut lost_rx) = watch::channel(false);
    let watcher = tokio::spawn(watch_fence(library.clone(), session.clone(), lost_tx));
    let mut stream = body.into_data_stream();
    let mut hasher = sha2::Sha256::new();
    let mut seen = 0u64;
    let mut failure = None;
    'body: loop {
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
                    if let Err(err) = send_chunk(tx, chunk.clone(), &mut lost_rx).await {
                        failure = Some(err);
                        break 'body;
                    }
                }
            }
        }
    }
    watcher.abort();
    // From the last ownership check until every destination has finished,
    // no cleanup on this node may delete these keys ([`discard_copy`] takes
    // the same locks), so a stale cleanup cannot remove what this write is
    // about to finish, nor this write land after a check that let a
    // cleanup proceed.
    unrecorded.copy_locks = lock_copies(&file.copies).await;
    if failure.is_none() {
        let digest = hex_lower(&hasher.finalize());
        if seen != file.size || digest != file.sha256 {
            failure = Some(StreamFailure::Mismatch);
        } else if *lost_rx.borrow() {
            failure = Some(StreamFailure::Lost);
        } else if let Err(failure_now) = match upload_fence_live(library, session).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(StreamFailure::Lost),
            // A read error is not a lost fence: answer 503 and keep the
            // upload's other files, as the check before the stream does.
            Err(_) => Err(StreamFailure::Broken),
        } {
            failure = Some(failure_now);
        } else {
            failure = still_writable(library, session, index).await.err();
        }
    }
    state.store(
        if failure.is_some() {
            STREAM_ABORTED
        } else {
            STREAM_FINISHED
        },
        Ordering::Release,
    );
    drop(senders);
    for slot in 0..unrecorded.pending.len() {
        let Some(write) = unrecorded.pending[slot].as_mut() else {
            continue;
        };
        let done = finish_write(write, failure.is_some()).await;
        unrecorded.pending[slot] = None;
        match done {
            Some((destination, key, Ok(_))) => unrecorded.written.push((destination, key)),
            Some((_, _, Err(err))) => {
                if failure.is_none() {
                    tracing::warn!(upload_id = %session.upload_id, index, error = %err, "a destination refused an uploaded file");
                    failure = Some(StreamFailure::Broken);
                }
            }
            None => {
                if failure.is_some() {
                    tracing::warn!(upload_id = %session.upload_id, index, "a destination did not discard an aborted file in time; its write was dropped");
                }
                failure = failure.or(Some(StreamFailure::Broken));
            }
        }
    }
    drop(std::mem::take(&mut unrecorded.copy_locks));
    match failure {
        None => Ok(unrecorded),
        Some(failure) => {
            // A destination that already finished keeps no partial title,
            // unless the key was claimed again meanwhile.
            let written = std::mem::take(&mut unrecorded.written);
            unrecorded.keep();
            for (destination, key) in written {
                discard_copy(library, session, &destination, &key).await;
            }
            Err(failure)
        }
    }
}

/// Runs [`tee`] for file `index` of `session` while the locks on the
/// file's keys are held here, drops it once it waits for them (its body
/// read in full, its destinations waiting for the end of stream), and then
/// gives the locks back: a PUT handler cancelled mid-request, for tests.
#[cfg(test)]
pub(crate) async fn drop_tee_waiting_for_its_locks(
    library: LibraryStore,
    session: UploadSession,
    index: usize,
    body: Body,
    destinations: Vec<OwnerDestination>,
) {
    let copies = session.files[index].copies.clone();
    let held = lock_copies(&copies).await;
    let first = copy_lock_name(&copies[0].destination, &copies[0].key);
    let references = || {
        NAMED_LOCKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&first)
            .map_or(0, Arc::strong_count)
    };
    let alone = references();
    let task = tokio::spawn(async move {
        let _ = tee(&library, &session, index, body, destinations).await;
    });
    // A waiter adds its own references to the lock's entry.
    while references() <= alone {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.abort();
    let _ = task.await;
    drop(held);
}

/// Re-checks, just before every destination sees the end of file `index`,
/// that the upload is still open and still owns each of the file's keys
/// ([`upload_owns_key`]). The checks before streaming can be minutes old
/// by then: a newer grant of the title, or another title's acquire, may
/// have taken a key, or the upload may have been aborted, and finishing
/// the write would overwrite that writer's file. On `Err` the caller ends
/// every stream with an error, so nothing is written over it; the copies
/// are not deleted, since a destination that never saw the end of the
/// stream wrote none. A key taken after this check, before the
/// destination's final rename, still loses that narrow race.
async fn still_writable(
    library: &LibraryStore,
    session: &UploadSession,
    index: usize,
) -> Result<(), StreamFailure> {
    match load_upload(library, &session.upload_id).await {
        Ok(Some(current)) if current.state == "open" => {}
        Ok(_) => return Err(StreamFailure::Lost),
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, error = %err, "could not re-check an upload before finishing a file");
            return Err(StreamFailure::Broken);
        }
    }
    for copy in &session.files[index].copies {
        match upload_owns_key(library, session, &copy.destination, &copy.key).await {
            Ok(true) => {}
            Ok(false) => return Err(StreamFailure::Taken),
            Err(err) => {
                tracing::warn!(upload_id = %session.upload_id, error = %err, "could not re-check an upload's key before finishing it");
                return Err(StreamFailure::Broken);
            }
        }
    }
    Ok(())
}

/// Lowercase hex of a digest.
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Locks by name ([`file_lock_name`], [`copy_lock_name`]), each present
/// while a task holds or waits for it ([`LockEntry`]).
type NamedLocks = std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>;

/// The locks of [`lock_named`].
static NAMED_LOCKS: std::sync::LazyLock<std::sync::Mutex<NamedLocks>> =
    std::sync::LazyLock::new(Default::default);

/// A task's share of one named lock's entry, held while it waits and
/// while it holds the lock. Dropping the last share removes the entry, so
/// the map holds only locks in use (a task cancelled while waiting drops
/// its share too).
struct LockEntry {
    /// The lock's name.
    name: String,
    /// This task's reference to the entry's lock; `None` once dropped.
    lock: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl Drop for LockEntry {
    fn drop(&mut self) {
        let mut locks = NAMED_LOCKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(self.lock.take());
        // Only the map's own reference is left: no task holds or waits.
        if locks
            .get(&self.name)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            locks.remove(&self.name);
        }
    }
}

/// Holds one named lock until dropped.
struct NamedLock {
    /// The held lock; dropped first, so the entry sees it released.
    _guard: tokio::sync::OwnedMutexGuard<()>,
    /// This task's share of the entry, dropped after the guard.
    _entry: LockEntry,
}

/// Waits for, then holds, the in-process lock called `name`. The upload
/// endpoint is this node's only writer and deleter of uploaded files, so
/// a lock here orders its writes and deletes against each other.
async fn lock_named(name: String) -> NamedLock {
    let lock = NAMED_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(name.clone())
        .or_default()
        .clone();
    let entry = LockEntry {
        name,
        lock: Some(Arc::clone(&lock)),
    };
    let guard = lock.lock_owned().await;
    NamedLock {
        _guard: guard,
        _entry: entry,
    }
}

/// The lock name of declared file `index` of `upload_id` ([`lock_file`]).
/// Neither part holds a newline (an upload id is a UUID).
fn file_lock_name(upload_id: &str, index: usize) -> String {
    format!("file\n{upload_id}\n{index}")
}

/// The lock name of `key` on `destination` ([`lock_copies`]). A storage key
/// holds no control character, so the newline separates the parts.
fn copy_lock_name(destination: &str, key: &str) -> String {
    format!("copy\n{destination}\n{key}")
}

/// Waits until no other PUT of file `index` of `upload_id` runs on this
/// node, then holds that file's lock until the answer is built. Two
/// attempts of one file (a host retrying a stalled PUT) would otherwise
/// stream at once, and the first's failure cleanup ([`discard_copy`])
/// could delete the copy the retry wrote under the same, still owned, key.
async fn lock_file(upload_id: &str, index: usize) -> NamedLock {
    lock_named(file_lock_name(upload_id, index)).await
}

/// Holds the locks of every key of `copies`, taken in sorted order so two
/// files sharing keys (uploads of one title) never deadlock.
async fn lock_copies(copies: &[bookclerk_node_proto::FileCopy]) -> Vec<NamedLock> {
    let names: std::collections::BTreeSet<String> = copies
        .iter()
        .map(|copy| copy_lock_name(&copy.destination, &copy.key))
        .collect();
    let mut held = Vec::with_capacity(names.len());
    for name in names {
        held.push(lock_named(name).await);
    }
    held
}

/// Records file `index` of `session`, written to every one of the `count`
/// destinations `ids`, as received, and answers the PUT. An upload no
/// longer open, or a failed record, deletes the copies again: unrecorded,
/// they are invisible to every later cleanup.
async fn record_written(
    state: &AppState,
    library: &LibraryStore,
    session: &UploadSession,
    index: usize,
    count: usize,
    ids: &[String],
) -> Response {
    match record_upload_file(library, &session.upload_id, index).await {
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
            // Not open any more. A committed upload already counts
            // this file as received (the commit needs every file), so
            // its copies are the title's and are never deleted; the
            // per-file lock makes this unreachable, but cleanup must
            // not depend on it.
            match load_upload(library, &session.upload_id).await {
                Ok(Some(current)) if current.state == "committed" => {
                    return answer(StatusCode::OK, "received", None, false);
                }
                Ok(_) => {}
                Err(err) => {
                    // Unknown state: keep the copies rather than risk a
                    // committed title's files.
                    tracing::warn!(upload_id = %session.upload_id, index, error = %err, "could not re-read an upload that stopped being open; its file is left in place");
                    return unavailable();
                }
            }
            // Aborted while this file streamed: its copies go too.
            tracing::warn!(upload_id = %session.upload_id, index, "the upload was aborted while a file streamed; deleting it");
            match owner_destinations(state)
                .await
                .ok()
                .and_then(|destinations| granted_destinations(destinations, ids))
            {
                Some(destinations) => {
                    discard_file(library, &destinations, session, index).await;
                }
                None => {
                    tracing::warn!(upload_id = %session.upload_id, index, "could not reopen this node's destinations to delete an aborted file")
                }
            }
            lost()
        }
        Err(err) => {
            // Unrecorded, the copies are invisible to every later
            // cleanup (it deletes only received files), so they go now;
            // a key owned by another writer meanwhile is left alone.
            tracing::warn!(upload_id = %session.upload_id, index, error = %err, "could not record an uploaded file; deleting it");
            match owner_destinations(state)
                .await
                .ok()
                .and_then(|destinations| granted_destinations(destinations, ids))
            {
                Some(destinations) => {
                    discard_file(library, &destinations, session, index).await;
                }
                None => {
                    tracing::warn!(upload_id = %session.upload_id, index, "could not reopen this node's destinations to delete an unrecorded file")
                }
            }
            unavailable()
        }
    }
}

/// `PUT /api/node/v1/uploads/{upload_id}/files/{index}`.
#[utoipa::path(
    put,
    path = "/api/node/v1/uploads/{upload_id}/files/{index}",
    security(("uploadToken" = [])),
    params(
        ("upload_id" = String, Path, description = "Upload id."),
        ("index" = u32, Path, description = "File index in the upload."),
        ("Content-Range" = Option<String>, Header, description = "For a file sent in parts: `bytes <first>-<last>/<size>`, the part this body is, each its own request in order; or `bytes */<size>` with an empty body, which asks for the offset the owner needs next (to resume). A file sent whole carries no range."),
    ),
    request_body(content = crate::openapi::BinaryBody, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "The file was stored (`received`), or a part that is not the file's last was taken, or a status query was answered, with the offset the owner needs next (`partial`).", body = bookclerk_node_proto::UploadAnswer, examples(
            ("received" = (summary = "The whole file was stored.", value = json!({"status": "received", "job_completed": false}))),
            ("partial" = (summary = "Send the file from this offset.", value = json!({"status": "partial", "offset": 67_108_864, "job_completed": false}))),
        )),
        (status = 401, description = "The upload token is missing or wrong, or names no live upload (`unauthorized`).", body = bookclerk_node_proto::UploadAnswer, example = json!({"status": "unauthorized", "job_completed": false})),
        (status = 403, description = "The connection failed the transport policy (`transport_insecure`, with the check in `reason`).", body = crate::http_error::TransportRefused, example = json!({"error": "transport_insecure", "reason": "plain_http"})),
        (status = 404, description = "The node API is not mounted on this listener (`node_api_unmounted`).", body = crate::http_error::ErrorMessage, example = json!({"error": "node_api_unmounted"})),
        (status = 409, description = "The upload lost its fence to a newer one (`lost`).", body = bookclerk_node_proto::UploadAnswer, example = json!({"status": "lost", "job_completed": false})),
        (status = 421, description = "This node is not the upload's receiving node (`misdirected`).", body = bookclerk_node_proto::UploadAnswer, example = json!({"status": "misdirected", "job_completed": false})),
        (status = 422, description = "The file was refused (`refused`): not in the upload (`unknown_file`), a `Content-Range` that is not exactly a part of the declared file (`invalid_range`), a size or content mismatch (`size_mismatch`, `content_mismatch`), the destinations changed (`destinations_changed`), or a granted storage key now belongs to another title or writer (`key_taken`).", body = bookclerk_node_proto::UploadAnswer, examples(
            ("unknown_file" = (summary = "The index names no file of the upload.", value = json!({"status": "refused", "reason": "unknown_file", "job_completed": false}))),
            ("invalid_range" = (summary = "The range is not a part of the declared file.", value = json!({"status": "refused", "reason": "invalid_range", "job_completed": false}))),
            ("size_mismatch" = (summary = "The body is not the granted size.", value = json!({"status": "refused", "reason": "size_mismatch", "job_completed": false}))),
            ("content_mismatch" = (summary = "The body's digest is not the granted one.", value = json!({"status": "refused", "reason": "content_mismatch", "job_completed": false}))),
            ("destinations_changed" = (summary = "The destinations changed since the grant.", value = json!({"status": "refused", "reason": "destinations_changed", "job_completed": false}))),
            ("key_taken" = (summary = "The storage key now belongs to another title or writer.", value = json!({"status": "refused", "reason": "key_taken", "job_completed": false}))),
        )),
        (status = 503, description = "Storage or the upload record is unavailable, or the file lock timed out; retry (`unavailable`). With reason `resume`, a part did not start where the open file stands, or broke part way: send the file from `offset` (0 when no write is open here).", body = bookclerk_node_proto::UploadAnswer, examples(
            ("unavailable" = (summary = "Retry the request.", value = json!({"status": "unavailable", "job_completed": false}))),
            ("resume" = (summary = "Send the file from this offset.", value = json!({"status": "unavailable", "reason": "resume", "offset": 67_108_864, "job_completed": false}))),
        )),
    ),
)]
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
    // A retry waits for an attempt still streaming, but not forever: one
    // stalled on a dead connection holds the lock until its body read
    // fails. Past [`FILE_LOCK_WAIT`] the retry answers 503, which the host
    // retries with backoff. The owner cannot tell a slow attempt from a
    // dead one, so a newer attempt never takes the file over: cancelling a
    // live, nearly finished stream for a retry would only cost the host
    // the whole file again.
    let Ok(_file_lock) =
        tokio::time::timeout(FILE_LOCK_WAIT, lock_file(&session.upload_id, index)).await
    else {
        tracing::warn!(upload_id = %session.upload_id, index, "another attempt at an uploaded file is still streaming; asked to retry");
        return unavailable();
    };
    // Another attempt of this file may have finished (or the upload ended)
    // while this one waited: answer from the upload as it stands now.
    let session = match load_upload(&library, &session.upload_id).await {
        Ok(Some(current)) if current.state == "open" => current,
        Ok(_) => return unauthorized(),
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, error = %err, "upload lookup failed");
            return unavailable();
        }
    };
    if session.received.contains(&index) {
        return answer(StatusCode::OK, "received", None, false);
    }
    let size = session.files[index].size;
    let range = match parts.headers.get(header::CONTENT_RANGE) {
        None => None,
        Some(range) => match parse_upload_range(range.as_bytes(), size) {
            Some(range) => Some(range),
            None => return refused("invalid_range"),
        },
    };
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let key = file_lock_name(&session.upload_id, index);
    let part = match range {
        Some(UploadRange::Status) => {
            if declared != Some(0) {
                return refused("size_mismatch");
            }
            // Where the open file stands (0 when none is open here).
            let next = take_part_stream(&key).map_or(0, |stream| {
                let next = stream.next;
                put_part_stream(stream);
                next
            });
            return partial(StatusCode::OK, None, next);
        }
        Some(UploadRange::Part(part)) => Some(part),
        // A file that fits in one request is sent whole, without a range.
        None => None,
    };
    let ranged = part.is_some();
    let part = part.unwrap_or(UploadPart {
        offset: 0,
        length: size,
    });
    if declared != Some(part.length) {
        return refused("size_mismatch");
    }
    let earlier = take_part_stream(&key);
    let mut stream = match earlier {
        Some(stream) if ranged && stream.next == part.offset => stream,
        Some(stream) if ranged => {
            // Not where the open file stands: say where, and keep it.
            let next = stream.next;
            put_part_stream(stream);
            return partial(StatusCode::SERVICE_UNAVAILABLE, Some("resume"), next);
        }
        earlier if part.offset == 0 => {
            // A whole file starts again: whatever an earlier attempt left
            // open is discarded first.
            if let Some(stale) = earlier {
                stale.abandon().await;
            }
            match start_file(&state, &library, &session, index, key).await {
                Ok(stream) => stream,
                Err(refusal) => return refusal,
            }
        }
        _ => {
            // No write is open here for this file (an earlier one failed,
            // timed out, or this node restarted): it starts at byte 0.
            tracing::warn!(upload_id = %session.upload_id, index, offset = part.offset, "an upload part does not continue an open file; asked to resume from its start");
            return partial(StatusCode::SERVICE_UNAVAILABLE, Some("resume"), 0);
        }
    };
    let (fed, bytes) = feed_part(&stream.feed, body, part.length).await;
    match fed {
        Fed::Part if part.is_last(size) => stream.finish().await,
        Fed::Part => {
            stream.next = part.end();
            put_part_stream(stream);
            partial(StatusCode::OK, None, part.end())
        }
        // A ranged part that broke keeps what reached the write, so the
        // host resends only the rest; a whole file starts again.
        Fed::BodyBroken if ranged => {
            stream.next = part.offset.saturating_add(bytes);
            let next = stream.next;
            put_part_stream(stream);
            partial(StatusCode::SERVICE_UNAVAILABLE, Some("resume"), next)
        }
        Fed::BodyBroken => {
            stream.abandon().await;
            unavailable()
        }
        // The write ended early (a lost fence, a mismatch, a taken key or
        // a failed destination): its answer says why.
        Fed::WriteEnded => stream.finish().await,
    }
}

/// `partial` (200), or `unavailable` with `reason` (503), naming the
/// offset of the next byte of the file the owner needs.
fn partial(code: StatusCode, reason: Option<&str>, next: u64) -> Response {
    let status = if code == StatusCode::OK {
        "partial"
    } else {
        "unavailable"
    };
    (
        code,
        Json(UploadAnswer {
            status: status.into(),
            reason: reason.map(str::to_string),
            offset: Some(next),
            job_completed: false,
        }),
    )
        .into_response()
}

/// How one part's body went into its file's write.
enum Fed {
    /// Every byte of the part was handed to the write.
    Part,
    /// The request body broke, or was not the part's length.
    BodyBroken,
    /// The write stopped reading: it already failed.
    WriteEnded,
}

/// Hands `length` bytes of `body` to a file's write through `feed`;
/// answers how it went and how many bytes the write was handed.
async fn feed_part(feed: &mpsc::Sender<io::Result<Bytes>>, body: Body, length: u64) -> (Fed, u64) {
    let mut stream = body.into_data_stream();
    let mut handed = 0u64;
    loop {
        // A write that fails (a lost fence) answers at once, without
        // waiting for a stalled host's next byte.
        let next = tokio::select! {
            next = stream.next() => next,
            () = feed.closed() => return (Fed::WriteEnded, handed),
        };
        let Some(next) = next else {
            break;
        };
        let Ok(chunk) = next else {
            return (Fed::BodyBroken, handed);
        };
        let size = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        if handed.saturating_add(size) > length {
            return (Fed::BodyBroken, handed);
        }
        if feed.send(Ok(chunk)).await.is_err() {
            return (Fed::WriteEnded, handed);
        }
        handed += size;
    }
    let fed = if handed == length {
        Fed::Part
    } else {
        Fed::BodyBroken
    };
    (fed, handed)
}

/// A file whose parts are arriving, between requests: the write that
/// streams every part into the file's destinations ([`tee`]) runs in a task
/// of its own, so the parts of one file are one write per destination and
/// the file is still never staged. Each part's request holds the file's
/// lock ([`lock_file`]) while it feeds the write, so parts never
/// interleave. Dropping it (a request cancelled mid-part) ends the write's
/// stream early, which fails the write and deletes its copies.
struct PartStream {
    /// Offset the file's next part must start at.
    next: u64,
    /// The write's input.
    feed: mpsc::Sender<io::Result<Bytes>>,
    /// The write; answers the file's PUT once its stream ends.
    write: tokio::task::JoinHandle<Response>,
    /// Tells this stream's entry in [`PART_STREAMS`] from a newer one of
    /// the same file.
    generation: u64,
    /// The file's [`file_lock_name`], its key in [`PART_STREAMS`].
    key: String,
}

impl PartStream {
    /// Ends the file's stream after its last part and answers with the
    /// write's outcome.
    async fn finish(self) -> Response {
        drop(self.feed);
        self.write.await.unwrap_or_else(|_| unavailable())
    }

    /// Fails the write (its copies are deleted) and waits for it to end.
    async fn abandon(self) {
        let _ = self
            .feed
            .try_send(Err(io::Error::other("the upload part was abandoned")));
        drop(self.feed);
        let _ = self.write.await;
    }
}

/// Files whose parts are arriving on this node, by [`file_lock_name`]. An
/// entry is out of the map while a part's request feeds it.
static PART_STREAMS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PartStream>>,
> = std::sync::LazyLock::new(Default::default);

/// Hands out [`PartStream::generation`] values.
static PART_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Takes the open file called `key` out of [`PART_STREAMS`].
fn take_part_stream(key: &str) -> Option<PartStream> {
    PART_STREAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(key)
}

/// Puts an open file back into [`PART_STREAMS`] to wait for its next part.
fn put_part_stream(stream: PartStream) {
    PART_STREAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(stream.key.clone(), stream);
}

/// Removes the entry of `key` once its write ended, unless it is a newer
/// stream of the same file.
fn forget_part_stream(key: &str, generation: u64) {
    let mut streams = PART_STREAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if streams
        .get(key)
        .is_some_and(|stream| stream.generation == generation)
    {
        streams.remove(key);
    }
}

/// The write's input: what the parts feed, failing once no part arrives
/// for [`PART_IDLE`].
fn part_body(rx: mpsc::Receiver<io::Result<Bytes>>) -> Body {
    Body::from_stream(futures::stream::unfold(Some(rx), |rx| async move {
        let mut rx = rx?;
        match tokio::time::timeout(PART_IDLE, rx.recv()).await {
            Ok(Some(item)) => Some((item, Some(rx))),
            Ok(None) => None,
            Err(_) => Some((
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no next upload part arrived",
                )),
                None,
            )),
        }
    }))
}

/// Checks that file `index` of `session` may be written now (the fence is
/// live and the title's destinations here are the granted ones) and starts
/// its write ([`tee`]) in a task of its own, fed through the returned
/// stream.
///
/// # Errors
///
/// The answer to send instead: `lost`, `destinations_changed` or
/// `unavailable`.
async fn start_file(
    state: &Arc<AppState>,
    library: &LibraryStore,
    session: &UploadSession,
    index: usize,
    key: String,
) -> Result<PartStream, Response> {
    match upload_fence_live(library, session).await {
        Ok(true) => {}
        Ok(false) => {
            abort_and_discard(state, library, &session.upload_id).await;
            return Err(lost());
        }
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, error = %err, "upload fence check failed");
            return Err(unavailable());
        }
    }
    let destinations = match owner_destinations(state).await {
        Ok(destinations) if !destinations.is_empty() => destinations,
        Ok(_) | Err(_) => {
            tracing::warn!(upload_id = %session.upload_id, "no output destination is deployed on this shared node");
            return Err(unavailable());
        }
    };
    let destinations = match session_target_set(library, destinations, session).await {
        Ok(destinations) => destinations,
        Err(err) => {
            tracing::warn!(upload_id = %session.upload_id, error = %err, "could not read the title's destination filters");
            return Err(unavailable());
        }
    };
    if destination_ids(&destinations) != session.destinations {
        // The host named the files for the destinations it was granted;
        // writing them to a changed set would mis-name, skip one, or write
        // where a filter now says not to.
        tracing::warn!(upload_id = %session.upload_id, "the title's destinations on this shared node changed since the upload was granted; aborted");
        abort_and_discard(state, library, &session.upload_id).await;
        return Err(refused("destinations_changed"));
    }
    let (feed, rx) = mpsc::channel(CHANNEL_CHUNKS);
    let generation = PART_GENERATION.fetch_add(1, Ordering::Relaxed);
    let write = {
        let state = Arc::clone(state);
        let library = library.clone();
        let session = session.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let count = destinations.len();
            let ids = destination_ids(&destinations);
            let outcome = tee(&library, &session, index, part_body(rx), destinations).await;
            let response =
                settle_file(&state, &library, &session, index, count, &ids, outcome).await;
            forget_part_stream(&key, generation);
            response
        })
    };
    Ok(PartStream {
        next: 0,
        feed,
        write,
        generation,
        key,
    })
}

/// Records a written file, or rolls a failed one back, and answers its PUT.
async fn settle_file(
    state: &AppState,
    library: &LibraryStore,
    session: &UploadSession,
    index: usize,
    count: usize,
    ids: &[String],
    outcome: Result<Unrecorded, StreamFailure>,
) -> Response {
    match outcome {
        Ok(unrecorded) => {
            unrecorded.keep();
            record_written(state, library, session, index, count, ids).await
        }
        Err(StreamFailure::Lost) => {
            tracing::warn!(upload_id = %session.upload_id, job_id = %session.job_id, index, "the job's fence was lost mid-upload; aborted");
            abort_and_discard(state, library, &session.upload_id).await;
            lost()
        }
        Err(StreamFailure::Mismatch) => {
            tracing::warn!(upload_id = %session.upload_id, index, "an uploaded file does not match its manifest; discarded");
            refused("content_mismatch")
        }
        Err(StreamFailure::Taken) => {
            tracing::warn!(upload_id = %session.upload_id, index, "an uploaded file's key was written by another title since its grant; refused");
            refused("key_taken")
        }
        Err(StreamFailure::Broken) => unavailable(),
    }
}

/// `POST /api/node/v1/uploads/{upload_id}/commit`.
#[utoipa::path(
    post,
    path = "/api/node/v1/uploads/{upload_id}/commit",
    security(("uploadToken" = [])),
    params(("upload_id" = String, Path, description = "Upload id.")),
    request_body = bookclerk_node_proto::UploadCommitRequest,
    responses(
        (status = 200, description = "The upload was committed to the destinations (`committed`).", body = bookclerk_node_proto::UploadAnswer, example = json!({"status": "committed", "job_completed": true})),
        (status = 401, description = "The upload token is missing or wrong, or names no live upload (`unauthorized`).", body = bookclerk_node_proto::UploadAnswer, example = json!({"status": "unauthorized", "job_completed": false})),
        (status = 403, description = "The connection failed the transport policy (`transport_insecure`, with the check in `reason`).", body = crate::http_error::TransportRefused, example = json!({"error": "transport_insecure", "reason": "plain_http"})),
        (status = 404, description = "The node API is not mounted on this listener (`node_api_unmounted`).", body = crate::http_error::ErrorMessage, example = json!({"error": "node_api_unmounted"})),
        (status = 409, description = "The upload lost its fence to a newer one (`lost`).", body = bookclerk_node_proto::UploadAnswer, example = json!({"status": "lost", "job_completed": false})),
        (status = 421, description = "This node is not the upload's receiving node (`misdirected`).", body = bookclerk_node_proto::UploadAnswer, example = json!({"status": "misdirected", "job_completed": false})),
        (status = 422, description = "The commit was refused (`refused`), with the cause in `reason`: the body does not parse (`invalid_body`); the upload is unknown, aborted, or its job was cancelled (`unknown_upload`, `upload_aborted`, `job_cancelled`); not every granted file arrived (`files_missing`); the destinations changed since the grant (`destinations_changed`); a storage key now belongs to another title or writer, or the title's holding is gone (`key_taken`, `unknown_title`); or the closing upload is mismatched: an ordinary upload claims to finish the job (`not_a_closing_upload`), the closing upload does not (`closing_upload_must_finish`), or it arrives while titles are still pending (`titles_remaining`).", body = bookclerk_node_proto::UploadAnswer, examples(
            ("invalid_body" = (summary = "The body does not parse as a commit request.", value = json!({"status": "refused", "reason": "invalid_body", "job_completed": false}))),
            ("unknown_upload" = (summary = "No live upload has this id.", value = json!({"status": "refused", "reason": "unknown_upload", "job_completed": false}))),
            ("upload_aborted" = (summary = "The upload was aborted before the commit.", value = json!({"status": "refused", "reason": "upload_aborted", "job_completed": false}))),
            ("job_cancelled" = (summary = "The job was cancelled before the commit.", value = json!({"status": "refused", "reason": "job_cancelled", "job_completed": false}))),
            ("files_missing" = (summary = "Not every granted file was received.", value = json!({"status": "refused", "reason": "files_missing", "job_completed": false}))),
            ("destinations_changed" = (summary = "The destinations changed since the grant.", value = json!({"status": "refused", "reason": "destinations_changed", "job_completed": false}))),
            ("key_taken" = (summary = "A storage key now belongs to another title or writer.", value = json!({"status": "refused", "reason": "key_taken", "job_completed": false}))),
            ("unknown_title" = (summary = "The holding the upload was granted for no longer exists.", value = json!({"status": "refused", "reason": "unknown_title", "job_completed": false}))),
            ("not_a_closing_upload" = (summary = "An ordinary upload claims to finish the job.", value = json!({"status": "refused", "reason": "not_a_closing_upload", "job_completed": false}))),
            ("closing_upload_must_finish" = (summary = "The closing upload does not claim to finish the job.", value = json!({"status": "refused", "reason": "closing_upload_must_finish", "job_completed": false}))),
            ("titles_remaining" = (summary = "The closing upload arrived while titles are still pending.", value = json!({"status": "refused", "reason": "titles_remaining", "job_completed": false}))),
        )),
        (status = 503, description = "Storage or the upload record is unavailable; retry (`unavailable`).", body = bookclerk_node_proto::UploadAnswer, example = json!({"status": "unavailable", "job_completed": false})),
    ),
)]
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
    // A title's files reached only the destinations it was granted for
    // (its target set); one deployed or undeployed since its last file, or
    // a filter edited since, would leave a destination without the title
    // once it is marked acquired (later plans skip it), or holding it
    // against its filter. The closing upload carries no files, and an
    // already committed title is answered as committed again.
    if !session.holding_id.is_empty() && session.state != "committed" {
        match owner_destinations(&state).await {
            Ok(destinations) if !destinations.is_empty() => {
                let target = match session_target_set(&library, destinations, &session).await {
                    Ok(target) => target,
                    Err(err) => {
                        tracing::warn!(upload_id = %session.upload_id, error = %err, "could not read the title's destination filters");
                        return unavailable();
                    }
                };
                if destination_ids(&target) != session.destinations {
                    tracing::warn!(upload_id = %session.upload_id, "the title's destinations on this shared node changed before the upload's commit; aborted");
                    abort_and_discard(&state, &library, &session.upload_id).await;
                    return refused("destinations_changed");
                }
            }
            Ok(_) | Err(_) => {
                tracing::warn!(upload_id = %session.upload_id, "no output destination is deployed on this shared node");
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
