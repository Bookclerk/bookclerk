//! Library tags on a work: `GET`/`PUT /api/library/works/{id}/tags`.
//!
//! Library tags are labels everyone sees, set by an Administrator (or read
//! from a sidecar file), which the Library filters by and a patron scope
//! can include or exclude (`docs/adr/patron-scopes.md`). Because a tag can
//! decide what a scoped patron sees, setting them falls under the "Set
//! patron limits" Administrator switch.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::control_plane::AdminControl;
use bookclerk_library::{LibraryError, WorkTag, WorkTagsBody};
use serde::Serialize;

use crate::api::AppState;
use crate::http_error::ErrorMessage;

/// An error answer: a status and JSON body, or the session-check refusal.
type TagError = axum::response::ErrorResponse;

/// A work's library tags.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct WorkTagsJson {
    /// The work.
    #[schema(example = "6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b")]
    work_id: String,
    /// Every tag, by key, with where it came from.
    tags: Vec<WorkTag>,
}

/// The caller, refused unless they are the operator, an Owner, or an
/// Administrator (with `control` on, when given).
async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    control: Option<AdminControl>,
) -> Result<Option<i64>, TagError> {
    crate::auth::library_administrator(state, headers, control)
        .await
        .map_err(|refusal| {
            refusal
                .answer(|status| {
            let message = if status == StatusCode::UNAUTHORIZED {
                "sign in first"
            } else if status == StatusCode::INTERNAL_SERVER_ERROR {
                "your permissions could not be read"
            } else if control.is_some() {
                "only an Administrator whom an Owner lets set patron limits can change library tags"
            } else {
                "only an Administrator can read library tags here"
            };
            (status, Json(ErrorMessage::new(message)))
                })
                .into()
        })
}

/// Maps a store error to its answer.
fn store_error(route: &str, err: LibraryError) -> TagError {
    let answer: (StatusCode, Json<ErrorMessage>) = match err {
        LibraryError::NotFound(_) => (StatusCode::NOT_FOUND, Json(ErrorMessage::new(err))),
        LibraryError::Invalid(message) => {
            (StatusCode::BAD_REQUEST, Json(ErrorMessage::new(message)))
        }
        other => {
            tracing::error!(error = %other, route, "library tag call failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new(
                    "the library tags could not be read or saved",
                )),
            )
        }
    };
    answer.into()
}

/// Reads a work's library tags.
///
/// Only the operator, an Owner or an Administrator may call it; patrons see
/// tags through the Library's tag filter, inside their scope.
#[utoipa::path(
    get,
    path = "/api/library/works/{id}/tags",
    params(("id" = String, Path, description = "A top-level work id.")),
    responses(
        (status = 200, description = "The work's tags.", body = WorkTagsJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can read library tags here"})),
        (status = 404, description = "No such work, or a podcast episode (tags go on its show).", body = ErrorMessage, example = json!({"error": "work 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 500, description = "The tags could not be read.", body = ErrorMessage, example = json!({"error": "the library tags could not be read or saved"})),
    ),
)]
pub(crate) async fn get_work_tags(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<WorkTagsJson>, TagError> {
    administrator(&state, &headers, None).await?;
    let library = state.library_snapshot().await;
    let tags = library
        .work_tags(&id)
        .await
        .map_err(|err| store_error("get_work_tags", err))?;
    Ok(Json(WorkTagsJson { work_id: id, tags }))
}

/// Replaces the tags an Administrator set on a work.
///
/// Sidecar tags stay. The Library filter and patron scopes see the change
/// at once. Only the operator, an Owner, or an Administrator whom an Owner
/// lets set patron limits may call it.
#[utoipa::path(
    put,
    path = "/api/library/works/{id}/tags",
    params(("id" = String, Path, description = "A top-level work id.")),
    request_body = WorkTagsBody,
    responses(
        (status = 200, description = "The work's tags afterwards.", body = WorkTagsJson),
        (status = 400, description = "Too many tags, or one too long.", body = ErrorMessage, example = json!({"error": "a work has at most 32 library tags"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator, or an Owner turned off setting patron limits for them.", body = ErrorMessage, example = json!({"error": "only an Administrator whom an Owner lets set patron limits can change library tags"})),
        (status = 404, description = "No such work, or a podcast episode (tags go on its show).", body = ErrorMessage, example = json!({"error": "work 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 500, description = "The tags could not be saved.", body = ErrorMessage, example = json!({"error": "the library tags could not be read or saved"})),
    ),
)]
pub(crate) async fn put_work_tags(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<WorkTagsBody>,
) -> Result<Json<WorkTagsJson>, TagError> {
    let user_id = administrator(&state, &headers, Some(AdminControl::SetPatronLimits)).await?;
    // The same parse the fuzz target drives (`parse_work_tags_request`).
    let request = body
        .validate()
        .map_err(|err| store_error("put_work_tags", err))?;
    let library = state.library_snapshot().await;
    let tags = library
        .set_work_tags(&id, request.tags())
        .await
        .map_err(|err| store_error("put_work_tags", err))?;
    tracing::info!(work_id = %id, ?user_id, count = tags.len(), "library tags set");
    Ok(Json(WorkTagsJson { work_id: id, tags }))
}

#[cfg(test)]
#[path = "work_tags_tests.rs"]
mod tests;
