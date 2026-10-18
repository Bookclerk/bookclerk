//! This daemon as a host node of owner clusters (H-4 design, step 7).
//!
//! At startup, every active `cluster_links` row in this install's database
//! gets one [`HostClient`]: it connects to the owner's node socket under
//! the transport policy and the link's SPKI pin, authenticates with the
//! node keys sealed at enrollment, heartbeats, acks wakes, and resumes held
//! fences across reconnects. A link enrolled while the daemon runs is
//! picked up at the next start.
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

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    current_process_incarnation, list_cluster_links, load_cluster_link_keys, remote_job_fence,
    set_cluster_link_state, NodeSampler, CLUSTER_LINK_REVOKED,
};
use bookclerk_library::{JobKind, JobPayload};
use bookclerk_node_client::{
    ClientExit, ClientTiming, HostClient, HostClientConfig, HostJobCtx, HostJobError, HostJobRunner,
};
use tokio::sync::watch;

use crate::api::AppState;
use crate::job_handler::{InProcessJobTransport, JobCommand, JobExecCtx, JobTransport};

/// Jobs a host client claims at once. Zero until host consent (step 8):
/// nothing on a host may run an owner plugin before its operator approves
/// it, and every job a host is trusted with runs one.
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
        let files_dir = state.config.read().await.paths().files_dir.clone();
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
                timing: ClientTiming::default(),
            };
            let runner = TransportRunner::new(InProcessJobTransport::new(Arc::clone(&state)));
            let client = match HostClient::new(
                config,
                runner,
                Box::new(NodeSampler::new(&files_dir)),
            ) {
                Ok((client, _handle)) => client,
                Err(err) => {
                    tracing::error!(%cluster_id, error = %err, "cluster link refused by the transport policy");
                    continue;
                }
            };
            let library = library.clone();
            tokio::spawn(async move {
                let (_stop, shutdown) = watch::channel(false);
                if client.run(shutdown).await == ClientExit::Revoked {
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
#[path = "cluster_link_tests.rs"]
mod tests;
