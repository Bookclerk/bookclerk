//! This daemon as a host node of owner clusters (H-4 design, steps 7-8).
//!
//! At startup, every active `cluster_links` row in this install's database
//! gets one [`HostClient`]: it connects to the owner's node socket under
//! the transport policy and the link's SPKI pin, authenticates with the
//! node keys sealed at enrollment, heartbeats, acks wakes, and resumes held
//! fences across reconnects. A link enrolled while the daemon runs is
//! picked up at the next start. A link that stops being active while its
//! client runs (`bookclerk clusters leave`, which also deletes its keys)
//! stops the client within [`consent::CONSENT_POLL`].
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

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    current_process_incarnation, list_cluster_links, load_cluster_link, load_cluster_link_keys,
    remote_job_fence, set_cluster_link_state, NodeSampler, CLUSTER_LINK_REVOKED,
};
use bookclerk_library::LibraryStore;
use bookclerk_library::{JobKind, JobPayload};
use bookclerk_node_client::{
    ClientExit, ClientTiming, HostClient, HostClientConfig, HostJobCtx, HostJobError, HostJobRunner,
};
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

/// Starts one host client per active cluster link.
pub fn spawn_cluster_links(state: Arc<AppState>) {
    tokio::spawn(async move {
        let library = state.library_snapshot().await;
        let links = match list_cluster_links(&library).await {
            Ok(links) => links,
            Err(err) => {
                tracing::warn!(error = %err, "could not read cluster links");
                return;
            }
        };
        let (files_dir, config) = {
            let cfg = state.config.read().await;
            (cfg.paths().files_dir.clone(), cfg.clone())
        };
        // A failed discovery approves no platform plugin automatically.
        let platform_payloads: std::collections::BTreeMap<String, String> =
            tokio::task::spawn_blocking(move || consent::platform_payloads(&config))
                .await
                .unwrap_or_default();
        for link in links.into_iter().filter(|link| link.is_active()) {
            let keys = match load_cluster_link_keys(&library, &link).await {
                Ok(keys) => keys,
                Err(err) => {
                    // Fail closed: without its keys the link is not dialed.
                    tracing::error!(
                        cluster_id = %link.owner_cluster_id,
                        error = %err,
                        "cluster link keys unavailable; not connecting"
                    );
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
                claim_kinds: HOST_RUNNABLE_KINDS
                    .iter()
                    .map(|kind| (*kind).to_string())
                    .collect(),
                timing: ClientTiming::default(),
            };
            let catalog = Arc::new(RemoteCatalog::default());
            let runner = TransportRunner::new(
                host_jobs::HostJobTransport::new(host_jobs::GuestHost::new(
                    Arc::clone(&state),
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
            let (stop, shutdown) = watch::channel(false);
            let stop = Arc::new(stop);
            tokio::spawn(stop_when_inactive(
                library.clone(),
                cluster_id.clone(),
                Arc::clone(&stop),
                shutdown.clone(),
            ));
            tokio::spawn(
                consent::LinkSupervisor {
                    library: library.clone(),
                    files_dir: files_dir.clone(),
                    handle,
                    catalog,
                    platform_payloads: platform_payloads.clone(),
                }
                .run(shutdown.clone()),
            );
            let library = library.clone();
            tokio::spawn(async move {
                let exit = client.run(shutdown).await;
                let _ = stop.send(true);
                if exit == ClientExit::Revoked {
                    for attempt in 0..5u32 {
                        match set_cluster_link_state(&library, &cluster_id, CLUSTER_LINK_REVOKED)
                            .await
                        {
                            Ok(_) => break,
                            Err(err) => {
                                tracing::warn!(%cluster_id, error = %err, "could not mark the cluster link revoked");
                                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                            }
                        }
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests;
