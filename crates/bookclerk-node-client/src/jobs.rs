//! What a host does with a claimed job: the runner interface, the job's
//! opened secrets, and token write-back.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use std::sync::atomic::AtomicU64;

use bookclerk_node_proto::{
    AcquireTarget, PlanDestination, SecretEntry, SecretRef, SecretValue, UploadFile, WorkJob,
};

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
    /// Plugin keys the job runs (each consented when it was claimed).
    pub plugin_keys: Vec<String>,
    /// The one plugin of `plugin_keys` whose source guest runs the job,
    /// when the owner named it.
    pub source_plugin_key: Option<String>,
    /// When the granted lease runs out, counted as the client counts it:
    /// from before the claim was sent, capped at [`crate::MAX_LEASE`]. Work
    /// handed a deadline should stop by then, since the fence may be lost
    /// once it passes without a renewal.
    pub lease_deadline: std::time::SystemTime,
    /// The claim resumed a suspended job: no attempt was spent, and the
    /// job's checkpoint applies.
    pub resumed: bool,
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

/// Why a call the job makes to the owner (results, plan, upload) failed.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum OwnerCallError {
    /// The fence is lost; the job must stop (it is already cancelled).
    #[error("the job's fence is lost")]
    Lost,
    /// The owner refused it; the reason is stable.
    #[error("owner refused: {0}")]
    Refused(String),
    /// No connection to the owner, or no usable answer.
    #[error("owner unavailable")]
    Unavailable,
}

/// An open upload of one title: its id and bearer token. The token is
/// zeroized on drop and never shown by `Debug`.
pub struct HostUpload {
    /// Upload id.
    pub upload_id: String,
    /// The bearer token, opened with this node's box key.
    token: SecretValue,
}

impl HostUpload {
    /// Wraps an opened grant.
    pub(crate) fn new(upload_id: String, token: SecretValue) -> Self {
        Self { upload_id, token }
    }

    /// The bearer token.
    pub(crate) fn token(&self) -> &SecretValue {
        &self.token
    }
}

impl Drop for HostUpload {
    /// The token is unique to this upload and unusable once it is dropped,
    /// so it leaves the redaction registry (it was registered when opened,
    /// `open_granted_upload`).
    fn drop(&mut self) {
        if let Ok(token) = std::str::from_utf8(self.token.expose()) {
            bookclerk_config::unregister_secret(token);
        }
    }
}

impl std::fmt::Debug for HostUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostUpload")
            .field("upload_id", &self.upload_id)
            .field("token", &"<redacted>")
            .finish()
    }
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
    /// Set when the fence is lost (the owner said so, or the lease ran out
    /// unconfirmed): the owner may have handed the job elsewhere.
    lost: Arc<AtomicBool>,
    /// Set when an upload's commit completed the job on the owner, so no
    /// `complete` follows.
    finished: Arc<AtomicBool>,
    /// Results batches sent so far.
    batches: AtomicU64,
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
        lost: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            job,
            secrets: Arc::new(secrets),
            cancel,
            lost,
            finished,
            batches: AtomicU64::new(0),
            shared,
        }
    }

    /// True when the runner should stop: the fence was lost or the
    /// operator cancelled the job.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// True once the job's fence is lost: the owner reported it, or the
    /// lease ran out without a confirmed renewal. The owner may already
    /// have handed the job to another run, which then owns the job's
    /// titles, so this run must not undo anything it wrote under them.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
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

impl HostJobCtx {
    /// Sends one batch of a scan's rows (the guest's `ScanBook` objects) to
    /// the owner, which validates and writes them (decision 6); answers the
    /// rows written. At most
    /// [`bookclerk_node_proto::MAX_SCAN_BATCH_ROWS`] rows, each at most
    /// [`bookclerk_node_proto::MAX_SCAN_ROW_BYTES`] serialized.
    ///
    /// # Errors
    ///
    /// Returns [`OwnerCallError`]; `Lost` also cancels the job.
    pub async fn send_scan_results(
        &self,
        plugin_key: &str,
        books: Vec<serde_json::Value>,
    ) -> Result<u64, OwnerCallError> {
        let batch = self.batches.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared
            .scan_results(&self.job.job.id, self.job.fence, plugin_key, batch, books)
            .await
    }

    /// Asks the owner which titles this acquire should fetch, and with
    /// which destinations (the owner node's, the primary first, each with
    /// its own `DownloadOptions`).
    ///
    /// # Errors
    ///
    /// Returns [`OwnerCallError`]; `Lost` also cancels the job.
    pub async fn acquire_plan(
        &self,
    ) -> Result<(Vec<AcquireTarget>, Vec<PlanDestination>), OwnerCallError> {
        self.shared
            .acquire_plan(&self.job.job.id, self.job.fence)
            .await
    }

    /// Opens an upload of one title's `files` (an empty `book_uuid` and no
    /// files for the job's closing upload).
    ///
    /// # Errors
    ///
    /// Returns [`OwnerCallError`]; `Lost` also cancels the job.
    pub async fn grant_upload(
        &self,
        book_uuid: &str,
        primary_key: &str,
        pdf_key: Option<&str>,
        files: Vec<UploadFile>,
    ) -> Result<HostUpload, OwnerCallError> {
        self.shared
            .upload_grant(
                &self.job.job.id,
                self.job.fence,
                book_uuid,
                primary_key,
                pdf_key,
                files,
            )
            .await
    }

    /// Streams declared file `index` of `upload` (`length` bytes of
    /// `body`) to the owner, which writes it to its destinations as it
    /// arrives.
    ///
    /// # Errors
    ///
    /// Returns [`OwnerCallError`]; `Lost` (the owner aborted the upload)
    /// also cancels the job.
    pub async fn upload_file<R: tokio::io::AsyncRead + Unpin + Send>(
        &self,
        upload: &HostUpload,
        index: usize,
        length: u64,
        body: R,
    ) -> Result<(), OwnerCallError> {
        self.shared
            .upload_file(
                &self.job.job.id,
                self.job.fence,
                upload,
                index,
                length,
                body,
            )
            .await
    }

    /// Commits `upload`. With `finish`, the owner also completes the job
    /// (recording the text as its progress) and the client sends no
    /// `complete` afterwards. Answers whether the job is finished.
    ///
    /// # Errors
    ///
    /// Returns [`OwnerCallError`]; `Lost` also cancels the job.
    pub async fn commit_upload(
        &self,
        upload: &HostUpload,
        finish: Option<&str>,
    ) -> Result<bool, OwnerCallError> {
        let completed = self
            .shared
            .commit_upload(&self.job.job.id, self.job.fence, upload, finish)
            .await?;
        if completed {
            self.finished.store(true, Ordering::SeqCst);
        }
        Ok(completed)
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
