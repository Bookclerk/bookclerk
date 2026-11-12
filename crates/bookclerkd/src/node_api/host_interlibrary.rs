//! Interlibrary requests on this library: the requests a branch claims and
//! the outcomes it reports (`control_plane::branch_requests`).
//!
//! | Frame | Answer |
//! |---|---|
//! | `requests.claim` | `requests.claimed {requests}` or `error {code: "refused"}` |
//! | `requests.finish` | `ok`, `lost` or `error {code: "refused"}` |
//!
//! The branch is this connection's authenticated node, and only requests
//! pinned to it are claimed or settled.

use bookclerk_library::control_plane::{
    claim_branch_requests, finish_branch_request, RequestFinished,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{Body, Frame, RequestOutcome, ITEM_REQUEST};

use super::work::{error, HostContext};

/// `requests.claim`.
pub(super) async fn claim(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
) -> bookclerk_library::Result<Frame> {
    match claim_branch_requests(library, &host.node_id).await? {
        Some(requests) => {
            if !requests.is_empty() {
                tracing::debug!(
                    node_id = %host.node_id,
                    requests = requests.len(),
                    "a branch claimed interlibrary requests"
                );
            }
            Ok(Frame::new(id, Body::RequestsClaimed { requests }))
        }
        None => Ok(error(id, "refused", "not_a_branch")),
    }
}

/// `requests.finish`.
pub(super) async fn finish(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    job: String,
    generation: i64,
    outcome: RequestOutcome,
) -> bookclerk_library::Result<Frame> {
    match finish_branch_request(library, &host.node_id, &job, generation, outcome).await? {
        RequestFinished::Recorded => {
            tracing::info!(
                node_id = %host.node_id,
                job_id = %job,
                outcome = outcome.as_str(),
                "a branch answered an interlibrary request"
            );
            Ok(Frame::new(id, Body::Accepted {}))
        }
        RequestFinished::Lost => Ok(Frame::new(
            id,
            Body::Lost {
                item: ITEM_REQUEST.into(),
                job_id: job,
                fence: generation,
            },
        )),
        RequestFinished::Refused => Ok(error(id, "refused", "not_a_branch")),
    }
}
