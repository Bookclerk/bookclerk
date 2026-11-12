//! On a branch: taking the interlibrary requests of the library it joined
//! (`control_plane::branch_requests`).
//!
//! One [`RequestTaker`] runs beside each link's client. On every welcomed
//! connection, on each `requests.queued` wake, and every [`REQUEST_POLL`],
//! it claims the requests waiting for this branch (`requests.claim`) and
//! answers each (`requests.finish`):
//!
//! - **Accepted** when this branch still lists the requested edition to
//!   that library ([`book_for_request`]: the owner's books always, the
//!   members' while the link's "List my members' books" setting is on and
//!   they did not opt out, and nothing while "Share this branch's books"
//!   is off). The book is recorded as requested by that library
//!   ([`record_link_request`]), so the catalog sync sends it there although
//!   it is a member's, and a book not yet downloaded is queued as an
//!   ordinary acquire in this install's own library, on the account here
//!   that holds it. A branch fulfils automatically for anything it listed:
//!   its listing settings are where it draws the edge.
//! - **Not listed** when it does not, which the library records for good.
//! - **Failed** when a local read or write failed; the library retries.
//!
//! The book itself goes up with the catalog sync's next pass, as the
//! owner's own books do. Nothing of the request names an account, user or
//! other library, and the library never learns which person's copy was
//! sent.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    book_for_request, load_cluster_link, record_link_request, ClusterLink, ScanRequester,
};
use bookclerk_library::{AcquireStatus, BookRecord, JobTrigger, LibraryStore};
use bookclerk_node_client::{HostClientHandle, OwnerCallError};
use bookclerk_node_proto::{BranchRequest, RequestOutcome};
use tokio::sync::{broadcast, watch};

use crate::api::AppState;

/// Between claims while connected, in case a wake was missed.
pub const REQUEST_POLL: Duration = Duration::from_secs(900);

/// Most claimed batches taken in one pass, so one pass ends even when the
/// library keeps queueing.
const MAX_BATCHES_PER_PASS: usize = 20;

/// Queues the download of a book this branch accepted a request for.
pub trait RequestAcquirer: Send + Sync + 'static {
    /// Queues an acquire of `book` in this install's own library, on the
    /// account that holds it.
    fn acquire(&self, book: &BookRecord) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// The daemon's [`RequestAcquirer`]: an ordinary acquire job, with the
/// daemon's own authority (the request was accepted under the link's
/// listing settings, which the branch's operator chose).
pub struct DaemonAcquirer {
    /// Daemon state.
    pub state: Arc<AppState>,
}

impl RequestAcquirer for DaemonAcquirer {
    async fn acquire(&self, book: &BookRecord) -> anyhow::Result<()> {
        let admitted = crate::jobs::enqueue_acquire(
            Arc::clone(&self.state),
            ScanRequester::Operator,
            crate::jobs::AcquireAsk {
                titles: vec![book.uuid.clone()],
                account: Some(book.account_id.clone()),
                ..crate::jobs::AcquireAsk::default()
            },
            JobTrigger::Scheduler,
        )
        .await?;
        anyhow::ensure!(
            !admitted
                .iter()
                .any(|outcome| matches!(outcome, crate::jobs::AdmitJob::QueueFull)),
            "the job queue is full"
        );
        Ok(())
    }
}

/// Takes one link's interlibrary requests.
pub struct RequestTaker<A> {
    /// This install's library.
    pub library: LibraryStore,
    /// The library this link joined.
    pub cluster_id: String,
    /// The link's client.
    pub handle: HostClientHandle,
    /// Queues the downloads.
    pub acquirer: A,
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TakeReport {
    /// Requests accepted.
    pub accepted: usize,
    /// Requests answered "not listed".
    pub not_listed: usize,
    /// Requests that failed here.
    pub failed: usize,
}

impl<A: RequestAcquirer> RequestTaker<A> {
    /// Runs passes until `shutdown` turns true.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut connections = self.handle.connections();
        let mut wakes = self.handle.subscribe_wakes();
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.handle.is_connected() {
                match self.take_once().await {
                    Ok(report) if report == TakeReport::default() => {}
                    Ok(report) => tracing::info!(
                        cluster_id = %self.cluster_id,
                        accepted = report.accepted,
                        not_listed = report.not_listed,
                        failed = report.failed,
                        "answered the library's interlibrary requests"
                    ),
                    Err(err) => tracing::warn!(
                        cluster_id = %self.cluster_id,
                        error = %err,
                        "could not take the library's interlibrary requests; retrying later"
                    ),
                }
            }
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break;
                    }
                    changed = connections.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break;
                    }
                    wake = wakes.recv() => match wake {
                        Ok(message) if is_requests_queued(&message) => break,
                        Ok(_) => {}
                        // Missed wakes may have queued; claim to be sure.
                        Err(broadcast::error::RecvError::Lagged(_)) => break,
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    () = tokio::time::sleep(REQUEST_POLL) => break,
                }
            }
        }
    }

    /// Claims and answers request batches until none is left (or
    /// [`MAX_BATCHES_PER_PASS`]).
    ///
    /// # Errors
    ///
    /// Returns an error when the library cannot be reached or refuses, or
    /// the link cannot be read.
    pub async fn take_once(&self) -> anyhow::Result<TakeReport> {
        let mut report = TakeReport::default();
        for _ in 0..MAX_BATCHES_PER_PASS {
            let Some(link) = load_cluster_link(&self.library, &self.cluster_id).await? else {
                return Ok(report);
            };
            if !link.is_active() {
                return Ok(report);
            }
            let requests = self
                .handle
                .requests_claim()
                .await
                .map_err(|err| anyhow::anyhow!("requests.claim: {err:?}"))?;
            if requests.is_empty() {
                break;
            }
            for request in &requests {
                let outcome = self.take(&link, request).await;
                match outcome {
                    RequestOutcome::Accepted => report.accepted += 1,
                    RequestOutcome::NotListed => report.not_listed += 1,
                    RequestOutcome::Failed => report.failed += 1,
                }
                match self
                    .handle
                    .request_finish(&request.job, request.generation, outcome)
                    .await
                {
                    Ok(()) => {}
                    // The lease moved on (it expired, or the request was
                    // cancelled); the library hands it out again if it
                    // still wants it.
                    Err(OwnerCallError::Lost) => tracing::debug!(
                        cluster_id = %self.cluster_id,
                        job_id = %request.job,
                        "an interlibrary request's lease was lost before its answer"
                    ),
                    Err(err) => anyhow::bail!("requests.finish: {err:?}"),
                }
            }
        }
        if report.accepted > 0 {
            // Requested books already here go up with this pass; the rest
            // with the pass after their download.
            super::catalog_sync::notify_sharing_changed();
        }
        Ok(report)
    }

    /// Decides one request.
    async fn take(&self, link: &ClusterLink, request: &BranchRequest) -> RequestOutcome {
        if !link.share_books {
            // Sharing off: this branch lists nothing to that library.
            return RequestOutcome::NotListed;
        }
        match self.accept(link, request).await {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::warn!(
                    cluster_id = %self.cluster_id,
                    job_id = %request.job,
                    error = %err,
                    "could not take an interlibrary request"
                );
                RequestOutcome::Failed
            }
        }
    }

    /// Records and queues a request this branch still lists.
    ///
    /// # Errors
    ///
    /// Returns the local read or write error.
    async fn accept(
        &self,
        link: &ClusterLink,
        request: &BranchRequest,
    ) -> anyhow::Result<RequestOutcome> {
        let Some(book) = book_for_request(&self.library, request, link.list_members_books).await?
        else {
            return Ok(RequestOutcome::NotListed);
        };
        if book.acquire_status != AcquireStatus::Acquired {
            self.acquirer.acquire(&book).await?;
        }
        record_link_request(&self.library, &self.cluster_id, &book).await?;
        Ok(RequestOutcome::Accepted)
    }
}

/// True for a `requests.queued` wake.
fn is_requests_queued(message: &serde_json::Value) -> bool {
    message.get("type").and_then(serde_json::Value::as_str) == Some("requests.queued")
}

#[cfg(test)]
#[path = "requests_tests.rs"]
mod tests;
