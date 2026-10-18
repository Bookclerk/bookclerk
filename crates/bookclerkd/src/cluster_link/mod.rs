//! This daemon as a host node of owner clusters (H-4 design, steps 7-8).
//!
//! Every active `cluster_links` row in this install's database gets one
//! [`HostClient`]: it connects to the owner's node socket under the
//! transport policy and the link's SPKI pin, authenticates with the node
//! keys sealed at enrollment, heartbeats, acks wakes, and resumes held
//! fences across reconnects. Clients start at startup and, with no restart,
//! for a link joined while the daemon runs: at once for a join through this
//! daemon ([`start_link_now`], the SPA's Join wizard and invite tab), and
//! within [`consent::CONSENT_POLL`] for one the CLI recorded. A link that
//! stops being active while its client runs (Leave library, which also
//! deletes its keys) stops the client within [`consent::CONSENT_POLL`];
//! [`leave_link`] tells the owner first (`node.leaving`) and stops the
//! client at once.
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
pub mod consent;
pub mod host_jobs;
pub mod join;
pub mod remote_bindings;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use bookclerk_library::control_plane::{
    current_process_incarnation, leave_cluster_link, list_cluster_links, load_cluster_link,
    load_cluster_link_keys, mark_cluster_link_revoked, remote_job_fence, ClusterLink, NodeSampler,
};
use bookclerk_library::LibraryStore;
use bookclerk_library::{JobKind, JobPayload};
use bookclerk_node_client::{
    ClientExit, ClientTiming, HostClient, HostClientConfig, HostClientHandle, HostJobCtx,
    HostJobError, HostJobRunner,
};
use bookclerk_node_proto::Body;
use tokio::sync::watch;

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
        let lease = chrono::Duration::seconds(i64::try_from(ctx.job.lease_secs).unwrap_or(60));
        let exec = JobExecCtx {
            fence: remote_job_fence(&ctx.job.node_id, &ctx.job.job.id, ctx.job.fence),
            cancel: ctx.cancel_flag(),
            attempt_count: ctx.job.job.attempt,
            dedup_key: String::new(),
            lease_expires_at: Some(chrono::Utc::now() + lease),
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

/// Stops a link's client (through `stop`) once its row is gone or no
/// longer active; returns when `shutdown` turns true.
async fn stop_when_inactive(
    library: LibraryStore,
    cluster_id: String,
    stop: Arc<watch::Sender<bool>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut poll = tokio::time::interval(consent::CONSENT_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            _ = poll.tick() => match load_cluster_link(&library, &cluster_id).await {
                Ok(Some(link)) if link.is_active() => {}
                Ok(_) => {
                    tracing::info!(%cluster_id, "the cluster link is no longer active; stopping its client");
                    let _ = stop.send(true);
                    return;
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
    /// Stops the client and its supervisor.
    stop: Arc<watch::Sender<bool>>,
    /// The client, for `node.leaving`.
    handle: HostClientHandle,
}

/// Running link clients, by owner cluster id.
static RUNNING: LazyLock<Mutex<HashMap<String, RunningLink>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Links whose client could not start (keys unreadable, origin refused):
/// the poll does not retry them, so the failure is logged once. A join
/// through this daemon clears the entry.
static FAILED: LazyLock<Mutex<std::collections::HashSet<String>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// The failed set, recovering from a poisoned lock.
fn failed() -> std::sync::MutexGuard<'static, std::collections::HashSet<String>> {
    FAILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A link's entry in the failed set: a new enrollment (a new node id) is
/// tried again.
fn failure_key(link: &ClusterLink) -> String {
    format!("{}\0{}", link.owner_cluster_id, link.remote_node_id)
}

/// Serial for the next started client.
static NEXT_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The running table, recovering from a poisoned lock (each critical
/// section is one map operation).
fn running() -> std::sync::MutexGuard<'static, HashMap<String, RunningLink>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// True while a client for `cluster_id` runs in this process.
#[must_use]
pub fn is_link_running(cluster_id: &str) -> bool {
    running().contains_key(cluster_id)
}

/// Platform plugins this install bundles, by key (payload digests). A
/// failed discovery approves no platform plugin automatically.
async fn platform_payloads(state: &Arc<AppState>) -> BTreeMap<String, String> {
    let config = state.config.read().await.clone();
    tokio::task::spawn_blocking(move || consent::platform_payloads(&config))
        .await
        .unwrap_or_default()
}

/// Starts the client, supervisor and inactive-watcher of one active link,
/// unless one already runs. Answers false when the link's keys cannot be
/// read or its origin fails the transport policy (fail closed: it is not
/// dialed).
async fn start_link(
    state: &Arc<AppState>,
    link: ClusterLink,
    platform_payloads: &BTreeMap<String, String>,
) -> bool {
    let cluster_id = link.owner_cluster_id.clone();
    if is_link_running(&cluster_id) {
        return true;
    }
    let failure_key = failure_key(&link);
    let library = state.library_snapshot().await;
    let keys = match load_cluster_link_keys(&library, &link).await {
        Ok(keys) => keys,
        Err(err) => {
            tracing::error!(%cluster_id, error = %err, "cluster link keys unavailable; not connecting");
            failed().insert(failure_key);
            return false;
        }
    };
    let files_dir = state.config.read().await.paths().files_dir.clone();
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
            failed().insert(failure_key);
            return false;
        }
    };
    let (stop, shutdown) = watch::channel(false);
    let stop = Arc::new(stop);
    let serial = NEXT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut table = running();
        if table.contains_key(&cluster_id) {
            // Another start won the race; this one never ran.
            return true;
        }
        table.insert(
            cluster_id.clone(),
            RunningLink {
                serial,
                stop: Arc::clone(&stop),
                handle: handle.clone(),
            },
        );
    }
    tracing::info!(%cluster_id, "starting the cluster link client");
    tokio::spawn(stop_when_inactive(
        library.clone(),
        cluster_id.clone(),
        Arc::clone(&stop),
        shutdown.clone(),
    ));
    tokio::spawn(
        consent::LinkSupervisor {
            library: library.clone(),
            files_dir,
            handle,
            catalog,
            platform_payloads: platform_payloads.clone(),
        }
        .run(shutdown.clone()),
    );
    tokio::spawn(async move {
        let exit = client.run(shutdown).await;
        let _ = stop.send(true);
        {
            let mut table = running();
            if table
                .get(&cluster_id)
                .is_some_and(|run| run.serial == serial)
            {
                table.remove(&cluster_id);
            }
        }
        if exit == ClientExit::Revoked {
            for attempt in 0..5u32 {
                // A link this install already left stays `left`.
                match mark_cluster_link_revoked(&library, &cluster_id).await {
                    Ok(_) => break,
                    Err(err) => {
                        tracing::warn!(%cluster_id, error = %err, "could not mark the cluster link revoked");
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                }
            }
        }
    });
    true
}

/// Starts clients for every active link that has none running.
async fn start_active_links(state: &Arc<AppState>, payloads: &BTreeMap<String, String>) {
    let library = state.library_snapshot().await;
    let links = match list_cluster_links(&library).await {
        Ok(links) => links,
        Err(err) => {
            tracing::warn!(error = %err, "could not read cluster links");
            return;
        }
    };
    for link in links.into_iter().filter(ClusterLink::is_active) {
        if !is_link_running(&link.owner_cluster_id) && !failed().contains(&failure_key(&link)) {
            start_link(state, link, payloads).await;
        }
    }
}

/// Starts a client for `cluster_id` now (a join through this daemon), with
/// no restart. Answers whether one runs.
pub async fn start_link_now(state: &Arc<AppState>, cluster_id: &str) -> bool {
    let library = state.library_snapshot().await;
    let Ok(Some(link)) = load_cluster_link(&library, cluster_id).await else {
        return false;
    };
    if !link.is_active() {
        return false;
    }
    failed().remove(&failure_key(&link));
    let payloads = platform_payloads(state).await;
    start_link(state, link, &payloads).await
}

/// How long Leave library waits for the owner to answer `node.leaving`.
pub const LEAVE_NOTICE_TIMEOUT: Duration = Duration::from_secs(10);

/// Leave library: tells the owner (`node.leaving`, through the running
/// client or, when none runs here, a short connection with the link's
/// keys), then marks the link `left`, deletes its node keys, and stops the
/// client. Answers the link as it now stands and whether the owner
/// accepted the notice; `None` when there is no such link.
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
    let handle = running().get(cluster_id).map(|run| run.handle.clone());
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
    let left = leave_cluster_link(&library, cluster_id).await?;
    if let Some(run) = running().remove(cluster_id) {
        let _ = run.stop.send(true);
    }
    Ok(left.map(|left| (left, notified)))
}

/// Starts one host client per active cluster link, then keeps starting
/// clients for links joined later (by the CLI, which records the link in
/// the database this daemon reads) within [`consent::CONSENT_POLL`].
pub fn spawn_cluster_links(state: Arc<AppState>) {
    tokio::spawn(async move {
        let payloads = platform_payloads(&state).await;
        start_active_links(&state, &payloads).await;
        let mut poll = tokio::time::interval(consent::CONSENT_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        poll.tick().await;
        loop {
            poll.tick().await;
            start_active_links(&state, &payloads).await;
        }
    });
}

#[cfg(test)]
mod tests;
