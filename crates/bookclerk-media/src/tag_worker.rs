//! The host side of the long-lived tag worker (`bookclerk-media-worker tags`).
//!
//! Reading embedded tags needs a few header bytes and the metadata region of
//! each file, not the audio, and the file may be an S3 object rather than a
//! local path. So the worker opens nothing: it runs one
//! [`bookclerk_tags::TagReader`] per file, asks the host for byte ranges, and
//! the host fetches each range from wherever the file lives (a
//! [`RangeSource`]) and writes it back. The worker is jailed to no path and
//! no network, exactly like the cover worker, and stays up for a whole page
//! of files and the pages after it.
//!
//! The worker parses untrusted bytes, so the host treats it as compromised:
//! it fetches only spans inside the file, caps rounds and bytes per file at
//! the reader's own limits, gives each round a deadline, and checks every
//! record with [`TagRecord::validate`].
//!
//! With isolation off the reader runs in this process instead, exactly like
//! a media job.

use std::collections::HashMap;
use std::future::Future;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bookclerk_tags::wire::{self, Reply, Request};
use bookclerk_tags::{ByteSpan, Poll, TagReader, TagRecord};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};

use crate::cover_worker::pathless_spec;
use crate::error::{MediaError, Result};
use crate::pool::{forward_worker_stderr, worker_env_allowed, Confinement, WorkerLaunch};
use crate::worker_pipe::{ClearOnDrop, ThreadPipe};
use crate::WORKER_ENFORCEMENT_ENV;

/// First worker argument that selects the tag reader.
pub const TAG_WORKER_ARG: &str = "tags";

/// Label of tag work in errors and logs.
const LABEL: &str = "tags";

/// Longest the worker may take to answer one round (or to take a frame).
const ROUND_TIMEOUT: Duration = Duration::from_secs(30);

/// Most files read at once, across all callers.
const MAX_OPEN_FILES: usize = 16;

/// Most fetched bytes held at once, across all files, in KiB: a span waits
/// for room before it is fetched, so a page of files with large tags cannot
/// pile up more than this in the daemon.
const MAX_BYTES_IN_FLIGHT_KIB: u32 = 64 * 1024;

/// Where a file's bytes come from: a local file, a storage plugin's ranged
/// read, or a buffer in tests.
pub trait RangeSource: Send + Sync {
    /// Reads exactly `span` from the file.
    ///
    /// # Arguments
    ///
    /// * `span` - Lies inside the file; the host checked it.
    ///
    /// # Errors
    ///
    /// Any I/O failure; the read of that file then fails.
    fn read_range(&self, span: ByteSpan) -> impl Future<Output = std::io::Result<Vec<u8>>> + Send;
}

/// Where tags are read.
#[derive(Debug, Clone)]
pub(crate) enum TagRunner {
    /// The long-lived confined worker.
    Worker {
        /// Worker binary, and the jail on spawn-confined platforms.
        launch: WorkerLaunch,
        /// Required or best-effort.
        confinement: Confinement,
    },
    /// This process (isolation off).
    InProcess,
    /// Refuse every file, with the reason.
    Refuse(String),
}

/// Replies routed to the read waiting on each file id.
type Routes = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Reply>>>>;

/// One running tag worker process.
struct Running {
    /// Its stdin, where request frames go.
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    /// Open files' reply channels.
    routes: Routes,
    /// Cleared once the reader sees the worker end or misbehave.
    alive: Arc<AtomicBool>,
    /// The process; killed when this is dropped or a round stalls.
    child: std::sync::Mutex<tokio::process::Child>,
}

/// Reads tags on a shared confined worker.
pub(crate) struct TagWorker {
    /// Where tags are read.
    runner: TagRunner,
    /// One permit per file being read.
    permits: Arc<Semaphore>,
    /// One permit per KiB of fetched bytes not yet handed over.
    bytes: Arc<Semaphore>,
    /// The worker, once started.
    running: tokio::sync::Mutex<Option<Arc<Running>>>,
    /// Next file id.
    next_id: AtomicU64,
}

impl std::fmt::Debug for TagWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagWorker")
            .field("runner", &self.runner)
            .finish_non_exhaustive()
    }
}

impl TagWorker {
    /// A tag worker that starts on first use.
    pub(crate) fn new(runner: TagRunner) -> Self {
        Self {
            runner,
            permits: Arc::new(Semaphore::new(MAX_OPEN_FILES)),
            bytes: Arc::new(Semaphore::new(
                usize::try_from(MAX_BYTES_IN_FLIGHT_KIB).unwrap_or(usize::MAX),
            )),
            running: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Reads one file's embedded tags.
    ///
    /// # Arguments
    ///
    /// * `size` - The file's size in bytes.
    /// * `source` - Where its bytes come from.
    ///
    /// # Errors
    ///
    /// [`MediaError::Tags`] when the file is not a supported container or is
    /// malformed, [`MediaError::Io`] when `source` fails,
    /// [`MediaError::NotIsolated`] when isolation is required but
    /// unavailable, and [`MediaError::Worker`] when the worker cannot be
    /// started, dies, stalls or answers out of bounds.
    pub(crate) async fn read<S: RangeSource>(&self, size: u64, source: &S) -> Result<TagRecord> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|err| worker_error(format!("tag worker closed: {err}")))?;
        match &self.runner {
            TagRunner::Refuse(detail) => Err(MediaError::NotIsolated {
                job: LABEL,
                detail: detail.clone(),
            }),
            TagRunner::InProcess => read_in_process(size, source, &self.bytes).await,
            TagRunner::Worker {
                launch,
                confinement,
            } => {
                let running = self.running(launch, *confinement).await?;
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                read_on_worker(&running, id, size, source, &self.bytes).await
            }
        }
    }

    /// The live worker, started when there is none or the last one died.
    async fn running(
        &self,
        launch: &WorkerLaunch,
        confinement: Confinement,
    ) -> Result<Arc<Running>> {
        let mut slot = self.running.lock().await;
        if let Some(running) = slot.as_ref() {
            if running.alive.load(Ordering::Acquire) {
                return Ok(Arc::clone(running));
            }
        }
        let running = Arc::new(spawn(launch, confinement)?);
        *slot = Some(Arc::clone(&running));
        Ok(running)
    }
}

/// Fetches one span the reader asked for and checks its length.
///
/// # Returns
///
/// The bytes, and the share of the in-flight budget they hold until the
/// caller has handed them over.
async fn fetch<'b, S: RangeSource>(
    source: &S,
    span: ByteSpan,
    budget: &'b Semaphore,
) -> Result<(Vec<u8>, tokio::sync::SemaphorePermit<'b>)> {
    let kib = u32::try_from(span.len.div_ceil(1024))
        .unwrap_or(u32::MAX)
        .clamp(1, MAX_BYTES_IN_FLIGHT_KIB);
    let held = budget
        .acquire_many(kib)
        .await
        .map_err(|err| worker_error(format!("tag worker closed: {err}")))?;
    let bytes = source.read_range(span).await?;
    if u64::try_from(bytes.len()).ok() != Some(span.len) {
        return Err(MediaError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!(
                "storage returned {} bytes for a {}-byte range",
                bytes.len(),
                span.len
            ),
        )));
    }
    Ok((bytes, held))
}

/// Runs the reader in this process (isolation off).
async fn read_in_process<S: RangeSource>(
    size: u64,
    source: &S,
    budget: &Semaphore,
) -> Result<TagRecord> {
    let mut reader = TagReader::new(size);
    loop {
        match reader.poll() {
            Poll::Done(result) => return result.map_err(|err| MediaError::Tags(err.to_string())),
            Poll::Need(spans) => {
                for span in spans {
                    let (bytes, _held) = fetch(source, span, budget).await?;
                    reader
                        .supply(span.offset, bytes)
                        .map_err(|err| MediaError::Tags(err.to_string()))?;
                }
            }
        }
    }
}

/// Forgets a file on the worker side when its read ends early (an error or
/// a dropped caller), so the worker does not hold its reader forever.
struct OpenFile<'a> {
    /// The worker.
    running: &'a Arc<Running>,
    /// The file's id.
    id: u64,
    /// Set once the worker has forgotten the file by itself.
    finished: bool,
}

impl Drop for OpenFile<'_> {
    fn drop(&mut self) {
        lock(&self.running.routes).remove(&self.id);
        if !self.finished && self.running.alive.load(Ordering::Acquire) {
            if let Ok(frame) = wire::encode_request(&Request::Close { id: self.id }) {
                let running = Arc::clone(self.running);
                // Best effort: a Close that never lands only costs the
                // worker one idle reader until it restarts.
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = send(&running, frame).await;
                    });
                }
            }
        }
    }
}

/// Reads one file on the worker.
async fn read_on_worker<S: RangeSource>(
    running: &Arc<Running>,
    id: u64,
    size: u64,
    source: &S,
    budget: &Semaphore,
) -> Result<TagRecord> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    lock(&running.routes).insert(id, tx);
    let mut open = OpenFile {
        running,
        id,
        finished: false,
    };
    send(running, request(&Request::Open { id, size })?).await?;
    let mut rounds = 0u32;
    let mut fetched = 0u64;
    loop {
        let reply = match tokio::time::timeout(ROUND_TIMEOUT, rx.recv()).await {
            Ok(Some(reply)) => reply,
            Ok(None) => return Err(worker_error("tag worker ended before replying".into())),
            Err(_) => {
                // A worker stuck on one file is restarted for the rest.
                running.stop();
                return Err(worker_error(format!(
                    "tag worker did not answer within {}s",
                    ROUND_TIMEOUT.as_secs()
                )));
            }
        };
        match reply {
            Reply::Need { spans, .. } => {
                rounds += 1;
                let asked: u64 = spans.iter().map(|span| span.len).sum();
                fetched = fetched.saturating_add(asked);
                if rounds > bookclerk_tags::MAX_ROUNDS || fetched > bookclerk_tags::MAX_FETCH_BYTES
                {
                    return Err(misbehaved(
                        running,
                        "asked for more than the reader's limits",
                    ));
                }
                if spans
                    .iter()
                    .any(|span| span.end().is_none_or(|end| end > size))
                {
                    return Err(misbehaved(running, "asked for bytes outside the file"));
                }
                for span in spans {
                    let (bytes, _held) = fetch(source, span, budget).await?;
                    let frame = request(&Request::Data {
                        id,
                        offset: span.offset,
                        bytes,
                    })?;
                    send(running, frame).await?;
                }
            }
            Reply::Done { record, .. } => {
                open.finished = true;
                if let Err(err) = record.validate(size) {
                    return Err(misbehaved(running, &format!("sent a bad record: {err}")));
                }
                return Ok(*record);
            }
            Reply::Failed { message, .. } => {
                open.finished = true;
                return Err(MediaError::Tags(message));
            }
        }
    }
}

/// Encodes a request frame.
fn request(request: &Request) -> Result<Vec<u8>> {
    wire::encode_request(request)
        .map_err(|err| worker_error(format!("could not encode request: {err}")))
}

/// Writes one frame to the worker from its own task, so a caller dropped
/// mid-write never leaves half a frame in the pipe.
async fn send(running: &Arc<Running>, frame: Vec<u8>) -> Result<()> {
    let writer = Arc::clone(running);
    let write = tokio::spawn(async move {
        let mut stdin = writer.stdin.lock().await;
        stdin.write_all(&frame).await?;
        stdin.flush().await
    });
    let outcome = match tokio::time::timeout(ROUND_TIMEOUT, write).await {
        Ok(Ok(Ok(()))) => return Ok(()),
        Ok(Ok(Err(err))) => format!("could not write to tag worker: {err}"),
        Ok(Err(err)) => format!("could not write to tag worker: {err}"),
        Err(_) => format!(
            "tag worker did not take a frame within {}s",
            ROUND_TIMEOUT.as_secs()
        ),
    };
    // Its input may now end in a partial frame.
    running.stop();
    Err(worker_error(outcome))
}

impl Running {
    /// Marks the worker dead and kills it; the next read starts a new one.
    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        let _ = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .start_kill();
    }
}

/// A [`MediaError::Worker`] for tag work.
fn worker_error(detail: String) -> MediaError {
    MediaError::Worker { job: LABEL, detail }
}

/// Stops a worker that broke the protocol's bounds (it read something that
/// took it over, or it is broken) and returns the error for the file.
fn misbehaved(running: &Running, detail: &str) -> MediaError {
    running.stop();
    worker_error(format!("tag worker {detail}"))
}

/// Locks the route map, recovering it after a panic elsewhere.
fn lock(routes: &Routes) -> std::sync::MutexGuard<'_, HashMap<u64, mpsc::UnboundedSender<Reply>>> {
    routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts a tag worker and the task that reads its replies.
fn spawn(launch: &WorkerLaunch, confinement: Confinement) -> Result<Running> {
    let mut command = match &launch.jail {
        Some(jail) => {
            let mut command = tokio::process::Command::new(jail);
            command.arg("--").arg(&launch.bin);
            command
        }
        None => tokio::process::Command::new(&launch.bin),
    };
    command
        .arg(TAG_WORKER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in std::env::vars() {
        if worker_env_allowed(&key) {
            command.env(key, value);
        }
    }
    command.env(WORKER_ENFORCEMENT_ENV, confinement.as_env_value());
    if launch.jail.is_some() {
        let spec = pathless_spec("media-worker:tags", confinement);
        command.env(
            bookclerk_sandbox::SPEC_ENV,
            serde_json::to_string(&spec)
                .map_err(|err| worker_error(format!("could not encode jail spec: {err}")))?,
        );
    }
    let mut child = bookclerk_sandbox::with_fd_spawn_lock(|| command.spawn()).map_err(|err| {
        worker_error(format!(
            "could not spawn {} {TAG_WORKER_ARG}: {err}",
            launch.bin.display()
        ))
    })?;
    if let Some(stderr) = child.stderr.take() {
        forward_worker_stderr(LABEL, stderr);
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| worker_error("tag worker stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| worker_error("tag worker stdout unavailable".into()))?;
    let routes: Routes = Arc::default();
    let alive = Arc::new(AtomicBool::new(true));
    // Read on a thread of our own: on Windows a pipe read on the runtime's
    // blocking pool would keep that runtime from shutting down for as long
    // as this long-lived worker runs.
    let stdout = ThreadPipe::spawn("media-tag-replies".to_owned(), stdout)
        .map_err(|err| worker_error(format!("could not read tag worker output: {err}")))?;
    tokio::spawn(read_replies(
        stdout,
        Arc::clone(&routes),
        Arc::clone(&alive),
    ));
    Ok(Running {
        stdin: tokio::sync::Mutex::new(stdin),
        routes,
        alive,
        child: std::sync::Mutex::new(child),
    })
}

/// Routes each reply to its file's read until the worker ends or
/// misbehaves, then ends every read still waiting.
async fn read_replies(
    mut stdout: impl tokio::io::AsyncRead + Unpin,
    routes: Routes,
    alive: Arc<AtomicBool>,
) {
    // Cancelled with its runtime, the worker has no reader left: mark it dead
    // so the next read, on whichever runtime, starts a fresh one.
    let _dead_when_done = ClearOnDrop(Arc::clone(&alive));
    let reason = loop {
        match read_reply(&mut stdout).await {
            Ok(reply) => {
                let route = lock(&routes).get(&reply.id()).cloned();
                if let Some(route) = route {
                    let _ = route.send(reply);
                }
            }
            Err(reason) => break reason,
        }
    };
    alive.store(false, Ordering::Release);
    tracing::warn!(reason = %reason, "tag worker stopped");
    // Dropping the senders ends each waiting read with "ended before replying".
    lock(&routes).clear();
}

/// Reads and checks one reply frame.
///
/// # Errors
///
/// Why the stream cannot continue: end of output, a read failure, an
/// oversized frame or a malformed reply.
async fn read_reply(
    stdout: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::result::Result<Reply, String> {
    let mut prefix = [0u8; 4];
    stdout
        .read_exact(&mut prefix)
        .await
        .map_err(|err| format!("worker output ended: {err}"))?;
    let len = wire::body_len(prefix);
    if len > wire::MAX_REPLY_BODY {
        return Err(format!("worker sent a {len}-byte reply"));
    }
    let mut body = vec![0u8; len];
    stdout
        .read_exact(&mut body)
        .await
        .map_err(|err| format!("worker reply truncated: {err}"))?;
    wire::decode_reply(&body).map_err(|err| format!("malformed worker reply: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file held in memory.
    struct Buffer(Vec<u8>);

    impl RangeSource for Buffer {
        async fn read_range(&self, span: ByteSpan) -> std::io::Result<Vec<u8>> {
            let from = usize::try_from(span.offset).map_err(std::io::Error::other)?;
            let to = usize::try_from(span.offset + span.len).map_err(std::io::Error::other)?;
            self.0
                .get(from..to)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| std::io::Error::other("out of range"))
        }
    }

    /// A source that returns one byte too few.
    struct Short;

    impl RangeSource for Short {
        async fn read_range(&self, span: ByteSpan) -> std::io::Result<Vec<u8>> {
            Ok(vec![0; usize::try_from(span.len).unwrap_or(1) - 1])
        }
    }

    /// The smallest FLAC the reader accepts: a `STREAMINFO` and one comment.
    fn flac() -> Vec<u8> {
        let mut comments = Vec::new();
        comments.extend_from_slice(&0u32.to_le_bytes());
        comments.extend_from_slice(&1u32.to_le_bytes());
        let title = b"TITLE=Book";
        comments.extend_from_slice(&u32::try_from(title.len()).unwrap_or(0).to_le_bytes());
        comments.extend_from_slice(title);
        let mut file = b"fLaC".to_vec();
        file.extend_from_slice(&[0, 0, 0, 34]);
        file.extend_from_slice(&[0; 34]);
        let len = u32::try_from(comments.len()).unwrap_or(0).to_be_bytes();
        file.extend_from_slice(&[0x84, len[1], len[2], len[3]]);
        file.extend_from_slice(&comments);
        file
    }

    #[tokio::test]
    async fn in_process_runner_reads_and_refuses() {
        let worker = TagWorker::new(TagRunner::InProcess);
        let file = flac();
        let size = u64::try_from(file.len()).unwrap_or(0);
        let record = worker.read(size, &Buffer(file)).await.expect("read");
        assert_eq!(record.entries[0].value, "Book");
        let err = worker
            .read(5, &Buffer(b"hello".to_vec()))
            .await
            .expect_err("refused");
        assert!(matches!(err, MediaError::Tags(_)), "{err:?}");
        let err = worker.read(100, &Short).await.expect_err("short");
        assert!(matches!(err, MediaError::Io(_)), "{err:?}");
    }

    #[tokio::test]
    async fn refusing_runner_refuses() {
        let worker = TagWorker::new(TagRunner::Refuse("no jail".into()));
        let err = worker.read(4, &Short).await.expect_err("refused");
        assert!(matches!(err, MediaError::NotIsolated { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn reader_routes_replies_and_ends_the_rest() {
        let routes: Routes = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (tx_left, mut rx_left) = mpsc::unbounded_channel();
        lock(&routes).insert(1, tx);
        lock(&routes).insert(2, tx_left);
        let stream = [
            wire::encode_reply(&Reply::Failed {
                id: 1,
                message: "bad".into(),
            })
            .expect("encode"),
            // A reply nobody waits for is dropped.
            wire::encode_reply(&Reply::Failed {
                id: 9,
                message: "stale".into(),
            })
            .expect("encode"),
        ]
        .concat();
        read_replies(&stream[..], Arc::clone(&routes), Arc::clone(&alive)).await;
        assert!(matches!(rx.recv().await, Some(Reply::Failed { id: 1, .. })));
        assert!(rx.recv().await.is_none());
        assert!(rx_left.recv().await.is_none());
        assert!(!alive.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn read_reply_refuses_oversized_and_malformed_frames() {
        let huge = u32::try_from(wire::MAX_REPLY_BODY + 1)
            .expect("fits")
            .to_be_bytes();
        assert!(read_reply(&mut &huge[..]).await.is_err());
        let garbage = [0, 0, 0, 2, 7, 2];
        assert!(read_reply(&mut &garbage[..]).await.is_err());
    }
}
