//! Versioned job command transport (in-process now, workerd RPC later).
//!
//! Durable **commands** live in `jobs`. Domain events such as `book.acquired`
//! stay on a separate notification path and must not become job kinds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bookclerk_library::{JobFence, JobKind, JobPayload, JobRecord, JOB_PAYLOAD_VERSION};
use bookclerk_plugin_host::JobCheckpoint;
use chrono::{DateTime, Utc};

use crate::api::AppState;
use crate::jobs::{run_acquire, run_integration_scan, run_listen_sync, run_scan};

/// Current command envelope version accepted by [`InProcessJobTransport`].
pub const JOB_COMMAND_VERSION: u32 = JOB_PAYLOAD_VERSION;

/// Fence + cooperative cancel token handed to a handler.
#[derive(Clone)]
pub struct JobExecCtx {
    /// Lease identity the handler must use for progress and finalization.
    pub fence: JobFence,
    /// Set when the heartbeat loses the fence or the operator cancels.
    pub cancel: Arc<AtomicBool>,
    /// 1-based attempt counter from the claim that produced this fence.
    pub attempt_count: i64,
    /// Durable dedup key; stable across reclaim of the same command.
    pub dedup_key: String,
    /// Lease expiry; used as the guest `deadlineUnixMs` hint.
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Checkpoint restored after a prior suspension, when present.
    pub checkpoint: Option<JobCheckpoint>,
    /// Resume ordinal from the durable payload; distinct from [`Self::attempt_count`].
    pub invocation_sequence: u32,
    /// Set when this daemon runs the job as a host node for an owner
    /// cluster: the secrets the owner handed with it (opened with this
    /// node's box key, never written to disk) and token write-back under
    /// the owner's fence. `None` for the daemon's own jobs.
    pub host: Option<Arc<bookclerk_node_client::HostJobCtx>>,
    /// Set with [`Self::host`]: what the owner plugin's guest is opened
    /// with on this host (`CONFIG`, `SECRETS` under the consented grant,
    /// and the job's store credentials).
    pub host_bindings: Option<Arc<crate::cluster_link::remote_bindings::HostJobBindings>>,
}

impl JobExecCtx {
    /// True when the handler should stop after the current step.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Marks the handler cancelled (heartbeat loss or operator request).
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Versioned command decoded from a durable job row.
pub struct JobCommand {
    /// Envelope version (`JobPayload.v`).
    pub version: u32,
    /// Command kind.
    pub kind: JobKind,
    /// Kind-specific filters.
    pub payload: JobPayload,
}

impl JobCommand {
    /// Builds a command from a claimed row, rejecting unknown/invalid kinds.
    ///
    /// # Errors
    ///
    /// Returns an error when the envelope version or kind cannot be executed.
    pub fn from_record(job: &JobRecord) -> anyhow::Result<Self> {
        Self::from_parts(job.kind, job.payload.clone())
    }

    /// Builds a command from a kind and payload (a job handed by an owner
    /// cluster), rejecting unknown/invalid kinds.
    ///
    /// # Errors
    ///
    /// Returns an error when the envelope version or kind cannot be executed.
    pub fn from_parts(kind: JobKind, payload: JobPayload) -> anyhow::Result<Self> {
        if kind == JobKind::Invalid {
            anyhow::bail!("invalid_job");
        }
        if payload.v != JOB_COMMAND_VERSION {
            anyhow::bail!(
                "unsupported job command version {} (expected {JOB_COMMAND_VERSION})",
                payload.v
            );
        }
        Ok(Self {
            version: payload.v,
            kind,
            payload,
        })
    }
}

/// Executes a validated [`JobCommand`].
pub trait JobTransport: Send + Sync {
    /// Runs `cmd` until it finishes or `ctx` is cancelled.
    fn execute(
        &self,
        cmd: JobCommand,
        ctx: JobExecCtx,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}

/// In-process adapter used by `bookclerkd` today.
pub struct InProcessJobTransport {
    /// Shared daemon state (config, library, integrations).
    state: Arc<AppState>,
}

impl InProcessJobTransport {
    /// Builds an adapter over `state`.
    #[must_use]
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

impl JobTransport for InProcessJobTransport {
    async fn execute(&self, cmd: JobCommand, ctx: JobExecCtx) -> anyhow::Result<String> {
        if ctx.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        // A job an owner cluster handed this host would run here against
        // this install's own library and plugins; refuse it.
        if ctx.host.is_some() || ctx.host_bindings.is_some() {
            anyhow::bail!("a job handed by an owner cluster does not run in process");
        }
        tracing::trace!(
            version = cmd.version,
            kind = cmd.kind.as_str(),
            "dispatch job command"
        );
        match cmd.kind {
            JobKind::Scan => {
                run_scan(
                    &self.state,
                    cmd.payload.account.as_deref(),
                    cmd.payload.plugin_instance_id.as_deref(),
                    Some(&ctx),
                )
                .await
            }
            JobKind::Acquire => {
                run_acquire(
                    &self.state,
                    cmd.payload.title.as_deref(),
                    cmd.payload.account.as_deref(),
                    cmd.payload.plugin_instance_id.as_deref(),
                    Some(&ctx),
                )
                .await
            }
            JobKind::ListenSync => run_listen_sync(&self.state, Some(&ctx)).await,
            JobKind::IntegrationScan => {
                let id =
                    cmd.payload.integration_id.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("integration_scan missing integration_id")
                    })?;
                run_integration_scan(&self.state, id, cmd.payload.force, Some(&ctx)).await
            }
            JobKind::PluginCopy => {
                crate::jobs::run_plugin_copy(
                    &self.state,
                    cmd.payload.plugin_id.as_deref(),
                    cmd.payload.source_key.as_deref(),
                    cmd.payload.dest_key.as_deref(),
                    Some(&ctx),
                )
                .await
            }
            JobKind::Invalid => anyhow::bail!("invalid_job"),
        }
    }
}
