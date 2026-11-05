//! The host side of the long-lived cover worker.
//!
//! Media jobs get a fresh worker each, jailed to the job's own paths. A cover
//! needs no path at all, so one `bookclerk-media-worker covers` process
//! (jailed to nothing) serves every cover a pool is asked for. It starts on
//! the first cover, decodes on as many threads as the pool has workers, and
//! is started again on the next cover if it dies. Each in-flight cover holds a
//! permit, so at most twice the thread count are queued in the worker at
//! once, which bounds how much untrusted input it holds in memory.
//!
//! With isolation off the cover is processed on a blocking thread in this
//! process instead, exactly like a media job.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bookclerk_cover::wire::{self, Reply, Request};
use bookclerk_cover::CoverSet;
use bookclerk_sandbox::{CpuRate, Enforcement, NetPolicy, Spec, SPEC_ENV};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Semaphore};

use crate::error::{MediaError, Result};
use crate::pool::{forward_worker_stderr, worker_env_allowed, Confinement, WorkerLaunch};
use crate::WORKER_ENFORCEMENT_ENV;

/// First worker argument that selects the cover server.
pub const COVER_WORKER_ARG: &str = "covers";

/// Environment variable carrying the cover worker's decode thread count.
pub const COVER_THREADS_ENV: &str = "BOOKCLERK_COVER_THREADS";

/// Label of cover work in errors and logs.
const LABEL: &str = "cover";

/// Longest one cover may take in the worker before the worker is restarted.
const COVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Replies the reader has not matched to a request yet, by request id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<CoverSet>>>>>;

/// Where covers run.
#[derive(Debug, Clone)]
pub(crate) enum CoverRunner {
    /// The long-lived confined worker.
    Worker {
        /// Worker binary, and the jail on spawn-confined platforms.
        launch: WorkerLaunch,
        /// Required or best-effort.
        confinement: Confinement,
    },
    /// A blocking thread in this process (isolation off).
    InProcess,
    /// Refuse every cover, with the reason.
    Refuse(String),
}

/// One running cover worker process.
struct Running {
    /// Its stdin, where request frames go.
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    /// Requests awaiting a reply.
    pending: Pending,
    /// Cleared once the reader sees the worker end or misbehave.
    alive: Arc<AtomicBool>,
    /// The process; killed when this is dropped, or when a cover stalls it.
    child: std::sync::Mutex<tokio::process::Child>,
}

/// Processes covers on a shared confined worker.
pub(crate) struct CoverWorker {
    /// Where covers run.
    runner: CoverRunner,
    /// Decode threads the worker starts.
    threads: usize,
    /// One permit per cover in flight.
    permits: Arc<Semaphore>,
    /// The worker, once started.
    running: tokio::sync::Mutex<Option<Arc<Running>>>,
    /// Next request id.
    next_id: AtomicU64,
}

impl std::fmt::Debug for CoverWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverWorker")
            .field("runner", &self.runner)
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}

impl CoverWorker {
    /// A cover worker that starts on first use.
    ///
    /// # Arguments
    ///
    /// * `runner` - Where covers run.
    /// * `threads` - Decode threads; at least one.
    pub(crate) fn new(runner: CoverRunner, threads: usize) -> Self {
        let threads = threads.max(1);
        Self {
            runner,
            threads,
            permits: Arc::new(Semaphore::new(threads * 2)),
            running: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Sniffs, decodes, resizes and re-encodes one untrusted cover.
    ///
    /// # Errors
    ///
    /// [`MediaError::Cover`] when the image is refused or takes longer than
    /// 30 s to decode, [`MediaError::NotIsolated`] when isolation is
    /// required but unavailable, and [`MediaError::Worker`] when the worker
    /// cannot be started, dies or answers nonsense.
    pub(crate) async fn process(&self, cover: Vec<u8>) -> Result<CoverSet> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|err| worker_error(format!("cover worker closed: {err}")))?;
        match &self.runner {
            CoverRunner::Refuse(detail) => Err(MediaError::NotIsolated {
                job: LABEL,
                detail: detail.clone(),
            }),
            CoverRunner::InProcess => tokio::task::spawn_blocking(move || {
                let _permit = permit;
                bookclerk_cover::process(&cover).map_err(|err| MediaError::Cover(err.to_string()))
            })
            .await
            .map_err(|err| worker_error(format!("in-process cover panicked: {err}")))?,
            CoverRunner::Worker {
                launch,
                confinement,
            } => {
                let running = self.running(launch, *confinement).await?;
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                // Once sent, a cover keeps its permit and its deadline until
                // the worker answers or is stopped, even when the caller is
                // dropped (a cancelled acquire), so cancellations cannot pile
                // covers up in the worker past the in-flight limit.
                tokio::spawn(async move {
                    let _permit = permit;
                    ask(&running, id, cover).await
                })
                .await
                .map_err(|err| worker_error(format!("cover request failed: {err}")))?
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
        let running = Arc::new(spawn(launch, confinement, self.threads)?);
        *slot = Some(Arc::clone(&running));
        Ok(running)
    }
}

/// Sends one cover, numbered `id`, and waits for its reply.
async fn ask(running: &Arc<Running>, id: u64, cover: Vec<u8>) -> Result<CoverSet> {
    let frame = wire::encode_request(&Request { id, cover })
        .map_err(|err| MediaError::Cover(err.to_string()))?;
    let (tx, rx) = oneshot::channel();
    lock(&running.pending).insert(id, tx);
    // The frame is written by its own task, so a caller that is dropped
    // mid-write (a cancelled acquire) never leaves half a frame in the
    // pipe for the next cover to be read against.
    let writer = Arc::clone(running);
    let send = tokio::spawn(async move {
        let mut stdin = writer.stdin.lock().await;
        stdin.write_all(&frame).await?;
        stdin.flush().await
    });
    // Waiting for stdin and writing get their own deadline: a worker
    // that stops reading must not hold covers forever.
    let sent = match tokio::time::timeout(COVER_TIMEOUT, send).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(err))) => Err(format!("could not send cover: {err}")),
        Ok(Err(err)) => Err(format!("could not send cover: {err}")),
        Err(_) => Err(format!(
            "cover worker did not take a cover within {}s",
            COVER_TIMEOUT.as_secs()
        )),
    };
    if let Err(detail) = sent {
        lock(&running.pending).remove(&id);
        // Its input may now end in a partial frame.
        running.stop();
        return Err(worker_error(detail));
    }
    match tokio::time::timeout(COVER_TIMEOUT, rx).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(_)) => Err(worker_error("cover worker ended before replying".into())),
        Err(_) => {
            lock(&running.pending).remove(&id);
            // A worker stuck on one cover is killed now, so it stops
            // burning a decode thread, and restarted for the next. The
            // covers it was also working on fail as a worker error.
            running.stop();
            // Only this cover ran out its time, so it is the one refused
            // (and remembered as refused by the caller).
            Err(MediaError::Cover(format!(
                "decoding took longer than {}s",
                COVER_TIMEOUT.as_secs()
            )))
        }
    }
}

impl Running {
    /// Marks the worker dead and kills it; the next cover starts a new one.
    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        let _ = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .start_kill();
    }
}

/// A [`MediaError::Worker`] for cover work.
fn worker_error(detail: String) -> MediaError {
    MediaError::Worker { job: LABEL, detail }
}

/// Locks the pending map, recovering it after a panic elsewhere.
fn lock(
    pending: &Pending,
) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<CoverSet>>>> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts a cover worker and the task that reads its replies.
fn spawn(launch: &WorkerLaunch, confinement: Confinement, threads: usize) -> Result<Running> {
    let mut command = match &launch.jail {
        Some(jail) => {
            let mut command = tokio::process::Command::new(jail);
            command.arg("--").arg(&launch.bin);
            command
        }
        None => tokio::process::Command::new(&launch.bin),
    };
    command
        .arg(COVER_WORKER_ARG)
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
    command
        .env(WORKER_ENFORCEMENT_ENV, confinement.as_env_value())
        .env(COVER_THREADS_ENV, threads.to_string());
    if launch.jail.is_some() {
        let spec = cover_spec(confinement);
        command.env(
            SPEC_ENV,
            serde_json::to_string(&spec)
                .map_err(|err| worker_error(format!("could not encode jail spec: {err}")))?,
        );
    }
    let mut child = bookclerk_sandbox::with_fd_spawn_lock(|| command.spawn()).map_err(|err| {
        worker_error(format!(
            "could not spawn {} {COVER_WORKER_ARG}: {err}",
            launch.bin.display()
        ))
    })?;
    if let Some(stderr) = child.stderr.take() {
        forward_worker_stderr(LABEL, stderr);
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| worker_error("cover worker stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| worker_error("cover worker stdout unavailable".into()))?;
    let pending: Pending = Arc::default();
    let alive = Arc::new(AtomicBool::new(true));
    tokio::spawn(read_replies(
        stdout,
        Arc::clone(&pending),
        Arc::clone(&alive),
    ));
    Ok(Running {
        stdin: tokio::sync::Mutex::new(stdin),
        pending,
        alive,
        child: std::sync::Mutex::new(child),
    })
}

/// Hands each reply to its waiter until the worker ends or misbehaves, then
/// fails every cover still waiting.
async fn read_replies(
    mut stdout: impl tokio::io::AsyncRead + Unpin,
    pending: Pending,
    alive: Arc<AtomicBool>,
) {
    let reason = loop {
        match read_reply(&mut stdout).await {
            Ok(reply) => {
                let waiter = lock(&pending).remove(&reply.id());
                if let Some(waiter) = waiter {
                    let _ = waiter.send(match reply {
                        Reply::Ok { set, .. } => Ok(set),
                        Reply::Err { message, .. } => Err(MediaError::Cover(message)),
                    });
                }
            }
            Err(reason) => break reason,
        }
    };
    alive.store(false, Ordering::Release);
    tracing::warn!(reason = %reason, "cover worker stopped");
    for (_, waiter) in lock(&pending).drain() {
        let _ = waiter.send(Err(worker_error(reason.clone())));
    }
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

/// The jail spec for the cover worker on spawn-confined platforms: no paths,
/// no network.
fn cover_spec(confinement: Confinement) -> Spec {
    let enforcement = match confinement {
        Confinement::Required => Enforcement::Required,
        Confinement::BestEffort => Enforcement::BestEffort,
        Confinement::Off => Enforcement::Disabled,
    };
    Spec {
        label: "media-worker:covers".into(),
        reads: Vec::new(),
        writes: Vec::new(),
        net: NetPolicy::Deny,
        allow_exec: true,
        system_paths: true,
        enforcement,
        preserve_fds: vec![],
        windows_profile_name: None,
        memory_bytes: None,
        active_processes: None,
        cpu_rate_percent: CpuRate::Unspecified,
        inherit_handles: Vec::new(),
        cgroup_dir: None,
        unix_socket_dirs: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_cover::wire::encode_reply;

    fn png() -> Vec<u8> {
        // 1×1 opaque PNG.
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    #[tokio::test]
    async fn in_process_runner_processes_and_refuses() {
        let worker = CoverWorker::new(CoverRunner::InProcess, 1);
        let set = worker.process(png()).await.expect("processed");
        assert_eq!((set.width, set.height), (1, 1));
        let err = worker
            .process(b"<html>".to_vec())
            .await
            .expect_err("refused");
        assert!(matches!(err, MediaError::Cover(_)), "{err:?}");
    }

    #[tokio::test]
    async fn refusing_runner_refuses() {
        let worker = CoverWorker::new(CoverRunner::Refuse("no jail".into()), 1);
        let err = worker.process(png()).await.expect_err("refused");
        assert!(matches!(err, MediaError::NotIsolated { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn reader_dispatches_replies_and_fails_the_rest_at_the_end() {
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (tx_ok, rx_ok) = oneshot::channel();
        let (tx_err, rx_err) = oneshot::channel();
        let (tx_left, rx_left) = oneshot::channel();
        lock(&pending).insert(1, tx_ok);
        lock(&pending).insert(2, tx_err);
        lock(&pending).insert(3, tx_left);
        let set = bookclerk_cover::process(&png()).expect("processed");
        let stream = [
            encode_reply(&Reply::Err {
                id: 2,
                message: "bad".into(),
            })
            .expect("encode"),
            encode_reply(&Reply::Ok {
                id: 1,
                set: set.clone(),
            })
            .expect("encode"),
            // A reply nobody waits for is dropped.
            encode_reply(&Reply::Err {
                id: 9,
                message: "stale".into(),
            })
            .expect("encode"),
        ]
        .concat();
        read_replies(&stream[..], Arc::clone(&pending), Arc::clone(&alive)).await;
        assert_eq!(rx_ok.await.expect("sent").expect("ok"), set);
        assert!(matches!(rx_err.await.expect("sent"), Err(MediaError::Cover(m)) if m == "bad"));
        assert!(matches!(
            rx_left.await.expect("sent"),
            Err(MediaError::Worker { .. })
        ));
        assert!(!alive.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn read_reply_refuses_oversized_and_malformed_frames() {
        let huge = u32::try_from(wire::MAX_REPLY_BODY + 1)
            .expect("fits")
            .to_be_bytes();
        assert!(read_reply(&mut &huge[..]).await.is_err());
        let garbage = [0, 0, 0, 2, 1, 2];
        assert!(read_reply(&mut &garbage[..]).await.is_err());
        let short = [0, 0, 0, 9, 1];
        assert!(read_reply(&mut &short[..]).await.is_err());
    }

    #[test]
    fn the_jail_spec_grants_nothing() {
        let spec = cover_spec(Confinement::Required);
        assert!(spec.reads.is_empty() && spec.writes.is_empty());
        assert_eq!(spec.net, NetPolicy::Deny);
        assert_eq!(spec.enforcement, Enforcement::Required);
        assert_eq!(
            cover_spec(Confinement::Off).enforcement,
            Enforcement::Disabled
        );
    }
}
