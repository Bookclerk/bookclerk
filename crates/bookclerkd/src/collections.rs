//! The library's collections: `GET`/`PUT /api/library/collections`.
//!
//! A collection is a named part of the library, defined by storage key
//! prefixes, which the Library filters by and a patron scope can include
//! or exclude (`docs/adr/patron-scopes.md`). Because collections decide
//! what a scoped patron sees, changing them falls under the "Set patron
//! limits" Administrator switch. A change rebuilds the page index.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::control_plane::{
    library_collections, set_library_collections, verify_patron_administrator, AdminControl,
    ConfigActor, LibraryCollections,
};
use bookclerk_library::LibraryError;

use crate::api::AppState;
use crate::http_error::ErrorMessage;

/// An error answer: a status and JSON body, or the session-check refusal.
type CollectionsError = axum::response::ErrorResponse;

/// The caller, refused unless they are the operator, an Owner, or an
/// Administrator (with `control` on, when given).
async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    control: Option<AdminControl>,
) -> Result<Option<i64>, CollectionsError> {
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
                "only an Administrator whom an Owner lets set patron limits can change collections"
            } else {
                "only an Administrator can read the collections here"
            };
            (status, Json(ErrorMessage::new(message)))
                })
                .into()
        })
}

/// Maps a library error to its answer.
fn library_error(route: &str, err: LibraryError) -> CollectionsError {
    let answer: (StatusCode, Json<ErrorMessage>) = match err {
        LibraryError::Invalid(message) => {
            (StatusCode::BAD_REQUEST, Json(ErrorMessage::new(message)))
        }
        LibraryError::Conflict(message) => (StatusCode::CONFLICT, Json(ErrorMessage::new(message))),
        other => {
            tracing::error!(error = %other, route, "collections call failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new(
                    "the collections could not be read or saved",
                )),
            )
        }
    };
    answer.into()
}

/// Reads the library's collections.
///
/// Only the operator, an Owner or an Administrator may call it; patrons see
/// collection names through the Library's collection filter, inside their
/// scope.
#[utoipa::path(
    get,
    path = "/api/library/collections",
    responses(
        (status = 200, description = "The collections, in the order shown.", body = LibraryCollections),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can read the collections here"})),
        (status = 500, description = "The collections could not be read.", body = ErrorMessage, example = json!({"error": "the collections could not be read or saved"})),
    ),
)]
pub(crate) async fn get_collections(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<LibraryCollections>, CollectionsError> {
    administrator(&state, &headers, None).await?;
    let library = state.library_snapshot().await;
    library_collections(&library)
        .await
        .map(Json)
        .map_err(|err| library_error("GET /api/library/collections", err))
}

/// Replaces the library's collections.
///
/// Names are trimmed and must differ ignoring case; each prefix gets a
/// closing `/`. When the collections change, the page index is rebuilt so
/// the Library filter and patron scopes follow. Only the operator, an
/// Owner, or an Administrator whom an Owner lets set patron limits may
/// call it.
#[utoipa::path(
    put,
    path = "/api/library/collections",
    request_body = LibraryCollections,
    responses(
        (status = 200, description = "The collections after the change.", body = LibraryCollections),
        (status = 400, description = "The collections are invalid.", body = ErrorMessage, example = json!({"error": "invalid collections: two collections are named Kids"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator, or an Owner turned off setting patron limits for them.", body = ErrorMessage, example = json!({"error": "only an Administrator whom an Owner lets set patron limits can change collections"})),
        (status = 409, description = "The collections kept changing underneath the write; retry.", body = ErrorMessage, example = json!({"error": "core.collections kept changing; retry"})),
        (status = 500, description = "The collections could not be saved.", body = ErrorMessage, example = json!({"error": "the collections could not be read or saved"})),
    ),
)]
pub(crate) async fn put_collections(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LibraryCollections>,
) -> Result<Json<LibraryCollections>, CollectionsError> {
    const ROUTE: &str = "PUT /api/library/collections";
    let user_id = administrator(&state, &headers, Some(AdminControl::SetPatronLimits)).await?;
    let library = state.library_snapshot().await;
    let actor = match user_id {
        None => ConfigActor::Operator {
            id: "operator".into(),
        },
        Some(user_id) => verify_patron_administrator(&library, user_id)
            .await
            .map_err(|err| library_error(ROUTE, err))?,
    };
    let before = library_collections(&library)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    // The same validation the fuzz target drives (`parse_collections_request`).
    let stored = set_library_collections(&library, &actor, body)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    if stored.body != before {
        if let Err(err) = crate::library_index::enqueue_rebuild(&state).await {
            // The heartbeat queues it soon anyway: the build is dropped.
            tracing::warn!(error = %err, "could not queue the library index rebuild");
        }
        let count = stored.body.collections.len();
        let audit_actor = user_id.map_or_else(|| "operator".to_string(), |id| format!("user:{id}"));
        let _ = library
            .insert_security_audit_event(
                &audit_actor,
                "collections",
                Some(&format!(r#"{{"collections":{count}}}"#)),
            )
            .await;
    }
    Ok(Json(stored.body))
}

#[cfg(test)]
#[path = "collections_tests.rs"]
mod tests;
