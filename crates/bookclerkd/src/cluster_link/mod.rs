//! This daemon as a host node of owner clusters (H-4 design, steps 7-8).
//!
//! At startup, every active `cluster_links` row in this install's database
//! gets one [`HostClient`]: it connects to the owner's node socket under
//! the transport policy and the link's SPKI pin, authenticates with the
//! node keys sealed at enrollment, heartbeats, acks the wakes a subscriber
//! received (one none receives stays unacked on the owner), and resumes
//! held fences across reconnects. A database swap on config reload stops
//! every client and starts one per active link of the new store, so no
//! client keeps dialing with the previous store's links and keys. A link
//! enrolled while the daemon runs is picked up at the next start or
//! database swap.
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
pub mod remote_bindings;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    current_process_incarnation, list_cluster_links, load_cluster_link_keys, remote_job_fence,
    set_cluster_link_state, NodeSampler, CLUSTER_LINK_REVOKED,
};
use bookclerk_library::{JobKind, JobPayload, LibraryStore};
use bookclerk_node_client::{
    ClientExit, ClientTiming, HostClient, HostClientConfig, HostJobCtx, HostJobError, HostJobRunner,
};
use tokio::sync::watch;
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
    let (files_dir, config) = {
        let cfg = state.config.read().await;
        (cfg.paths().files_dir.clone(), cfg.clone())
    };
    // A failed discovery approves no platform plugin automatically.
    let platform_payloads: std::collections::BTreeMap<String, String> =
        tokio::task::spawn_blocking(move || consent::platform_payloads(&config))
            .await
            .unwrap_or_default();
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
        let node_id = link.remote_node_id.clone();
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
                continue;
            }
        };
        running.insert(cluster_id.clone());
        // The link's consent supervisor stops with its client: on a swap,
        // when the client is revoked, or when its task is aborted.
        let (link_stop, link_shutdown) = watch::channel(false);
        let supervisor = tokio::spawn(
            consent::LinkSupervisor {
                library: library.clone(),
                files_dir: files_dir.clone(),
                handle,
                catalog,
                platform_payloads: platform_payloads.clone(),
            }
            .run(link_shutdown),
        );
        let library = library.clone();
        let shutdown = shutdown.clone();
        clients.push(tokio::spawn(async move {
            let exit = client.run(shutdown).await;
            let _ = link_stop.send(true);
            if exit == ClientExit::Revoked {
                // The link belongs to the store it was read from. Only the
                // enrollment this client ran as is revoked: a re-enrollment
                // recorded meanwhile keeps its new link.
                for attempt in 0..5u32 {
                    match set_cluster_link_state(
                        &library,
                        &cluster_id,
                        CLUSTER_LINK_REVOKED,
                        Some(&node_id),
                    )
                    .await
                    {
                        Ok(true) => break,
                        Ok(false) => {
                            tracing::info!(%cluster_id, %node_id, "revoked enrollment was already replaced; keeping the new link");
                            break;
                        }
                        Err(err) => {
                            tracing::warn!(%cluster_id, error = %err, "could not mark the cluster link revoked");
                            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        }
                    }
                }
            }
        }));
        clients.push(supervisor);
    }
    complete
}

#[cfg(test)]
mod tests;
