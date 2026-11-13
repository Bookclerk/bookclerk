//! Interlibrary requests on this library: `POST /api/branch-requests`
//! asks a branch for a book it listed (`control_plane::branch_requests`).
//!
//! The request becomes a `branch_request` job pinned to the branch's node,
//! and the branch is woken to claim it. The branch takes it automatically
//! for anything it listed; the book reaches this library through the
//! branch's catalog sync and is held through that branch.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::control_plane::{
    notify_requests_queued, plan_branch_request, BranchRequestBody,
};
use bookclerk_library::{EnqueueJobSpec, JobKind, JobTrigger, LibraryError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::AppState;
use crate::jobs::{admit, queue_limits, AdmitJob};

/// `POST /api/branch-requests` answer.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BranchRequestQueued {
    /// The request's job (kind `branch_request`), as `/api/jobs` lists it.
    #[schema(example = "branch_request-5e0f2a1c-7b3d-4c6e-8f9a-1b2c3d4e5f6a")]
    pub job_id: String,
    /// The branch the request went to.
    #[schema(example = "9b1c7d2e-4f3a-4b6c-8d1e-2f3a4b5c6d7e")]
    pub node_id: String,
    /// `true` when a request for this work to this branch was already
    /// waiting or being taken; `job_id` is that request.
    #[schema(example = false)]
    pub already_queued: bool,
}

/// Asks a branch for a book it listed.
///
/// The work must currently be available through a branch this library
/// trusts (its `available_through` in Discover and search). The branch
/// takes the request automatically for anything it listed; the book
/// arrives with the branch's catalog sync and is then held through that
/// branch. Asking again while a request for the same work and branch
/// waits returns that request.
#[utoipa::path(
    post,
    path = "/api/branch-requests",
    request_body = BranchRequestBody,
    responses(
        (status = 200, description = "The request was queued for the branch.", body = BranchRequestQueued),
        (status = 400, description = "The work or node id is malformed.", body = String, content_type = "text/plain", example = json!("invalid work_id")),
        (status = 401, description = "The caller is not signed in."),
        (status = 404, description = "No branch this library trusts (or not the named one) currently lists the work.", body = String, content_type = "text/plain", example = json!("book not found: work 0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b through a branch")),
        (status = 503, description = "The job queue is full; try again later.", body = String, content_type = "text/plain", example = json!("the job queue is full")),
        (status = 500, description = "The library could not be read or written.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
    ),
)]
pub(crate) async fn create_branch_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BranchRequestBody>,
) -> Result<Json<BranchRequestQueued>, (StatusCode, String)> {
    // The same parse the fuzz target drives (`parse_branch_request`).
    let ask = body
        .validate()
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    let work_id = ask.work_id.as_str();
    let node_id = ask.node_id.as_deref();
    let library = state.library_snapshot().await;
    let plan = plan_branch_request(&library, work_id, node_id, JobTrigger::Api)
        .await
        .map_err(|err| match err {
            LibraryError::NotFound(_) => (StatusCode::NOT_FOUND, err.to_string()),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?;
    let (max_pending, max_attempts) = queue_limits(&state).await;
    let outcome = admit(
        &state,
        EnqueueJobSpec {
            kind: JobKind::BranchRequest,
            payload: plan.payload,
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
    .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let (job_id, already_queued) = match outcome {
        AdmitJob::Created(id) => (id, false),
        AdmitJob::Duplicate(id) => (id, true),
        AdmitJob::QueueFull => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "the job queue is full".into(),
            ))
        }
    };
    if !already_queued {
        notify_requests_queued(&library, &plan.node_id).await;
        let actor = match crate::auth::caller_portal_identity(&state, &headers).await {
            Some(identity) => format!("portal:{}", identity.id),
            None => "operator".to_string(),
        };
        let detail = json!({
            "job_id": job_id,
            "node_id": plan.node_id,
            "work_id": work_id,
        })
        .to_string();
        let _ = library
            .insert_security_audit_event(&actor, "branch_request_queued", Some(&detail))
            .await;
    }
    Ok(Json(BranchRequestQueued {
        job_id,
        node_id: plan.node_id,
        already_queued,
    }))
}

#[cfg(test)]
#[path = "branch_requests_tests.rs"]
mod tests;
