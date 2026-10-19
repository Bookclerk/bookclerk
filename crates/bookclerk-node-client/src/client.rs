//! The host's connection to its owner (H-4 design, section 3).
//!
//! [`HostClient::run`] keeps one authenticated socket to the owner:
//!
//! 1. Dial under the transport policy ([`crate::OwnerEndpoint`]).
//! 2. `hello` (listing every fence this host still holds), `challenge`,
//!    `auth`: the challenge must name the cluster this host enrolled into,
//!    or nothing is signed; the signature covers the origin this host
//!    dialed. `welcome` carries the connection's owner box key.
//! 3. When the hello listed fences, the owner's `resumed` says which are
//!    still this host's; work under a lost fence is cancelled and its
//!    result discarded.
//! 4. While connected: a `heartbeat` every [`ClientTiming::heartbeat_interval`]
//!    (the host shows up in `node_status` like any node), pings every
//!    [`ClientTiming::ping_interval`], an idle close after
//!    [`ClientTiming::idle_timeout`], and every `wake` is handed to
//!    [`HostClientHandle::subscribe_wakes`] subscribers and then acked. A
//!    wake no subscriber receives is not acked: the owner keeps the
//!    delivery and sends it again (after its lease, or on the next
//!    connection). A heartbeat or claim the owner does not answer in
//!    [`ClientTiming::request_timeout`] is retried with backoff from
//!    [`ClientTiming::backoff_min`] to [`ClientTiming::backoff_max`]; those
//!    loops end only with their connection.
//! 5. With [`HostClientConfig::max_jobs`] above zero, a claim loop pulls
//!    jobs. Each job runs on its own task through the [`HostJobRunner`],
//!    with its secrets opened by this node's box key; a renewer extends
//!    the lease every third of it; the result is reported with `complete`
//!    or `fail` under the fence. Job tasks outlive a connection: renewals
//!    and the final report wait for the next one, whose hello resumes the
//!    fence. A job whose lease the owner has not confirmed (by `work` or
//!    `renewed`) within its length is cancelled as lost, connected or
//!    not, so it never runs past a lease the owner may have handed on. A
//!    `work` answering a claim that already timed out is failed at once
//!    with [`CLAIM_ABANDONED`] under its fence.
//! 6. On disconnect the client backs off from
//!    [`ClientTiming::backoff_min`] to [`ClientTiming::backoff_max`] with
//!    jitter, and the backoff resets after a welcomed connection. A close
//!    with [`CLOSE_REVOKED`] ends the client for good
//!    ([`ClientExit::Revoked`]) and cancels every held job. Shutdown
//!    cancels every held job the same way, so no job task outlives the
//!    client.
//!
//! Nothing secret is ever frame text: job secrets arrive sealed to this
//! node's box key and write-backs leave sealed to the owner's per-connection
//! key. Log lines name jobs, fences and close codes only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bookclerk_library::control_plane::{
    ClusterLink, ClusterLinkKeys, NodeHeartbeat, NodeSampler, NODE_HEARTBEAT_INTERVAL,
};
use bookclerk_node_proto::{
    decode_nonce, open_secrets, seal_secret_update, sign_auth, validate_box_public_key,
    AuthContext, Body, Frame, HeldFence, SecretRef, SigningKey, CLOSE_PROTOCOL, CLOSE_REVOKED,
    HANDSHAKE_TIMEOUT_SECS, IDLE_TIMEOUT_SECS, ITEM_JOB, PING_INTERVAL_SECS, SEND_QUEUE_FRAMES,
};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt as _, StreamExt as _};
use rand::{Rng as _, RngCore as _};
use tokio::sync::{broadcast, mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use zeroize::Zeroizing;

use crate::endpoint::{DialError, OwnerEndpoint, OwnerSocket};
use crate::jobs::{
    HandedSecrets, HostJob, HostJobCtx, HostJobError, HostJobRunner, WriteBackError,
};

/// Error kind reported when a job's secrets do not open with this node's
/// box key.
pub const SECRETS_UNOPENABLE: &str = "secrets_unopenable";

/// Error kind of the `fail` a host sends for a job the owner granted to a
/// claim that had already timed out here: nothing runs it, so the lease is
/// handed back at once instead of being left to expire.
pub const CLAIM_ABANDONED: &str = "claim_abandoned";

/// Longest lease this host counts on from one `work` or `renewed`; a
/// longer grant is renewed as if it were this long.
const MAX_LEASE: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a finished job's report keeps waiting for an owner connection.
const REPORT_DEADLINE: Duration = Duration::from_secs(600);

/// Longest outcome or error text sent to the owner.
const MAX_REPORT_TEXT_BYTES: usize = 4 * 1024;

/// Wakes buffered for slow subscribers.
const WAKE_BUFFER: usize = 64;

/// Timers of the client. The defaults follow the protocol constants.
#[derive(Debug, Clone, Copy)]
pub struct ClientTiming {
    /// Between heartbeats.
    pub heartbeat_interval: Duration,
    /// Between pings.
    pub ping_interval: Duration,
    /// Without any inbound frame before the client closes.
    pub idle_timeout: Duration,
    /// First reconnect delay.
    pub backoff_min: Duration,
    /// Largest reconnect delay.
    pub backoff_max: Duration,
    /// How long a request waits for room in the send queue and then for
    /// its answer.
    pub request_timeout: Duration,
}

impl Default for ClientTiming {
    fn default() -> Self {
        Self {
            heartbeat_interval: NODE_HEARTBEAT_INTERVAL,
            ping_interval: Duration::from_secs(PING_INTERVAL_SECS),
            idle_timeout: Duration::from_secs(IDLE_TIMEOUT_SECS),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// Measurements for `heartbeat` frames.
pub trait HeartbeatSource: Send + 'static {
    /// Samples this host now. May block briefly (it runs on a blocking
    /// thread).
    fn sample(&mut self) -> NodeHeartbeat;
}

impl HeartbeatSource for NodeSampler {
    fn sample(&mut self) -> NodeHeartbeat {
        NodeSampler::sample(self)
    }
}

/// What one client connects with.
pub struct HostClientConfig {
    /// The active cluster link.
    pub link: ClusterLink,
    /// Its node keys.
    pub keys: ClusterLinkKeys,
    /// This host's Bookclerk version.
    pub software_version: String,
    /// This process's incarnation (a plain id).
    pub incarnation: String,
    /// Jobs run at once. Zero claims nothing.
    pub max_jobs: usize,
    /// Timers.
    pub timing: ClientTiming,
}

/// Why [`HostClient::run`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientExit {
    /// The shutdown signal fired.
    Shutdown,
    /// The owner closed with [`CLOSE_REVOKED`]: this node's credential is
    /// revoked. The caller should mark the link revoked.
    Revoked,
}

/// One job this host holds a fence for.
struct HeldJob {
    /// Lease generation.
    fence: i64,
    /// Asks the runner to stop.
    cancel: Arc<AtomicBool>,
    /// Set when the owner says the fence is lost; the result is discarded.
    lost: Arc<AtomicBool>,
}

/// One live connection's request side.
#[derive(Clone)]
struct Conn {
    /// Distinguishes connections.
    serial: u64,
    /// Frames for the writer task.
    outbox: mpsc::Sender<Message>,
    /// Requests waiting for their answer, by frame id.
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Frame>>>>,
    /// Next request id.
    next_id: Arc<AtomicU64>,
    /// The owner's box key for this connection (public).
    owner_box_key: [u8; 32],
    /// Request timeout.
    timeout: Duration,
}

/// A request that got no answer.
#[derive(Debug)]
struct Unavailable;

impl Conn {
    /// Sends `body` and waits for the frame answering it.
    async fn request(&self, body: Body) -> Result<Frame, Unavailable> {
        let id = format!("q{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (answer, answered) = oneshot::channel();
        lock(&self.pending).insert(id.clone(), answer);
        let message = Message::Text(Frame::new(id.clone(), body).to_text().into());
        let sent = matches!(
            tokio::time::timeout(self.timeout, self.outbox.send(message)).await,
            Ok(Ok(()))
        );
        let frame = if sent {
            match tokio::time::timeout(self.timeout, answered).await {
                Ok(Ok(frame)) => Some(frame),
                _ => None,
            }
        } else {
            None
        };
        if frame.is_none() {
            lock(&self.pending).remove(&id);
        }
        frame.ok_or(Unavailable)
    }

    /// True once the writer task is gone (the connection is over).
    fn is_closed(&self) -> bool {
        self.outbox.is_closed()
    }
}

/// The next retry delay after `delay`: doubled, up to `max`.
fn next_retry(delay: Duration, max: Duration) -> Duration {
    (delay * 2).min(max)
}

/// Locks a mutex, recovering from poisoning (every critical section is a
/// single map operation).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// State shared by the connection loop, job tasks, and handles.
pub(crate) struct Shared {
    /// Where the owner is.
    endpoint: OwnerEndpoint,
    /// Owner cluster id the host enrolled into.
    cluster_id: String,
    /// This host's node id there.
    node_id: String,
    /// This process's incarnation.
    incarnation: String,
    /// This host's version.
    software_version: String,
    /// Ed25519 signing key (zeroized on drop).
    sign_key: SigningKey,
    /// X25519 box secret.
    box_secret: Zeroizing<[u8; 32]>,
    /// Timers.
    timing: ClientTiming,
    /// Jobs at once.
    max_jobs: usize,
    /// Fences held, by job id.
    held: Mutex<HashMap<String, HeldJob>>,
    /// The live connection, if any.
    conn: watch::Sender<Option<Conn>>,
    /// Welcomed connections so far.
    connections: watch::Sender<u64>,
    /// Wakes for local handlers.
    wakes: broadcast::Sender<serde_json::Value>,
    /// One permit per job slot.
    job_slots: Arc<Semaphore>,
    /// Serial of the next connection.
    next_serial: AtomicU64,
    /// Set (under the `held` lock) once every job was abandoned; a job
    /// starting after that is abandoned at once.
    abandoned: AtomicBool,
}

impl Shared {
    /// Every fence still held (not lost), for a hello.
    fn held_fences(&self) -> Vec<HeldFence> {
        let mut fences: Vec<HeldFence> = lock(&self.held)
            .iter()
            .filter(|(_, held)| !held.lost.load(Ordering::SeqCst))
            .map(|(id, held)| HeldFence {
                kind: ITEM_JOB.into(),
                id: id.clone(),
                fence: held.fence,
            })
            .collect();
        fences.sort_by(|a, b| a.id.cmp(&b.id));
        fences
    }

    /// Marks the job fences the owner reports lost: their work is
    /// cancelled and its result discarded.
    fn mark_lost(&self, lost: &[HeldFence]) {
        let held = lock(&self.held);
        for fence in lost {
            if fence.kind != ITEM_JOB {
                continue;
            }
            if let Some(job) = held.get(&fence.id).filter(|job| job.fence == fence.fence) {
                tracing::warn!(job_id = %fence.id, fence = fence.fence, "owner reports the job's fence lost; cancelling it");
                job.lost.store(true, Ordering::SeqCst);
                job.cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Cancels every held job as lost (the node was revoked, or the client
    /// is shutting down); their results are discarded.
    fn abandon_all(&self) {
        let held = lock(&self.held);
        self.abandoned.store(true, Ordering::SeqCst);
        for job in held.values() {
            job.lost.store(true, Ordering::SeqCst);
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Forgets a job's fence once it is reported or abandoned.
    fn release(&self, job_id: &str, fence: i64) {
        let mut held = lock(&self.held);
        if held.get(job_id).is_some_and(|job| job.fence == fence) {
            held.remove(job_id);
        }
    }

    /// True while this host holds `fence` for `job_id` and it is not lost.
    fn holds(&self, job_id: &str, fence: i64) -> bool {
        lock(&self.held)
            .get(job_id)
            .is_some_and(|job| job.fence == fence && !job.lost.load(Ordering::SeqCst))
    }

    /// The live connection.
    fn current(&self) -> Option<Conn> {
        self.conn.borrow().clone()
    }

    /// The live connection, waiting for one until `deadline`.
    async fn wait_conn(&self, deadline: Instant) -> Option<Conn> {
        let mut receiver = self.conn.subscribe();
        loop {
            if let Some(conn) = receiver.borrow_and_update().clone() {
                return Some(conn);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(left, receiver.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    /// `secret.update`: seals `value` to the live connection's owner box
    /// key, bound to this node, the job, its fence and the ref.
    pub(crate) async fn write_back(
        &self,
        job_id: &str,
        fence: i64,
        secret_ref: &SecretRef,
        value: &[u8],
    ) -> Result<(), WriteBackError> {
        if !self.holds(job_id, fence) {
            return Err(WriteBackError::Lost);
        }
        let conn = self
            .wait_conn(Instant::now() + self.timing.request_timeout)
            .await
            .ok_or(WriteBackError::Unavailable)?;
        let mut ephemeral = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(ephemeral.as_mut_slice());
        let sealed_value = seal_secret_update(
            &conn.owner_box_key,
            &ephemeral,
            &self.node_id,
            job_id,
            fence,
            secret_ref,
            value,
        )
        .map_err(|_| WriteBackError::Unavailable)?;
        drop(ephemeral);
        let answer = conn
            .request(Body::SecretUpdate {
                job_id: job_id.to_string(),
                fence,
                secret_ref: secret_ref.clone(),
                sealed_value,
            })
            .await
            .map_err(|Unavailable| WriteBackError::Unavailable)?;
        match answer.body {
            Body::Accepted {} => {
                tracing::info!(job_id, fence, kind = %secret_ref.kind, account_id = %secret_ref.account_id, "wrote a refreshed store token back to the owner");
                Ok(())
            }
            Body::Lost { .. } => Err(WriteBackError::Lost),
            Body::Error { code, message } if code == "refused" => {
                tracing::warn!(job_id, fence, reason = %message, "owner refused the token write-back");
                if message == "fence_not_live" {
                    Err(WriteBackError::Lost)
                } else {
                    Err(WriteBackError::Refused(message))
                }
            }
            _ => Err(WriteBackError::Unavailable),
        }
    }
}

/// Watches a running client.
#[derive(Clone)]
pub struct HostClientHandle {
    /// The client's state.
    shared: Arc<Shared>,
}

impl HostClientHandle {
    /// Wakes the owner forwarded (`{"type": "deployment.changed", ...}`).
    ///
    /// A wake is acked once it is handed to at least one subscriber; while
    /// there is none it stays unacked and the owner redelivers it later.
    /// A subscriber that falls more than 64 wakes behind sees
    /// [`broadcast::error::RecvError::Lagged`] for wakes already acked, and
    /// must then resync the state they announce from the owner.
    #[must_use]
    pub fn subscribe_wakes(&self) -> broadcast::Receiver<serde_json::Value> {
        self.shared.wakes.subscribe()
    }

    /// Count of welcomed connections so far.
    #[must_use]
    pub fn connections(&self) -> watch::Receiver<u64> {
        self.shared.connections.subscribe()
    }

    /// True while a welcomed connection is live.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.shared.current().is_some()
    }

    /// Fences this host holds and has not lost.
    #[must_use]
    pub fn held_fences(&self) -> Vec<HeldFence> {
        self.shared.held_fences()
    }

    /// The node id this host has in the owner cluster.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.shared.node_id
    }
}

/// Aborts its tasks when dropped.
struct AbortOnDrop(Vec<JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// How one connection attempt ended.
enum SessionEnd {
    /// It was welcomed and later ended; the backoff resets.
    Served,
    /// The owner revoked this node.
    Revoked,
    /// It failed before the welcome.
    Failed(String),
}

/// The host side of the node API for one cluster link.
pub struct HostClient<R: HostJobRunner> {
    /// Shared state.
    shared: Arc<Shared>,
    /// Runs claimed jobs.
    runner: Arc<R>,
    /// Heartbeat measurements.
    sampler: Arc<Mutex<Box<dyn HeartbeatSource>>>,
}

impl<R: HostJobRunner> HostClient<R> {
    /// A client for `config`, running jobs through `runner`.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the link's origin or pin fails the
    /// transport policy.
    pub fn new(
        config: HostClientConfig,
        runner: R,
        sampler: Box<dyn HeartbeatSource>,
    ) -> Result<(Self, HostClientHandle), DialError> {
        let endpoint =
            OwnerEndpoint::new(&config.link.owner_origin, config.link.spki_pin.as_deref())?;
        let (conn, _) = watch::channel(None);
        let (connections, _) = watch::channel(0);
        let (wakes, _) = broadcast::channel(WAKE_BUFFER);
        let shared = Arc::new(Shared {
            endpoint,
            cluster_id: config.link.owner_cluster_id.clone(),
            node_id: config.link.remote_node_id.clone(),
            incarnation: config.incarnation,
            software_version: config.software_version,
            sign_key: SigningKey::from_bytes(config.keys.sign_seed()),
            box_secret: Zeroizing::new(*config.keys.box_secret()),
            timing: config.timing,
            max_jobs: config.max_jobs,
            held: Mutex::new(HashMap::new()),
            conn,
            connections,
            wakes,
            job_slots: Arc::new(Semaphore::new(config.max_jobs)),
            next_serial: AtomicU64::new(1),
            abandoned: AtomicBool::new(false),
        });
        let handle = HostClientHandle {
            shared: Arc::clone(&shared),
        };
        Ok((
            Self {
                shared,
                runner: Arc::new(runner),
                sampler: Arc::new(Mutex::new(sampler)),
            },
            handle,
        ))
    }

    /// Connects, serves, and reconnects until `shutdown` turns true (or its
    /// sender is dropped) or the owner revokes this node.
    ///
    /// Every held job is cancelled as lost before this returns, whatever
    /// the reason, so no job task keeps running for a client that is gone.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> ClientExit {
        let exit = self.serve_until_exit(shutdown).await;
        self.shared.abandon_all();
        exit
    }

    /// The reconnect loop behind [`Self::run`].
    async fn serve_until_exit(&self, mut shutdown: watch::Receiver<bool>) -> ClientExit {
        let timing = self.shared.timing;
        let mut backoff = timing.backoff_min;
        loop {
            if *shutdown.borrow() {
                return ClientExit::Shutdown;
            }
            let mut stopping = false;
            let end = tokio::select! {
                end = self.connect_and_serve() => end,
                changed = shutdown.changed() => {
                    stopping = changed.is_err();
                    SessionEnd::Served
                }
            };
            self.shared.conn.send_replace(None);
            if stopping || *shutdown.borrow() {
                return ClientExit::Shutdown;
            }
            match end {
                SessionEnd::Revoked => {
                    tracing::error!(
                        node_id = %self.shared.node_id,
                        cluster_id = %self.shared.cluster_id,
                        "the owner revoked this host node; stopping the cluster link"
                    );
                    return ClientExit::Revoked;
                }
                SessionEnd::Served => backoff = timing.backoff_min,
                SessionEnd::Failed(reason) => {
                    tracing::warn!(
                        cluster_id = %self.shared.cluster_id,
                        origin = %self.shared.endpoint.origin(),
                        %reason,
                        "owner connection failed"
                    );
                }
            }
            let jitter_ms = u64::try_from(backoff.as_millis() / 4).unwrap_or(0);
            let delay =
                backoff + Duration::from_millis(rand::thread_rng().gen_range(0..=jitter_ms.max(1)));
            backoff = (backoff * 2).min(timing.backoff_max);
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        return ClientExit::Shutdown;
                    }
                }
            }
        }
    }

    /// One connection: dial, handshake, serve.
    async fn connect_and_serve(&self) -> SessionEnd {
        let socket = match self.shared.endpoint.connect_socket().await {
            Ok(socket) => socket,
            Err(err) => return SessionEnd::Failed(err.to_string()),
        };
        let (socket, owner_box_key) = match self.handshake(socket).await {
            Ok(welcomed) => welcomed,
            Err(end) => return end,
        };
        self.serve(socket, owner_box_key).await
    }

    /// hello → challenge → auth → welcome, then `resumed` when the hello
    /// listed fences.
    async fn handshake(
        &self,
        mut socket: OwnerSocket,
    ) -> Result<(OwnerSocket, [u8; 32]), SessionEnd> {
        let shared = &self.shared;
        let resume = shared.held_fences();
        let resuming = !resume.is_empty();
        let hello = Frame::new(
            "hello",
            Body::Hello {
                node_id: shared.node_id.clone(),
                incarnation: shared.incarnation.clone(),
                software_version: shared.software_version.clone(),
                resume,
            },
        );
        send(&mut socket, &hello).await?;
        let challenge = next_handshake_frame(&mut socket).await?;
        let Body::Challenge { nonce, cluster_id } = challenge.body else {
            return Err(SessionEnd::Failed("expected a challenge".into()));
        };
        if cluster_id != shared.cluster_id {
            // Never sign a challenge for another cluster.
            close(&mut socket, CLOSE_PROTOCOL).await;
            return Err(SessionEnd::Failed(
                "the owner answered for another cluster".into(),
            ));
        }
        let Some(nonce) = decode_nonce(&nonce) else {
            return Err(SessionEnd::Failed("malformed challenge nonce".into()));
        };
        let sig = sign_auth(
            &shared.sign_key,
            &AuthContext {
                cluster_id: &shared.cluster_id,
                node_id: &shared.node_id,
                incarnation: &shared.incarnation,
                nonce: &nonce,
                owner_origin: shared.endpoint.origin(),
            },
        );
        send(&mut socket, &Frame::new("auth", Body::Auth { sig })).await?;
        let welcome = next_handshake_frame(&mut socket).await?;
        let Body::Welcome {
            node_id,
            cluster_id,
            box_key,
        } = welcome.body
        else {
            return Err(SessionEnd::Failed("expected a welcome".into()));
        };
        if node_id != shared.node_id || cluster_id != shared.cluster_id {
            close(&mut socket, CLOSE_PROTOCOL).await;
            return Err(SessionEnd::Failed("the welcome names another node".into()));
        }
        let owner_box_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(box_key.trim())
            .ok()
            .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
            .filter(|key| validate_box_public_key(key).is_ok());
        let Some(owner_box_key) = owner_box_key else {
            close(&mut socket, CLOSE_PROTOCOL).await;
            return Err(SessionEnd::Failed(
                "the welcome carries no valid owner box key".into(),
            ));
        };
        if resuming {
            let resumed = next_handshake_frame(&mut socket).await?;
            let Body::Resumed { renewed, lost } = resumed.body else {
                return Err(SessionEnd::Failed("expected resumed".into()));
            };
            tracing::info!(
                renewed = renewed.len(),
                lost = lost.len(),
                "owner answered the held fences"
            );
            shared.mark_lost(&lost);
        }
        Ok((socket, owner_box_key))
    }

    /// The welcomed connection: writer, heartbeat and claim tasks, and the
    /// read loop.
    async fn serve(&self, socket: OwnerSocket, owner_box_key: [u8; 32]) -> SessionEnd {
        let shared = &self.shared;
        let (sink, stream) = socket.split();
        let (outbox, queue) = mpsc::channel(SEND_QUEUE_FRAMES);
        let conn = Conn {
            serial: shared.next_serial.fetch_add(1, Ordering::Relaxed),
            outbox,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            owner_box_key,
            timeout: shared.timing.request_timeout,
        };
        let mut tasks = vec![tokio::spawn(writer(sink, queue))];
        tasks.push(tokio::spawn(heartbeat_loop(
            Arc::clone(shared),
            conn.clone(),
            Arc::clone(&self.sampler),
        )));
        if shared.max_jobs > 0 {
            tasks.push(tokio::spawn(claim_loop(
                Arc::clone(shared),
                conn.clone(),
                Arc::clone(&self.runner),
            )));
        }
        let _tasks = AbortOnDrop(tasks);
        shared.conn.send_replace(Some(conn.clone()));
        shared.connections.send_modify(|count| *count += 1);
        tracing::info!(
            node_id = %shared.node_id,
            cluster_id = %shared.cluster_id,
            serial = conn.serial,
            "connected to the owner cluster"
        );
        let end = read_loop(shared, stream, &conn).await;
        shared.conn.send_if_modified(|live| {
            let ours = live.as_ref().is_some_and(|live| live.serial == conn.serial);
            if ours {
                *live = None;
            }
            ours
        });
        lock(&conn.pending).clear();
        tracing::info!(node_id = %shared.node_id, serial = conn.serial, "owner connection ended");
        end
    }
}

/// Sends one frame during the handshake.
async fn send(socket: &mut OwnerSocket, frame: &Frame) -> Result<(), SessionEnd> {
    socket
        .send(Message::Text(frame.to_text().into()))
        .await
        .map_err(|err| SessionEnd::Failed(format!("send failed: {err}")))
}

/// Sends a close frame; the owner may already be gone.
async fn close(socket: &mut OwnerSocket, code: u16) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        })))
        .await;
}

/// The next handshake frame, within the handshake timeout. A close with
/// [`CLOSE_REVOKED`] is [`SessionEnd::Revoked`].
async fn next_handshake_frame(socket: &mut OwnerSocket) -> Result<Frame, SessionEnd> {
    let deadline = Instant::now() + Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);
    loop {
        let message = tokio::time::timeout_at(deadline.into(), socket.next())
            .await
            .map_err(|_| SessionEnd::Failed("handshake timed out".into()))?;
        match message {
            Some(Ok(Message::Text(text))) => {
                return Frame::parse(text.as_str())
                    .map_err(|err| SessionEnd::Failed(format!("handshake frame: {err}")));
            }
            Some(Ok(Message::Close(frame))) => {
                let code = frame.map(|frame| u16::from(frame.code));
                return Err(if code == Some(CLOSE_REVOKED) {
                    SessionEnd::Revoked
                } else {
                    SessionEnd::Failed(format!("owner closed the handshake with {code:?}"))
                });
            }
            Some(Ok(Message::Binary(_))) => {
                return Err(SessionEnd::Failed("binary handshake frame".into()))
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => return Err(SessionEnd::Failed(format!("handshake: {err}"))),
            None => return Err(SessionEnd::Failed("owner closed the handshake".into())),
        }
    }
}

/// Drains the send queue into the socket.
async fn writer(mut sink: SplitSink<OwnerSocket, Message>, mut queue: mpsc::Receiver<Message>) {
    while let Some(message) = queue.recv().await {
        let closing = matches!(message, Message::Close(_));
        if sink.send(message).await.is_err() || closing {
            break;
        }
    }
    let _ = sink.close().await;
}

/// Reads frames until the connection ends: answers go to their requests,
/// wakes to subscribers (then acked); pings and the idle timeout run here.
async fn read_loop(
    shared: &Arc<Shared>,
    mut stream: SplitStream<OwnerSocket>,
    conn: &Conn,
) -> SessionEnd {
    let timing = shared.timing;
    let mut ping = tokio::time::interval(timing.ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.reset();
    let mut last_inbound = Instant::now();
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_inbound.elapsed() >= timing.idle_timeout {
                    let _ = conn.outbox.try_send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Normal,
                        reason: "idle".into(),
                    })));
                    return SessionEnd::Served;
                }
                // A full queue already keeps the owner busy; skip the ping.
                if let Err(mpsc::error::TrySendError::Closed(_)) =
                    conn.outbox.try_send(Message::Ping(Default::default()))
                {
                    return SessionEnd::Served;
                }
            }
            message = stream.next() => {
                let Some(Ok(message)) = message else { return SessionEnd::Served };
                last_inbound = Instant::now();
                match message {
                    Message::Text(text) => {
                        let Ok(frame) = Frame::parse(text.as_str()) else {
                            let _ = conn.outbox.try_send(Message::Close(Some(CloseFrame {
                                code: CloseCode::from(CLOSE_PROTOCOL),
                                reason: "malformed frame".into(),
                            })));
                            return SessionEnd::Served;
                        };
                        if let Body::Wake { message } = frame.body {
                            let kind = message
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            tracing::debug!(kind = %kind, "owner wake");
                            // Ack only a wake some handler received: an
                            // unacked delivery stays durable on the owner,
                            // which sends it again after its lease or on
                            // the next connection.
                            if shared.wakes.send(message).is_err() {
                                tracing::debug!(kind = %kind, "no wake subscriber; leaving the wake unacked");
                                continue;
                            }
                            let ack = Message::Text(Frame::new(frame.id, Body::Ack {}).to_text().into());
                            if !matches!(
                                tokio::time::timeout(timing.request_timeout, conn.outbox.send(ack)).await,
                                Ok(Ok(()))
                            ) {
                                return SessionEnd::Served;
                            }
                            continue;
                        }
                        let waiting = lock(&conn.pending).remove(&frame.id);
                        match waiting {
                            Some(answer) => {
                                let _ = answer.send(frame);
                            }
                            None => unsolicited(shared, conn, frame),
                        }
                    }
                    Message::Close(frame) => {
                        let code = frame.map(|frame| u16::from(frame.code));
                        if code == Some(CLOSE_REVOKED) {
                            return SessionEnd::Revoked;
                        }
                        tracing::info!(?code, "owner closed the connection");
                        return SessionEnd::Served;
                    }
                    Message::Binary(_) => {
                        let _ = conn.outbox.try_send(Message::Close(Some(CloseFrame {
                            code: CloseCode::from(CLOSE_PROTOCOL),
                            reason: "binary frames are refused".into(),
                        })));
                        return SessionEnd::Served;
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        }
    }
}

/// Handles an owner frame no request is waiting for. A `work` for a job
/// answers a claim that already timed out (and was retried): nothing here
/// runs it, so it is failed with [`CLAIM_ABANDONED`] under its fence and
/// the owner frees the lease now instead of when it expires. Anything else
/// is dropped.
fn unsolicited(shared: &Arc<Shared>, conn: &Conn, frame: Frame) {
    let Body::Work {
        item, job, fence, ..
    } = frame.body
    else {
        tracing::debug!(id = %frame.id, "unsolicited owner frame");
        return;
    };
    if item != ITEM_JOB || shared.holds(&job.id, fence) {
        tracing::debug!(id = %frame.id, "unsolicited owner work frame");
        return;
    }
    tracing::warn!(job_id = %job.id, fence, "owner granted a job to a claim that timed out; handing it back");
    let conn = conn.clone();
    tokio::spawn(async move {
        let answer = conn
            .request(Body::Fail {
                job_id: job.id.clone(),
                fence,
                error_kind: CLAIM_ABANDONED.into(),
                message: "the host's claim timed out before the owner answered".into(),
            })
            .await;
        if !matches!(
            answer.map(|frame| frame.body),
            Ok(Body::Accepted {} | Body::Lost { .. })
        ) {
            tracing::warn!(job_id = %job.id, fence, "the owner did not take the abandoned job back; its lease will expire");
        }
    });
}

/// Sends a heartbeat every interval until the connection goes away. A
/// heartbeat the owner does not answer is retried with backoff (never
/// slower than the interval); the loop ends only with its connection.
async fn heartbeat_loop(
    shared: Arc<Shared>,
    conn: Conn,
    sampler: Arc<Mutex<Box<dyn HeartbeatSource>>>,
) {
    let interval = shared.timing.heartbeat_interval;
    let mut retry = shared.timing.backoff_min;
    loop {
        let sampler = Arc::clone(&sampler);
        let Ok(sample) = tokio::task::spawn_blocking(move || lock(&sampler).sample()).await else {
            return;
        };
        let beat = Body::Heartbeat {
            started_at: sample.started_at,
            interval_secs: interval.as_secs().max(1),
            cpu_millicores: sample.cpu_millicores,
            memory_bytes: sample.memory_bytes,
            cpu_used_millicores: sample.cpu_used_millicores,
            memory_available_bytes: sample.memory_available_bytes,
            files_dir_free_bytes: sample.files_dir_free_bytes,
        };
        let delay = match conn.request(beat).await {
            Ok(answer) => {
                if !matches!(answer.body, Body::Accepted {}) {
                    tracing::debug!("owner did not record the heartbeat");
                }
                retry = shared.timing.backoff_min;
                interval
            }
            Err(Unavailable) => {
                if conn.is_closed() {
                    return;
                }
                tracing::warn!(
                    serial = conn.serial,
                    "owner did not answer the heartbeat; retrying"
                );
                let delay = retry.min(interval);
                retry = next_retry(retry, shared.timing.backoff_max);
                delay
            }
        };
        tokio::time::sleep(delay).await;
    }
}

/// Pulls jobs while job slots are free. A claim the owner does not answer
/// is retried with backoff; the loop ends only with its connection.
async fn claim_loop<R: HostJobRunner>(shared: Arc<Shared>, conn: Conn, runner: Arc<R>) {
    let mut retry = shared.timing.backoff_min;
    loop {
        let Ok(slot) = Arc::clone(&shared.job_slots).acquire_owned().await else {
            return;
        };
        // The owner starts the lease after this; counting from here never
        // overstates it.
        let claimed_at = tokio::time::Instant::now();
        let answer = conn
            .request(Body::Claim {
                item: ITEM_JOB.into(),
                class: None,
            })
            .await;
        if answer.is_ok() {
            retry = shared.timing.backoff_min;
        }
        match answer {
            Ok(Frame {
                body:
                    Body::Work {
                        item,
                        job,
                        fence,
                        lease_secs,
                        sealed_secrets,
                    },
                ..
            }) if item == ITEM_JOB => {
                tracing::info!(job_id = %job.id, fence, kind = %job.kind, "claimed a job from the owner");
                let host_job = HostJob {
                    node_id: shared.node_id.clone(),
                    job,
                    fence,
                    lease_secs,
                };
                tokio::spawn(run_job(
                    Arc::clone(&shared),
                    Arc::clone(&runner),
                    slot,
                    host_job,
                    sealed_secrets,
                    claimed_at,
                ));
            }
            Ok(Frame {
                body: Body::NoWork { retry_after_secs },
                ..
            }) => {
                drop(slot);
                tokio::time::sleep(Duration::from_secs(retry_after_secs.clamp(1, 300))).await;
            }
            Ok(other) => {
                drop(slot);
                if let Body::Error { code, .. } = &other.body {
                    tracing::warn!(%code, "owner did not serve the claim");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(Unavailable) => {
                drop(slot);
                if conn.is_closed() {
                    return;
                }
                tracing::warn!(
                    serial = conn.serial,
                    "owner did not answer the claim; retrying"
                );
                tokio::time::sleep(retry).await;
                retry = next_retry(retry, shared.timing.backoff_max);
            }
        }
    }
}

/// Text cut to [`MAX_REPORT_TEXT_BYTES`] on a character boundary.
fn bounded(mut text: String) -> String {
    if text.len() > MAX_REPORT_TEXT_BYTES {
        let mut end = MAX_REPORT_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Runs one claimed job to its report.
async fn run_job<R: HostJobRunner>(
    shared: Arc<Shared>,
    runner: Arc<R>,
    slot: OwnedSemaphorePermit,
    host_job: HostJob,
    sealed_secrets: Option<String>,
    claimed_at: tokio::time::Instant,
) {
    let job_id = host_job.job.id.clone();
    let fence = host_job.fence;
    let lease_secs = host_job.lease_secs;
    let cancel = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));
    {
        let mut held = lock(&shared.held);
        if shared.abandoned.load(Ordering::SeqCst) {
            // The client stopped between the claim and here.
            lost.store(true, Ordering::SeqCst);
            cancel.store(true, Ordering::SeqCst);
        }
        held.insert(
            job_id.clone(),
            HeldJob {
                fence,
                cancel: Arc::clone(&cancel),
                lost: Arc::clone(&lost),
            },
        );
    }
    let opened = match sealed_secrets.as_deref() {
        None => Ok(Vec::new()),
        Some(sealed) => open_secrets(&shared.box_secret, &shared.node_id, &job_id, fence, sealed),
    };
    let result = match opened {
        Err(_) => {
            tracing::warn!(%job_id, fence, "job secrets do not open with this node's box key");
            Err(HostJobError::new(
                SECRETS_UNOPENABLE,
                "the job's secrets do not open with this host's enrolled key",
            ))
        }
        Ok(entries) => {
            let ctx = HostJobCtx::new(
                host_job,
                HandedSecrets::new(entries),
                Arc::clone(&cancel),
                Arc::clone(&shared),
            );
            let _renewer = AbortOnDrop(vec![tokio::spawn(renew_loop(
                Arc::clone(&shared),
                job_id.clone(),
                fence,
                claimed_at + Duration::from_secs(lease_secs).min(MAX_LEASE),
                Arc::clone(&cancel),
                Arc::clone(&lost),
            ))]);
            runner.run(ctx).await
        }
    };
    if lost.load(Ordering::SeqCst) {
        tracing::warn!(%job_id, fence, "discarding the result of a job whose fence was lost");
    } else {
        report(&shared, &job_id, fence, result, &lost).await;
    }
    shared.release(&job_id, fence);
    drop(slot);
}

/// Extends a job's lease every third of it. A `lost` answer cancels the
/// job; `cancel` from the operator asks the runner to stop.
///
/// `confirmed_until` is when the lease the owner last confirmed runs out:
/// the claim's, then each `renewed` (counted from when its renew was
/// sent). Once it passes without a newer confirmation, whether the host is
/// disconnected or the owner leaves renewals unanswered, the job is marked
/// lost and cancelled: the owner may already have handed it elsewhere.
async fn renew_loop(
    shared: Arc<Shared>,
    job_id: String,
    fence: i64,
    mut confirmed_until: tokio::time::Instant,
    cancel: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
) {
    let lease = confirmed_until.saturating_duration_since(tokio::time::Instant::now());
    let every = Duration::from_millis(
        u64::try_from(lease.as_millis() / 3)
            .unwrap_or(u64::MAX)
            .max(500),
    );
    loop {
        tokio::time::sleep_until((tokio::time::Instant::now() + every).min(confirmed_until)).await;
        if lost.load(Ordering::SeqCst) {
            return;
        }
        // Disconnected: the next hello resumes the fence, if the lease
        // still holds by then.
        if let Some(conn) = shared.current() {
            let sent_at = tokio::time::Instant::now();
            let answer = conn
                .request(Body::Renew {
                    item: ITEM_JOB.into(),
                    job_id: job_id.clone(),
                    fence,
                    progress: None,
                })
                .await;
            match answer.map(|frame| frame.body) {
                Ok(Body::Renewed {
                    lease_secs,
                    cancel: cancelled,
                }) => {
                    confirmed_until = sent_at + Duration::from_secs(lease_secs).min(MAX_LEASE);
                    if cancelled {
                        tracing::info!(%job_id, fence, "the operator cancelled the job");
                        cancel.store(true, Ordering::SeqCst);
                    }
                }
                Ok(Body::Lost { .. }) => {
                    tracing::warn!(%job_id, fence, "the job's fence is lost; cancelling it");
                    lost.store(true, Ordering::SeqCst);
                    cancel.store(true, Ordering::SeqCst);
                    return;
                }
                _ => {}
            }
        }
        if tokio::time::Instant::now() >= confirmed_until {
            tracing::warn!(%job_id, fence, "the owner did not confirm the job's lease before it ran out; cancelling it");
            lost.store(true, Ordering::SeqCst);
            cancel.store(true, Ordering::SeqCst);
            return;
        }
    }
}

/// Reports a finished job under its fence, waiting for a connection when
/// there is none, until the owner answers or [`REPORT_DEADLINE`] passes.
async fn report(
    shared: &Arc<Shared>,
    job_id: &str,
    fence: i64,
    result: Result<String, HostJobError>,
    lost: &AtomicBool,
) {
    let failed = result.is_err();
    let body = match result {
        Ok(detail) => Body::Complete {
            job_id: job_id.to_string(),
            fence,
            detail: Some(bounded(detail)),
        },
        Err(err) => Body::Fail {
            job_id: job_id.to_string(),
            fence,
            error_kind: bounded(err.kind),
            message: bounded(err.message),
        },
    };
    let deadline = Instant::now() + REPORT_DEADLINE;
    loop {
        if lost.load(Ordering::SeqCst) {
            tracing::warn!(job_id, fence, "the job's fence was lost before its report");
            return;
        }
        let Some(conn) = shared.wait_conn(deadline).await else {
            tracing::warn!(
                job_id,
                fence,
                "no owner connection to report the job; its lease will expire"
            );
            return;
        };
        match conn.request(body.clone()).await.map(|frame| frame.body) {
            Ok(Body::Accepted {}) => {
                tracing::info!(job_id, fence, failed, "reported the job to the owner");
                return;
            }
            Ok(Body::Lost { .. }) => {
                tracing::warn!(job_id, fence, "the owner reports the job's fence lost");
                return;
            }
            Ok(Body::Error { code, message }) if code == "refused" => {
                tracing::warn!(job_id, fence, reason = %message, "the owner refused the job's report");
                return;
            }
            _ => {
                if Instant::now() >= deadline {
                    return;
                }
                tokio::time::sleep(shared.timing.backoff_min).await;
            }
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
