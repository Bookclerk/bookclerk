//! This daemon as a host node of owner clusters (H-4 design, step 7).
//!
//! At startup, every active `cluster_links` row in this install's database
//! gets one [`HostClient`]: it connects to the owner's node socket under
//! the transport policy and the link's SPKI pin, authenticates with the
//! node keys sealed at enrollment, heartbeats, and resumes held fences
//! across reconnects. A database swap on config reload stops every client
//! and starts one per active link of the new store, so no client keeps
//! dialing with the previous store's links and keys. A link enrolled while
//! the daemon runs is picked up at the next start or database swap.
//!
//! Wakes stay unacked on the owner until a host-side handler subscribes
//! to them (host consent, step 8): the client acks only a wake some
//! subscriber received, so none is acknowledged and lost meanwhile.
//!
//! Claimed jobs run through the daemon's own job transport
//! ([`TransportRunner`] over [`InProcessJobTransport`]), with the owner's
//! handed secrets and token write-back on [`JobExecCtx::host`]. Claims stay
//! off ([`HOST_MAX_JOBS`] is zero) until host consent lands (step 8): every
//! job a host may claim runs an owner plugin, and a host runs an owner
//! plugin only after its own operator approves it.
//!
//! A close with 4403 (this node revoked) marks the link `revoked`, cancels
//! held work, and stops that client; the link is not dialed again.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    current_process_incarnation, list_cluster_links, load_cluster_link_keys, remote_job_fence,
    set_cluster_link_state, NodeSampler, CLUSTER_LINK_REVOKED,
};
use bookclerk_library::{JobKind, JobPayload, LibraryStore};
use bookclerk_node_client::{
    ClientExit, ClientTiming, HostClient, HostClientConfig, HostJobCtx, HostJobError,
    HostJobRunner, MAX_LEASE,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::api::AppState;
use crate::job_handler::{InProcessJobTransport, JobCommand, JobExecCtx, JobTransport};

/// Jobs a host client claims at once. Zero until host consent (step 8):
/// nothing on a host may run an owner plugin before its operator approves
/// it, and every job a host is trusted with runs one.
///
/// It must also stay zero until the local job handlers consume
/// [`JobExecCtx::host`] and report over the owner connection: a handler
/// that ignores it runs the job against this host's own state, not the
/// owner's.
pub const HOST_MAX_JOBS: usize = 0;

/// Error kind for an acquire handed to a host before streamed uploads
/// (step 9) give its result a way back to the owner.
pub const UPLOAD_UNAVAILABLE: &str = "upload_unavailable";

/// Error kind for a handed job this daemon cannot decode.
pub const INVALID_HANDED_JOB: &str = "invalid_job";

/// Runs jobs an owner cluster handed this host through a [`JobTransport`],
/// the same path the daemon's own workers take.
pub struct TransportRunner<T> {
    /// The job transport.
    transport: T,
}

impl<T> TransportRunner<T> {
    /// A runner over `transport`.
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: JobTransport + 'static> HostJobRunner for TransportRunner<T> {
    async fn run(&self, ctx: HostJobCtx) -> Result<String, HostJobError> {
        let invalid = |detail: &str| HostJobError::new(INVALID_HANDED_JOB, detail);
        let kind = JobKind::parse(&ctx.job.job.kind).ok_or_else(|| invalid("unknown job kind"))?;
        if kind == JobKind::Acquire {
            // `complete` is refused for an acquire until its upload commits.
            return Err(HostJobError::new(
                UPLOAD_UNAVAILABLE,
                "a host cannot return acquired files to the owner yet",
            ));
        }
        let payload: JobPayload = serde_json::from_value(ctx.job.job.payload.clone())
            .map_err(|_| invalid("malformed job payload"))?;
        let command =
            JobCommand::from_parts(kind, payload).map_err(|err| invalid(&err.to_string()))?;
        // The owner's grant is capped as the client caps it, so an extreme
        // `lease_secs` cannot overflow the duration or `now + lease`.
        let lease_secs = ctx.job.lease_secs.min(MAX_LEASE.as_secs());
        let lease = chrono::Duration::seconds(i64::try_from(lease_secs).unwrap_or(60));
        let exec = JobExecCtx {
            fence: remote_job_fence(&ctx.job.node_id, &ctx.job.job.id, ctx.job.fence),
            cancel: ctx.cancel_flag(),
            attempt_count: ctx.job.job.attempt,
            dedup_key: String::new(),
            lease_expires_at: Some(chrono::Utc::now() + lease),
            checkpoint: command.payload.checkpoint.clone(),
            invocation_sequence: command.payload.invocation_sequence.unwrap_or(1).max(1),
            host: None,
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

/// How long a stopped host client may take to exit after a database swap
/// before its task is aborted.
const CLIENT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

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
/// When the links or a link's keys could not be read, the read is retried
/// with a growing delay ([`LINK_RETRY_MIN`] to [`LINK_RETRY_MAX`]) and only
/// the links not yet running are started, so a transient database error at
/// startup does not leave an enrolled node undialed until the next swap.
async fn supervise_cluster_links(state: Arc<AppState>) {
    let mut swaps = state.library_swaps.subscribe();
    loop {
        // Mark the current swap seen before reading the library, so a swap
        // published after this read always wakes the wait below.
        swaps.borrow_and_update();
        let library = state.library_snapshot().await;
        let (stop, shutdown) = watch::channel(false);
        let mut running = BTreeSet::new();
        let mut clients = Vec::new();
        let mut retry = LINK_RETRY_MIN;
        let swapped = loop {
            let complete =
                start_clients(&state, &library, &shutdown, &mut running, &mut clients).await;
            if complete {
                break swaps.changed().await.is_ok();
            }
            tokio::select! {
                changed = swaps.changed() => break changed.is_ok(),
                () = tokio::time::sleep(retry) => {
                    retry = (retry * 2).min(LINK_RETRY_MAX);
                }
            }
        };
        let _ = stop.send(true);
        for mut client in clients {
            if tokio::time::timeout(CLIENT_STOP_TIMEOUT, &mut client)
                .await
                .is_err()
            {
                client.abort();
            }
        }
        if !swapped {
            return;
        }
        tracing::info!("database swapped; restarting cluster links on the new store");
    }
}

/// Starts one host client per active link in `library` that is not yet in
/// `running`, each stopping when `shutdown` turns true, and adds it to
/// `running` and `clients`.
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
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let pending: Vec<_> = links
        .into_iter()
        .filter(|link| link.is_active() && !running.contains(&link.owner_cluster_id))
        .collect();
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
        let config = HostClientConfig {
            link,
            keys,
            software_version: env!("CARGO_PKG_VERSION").into(),
            incarnation: current_process_incarnation().into(),
            max_jobs: HOST_MAX_JOBS,
            timing: ClientTiming::default(),
        };
        let runner = TransportRunner::new(InProcessJobTransport::new(Arc::clone(state)));
        let client = match HostClient::new(config, runner, Box::new(NodeSampler::new(&files_dir))) {
            Ok((client, _handle)) => client,
            Err(err) => {
                tracing::error!(%cluster_id, error = %err, "cluster link refused by the transport policy");
                continue;
            }
        };
        running.insert(cluster_id.clone());
        let library = library.clone();
        let shutdown = shutdown.clone();
        clients.push(tokio::spawn(async move {
            if client.run(shutdown).await == ClientExit::Revoked {
                // The link belongs to the store it was read from.
                for attempt in 0..5u32 {
                    match set_cluster_link_state(&library, &cluster_id, CLUSTER_LINK_REVOKED).await
                    {
                        Ok(_) => break,
                        Err(err) => {
                            tracing::warn!(%cluster_id, error = %err, "could not mark the cluster link revoked");
                            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        }
                    }
                }
            }
        }));
    }
    complete
}

#[cfg(test)]
#[path = "cluster_link_tests.rs"]
mod tests;
