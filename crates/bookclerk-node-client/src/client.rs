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
//!    jobs, but only while [`HostClientConfig::claim_kinds`] and the
//!    consented plugin set ([`HostClientHandle::set_consented_plugins`])
//!    are both non-empty; each claim carries both as its filter, so the
//!    owner never hands this host a job of a plugin its operator did not
//!    approve. A job whose plugins are not all consented when it arrives,
//!    or whose consent is revoked while it runs, is cancelled and given
//!    back with `release` (unspent) instead of being reported.
//!    Each job runs on its own task through the [`HostJobRunner`],
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

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bookclerk_library::control_plane::{
    ClusterLink, ClusterLinkKeys, NodeHeartbeat, NodeSampler, NODE_HEARTBEAT_INTERVAL,
};
use bookclerk_node_proto::{
    decode_nonce, open_instance_secrets, open_secrets, seal_secret_update, sign_artifact_request,
    sign_auth, validate_box_public_key, ArtifactRequest, AuthContext, Body, ClaimFilter, Frame,
    HeldFence, SealError, SecretEntry, SecretRef, SecretValue, SigningKey, ARTIFACTS_PATH,
    CLOSE_PROTOCOL, CLOSE_REVOKED, HANDSHAKE_TIMEOUT_SECS, IDLE_TIMEOUT_SECS, ITEM_JOB,
    PING_INTERVAL_SECS, SEND_QUEUE_FRAMES,
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

/// Release reason for a job whose plugins this host's operator has not
/// (or no longer) approved.
pub const AWAITING_HOST_CONSENT: &str = "awaiting_host_consent";

/// Release reason for a job of a kind this host does not run.
pub const KIND_NOT_RUN_HERE: &str = "kind_not_run_here";

/// Largest artifact fetched from the owner (the library blob cap).
pub const MAX_ARTIFACT_BYTES: usize = bookclerk_library::control_plane::MAX_ARTIFACT_BLOB_BYTES;

/// Error kind of the `fail` a host sends for a job the owner granted to a
/// claim that had already timed out here: nothing runs it, so the lease is
/// handed back at once instead of being left to expire.
pub const CLAIM_ABANDONED: &str = "claim_abandoned";

/// Longest lease this host counts on from one `work` or `renewed`; a
/// longer grant is renewed as if it were this long.
pub const MAX_LEASE: Duration = Duration::from_secs(24 * 60 * 60);

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
    /// Job kinds this host runs (`scan`, ...). Empty claims nothing.
    pub claim_kinds: Vec<String>,
    /// When set, a connection claims nothing until
    /// [`HostClientHandle::deployments_refreshed`] names it, so the caller
    /// can re-read what the owner deployed (which may have changed while
    /// this host was away) before work runs against it.
    pub claims_after_refresh: bool,
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
    /// Plugin keys the job runs.
    plugin_keys: Vec<String>,
    /// Asks the runner to stop.
    cancel: Arc<AtomicBool>,
    /// Set when the owner says the fence is lost; the result is discarded.
    lost: Arc<AtomicBool>,
    /// Set when the job must be given back unrun (consent revoked); the
    /// result is discarded and `release` is sent instead.
    release: Arc<Mutex<Option<&'static str>>>,
    /// The lease length the claim granted (capped at [`MAX_LEASE`]).
    grant: Duration,
    /// Where a `resumed` answer hands the job's renew loop its renewal.
    resumed: Arc<ResumeSlot>,
}

/// A `resumed` renewal handed to a job's renew loop, which wakes for it.
#[derive(Default)]
struct ResumeSlot {
    /// The `hello` the owner renewed the lease from, and that lease's
    /// length.
    latest: Mutex<Option<(tokio::time::Instant, Duration)>>,
    /// Wakes the renew loop.
    wake: tokio::sync::Notify,
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
    /// Job kinds this host runs.
    claim_kinds: BTreeSet<String>,
    /// Plugin keys this host's operator consents to now.
    consented: watch::Sender<BTreeSet<String>>,
    /// Fences held, by job id.
    held: Mutex<HashMap<String, HeldJob>>,
    /// The live connection, if any.
    conn: watch::Sender<Option<Conn>>,
    /// Welcomed connections so far.
    connections: watch::Sender<u64>,
    /// Whether claims wait for [`Self::refreshed`] to reach the connection.
    claims_after_refresh: bool,
    /// The newest connection (by its [`Self::connections`] count) whose
    /// deployments the caller re-read.
    refreshed: watch::Sender<u64>,
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

/// Makes each handed secret known to the log and diagnostics scrubber
/// before the runner sees it, so this host redacts store tokens it never
/// held at rest. A value that is not UTF-8 cannot appear in text output
/// verbatim and is skipped.
fn register_for_redaction(entries: &[SecretEntry]) {
    for entry in entries {
        register_value_for_redaction(entry.value.expose());
    }
}

/// Registers one secret value with the scrubber when it is UTF-8 text.
/// A JSON credential (a token bundle) also registers each string inside
/// it, since a log quotes the token field, not the whole document.
fn register_value_for_redaction(value: &[u8]) {
    let Ok(text) = std::str::from_utf8(value) else {
        return;
    };
    bookclerk_config::register_secret(text);
    if let Ok(document) = serde_json::from_str::<serde_json::Value>(text) {
        register_json_strings(&document);
    }
}

/// Registers every string in `value`, at any depth.
fn register_json_strings(value: &serde_json::Value) {
    match value {
        serde_json::Value::String(text) => bookclerk_config::register_secret(text),
        serde_json::Value::Array(items) => items.iter().for_each(register_json_strings),
        serde_json::Value::Object(fields) => fields.values().for_each(register_json_strings),
        _ => {}
    }
}

/// Cancels every job `0` holds when dropped: [`HostClient::run`] keeps one
/// so its jobs are abandoned however it ends, an abort included.
struct AbandonOnDrop<'a>(&'a Shared);

impl Drop for AbandonOnDrop<'_> {
    fn drop(&mut self) {
        self.0.abandon_all();
    }
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

    /// Cancels and marks for release every held job that runs a plugin
    /// outside `consented`.
    fn release_unconsented(&self, consented: &BTreeSet<String>) {
        for (job_id, job) in lock(&self.held).iter() {
            if job.plugin_keys.iter().all(|key| consented.contains(key)) {
                continue;
            }
            let mut release = lock(&job.release);
            if release.is_none() {
                tracing::warn!(%job_id, fence = job.fence, "host consent for the job's plugin was revoked; cancelling and giving it back");
                *release = Some(AWAITING_HOST_CONSENT);
            }
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// True when every plugin in `keys` is consented now (and there is at
    /// least one).
    fn consents_to(&self, keys: &[String]) -> bool {
        let consented = self.consented.borrow();
        !keys.is_empty() && keys.iter().all(|key| consented.contains(key))
    }

    /// The claim filter, when this host would claim anything now.
    fn claim_filter(&self) -> Option<ClaimFilter> {
        let consented = self.consented.borrow();
        if self.claim_kinds.is_empty() || consented.is_empty() {
            return None;
        }
        Some(ClaimFilter {
            kinds: self.claim_kinds.iter().cloned().collect(),
            plugins: consented.iter().cloned().collect(),
        })
    }

    /// Records the job fences the owner renewed while answering a `hello`
    /// sent at `hello_at`, so their renew loops count the renewal as a
    /// confirmation of the lease: `lease_secs` long when the owner said
    /// (capped at [`MAX_LEASE`]), else each job's original grant.
    fn mark_renewed(
        &self,
        renewed: &[HeldFence],
        hello_at: tokio::time::Instant,
        lease_secs: Option<u64>,
    ) {
        let lease = lease_secs.map(|secs| Duration::from_secs(secs).min(MAX_LEASE));
        let held = lock(&self.held);
        for fence in renewed {
            if fence.kind != ITEM_JOB {
                continue;
            }
            if let Some(job) = held.get(&fence.id).filter(|job| job.fence == fence.fence) {
                *lock(&job.resumed.latest) = Some((hello_at, lease.unwrap_or(job.grant)));
                job.resumed.wake.notify_one();
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
        // A refreshed token is new to this process too: scrub it from
        // anything the job prints after this, whether or not it is sent.
        register_value_for_redaction(value);
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

    /// Records that the deployments were re-read on the connection counted
    /// `connection` (a [`Self::connections`] value read before the
    /// request), which opens its claims under
    /// [`HostClientConfig::claims_after_refresh`]. A read made on an
    /// earlier connection opens nothing on a later one.
    pub fn deployments_refreshed(&self, connection: u64) {
        self.shared.refreshed.send_if_modified(|through| {
            let newer = connection > *through;
            if newer {
                *through = connection;
            }
            newer
        });
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

    /// The owner cluster id.
    #[must_use]
    pub fn cluster_id(&self) -> &str {
        &self.shared.cluster_id
    }

    /// Sets the plugin keys this host's operator consents to. Claims carry
    /// them as their filter from the next claim on; a held job that runs a
    /// plugin outside them is cancelled and given back unrun.
    ///
    /// The new set is published before the held jobs are scanned: a job
    /// that is being started concurrently is either already held (and
    /// caught by the scan) or checks consent after it is held and so reads
    /// the new set. Scanning first would leave a gap where a job inserted
    /// after the scan still read the old set and ran.
    pub fn set_consented_plugins(&self, plugins: BTreeSet<String>) {
        self.shared.consented.send_if_modified(|current| {
            let changed = *current != plugins;
            current.clone_from(&plugins);
            changed
        });
        self.shared.release_unconsented(&plugins);
    }

    /// The plugin keys consented to now.
    #[must_use]
    pub fn consented_plugins(&self) -> BTreeSet<String> {
        self.shared.consented.borrow().clone()
    }

    /// Sends `body` on the live connection (waiting up to the request
    /// timeout for one) and answers the owner's frame, or `None` when no
    /// connection answered.
    pub async fn request(&self, body: Body) -> Option<Frame> {
        let timeout = self.shared.timing.request_timeout;
        let conn = self.shared.wait_conn(Instant::now() + timeout).await?;
        conn.request(body).await.ok()
    }

    /// Opens an instance's `SECRETS` sealed to this node's box key
    /// (`instance_config.sealed_secrets`), registering the values with the
    /// log and diagnostics scrubber.
    ///
    /// # Errors
    ///
    /// Returns [`SealError`] when it was not sealed to this node for this
    /// instance and revision.
    pub fn open_instance_secrets(
        &self,
        instance_id: &str,
        revision: i64,
        sealed: &str,
    ) -> Result<SecretValue, SealError> {
        let opened = open_instance_secrets(
            &self.shared.box_secret,
            &self.shared.node_id,
            instance_id,
            revision,
            sealed,
        )?;
        register_value_for_redaction(opened.expose());
        Ok(opened)
    }

    /// Downloads one archive the owner stores (`blob:sha256:<hex>`) from its
    /// artifact endpoint, signing the request with this node's key. The
    /// caller checks the digest.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the request fails or the owner does not
    /// answer 200 with at most [`MAX_ARTIFACT_BYTES`].
    pub async fn fetch_artifact(&self, archive_sha256: &str) -> Result<Vec<u8>, DialError> {
        if archive_sha256.len() != 64 || !archive_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DialError::Http);
        }
        let shared = &self.shared;
        let issued_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let authorization = sign_artifact_request(
            &shared.sign_key,
            &ArtifactRequest {
                cluster_id: &shared.cluster_id,
                node_id: &shared.node_id,
                owner_origin: shared.endpoint.origin(),
                archive_sha256,
                issued_at,
            },
        );
        let (status, body) = shared
            .endpoint
            .get_authorized(
                &format!("{ARTIFACTS_PATH}{archive_sha256}"),
                &authorization,
                MAX_ARTIFACT_BYTES,
            )
            .await?;
        if status != 200 {
            tracing::warn!(status, archive_sha256, "owner refused the artifact request");
            return Err(DialError::Http);
        }
        Ok(body)
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
        let (refreshed, _) = watch::channel(0);
        let (consented, _) = watch::channel(BTreeSet::new());
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
            claim_kinds: config.claim_kinds.into_iter().collect(),
            consented,
            held: Mutex::new(HashMap::new()),
            conn,
            connections,
            claims_after_refresh: config.claims_after_refresh,
            refreshed,
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
    /// Every held job is cancelled as lost when this returns, whatever the
    /// reason, and also when the future is dropped or its task aborted
    /// first, so no job task keeps running for a client that is gone.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> ClientExit {
        let _abandon = AbandonOnDrop(&self.shared);
        self.serve_until_exit(shutdown).await
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
        let hello_at = tokio::time::Instant::now();
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
            let Body::Resumed {
                renewed,
                lost,
                unchecked,
                lease_secs,
            } = resumed.body
            else {
                return Err(SessionEnd::Failed("expected resumed".into()));
            };
            // An unchecked fence is neither renewed nor lost: its renew
            // loop keeps renewing it and that answer decides.
            tracing::info!(
                renewed = renewed.len(),
                lost = lost.len(),
                unchecked = unchecked.len(),
                "owner answered the held fences"
            );
            shared.mark_lost(&lost);
            shared.mark_renewed(&renewed, hello_at, lease_secs);
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
            // The count this connection gets once welcomed below; only this
            // task serves a connection, so nothing else bumps it first.
            let number = *shared.connections.borrow() + 1;
            tasks.push(tokio::spawn(claim_loop(
                Arc::clone(shared),
                conn.clone(),
                number,
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
                            // A request that timed out as its answer came
                            // in dropped its receiver: the answer is then
                            // unsolicited (a late `work` is handed back).
                            Some(answer) => {
                                if let Err(frame) = answer.send(frame) {
                                    unsolicited(shared, conn, frame);
                                }
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

/// Pulls jobs while job slots are free and the host would claim anything
/// (some kind runs here and some plugin is consented). Under
/// [`HostClientConfig::claims_after_refresh`] it first waits for the
/// caller to re-read the deployments on connection `number`. A claim the
/// owner does not answer is retried with backoff; the loop ends only with
/// its connection.
async fn claim_loop<R: HostJobRunner>(
    shared: Arc<Shared>,
    conn: Conn,
    number: u64,
    runner: Arc<R>,
) {
    if shared.claims_after_refresh {
        // Consent and deployments still describe the last connection until
        // the caller re-reads them on this one.
        let mut refreshed = shared.refreshed.subscribe();
        if refreshed
            .wait_for(|through| *through >= number)
            .await
            .is_err()
        {
            return;
        }
    }
    let mut consented = shared.consented.subscribe();
    let mut retry = shared.timing.backoff_min;
    loop {
        if shared.claim_filter().is_none() {
            // Nothing to claim until the operator consents to something.
            if consented.changed().await.is_err() {
                return;
            }
            continue;
        }
        let Ok(slot) = Arc::clone(&shared.job_slots).acquire_owned().await else {
            return;
        };
        // The wait for a slot can outlast a revoke: claim under the consent
        // that holds now.
        let Some(filter) = shared.claim_filter() else {
            drop(slot);
            continue;
        };
        // The owner starts the lease after this; counting from here never
        // overstates it.
        let claimed_at = tokio::time::Instant::now();
        let claimed_wall = std::time::SystemTime::now();
        let answer = conn
            .request(Body::Claim {
                item: ITEM_JOB.into(),
                class: None,
                filter: Some(filter),
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
                        plugin_keys,
                        resumed,
                    },
                ..
            }) if item == ITEM_JOB => {
                tracing::info!(job_id = %job.id, fence, kind = %job.kind, "claimed a job from the owner");
                let host_job = HostJob {
                    node_id: shared.node_id.clone(),
                    job,
                    fence,
                    lease_secs,
                    plugin_keys,
                    lease_deadline: claimed_wall + Duration::from_secs(lease_secs).min(MAX_LEASE),
                    resumed,
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
                // A new consent is worth a claim right away.
                let _ = tokio::time::timeout(
                    Duration::from_secs(retry_after_secs.clamp(1, 300)),
                    consented.changed(),
                )
                .await;
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
    let claim_resumed = host_job.resumed;
    let cancel = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Mutex::new(None));
    let grant = Duration::from_secs(lease_secs).min(MAX_LEASE);
    let resumed = Arc::new(ResumeSlot::default());
    {
        let mut held = lock(&shared.held);
        if shared.abandoned.load(Ordering::SeqCst) {
            // The client stopped between the claim and here: the job is
            // never started, and its lease runs out on the owner.
            tracing::warn!(%job_id, fence, "the client stopped before the job started; dropping it");
            return;
        }
        held.insert(
            job_id.clone(),
            HeldJob {
                fence,
                plugin_keys: host_job.plugin_keys.clone(),
                cancel: Arc::clone(&cancel),
                lost: Arc::clone(&lost),
                release: Arc::clone(&release),
                grant,
                resumed: Arc::clone(&resumed),
            },
        );
    }
    // Checked after the job is held, so a revocation racing the claim is
    // seen either here or by `release_unconsented`.
    if !shared.claim_kinds.contains(&host_job.job.kind) {
        *lock(&release) = Some(KIND_NOT_RUN_HERE);
    } else if !shared.consents_to(&host_job.plugin_keys) {
        *lock(&release) = Some(AWAITING_HOST_CONSENT);
    }
    let refused = *lock(&release);
    if let Some(reason) = refused {
        tracing::warn!(%job_id, fence, reason, "giving back a job this host will not run");
        give_back(&shared, &job_id, fence, claim_resumed, reason, &lost).await;
        shared.release(&job_id, fence);
        drop(slot);
        return;
    }
    let opened = match sealed_secrets.as_deref() {
        None => Ok(Vec::new()),
        Some(sealed) => open_secrets(&shared.box_secret, &shared.node_id, &job_id, fence, sealed),
    };
    if let Ok(entries) = &opened {
        register_for_redaction(entries);
    }
    // Renewals keep running through `report`, which can take up to
    // [`REPORT_DEADLINE`]: a slow report must not let the fence lapse and
    // throw away finished work, nor a fail for secrets that do not open.
    let renewer = (!lost.load(Ordering::SeqCst)).then(|| {
        AbortOnDrop(vec![tokio::spawn(renew_loop(
            Arc::clone(&shared),
            job_id.clone(),
            fence,
            (claimed_at, grant),
            Arc::clone(&cancel),
            Arc::clone(&lost),
            resumed,
        ))])
    });
    let result = match opened {
        // Abandoned (shutdown, revocation) since it was held: never start
        // the runner under a fence whose result is discarded.
        _ if lost.load(Ordering::SeqCst) => Err(HostJobError::new(
            "cancelled",
            "the client stopped before the job started",
        )),
        // The grant ran out before the job could start (a late `work`, a
        // starved runtime): the owner may already have handed the fence to
        // another host, so never start it and drop the result.
        _ if tokio::time::Instant::now() >= claimed_at + grant => {
            tracing::warn!(%job_id, fence, "the job's lease ran out before it started; dropping it");
            lost.store(true, Ordering::SeqCst);
            Err(HostJobError::new(
                "cancelled",
                "the job's lease ran out before it started",
            ))
        }
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
            runner.run(ctx).await
        }
    };
    let given_back = *lock(&release);
    if lost.load(Ordering::SeqCst) {
        tracing::warn!(%job_id, fence, "discarding the result of a job whose fence was lost");
    } else if let Some(reason) = given_back {
        tracing::warn!(%job_id, fence, reason, "discarding the result of a job given back");
        give_back(&shared, &job_id, fence, claim_resumed, reason, &lost).await;
    } else {
        let give_back_on = GiveBackOn {
            release: &release,
            resumed: claim_resumed,
        };
        report(&shared, &job_id, fence, result, &lost, give_back_on).await;
    }
    drop(renewer);
    shared.release(&job_id, fence);
    drop(slot);
}

/// Gives a job back unrun with `release` under its fence. `resumed` is
/// the claim's [`HostJob::resumed`].
async fn give_back(
    shared: &Arc<Shared>,
    job_id: &str,
    fence: i64,
    resumed: bool,
    reason: &'static str,
    lost: &AtomicBool,
) {
    let body = Body::Release {
        job_id: job_id.to_string(),
        fence,
        reason: reason.into(),
        resumed,
    };
    send_final(shared, job_id, fence, body, false, lost, None).await;
}

/// Where a report finds a give-back marked while it waits to be sent.
#[derive(Clone, Copy)]
struct GiveBackOn<'a> {
    /// The job's release marker.
    release: &'a Mutex<Option<&'static str>>,
    /// The claim's [`HostJob::resumed`].
    resumed: bool,
}

/// Extends a job's lease every third of it. A `lost` answer cancels the
/// job; `cancel` from the operator asks the runner to stop.
///
/// The lease the owner last confirmed is the claim's (`grant`, from
/// `claimed_at`), then each `renewed` (counted from when its renew was
/// sent) or `resumed` that lists the fence (counted from its `hello`, for
/// the lease the resume names), whichever is newest. Renewals follow the
/// newest lease's length, and one that is not answered before the lease
/// runs out counts as unanswered. Once it passes without a newer confirmation, whether the host is
/// disconnected or the owner leaves renewals unanswered, the job is marked
/// lost and cancelled: the owner may already have handed it elsewhere.
async fn renew_loop(
    shared: Arc<Shared>,
    job_id: String,
    fence: i64,
    (claimed_at, grant): (tokio::time::Instant, Duration),
    cancel: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    resumed: Arc<ResumeSlot>,
) {
    /// A third of `lease`, at least 500 ms: how often it is renewed.
    fn renew_every(lease: Duration) -> Duration {
        Duration::from_millis(
            u64::try_from(lease.as_millis() / 3)
                .unwrap_or(u64::MAX)
                .max(500),
        )
    }
    // When the newest confirmation was made, starting with the claim: a
    // `resumed` replaces the lease only when its `hello` came after it,
    // since the owner's resume renewal set the lease from then (shorter,
    // if the owner's lease length changed while the host was away).
    let mut confirmed_at = claimed_at;
    let mut confirmed_until = claimed_at + grant;
    let mut every = renew_every(grant);
    loop {
        // A resume wakes the loop at once: its lease may be shorter than
        // the one it was sleeping on.
        let woken_by_resume = tokio::select! {
            () = tokio::time::sleep_until(
                (tokio::time::Instant::now() + every).min(confirmed_until),
            ) => false,
            () = resumed.wake.notified() => true,
        };
        if lost.load(Ordering::SeqCst) {
            return;
        }
        // Disconnected: the next hello resumes the fence, if the lease
        // still holds by then.
        if let Some(conn) = shared.current().filter(|_| !woken_by_resume) {
            let sent_at = tokio::time::Instant::now();
            // An answer that comes after the lease ran out is too late:
            // the owner may already have handed the job elsewhere.
            let answer = tokio::time::timeout_at(
                confirmed_until,
                conn.request(Body::Renew {
                    item: ITEM_JOB.into(),
                    job_id: job_id.clone(),
                    fence,
                    progress: None,
                }),
            )
            .await;
            match answer.map(|answer| answer.map(|frame| frame.body)) {
                Ok(Ok(Body::Renewed {
                    lease_secs,
                    cancel: cancelled,
                })) => {
                    let lease = Duration::from_secs(lease_secs).min(MAX_LEASE);
                    confirmed_at = sent_at;
                    confirmed_until = sent_at + lease;
                    every = renew_every(lease);
                    if cancelled {
                        tracing::info!(%job_id, fence, "the operator cancelled the job");
                        cancel.store(true, Ordering::SeqCst);
                    }
                }
                Ok(Ok(Body::Lost { .. })) => {
                    tracing::warn!(%job_id, fence, "the job's fence is lost; cancelling it");
                    lost.store(true, Ordering::SeqCst);
                    cancel.store(true, Ordering::SeqCst);
                    return;
                }
                _ => {}
            }
        }
        if let Some((hello_at, lease)) = lock(&resumed.latest).take() {
            if hello_at >= confirmed_at {
                confirmed_at = hello_at;
                confirmed_until = hello_at + lease;
                every = renew_every(lease);
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
///
/// The detail or error text is scrubbed of registered secrets first, so a
/// runner error that quotes a handed token never reaches the owner. A
/// give-back marked on `give_back_on` while the report waits (consent
/// revoked during a reconnect) sends `release` instead.
async fn report(
    shared: &Arc<Shared>,
    job_id: &str,
    fence: i64,
    result: Result<String, HostJobError>,
    lost: &AtomicBool,
    give_back_on: GiveBackOn<'_>,
) {
    let failed = result.is_err();
    let body = match result {
        Ok(detail) => Body::Complete {
            job_id: job_id.to_string(),
            fence,
            detail: Some(bounded(bookclerk_config::redact_str(&detail))),
        },
        Err(err) => Body::Fail {
            job_id: job_id.to_string(),
            fence,
            error_kind: bounded(err.kind),
            message: bounded(bookclerk_config::redact_str(&err.message)),
        },
    };
    send_final(
        shared,
        job_id,
        fence,
        body,
        failed,
        lost,
        Some(give_back_on),
    )
    .await;
}

/// Sends a job's final frame (`complete`, `fail` or `release`), waiting for
/// a connection when there is none, until the owner answers or
/// [`REPORT_DEADLINE`] passes. With `give_back_on`, each attempt first
/// checks the release marker and sends `release` once it is set.
async fn send_final(
    shared: &Arc<Shared>,
    job_id: &str,
    fence: i64,
    mut body: Body,
    failed: bool,
    lost: &AtomicBool,
    give_back_on: Option<GiveBackOn<'_>>,
) {
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
        if let Some(on) = give_back_on {
            let marked = *lock(on.release);
            if let Some(reason) = marked.filter(|_| !matches!(body, Body::Release { .. })) {
                tracing::warn!(
                    job_id,
                    fence,
                    reason,
                    "the job was given back before its report went out"
                );
                body = Body::Release {
                    job_id: job_id.to_string(),
                    fence,
                    reason: reason.into(),
                    resumed: on.resumed,
                };
            }
        }
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
