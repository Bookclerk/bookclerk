//! This daemon as a host node of owner clusters (H-4 design, steps 7-8).
//!
//! Every active `cluster_links` row in this install's database gets one
//! [`HostClient`]: it connects to the owner's node socket under the
//! transport policy and the link's SPKI pin, authenticates with the node
//! keys sealed at enrollment, heartbeats, acks the wakes a subscriber
//! received (one none receives stays unacked on the owner), and resumes
//! held fences across reconnects. A database swap on config reload stops
//! every client and starts one per active link of the new store, so no
//! client keeps dialing with the previous store's links and keys. A link
//! joined while the daemon runs gets a client at once for a join through
//! this daemon ([`start_link_now`], the SPA's Join wizard and invite tab),
//! and within [`consent::CONSENT_POLL`] for one the CLI recorded. A link
//! that stops being active while its client runs (Leave library or
//! `bookclerk clusters leave`, which also delete its keys) stops the client
//! within [`consent::CONSENT_POLL`]; [`leave_link`] tells the owner first
//! (`node.leaving`) and stops the client at once. A link re-enrolled as
//! another node (`bookclerk clusters join` again) stops the client of the
//! previous node the same way, and once it and its consent supervisor have
//! exited a client is started for the new node.
//!
//! Each link also runs a [`consent::LinkSupervisor`]: it reads what the
//! owner deployed here, records each plugin's request in
//! `remote_plugin_consents`, materializes and configures only what this
//! host's operator approved (`bookclerk clusters approve`), and tells the
//! client which plugin keys it may claim jobs for. The client sends that
//! set with every claim, so the owner never hands it a job for a plugin
//! this host has not consented to; a revoke cancels and gives back held
//! jobs of that plugin.
//!
//! A claimed job runs through a [`JobTransport`] ([`TransportRunner`]) with
//! the owner's handed secrets and token write-back on [`JobExecCtx::host`]
//! and the guest's `CONFIG`, `SECRETS` and store credentials on
//! [`JobExecCtx::host_bindings`] ([`remote_bindings::host_job_bindings`]).
//! The transport is [`host_jobs::HostJobTransport`]: it spawns the owner's
//! plugin jailed under the consented grant and returns a scan's rows in
//! fenced results batches, or streams an acquire's files to the owner
//! (step 9). Scans and acquires run on a host ([`HOST_RUNNABLE_KINDS`]).
//! The in-process transport refuses host jobs outright, since it would run
//! them against this install's own library.
//!
//! A close with 4403 (this node revoked) marks the link `revoked`, cancels
//! held work, and stops that client; the link is not dialed again.

pub mod api;
pub mod catalog_sync;
pub mod consent;
pub mod host_jobs;
pub mod join;
pub mod remote_bindings;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use bookclerk_library::control_plane::{
    current_process_incarnation, leave_cluster_link, list_cluster_links, load_cluster_link,
    load_cluster_link_keys, mark_cluster_link_revoked, remote_job_fence, ClusterLink, NodeSampler,
};
use bookclerk_library::{JobKind, JobPayload, LibraryStore};
use bookclerk_node_client::{
    ClientExit, ClientTiming, HostClient, HostClientConfig, HostClientHandle, HostJobCtx,
    HostJobError, HostJobRunner,
};
use bookclerk_node_proto::Body;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::api::AppState;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

use self::remote_bindings::{host_job_bindings, RemoteCatalog};

/// Jobs a host client holds at once.
pub const HOST_MAX_JOBS: usize = 2;

/// Job kinds a host claims: a scan returns its rows in fenced results
/// batches, an acquire streams its files to the owner.
pub const HOST_RUNNABLE_KINDS: &[&str] = &["scan", "acquire"];

/// Error kind for a handed job this daemon cannot decode.
pub const INVALID_HANDED_JOB: &str = "invalid_job";

/// Runs jobs an owner cluster handed this host through a [`JobTransport`],
/// the same path the daemon's own workers take.
pub struct TransportRunner<T> {
    /// The job transport.
    transport: T,
    /// The link's catalog of consented, fetched instances.
    catalog: Arc<RemoteCatalog>,
}

impl<T> TransportRunner<T> {
    /// A runner over `transport` for the instances in `catalog`.
    pub fn new(transport: T, catalog: Arc<RemoteCatalog>) -> Self {
        Self { transport, catalog }
    }
}

impl<T: JobTransport + 'static> HostJobRunner for TransportRunner<T> {
    async fn run(&self, ctx: HostJobCtx) -> Result<String, HostJobError> {
        let invalid = |detail: &str| HostJobError::new(INVALID_HANDED_JOB, detail);
        let kind = JobKind::parse(&ctx.job.job.kind).ok_or_else(|| invalid("unknown job kind"))?;
        let payload: JobPayload = serde_json::from_value(ctx.job.job.payload.clone())
            .map_err(|_| invalid("malformed job payload"))?;
        let command =
            JobCommand::from_parts(kind, payload).map_err(|err| invalid(&err.to_string()))?;
        // Fail closed: no consented, fetched instance, no run.
        let bindings = Arc::new(host_job_bindings(&self.catalog, &ctx)?);
        let exec = JobExecCtx {
            fence: remote_job_fence(&ctx.job.node_id, &ctx.job.job.id, ctx.job.fence),
            cancel: ctx.cancel_flag(),
            attempt_count: ctx.job.job.attempt,
            dedup_key: String::new(),
            // The client's count of the lease, from before the claim was
            // sent: a guest honouring it stops before the fence can lapse.
            lease_expires_at: Some(chrono::DateTime::<chrono::Utc>::from(
                ctx.job.lease_deadline,
            )),
            checkpoint: command.payload.checkpoint.clone(),
            invocation_sequence: command.payload.invocation_sequence.unwrap_or(1).max(1),
            host: None,
            host_bindings: Some(bindings),
        };
        let cancel = exec.cancel.clone();
        let exec = JobExecCtx {
            host: Some(Arc::new(ctx)),
            ..exec
        };
        self.transport.execute(command, exec).await.map_err(|err| {
            let kind = if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                "cancelled"
            } else {
                "handler"
            };
            HostJobError::new(kind, err.to_string())
        })
    }
}

/// Why [`stop_when_inactive`] stopped a link's client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkStop {
    /// The store's shutdown fired (a database swap or daemon exit).
    Shutdown,
    /// The link's row is gone or no longer active.
    Inactive,
    /// The link is active but enrolled as another node, so the client runs
    /// with an identity and keys the link no longer names.
    Replaced,
}

/// Stops a link's client (through `stop`) once its row is gone, no longer
/// active, or enrolled as another node than `node_id` (the one the client
/// runs as), or when `shutdown` (the store's) turns true.
///
/// # Returns
///
/// Why the client was stopped. On [`LinkStop::Replaced`] and
/// [`LinkStop::Inactive`] the caller lets the supervisor start the
/// cluster's next enrollment (the new node, or a later join).
async fn stop_when_inactive(
    library: LibraryStore,
    cluster_id: String,
    node_id: String,
    stop: Arc<watch::Sender<bool>>,
    mut shutdown: watch::Receiver<bool>,
) -> LinkStop {
    let mut poll = tokio::time::interval(consent::CONSENT_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = stop.send(true);
                    return LinkStop::Shutdown;
                }
            }
            _ = poll.tick() => match load_cluster_link(&library, &cluster_id).await {
                Ok(Some(link)) if link.is_active() && link.remote_node_id == node_id => {}
                Ok(Some(link)) if link.is_active() => {
                    tracing::info!(
                        %cluster_id,
                        previous_node_id = %node_id,
                        node_id = %link.remote_node_id,
                        "the cluster link was re-enrolled as another node; restarting its client"
                    );
                    let _ = stop.send(true);
                    return LinkStop::Replaced;
                }
                Ok(_) => {
                    tracing::info!(%cluster_id, "the cluster link is no longer active; stopping its client");
                    let _ = stop.send(true);
                    return LinkStop::Inactive;
                }
                // A failed read is not a departure; the next poll retries.
                Err(err) => tracing::warn!(%cluster_id, error = %err, "could not re-read the cluster link"),
            },
        }
    }
}

/// One running link client in this process.
struct RunningLink {
    /// Distinguishes this run from a later one for the same link.
    serial: u64,
    /// The node the client runs as, so a join can wait for its own
    /// enrollment's client rather than the one it replaced.
    node_id: String,
    /// Stops the client (and, through it, its consent supervisor).
    stop: Arc<watch::Sender<bool>>,
    /// The client: Leave library's `node.leaving` and the operator's
    /// requests to the owner ([`link_handle`]) go through it.
    handle: HostClientHandle,
}

/// Running link clients, by owner cluster id.
static RUNNING: LazyLock<Mutex<HashMap<String, RunningLink>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Serial for the next started client.
static NEXT_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The running table, recovering from a poisoned lock (each critical
/// section is one map operation).
fn running() -> std::sync::MutexGuard<'static, HashMap<String, RunningLink>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// True while a client for `cluster_id` runs in this process (tests).
#[cfg(test)]
#[must_use]
pub fn is_link_running(cluster_id: &str) -> bool {
    running().contains_key(cluster_id)
}

/// The running client of the link to owner cluster `cluster_id` as
/// `node_id` (the node the link names now), so operator requests
/// (`POST /api/config/clusters/{id}/scan`) reach the owner over that
/// enrollment's socket, never one a re-join replaced.
pub(crate) fn link_handle(cluster_id: &str, node_id: &str) -> Option<HostClientHandle> {
    running()
        .get(cluster_id)
        .filter(|run| run.node_id == node_id)
        .map(|run| run.handle.clone())
}

/// True while a client for `cluster_id` runs in this process as
/// `node_id` (a client of an enrollment a re-join replaced does not count).
fn is_node_running(cluster_id: &str, node_id: &str) -> bool {
    running()
        .get(cluster_id)
        .is_some_and(|run| run.node_id == node_id)
}

/// Links whose client the transport policy refused: the supervisor's poll
/// does not retry them, so the failure is logged once. A join through this
/// daemon clears the entry, and any new enrollment (a new node id, the
/// CLI's included) is tried afresh.
static FAILED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// The failed set, recovering from a poisoned lock.
fn failed() -> std::sync::MutexGuard<'static, HashSet<String>> {
    FAILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A link's entry in the failed set: a new enrollment (a new node id) is
/// tried again.
fn failure_key(link: &ClusterLink) -> String {
    format!("{}\0{}", link.owner_cluster_id, link.remote_node_id)
}

/// Bumped by [`start_link_now`] so every running supervisor starts the
/// links it has no client for at once, instead of at its next poll.
static JOINED: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// How long [`start_link_now`] waits for the supervisor to start a client:
/// longer than [`consent::CONSENT_POLL`], within which a replaced
/// enrollment's client is stopped.
pub(super) const START_NOW_TIMEOUT: Duration = Duration::from_secs(12);

/// Starts a client for `cluster_id` now (a join through this daemon), with
/// no restart: the supervisor started by [`spawn_cluster_links`] is woken
/// to start the link, and this waits up to [`START_NOW_TIMEOUT`] for a
/// client running as the link's current node. A client of an enrollment
/// the join replaced is stopped first (within [`consent::CONSENT_POLL`]),
/// so a join over a still-active link can take that long.
///
/// # Returns
///
/// Whether a client for the link's current enrollment runs.
pub async fn start_link_now(state: &Arc<AppState>, cluster_id: &str) -> bool {
    let library = state.library_snapshot().await;
    let Ok(Some(link)) = load_cluster_link(&library, cluster_id).await else {
        return false;
    };
    if !link.is_active() {
        return false;
    }
    failed().remove(&failure_key(&link));
    JOINED.send_modify(|joined| *joined = joined.wrapping_add(1));
    let deadline = tokio::time::Instant::now() + START_NOW_TIMEOUT;
    while !is_node_running(cluster_id, &link.remote_node_id) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

/// How long Leave library waits for the owner to answer `node.leaving`.
pub const LEAVE_NOTICE_TIMEOUT: Duration = Duration::from_secs(10);

/// Leave library: tells the owner (`node.leaving`, through the running
/// client of the node the link names or, when none runs here as that node,
/// a short connection with the link's keys), then marks that node's link
/// `left`, deletes its node keys, and stops the client. A link a join
/// enrolled as another node meanwhile is not left (an error).
///
/// # Returns
///
/// The link as it now stands and whether the owner accepted the notice;
/// `None` when there is no such link.
///
/// # Errors
///
/// Returns the read or write error of the local leave.
pub async fn leave_link(
    state: &Arc<AppState>,
    cluster_id: &str,
) -> bookclerk_library::Result<Option<(ClusterLink, bool)>> {
    let library = state.library_snapshot().await;
    let Some(link) = load_cluster_link(&library, cluster_id).await? else {
        return Ok(None);
    };
    // Only a client running as the node the link names may say it leaves;
    // one of an enrollment a re-join replaced would revoke the wrong node.
    let handle = link_handle(cluster_id, &link.remote_node_id);
    let notified = if !link.is_active() {
        false
    } else if let Some(handle) = handle {
        matches!(
            tokio::time::timeout(LEAVE_NOTICE_TIMEOUT, handle.request(Body::NodeLeaving {})).await,
            Ok(Some(frame)) if matches!(frame.body, Body::Accepted {})
        )
    } else {
        match load_cluster_link_keys(&library, &link).await {
            Ok(keys) => {
                let files_dir = state.config.read().await.paths().files_dir.clone();
                bookclerk_node_client::announce_leaving(
                    link.clone(),
                    keys,
                    env!("CARGO_PKG_VERSION"),
                    Box::new(NodeSampler::new(&files_dir)),
                    LEAVE_NOTICE_TIMEOUT,
                )
                .await
            }
            Err(_) => false,
        }
    };
    // Only the node the owner was told about is left.
    let left = leave_cluster_link(&library, cluster_id, Some(&link.remote_node_id)).await?;
    // A client already running for a newer join of this library is kept.
    let run = {
        let mut running = running();
        let ours = running
            .get(cluster_id)
            .is_some_and(|run| run.node_id == link.remote_node_id);
        if ours {
            running.remove(cluster_id)
        } else {
            None
        }
    };
    if let Some(run) = run {
        let _ = run.stop.send(true);
    }
    Ok(left.map(|left| (left, notified)))
}

/// How long a stopped host client may take to exit after a database swap
/// before its task is aborted.
const CLIENT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Waits up to [`CLIENT_STOP_TIMEOUT`] for a stopped `task` to exit, then
/// aborts it.
async fn join_or_abort(task: JoinHandle<()>) {
    // The guard aborts a task that did not stop in time, and also one
    // whose wait is itself dropped (an aborted caller).
    let mut guard = AbortOnDrop(Some(task));
    if let Some(task) = guard.0.as_mut() {
        let _ = tokio::time::timeout(CLIENT_STOP_TIMEOUT, task).await;
    }
}

/// A task aborted when its guard drops (a no-op once it finished), so a
/// task spawned for a link's client goes with that client's task when
/// [`join_or_abort`] aborts it.
struct AbortOnDrop<T>(Option<JoinHandle<T>>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// Starts the cluster-link supervisor: one host client per active link of
/// the live library, restarted on the new store after every database swap
/// ([`AppState::library_swaps`]).
pub fn spawn_cluster_links(state: Arc<AppState>) {
    tokio::spawn(supervise_cluster_links(state));
}

/// First delay before the active links are read again after a failed read.
const LINK_RETRY_MIN: Duration = Duration::from_secs(5);

/// Longest delay between attempts to read the active links again.
const LINK_RETRY_MAX: Duration = Duration::from_secs(300);

/// Runs the clients of the live library's active links until the library
/// is swapped, then stops them (waiting for each to exit, so the same
/// node never dials twice) and starts the new store's set.
///
/// Links joined while it runs are started at once for a join through this
/// daemon ([`start_link_now`]) and within [`consent::CONSENT_POLL`] for one
/// the CLI recorded. When the links or a link's keys could not be read,
/// the read is retried with a growing delay ([`LINK_RETRY_MIN`] to
/// [`LINK_RETRY_MAX`]) and only the links not yet running are started, so a
/// transient database error at startup does not leave an enrolled node
/// undialed. A link whose client stopped because it was re-enrolled as
/// another node is started again, as the new node, once that client has
/// exited.
async fn supervise_cluster_links(state: Arc<AppState>) {
    let mut swaps = state.library_swaps.subscribe();
    let mut joins = JOINED.subscribe();
    loop {
        // Mark the current swap seen before reading the library, so a swap
        // published after this read always wakes the wait below.
        swaps.borrow_and_update();
        let library = state.library_snapshot().await;
        let (stop, shutdown) = watch::channel(false);
        // Clusters whose client exited for anything but a swap (re-enrolled,
        // left, inactive or revoked), so a later enrollment can start.
        let (relink, mut relinks) = mpsc::unbounded_channel();
        let mut running = BTreeSet::new();
        let mut clients = Vec::new();
        let mut retry = LINK_RETRY_MIN;
        let swapped = loop {
            joins.borrow_and_update();
            let complete = start_clients(
                &state,
                &library,
                &shutdown,
                &relink,
                &mut running,
                &mut clients,
            )
            .await;
            let wait = if complete {
                consent::CONSENT_POLL
            } else {
                retry
            };
            tokio::select! {
                changed = swaps.changed() => break changed.is_ok(),
                Some(cluster_id) = relinks.recv() => {
                    running.remove(&cluster_id);
                }
                Ok(()) = joins.changed() => {}
                () = tokio::time::sleep(wait) => {
                    if !complete {
                        retry = (retry * 2).min(LINK_RETRY_MAX);
                    }
                }
            }
        };
        let _ = stop.send(true);
        for client in clients {
            join_or_abort(client).await;
        }
        if !swapped {
            return;
        }
        tracing::info!("database swapped; restarting cluster links on the new store");
    }
}

/// Starts one host client per active link in `library` that is not yet in
/// `running`, each stopping when `shutdown` turns true, and adds it to
/// `running` and `clients`. A client stopped because its link was
/// re-enrolled as another node sends its cluster id on `relink` once it
/// and its consent supervisor have exited; one stopped because its link
/// is no longer active, or revoked, sends it too, so a later join of the
/// same library is started. A link the transport policy refused is skipped
/// until it is enrolled again.
///
/// # Returns
///
/// `false` when the links or a link's keys could not be read, so the
/// caller should try again; `true` when every active link was handled
/// (started, or refused for a reason a retry would not change).
async fn start_clients(
    state: &Arc<AppState>,
    library: &LibraryStore,
    shutdown: &watch::Receiver<bool>,
    relink: &mpsc::UnboundedSender<String>,
    running: &mut BTreeSet<String>,
    clients: &mut Vec<JoinHandle<()>>,
) -> bool {
    let links = match list_cluster_links(library).await {
        Ok(links) => links,
        Err(err) => {
            tracing::warn!(error = %err, "could not read cluster links; retrying");
            return false;
        }
    };
    let mut complete = true;
    let pending: Vec<_> = {
        let failed = failed();
        links
            .into_iter()
            .filter(|link| {
                link.is_active()
                    && !running.contains(&link.owner_cluster_id)
                    && !failed.contains(&failure_key(link))
            })
            .collect()
    };
    if pending.is_empty() {
        return complete;
    }
    let (files_dir, config) = {
        let cfg = state.config.read().await;
        (cfg.paths().files_dir.clone(), cfg.clone())
    };
    // A failed discovery approves no platform plugin automatically.
    let platform_payloads: std::collections::BTreeMap<String, String> =
        tokio::task::spawn_blocking(move || consent::platform_payloads(&config))
            .await
            .unwrap_or_default();
    for link in pending {
        let keys = match load_cluster_link_keys(library, &link).await {
            Ok(keys) => keys,
            Err(err) => {
                // Fail closed: without its keys the link is not dialed.
                tracing::error!(
                    cluster_id = %link.owner_cluster_id,
                    error = %err,
                    "cluster link keys unavailable; not connecting yet"
                );
                complete = false;
                continue;
            }
        };
        let cluster_id = link.owner_cluster_id.clone();
        let node_id = link.remote_node_id.clone();
        let failure = failure_key(&link);
        let config = HostClientConfig {
            link,
            keys,
            software_version: env!("CARGO_PKG_VERSION").into(),
            incarnation: current_process_incarnation().into(),
            max_jobs: HOST_MAX_JOBS,
            claim_kinds: HOST_RUNNABLE_KINDS
                .iter()
                .map(|kind| (*kind).to_string())
                .collect(),
            // The supervisor re-reads deployments on each connection first.
            claims_after_refresh: true,
            timing: ClientTiming::default(),
        };
        let catalog = Arc::new(RemoteCatalog::default());
        let runner = TransportRunner::new(
            host_jobs::HostJobTransport::new(host_jobs::GuestHost::new(
                Arc::clone(state),
                files_dir.clone(),
            )),
            Arc::clone(&catalog),
        );
        let (client, handle) = match HostClient::new(
            config,
            runner,
            Box::new(NodeSampler::new(&files_dir)),
        ) {
            Ok(started) => started,
            Err(err) => {
                tracing::error!(%cluster_id, error = %err, "cluster link refused by the transport policy");
                failed().insert(failure);
                continue;
            }
        };
        running.insert(cluster_id.clone());
        // The link's consent supervisor stops with its client: on a swap,
        // when the client is revoked, or when its task is aborted (the
        // guard aborts it with the task).
        let (link_stop, link_shutdown) = watch::channel(false);
        let supervisor = AbortOnDrop(Some(tokio::spawn(
            consent::LinkSupervisor {
                library: library.clone(),
                files_dir: files_dir.clone(),
                handle: handle.clone(),
                catalog,
                platform_payloads: platform_payloads.clone(),
            }
            .run(link_shutdown.clone()),
        )));
        // The link's catalog sync (a branch sharing its books with this
        // library) stops with its client the same way.
        let catalog_sync = AbortOnDrop(Some(tokio::spawn(
            catalog_sync::CatalogSync {
                library: library.clone(),
                cluster_id: cluster_id.clone(),
                handle: handle.clone(),
                shelf: catalog_sync::DaemonShelf {
                    state: Arc::clone(state),
                },
            }
            .run(link_shutdown),
        )));
        // The client stops on a swap (forwarded from `shutdown`), once its
        // link is no longer active, once it is enrolled as another node, or
        // at once on Leave library ([`leave_link`]).
        let (client_stop, client_shutdown) = watch::channel(false);
        let client_stop = Arc::new(client_stop);
        let serial = NEXT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        running_insert(
            &cluster_id,
            RunningLink {
                serial,
                node_id: node_id.clone(),
                stop: Arc::clone(&client_stop),
                handle,
            },
        );
        // Drops the entry however the task ends, an abort after a swap
        // included.
        let listed = RunningEntry {
            cluster_id: cluster_id.clone(),
            serial,
        };
        let watcher = AbortOnDrop(Some(tokio::spawn(stop_when_inactive(
            library.clone(),
            cluster_id.clone(),
            node_id.clone(),
            client_stop,
            shutdown.clone(),
        ))));
        let library = library.clone();
        let relink = relink.clone();
        clients.push(tokio::spawn(async move {
            let exit = client.run(client_shutdown).await;
            drop(listed);
            let _ = link_stop.send(true);
            // Both exit before a client for a new enrollment can start, so
            // one cluster never has two consent supervisors (or two catalog
            // syncs).
            let mut supervisor = supervisor;
            if let Some(supervisor) = supervisor.0.take() {
                join_or_abort(supervisor).await;
            }
            let mut catalog_sync = catalog_sync;
            if let Some(catalog_sync) = catalog_sync.0.take() {
                join_or_abort(catalog_sync).await;
            }
            let mut watcher = watcher;
            if exit != ClientExit::Revoked {
                if let Some(stopped) = watcher.0.as_mut() {
                    // A stop for a swap ends with the store; any other lets
                    // the supervisor start the cluster's next enrollment.
                    if matches!(stopped.await, Ok(LinkStop::Replaced | LinkStop::Inactive)) {
                        let _ = relink.send(cluster_id);
                    }
                }
                return;
            }
            drop(watcher);
            // The link belongs to the store it was read from. Only the
            // enrollment this client ran as, and only while active, is
            // revoked: a re-enrollment recorded meanwhile keeps its new
            // link, and a Leave library racing the close stays `left`.
            for attempt in 0..5u32 {
                match mark_cluster_link_revoked(&library, &cluster_id, &node_id).await {
                    Ok(true) => {
                        crate::live::poke(bookclerk_library::control_plane::LiveTopic::Clusters);
                        break;
                    }
                    Ok(false) => {
                        tracing::info!(%cluster_id, %node_id, "revoked enrollment was already left or replaced; keeping the link as it is");
                        break;
                    }
                    Err(err) => {
                        tracing::warn!(%cluster_id, error = %err, "could not mark the cluster link revoked");
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                }
            }
            // The supervisor re-reads the link before it dials: a revoked or
            // left one is started only once joined again, and one still
            // active after failed writes is retried (and refused again).
            let _ = relink.send(cluster_id);
        }));
    }
    complete
}

/// Records `run` as the running client of `cluster_id`. A client still
/// listed for the link (one stopped on a swap or a re-enrollment that has
/// not exited yet) is replaced: Leave library then reaches the new one.
fn running_insert(cluster_id: &str, run: RunningLink) {
    running().insert(cluster_id.to_owned(), run);
}

/// A client's entry in the running table, dropped with the client's task
/// (when it returns, or is aborted).
struct RunningEntry {
    /// The link the entry is for.
    cluster_id: String,
    /// The run that owns the entry.
    serial: u64,
}

impl Drop for RunningEntry {
    /// Drops the entry of `cluster_id` if it is still this run's (a later
    /// client for the link keeps its own).
    fn drop(&mut self) {
        let mut table = running();
        if table
            .get(&self.cluster_id)
            .is_some_and(|run| run.serial == self.serial)
        {
            table.remove(&self.cluster_id);
        }
    }
}

#[cfg(test)]
mod tests;
