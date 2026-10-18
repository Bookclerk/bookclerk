//! What a host does with a claimed job: the runner interface, the job's
//! opened secrets, and token write-back.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bookclerk_node_proto::{SecretEntry, SecretRef, SecretValue, WorkJob};

use crate::client::Shared;

/// A job the owner handed this host, under its fence.
#[derive(Debug, Clone)]
pub struct HostJob {
    /// This host's node id in the owner cluster (the lease owner is
    /// `host:<node_id>`).
    pub node_id: String,
    /// The job as the owner stores it.
    pub job: WorkJob,
    /// Lease generation; every frame for the job names it.
    pub fence: i64,
    /// Lease length the owner granted.
    pub lease_secs: u64,
}

/// The job's secrets, opened with this node's box key. Values are
/// zeroized on drop and `Debug` shows only which rows they are.
pub struct HandedSecrets(Vec<SecretEntry>);

impl HandedSecrets {
    /// Wraps opened entries.
    #[must_use]
    pub(crate) fn new(entries: Vec<SecretEntry>) -> Self {
        Self(entries)
    }

    /// The rows handed with the job.
    pub fn refs(&self) -> impl Iterator<Item = &SecretRef> {
        self.0.iter().map(|entry| &entry.secret_ref)
    }

    /// The value of one handed row.
    #[must_use]
    pub fn get(&self, secret_ref: &SecretRef) -> Option<&SecretValue> {
        self.0
            .iter()
            .find(|entry| entry.secret_ref == *secret_ref)
            .map(|entry| &entry.value)
    }

    /// Number of handed rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when the job needs no secrets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for HandedSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.refs()).finish()
    }
}

/// Why a token write-back was not stored.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WriteBackError {
    /// The ref was not handed with this job; nothing was sent.
    #[error("secret was not handed with this job")]
    NotHanded,
    /// The value is empty or larger than the owner accepts; nothing was
    /// sent.
    #[error("secret value size is not accepted")]
    Size,
    /// The fence is lost; the job must stop.
    #[error("the job's fence is lost")]
    Lost,
    /// The owner refused it (`fence_not_live`, `not_trusted`, ...).
    #[error("owner refused the write-back: {0}")]
    Refused(String),
    /// No connection to the owner, or it did not answer in time.
    #[error("owner unavailable")]
    Unavailable,
}

/// How a job failed: a stable kind and an operator-facing message. Neither
/// may contain a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostJobError {
    /// Stable error kind recorded on the owner's job row.
    pub kind: String,
    /// Operator-facing message.
    pub message: String,
}

impl HostJobError {
    /// A failure of `kind`.
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

/// Everything a runner gets for one job.
pub struct HostJobCtx {
    /// The job and its fence.
    pub job: HostJob,
    /// Its opened secrets.
    pub secrets: Arc<HandedSecrets>,
    /// Set when the fence is lost or the operator asked to cancel.
    cancel: Arc<AtomicBool>,
    /// The client, for write-back.
    shared: Arc<Shared>,
}

impl std::fmt::Debug for HostJobCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostJobCtx")
            .field("job", &self.job)
            .field("secrets", &self.secrets)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl HostJobCtx {
    /// Builds a context.
    pub(crate) fn new(
        job: HostJob,
        secrets: HandedSecrets,
        cancel: Arc<AtomicBool>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            job,
            secrets: Arc::new(secrets),
            cancel,
            shared,
        }
    }

    /// True when the runner should stop: the fence was lost or the
    /// operator cancelled the job.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// The cancel flag, for runners that poll it elsewhere.
    #[must_use]
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Writes a refreshed store token back to the owner (decision 3).
    ///
    /// Only a row handed with this job may be written, and only under its
    /// live fence. The value is sealed to the current connection's owner
    /// box key, bound to this node, job, fence and ref; it never travels or
    /// is logged as text.
    ///
    /// # Errors
    ///
    /// Returns [`WriteBackError`] when the ref was not handed, the owner
    /// refuses, the fence is lost, or no owner connection answers.
    pub async fn write_back(
        &self,
        secret_ref: &SecretRef,
        value: &[u8],
    ) -> Result<(), WriteBackError> {
        if self.secrets.get(secret_ref).is_none() {
            return Err(WriteBackError::NotHanded);
        }
        if value.is_empty() || value.len() > bookclerk_library::control_plane::MAX_WRITE_BACK_BYTES
        {
            return Err(WriteBackError::Size);
        }
        self.shared
            .write_back(&self.job.job.id, self.job.fence, secret_ref, value)
            .await
    }
}

/// Runs claimed jobs on this host.
///
/// The runner must stop soon after [`HostJobCtx::is_cancelled`] turns true;
/// its result is then discarded. Error kinds and messages are recorded on
/// the owner and must never contain secret material.
pub trait HostJobRunner: Send + Sync + 'static {
    /// Runs one job; `Ok` carries the outcome text recorded as progress.
    fn run(&self, ctx: HostJobCtx) -> impl Future<Output = Result<String, HostJobError>> + Send;
}
