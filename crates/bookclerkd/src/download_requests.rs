//! Download requests on this library: `POST /api/download-requests` asks
//! the library to download a book it holds on one of its own store
//! accounts but has no copy of yet (`control_plane::plan_download_request`).
//!
//! The request becomes an ordinary acquire of one edition on the account
//! that holds it, paced like any download the host starts for someone.
//! Once the copy is there, the patron's clerk takes it like any other
//! book the library has a copy of.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::control_plane::{
    plan_download_request, DownloadRequestBody, MemberControl, ScanRequester,
};
use bookclerk_library::{JobTrigger, LibraryError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::AppState;
use crate::jobs::{enqueue_acquire, AcquireAsk, AdmitJob, JobUnroutable};

/// The refusal a patron whose member controls turn requests off gets.
const REQUESTS_NOT_ALLOWED: &str =
    "this library's administrator hasn't allowed you to ask for books";

/// `POST /api/download-requests` answer.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DownloadRequestQueued {
    /// The acquire job, as `/api/jobs` lists it.
    #[schema(example = "acquire-3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    pub job_id: String,
    /// The edition being downloaded.
    #[schema(example = "5c2e8f1a-9b3d-4e7f-a1c2-d3e4f5a6b7c8")]
    pub edition_id: String,
    /// `true` when a download of that edition was already waiting or
    /// running; `job_id` is that download.
    #[schema(example = false)]
    pub already_queued: bool,
}

/// Asks this library to download a book it holds but has no copy of.
///
/// The work must be on one of the library's store accounts, with no
/// edition of it downloaded yet. The library downloads the first such
/// edition (in edition id order) on the account that holds it, counted
/// against that account's pace. Asking again while that download waits
/// or runs returns it. A library card (a clerk) may ask, as may any
/// signed-in patron whose member controls allow requesting books.
#[utoipa::path(
    post,
    path = "/api/download-requests",
    request_body = DownloadRequestBody,
    responses(
        (status = 200, description = "The download was queued.", body = DownloadRequestQueued),
        (status = 400, description = "The work id is malformed.", body = String, content_type = "text/plain", example = json!("invalid work_id")),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller's member controls turn requesting books off.", body = String, content_type = "text/plain", example = json!("this library's administrator hasn't allowed you to ask for books")),
        (status = 404, description = "No store account of this library holds the work.", body = String, content_type = "text/plain", example = json!("work 0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b on this library's store accounts not found")),
        (status = 409, description = "The library already has a copy, or no node can run the download.", body = String, content_type = "text/plain", example = json!("this library already has a copy of work 0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")),
        (status = 503, description = "The job queue is full or admission is paused; try again later.", body = String, content_type = "text/plain", example = json!("the job queue is full")),
        (status = 500, description = "The library could not be read or written.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
    ),
)]
pub(crate) async fn create_download_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DownloadRequestBody>,
) -> Result<Json<DownloadRequestQueued>, (StatusCode, String)> {
    // The same parse the fuzz target drives (`parse_download_request`).
    let ask = body
        .validate()
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    crate::patron_scopes::require_control(
        &state,
        &headers,
        MemberControl::RequestBooks,
        REQUESTS_NOT_ALLOWED,
    )
    .await
    .map_err(|(status, Json(body))| (status, body.error))?;
    let work_id = ask.work_id.as_str();
    let library = state.library_snapshot().await;
    let plan = plan_download_request(&library, work_id)
        .await
        .map_err(|err| match err {
            LibraryError::NotFound(_) => (StatusCode::NOT_FOUND, err.to_string()),
            LibraryError::Conflict(_) => (StatusCode::CONFLICT, err.to_string()),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?;
    // The library's own download: any of its accounts may serve it, and
    // it waits for that account's pace like other downloads the host
    // starts on someone's behalf.
    let admits = enqueue_acquire(
        Arc::clone(&state),
        ScanRequester::Operator,
        AcquireAsk {
            titles: vec![plan.title.clone()],
            paced: true,
            ..AcquireAsk::default()
        },
        JobTrigger::Api,
    )
    .await
    .map_err(|err| {
        if err.downcast_ref::<JobUnroutable>().is_some() {
            (StatusCode::CONFLICT, err.to_string())
        } else if err.to_string().contains("admission paused") {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        }
    })?;
    let (job_id, already_queued) = match admits.into_iter().next() {
        Some(AdmitJob::Created(id)) => (id, false),
        Some(AdmitJob::Duplicate(id)) => (id, true),
        Some(AdmitJob::QueueFull) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "the job queue is full".into(),
            ))
        }
        // The planner found nothing left to fetch for that title (it was
        // downloaded since the plan read it).
        None => {
            return Err((
                StatusCode::CONFLICT,
                format!("this library already has a copy of work {work_id}"),
            ))
        }
    };
    if !already_queued {
        let actor = match crate::auth::caller_portal_identity(&state, &headers).await {
            Some(identity) => format!("portal:{}", identity.id),
            None => "operator".to_string(),
        };
        let detail = json!({
            "job_id": job_id,
            "edition_id": plan.edition_id,
            "work_id": work_id,
        })
        .to_string();
        let _ = library
            .insert_security_audit_event(&actor, "download_request_queued", Some(&detail))
            .await;
    }
    Ok(Json(DownloadRequestQueued {
        job_id,
        edition_id: plan.edition_id,
        already_queued,
    }))
}

#[cfg(test)]
#[path = "download_requests_tests.rs"]
pub(crate) mod tests;
