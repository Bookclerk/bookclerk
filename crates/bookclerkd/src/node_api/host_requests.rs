//! Library work a host node asks the owner cluster for (H-5).
//!
//! | Frame | Answer |
//! |---|---|
//! | `scan.request` | `scan.planned {created, duplicates, queue_full}`, or `error {code: "forbidden" \| "invalid"}` |
//! | `job.status` | `job.statuses {jobs}` |
//!
//! A host's request runs as the user who owns the node
//! ([`remote_node_requester`]), through the same scan planner as the HTTP
//! API (decision 2): an Owner or Administrator scanning every account gets
//! one job per scan-enabled account, distributed to every eligible owner
//! node and host by the routed claim and the per-item trust check; anyone
//! else scans only the accounts they own. A node that is trusted with
//! nothing (revoked, ownerless, a disabled owner) is refused. `job.status`
//! reports only jobs the node would be trusted to claim, so a host learns
//! nothing about other users' work.

use bookclerk_library::control_plane::{remote_node_may_view_job, remote_node_requester};
use bookclerk_library::{JobTrigger, LibraryError, LibraryStore};
use bookclerk_node_proto::{Body, Frame, JobStatusEntry, MAX_STATUS_JOBS};

use super::work::{error, HostContext};
use crate::jobs::AdmitJob;

/// `scan.request`: plans and queues a scan for the node's owner.
pub(super) async fn scan_request(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    account: Option<String>,
    plugin_instance_id: Option<String>,
) -> bookclerk_library::Result<Frame> {
    let Some(requester) = remote_node_requester(library, &host.node_id).await? else {
        tracing::warn!(node_id = %host.node_id, "refused a scan request from an untrusted host node");
        return Ok(error(
            id,
            "forbidden",
            "this node may not queue library work",
        ));
    };
    let admits = match crate::jobs::enqueue_scan(
        std::sync::Arc::clone(&host.state),
        requester,
        account,
        plugin_instance_id,
        JobTrigger::Api,
    )
    .await
    {
        Ok(admits) => admits,
        Err(err) => {
            return match err.downcast_ref::<LibraryError>() {
                Some(LibraryError::Forbidden(message)) => Ok(error(id, "forbidden", message)),
                Some(LibraryError::Invalid(message) | LibraryError::Conflict(message)) => {
                    Ok(error(id, "invalid", message))
                }
                _ => Err(LibraryError::Other(err)),
            }
        }
    };
    let mut created = Vec::new();
    let mut duplicates = Vec::new();
    let mut queue_full = 0u64;
    for admit in admits {
        match admit {
            AdmitJob::Created(job) => created.push(job),
            AdmitJob::Duplicate(job) => duplicates.push(job),
            AdmitJob::QueueFull => queue_full += 1,
        }
    }
    tracing::info!(
        node_id = %host.node_id,
        created = created.len(),
        duplicates = duplicates.len(),
        queue_full,
        "queued a scan a host node asked for"
    );
    Ok(Frame::new(
        id,
        Body::ScanPlanned {
            created,
            duplicates,
            queue_full,
        },
    ))
}

/// `job.status`: the state of each listed job the node may see.
pub(super) async fn job_status(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    job_ids: Vec<String>,
) -> bookclerk_library::Result<Frame> {
    if job_ids.len() > MAX_STATUS_JOBS {
        return Ok(error(id, "invalid", "too many job ids"));
    }
    let mut jobs = Vec::with_capacity(job_ids.len());
    for job_id in job_ids {
        let Some(job) = library.get_job(&job_id).await? else {
            continue;
        };
        if !remote_node_may_view_job(library, &host.node_id, &job).await? {
            continue;
        }
        jobs.push(JobStatusEntry {
            id: job.id,
            kind: job.kind.as_str().to_string(),
            state: job.state.as_str().to_string(),
            progress: job.progress,
            error_kind: job.error_kind,
            error_message: job.error_message,
        });
    }
    Ok(Frame::new(id, Body::JobStatuses { jobs }))
}
