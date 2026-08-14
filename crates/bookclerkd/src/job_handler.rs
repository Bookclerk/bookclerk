//! Versioned job command transport (in-process now, workerd RPC later).
//!
//! Durable **commands** live in `jobs`. Domain events such as `book.acquired`
//! stay on a separate notification path and must not become job kinds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bookclerk_library::{JobFence, JobKind, JobPayload, JobRecord, JOB_PAYLOAD_VERSION};

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
        if job.kind == JobKind::Invalid {
            anyhow::bail!("invalid_job");
        }
        if job.payload.v != JOB_COMMAND_VERSION {
            anyhow::bail!(
                "unsupported job command version {} (expected {JOB_COMMAND_VERSION})",
                job.payload.v
            );
        }
        Ok(Self {
            version: job.payload.v,
            kind: job.kind,
            payload: job.payload.clone(),
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
        tracing::trace!(
            version = cmd.version,
            kind = cmd.kind.as_str(),
            "dispatch job command"
        );
        match cmd.kind {
            JobKind::Scan => {
                run_scan(&self.state, cmd.payload.account.as_deref(), Some(&ctx)).await
            }
            JobKind::Acquire => {
                run_acquire(
                    &self.state,
                    cmd.payload.title.as_deref(),
                    cmd.payload.account.as_deref(),
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
            JobKind::Invalid => anyhow::bail!("invalid_job"),
        }
    }
}
