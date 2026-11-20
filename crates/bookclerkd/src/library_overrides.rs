//! An Administrator's resolution decisions: pin a store listing to the
//! edition it sells, remove a pin, page the conflicts the resolver left,
//! and split a wrongly merged work (`docs/adr/library-data-model.md`).
//!
//! Only the operator, an Owner or an Administrator may call these routes.
//! A pin moves the listing at once and publishes the edition events the
//! library page index and the works search index refresh from.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::{
    ConflictCursor, EntityKind, LibraryError, PinOutcome, ResolutionConflict, SplitOutcome,
    WorkSplitBody, MAX_SPLIT_EXPRESSIONS,
};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::http_error::ErrorMessage;

/// An error answer: a status and JSON body, or the session-check refusal.
type OverrideError = axum::response::ErrorResponse;

/// Default page size of `GET /api/library/conflicts`.
const DEFAULT_CONFLICT_LIMIT: u64 = 50;

/// Body of `PUT /api/library/listings/{id}/pin`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinBody {
    /// The edition the listing sells.
    #[schema(example = "6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b")]
    edition_id: String,
}

/// A listing's pin.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PinJson {
    /// The listing.
    #[schema(example = "2f4e6a8c-1b3d-4e5f-9a7b-8c6d4e2f0a1b")]
    listing_id: String,
    /// The edition it is pinned to, and now sells.
    #[schema(example = "6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b")]
    edition_id: String,
    /// The edition it sold before, when the pin moved it; absent when it
    /// already sold the pinned edition.
    #[schema(example = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d")]
    moved_from: Option<String>,
}

/// Query string of `GET /api/library/conflicts`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ConflictQuery {
    /// `next_cursor` of the previous page.
    cursor: Option<String>,
    /// Page size, 1 to 200 (default 50).
    limit: Option<u64>,
}

/// One conflict the resolver could not decide.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ConflictJson {
    /// The listing whose resolution conflicted.
    #[schema(example = "2f4e6a8c-1b3d-4e5f-9a7b-8c6d4e2f0a1b")]
    listing_id: String,
    /// What conflicted: `strong_identifiers` (two editions share the
    /// listing's strong identifiers), `work_identifiers` (its work
    /// identifiers name two works), `ambiguous_work` (two works match its
    /// title and authors) or `work_kind_mismatch` (its work identifier names
    /// a work of another kind).
    #[schema(example = "strong_identifiers")]
    kind: String,
    /// The editions (`strong_identifiers`) or works (the other kinds)
    /// involved.
    #[schema(example = json!(["6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b", "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d"]))]
    subject_ids: Vec<String>,
    /// The listing's storefront PluginKey.
    #[schema(example = "plugin:audible")]
    plugin_key: String,
    /// The store's product id.
    #[schema(example = "B0CXYZ1234")]
    product_id: String,
    /// The listing's title as the store reported it.
    #[schema(example = "Dune")]
    title: String,
    /// The edition the listing sells now.
    #[schema(example = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d")]
    edition_id: String,
    /// When the conflict was last seen (RFC 3339).
    #[schema(example = "2026-11-15T21:00:00Z")]
    updated_at: String,
}

/// One page of conflicts.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ConflictPage {
    /// The conflicts, by listing.
    conflicts: Vec<ConflictJson>,
    /// Pass as `cursor` for the next page; absent on the last page.
    #[schema(
        example = "eyJsaXN0aW5nX2lkIjoiMmY0ZTZhOGMiLCJraW5kIjoiYW1iaWd1b3VzX3dvcmsiLCJzdWJqZWN0X2lkcyI6IltdIn0"
    )]
    next_cursor: Option<String>,
}

/// The Administrator behind `headers`, or the refusal. With `fixes`, the
/// caller is about to change how listings resolve, which an Owner can turn
/// off for one Administrator (the "Fix the catalog" switch).
async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    fixes: bool,
) -> Result<Option<i64>, OverrideError> {
    let control = fixes.then_some(bookclerk_library::control_plane::AdminControl::FixCatalog);
    crate::auth::library_administrator(state, headers, control)
        .await
        .map_err(|refusal| {
            refusal
                .answer(|status| {
                    let message = if status == StatusCode::UNAUTHORIZED {
                        "sign in first"
                    } else if status == StatusCode::INTERNAL_SERVER_ERROR {
                        "your permissions could not be read"
                    } else {
                        "only an Administrator can change how listings resolve"
                    };
                    (status, Json(ErrorMessage::new(message)))
                })
                .into()
        })
}

/// Logs a failed store call and returns the 500 answer.
fn failed(route: &str, err: &LibraryError) -> OverrideError {
    tracing::error!(error = %err, route, "library override call failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorMessage::new("the library could not be changed")),
    )
        .into()
}

/// The 404 answer for an unknown `kind` id.
fn not_found(kind: EntityKind, id: &str) -> OverrideError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorMessage::new(LibraryError::not_found(kind, id))),
    )
        .into()
}

/// Pins a store listing to the edition it sells.
///
/// The listing, with its holdings and offers, moves to the edition at once,
/// and its recorded conflicts are dropped. Later scans keep it there. Only
/// the operator, an Owner or an Administrator may pin.
#[utoipa::path(
    put,
    path = "/api/library/listings/{id}/pin",
    params(("id" = String, Path, description = "Listing id.")),
    request_body = PinBody,
    responses(
        (status = 200, description = "The pin, and the edition the listing left when it moved.", body = PinJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change how listings resolve"})),
        (status = 404, description = "No such listing or edition.", body = ErrorMessage, example = json!({"error": "edition 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 500, description = "The pin could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn put_listing_pin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<PinBody>,
) -> Result<Json<PinJson>, OverrideError> {
    let user_id = administrator(&state, &headers, true).await?;
    let library = state.library_snapshot().await;
    let outcome = library
        .pin_listing(&id, &body.edition_id, user_id)
        .await
        .map_err(|err| failed("put_listing_pin", &err))?;
    let moved_from = match outcome {
        PinOutcome::Moved { from } => Some(from),
        PinOutcome::Unchanged => None,
        PinOutcome::UnknownListing => return Err(not_found(EntityKind::Listing, &id)),
        PinOutcome::UnknownEdition => return Err(not_found(EntityKind::Edition, &body.edition_id)),
    };
    tracing::info!(listing_id = %id, edition_id = %body.edition_id, ?user_id, "listing pinned");
    Ok(Json(PinJson {
        listing_id: id,
        edition_id: body.edition_id,
        moved_from,
    }))
}

/// Removes a listing's pin.
///
/// The listing keeps selling the edition it was pinned to; only a new pin
/// moves it again.
#[utoipa::path(
    delete,
    path = "/api/library/listings/{id}/pin",
    params(("id" = String, Path, description = "Listing id.")),
    responses(
        (status = 204, description = "The pin is removed."),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change how listings resolve"})),
        (status = 404, description = "The listing has no pin.", body = ErrorMessage, example = json!({"error": "listing 2f4e6a8c-1b3d-4e5f-9a7b-8c6d4e2f0a1b has no pin"})),
        (status = 500, description = "The pin could not be removed.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn delete_listing_pin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, OverrideError> {
    let user_id = administrator(&state, &headers, true).await?;
    let library = state.library_snapshot().await;
    let removed = library
        .unpin_listing(&id)
        .await
        .map_err(|err| failed("delete_listing_pin", &err))?;
    if !removed {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new(format!("listing {id} has no pin"))),
        )
            .into());
    }
    tracing::info!(listing_id = %id, ?user_id, "listing pin removed");
    Ok(StatusCode::NO_CONTENT)
}

/// Pages the conflicts the resolver could not decide, for an Administrator
/// to settle with a pin.
#[utoipa::path(
    get,
    path = "/api/library/conflicts",
    params(ConflictQuery),
    responses(
        (status = 200, description = "One page of conflicts.", body = ConflictPage),
        (status = 400, description = "The cursor is not one this route made.", body = ErrorMessage, example = json!({"error": "invalid page cursor"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change how listings resolve"})),
        (status = 500, description = "The conflicts could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn list_conflicts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ConflictQuery>,
) -> Result<Json<ConflictPage>, OverrideError> {
    administrator(&state, &headers, false).await?;
    let after = query
        .cursor
        .as_deref()
        .map(ConflictCursor::decode)
        .transpose()
        .map_err(|err| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorMessage::new(err.to_string())),
            )
        })?;
    let library = state.library_snapshot().await;
    let (rows, next) = library
        .list_resolution_conflicts(
            after.as_ref(),
            query.limit.unwrap_or(DEFAULT_CONFLICT_LIMIT),
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "library conflicts read failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new("the library could not be read")),
            )
        })?;
    Ok(Json(ConflictPage {
        conflicts: rows.into_iter().map(conflict_json).collect(),
        next_cursor: next.map(|c| c.encode()),
    }))
}

/// The result of a split.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct SplitJson {
    /// The new work the expressions moved to.
    #[schema(example = "7d6c5b4a-3928-4f1e-8d7c-6b5a49382716")]
    work_id: String,
    /// The work that was split.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    split_from: String,
}

/// Splits a work an automatic merge joined wrongly.
///
/// The named expressions move, with their editions, listings and holdings,
/// to a new work that copies the work's title, credits, series and each
/// person's own state. New listings with the same title keep joining the
/// original work. Only the operator, an Owner or an Administrator may
/// split.
#[utoipa::path(
    post,
    path = "/api/library/works/{id}/split",
    params(("id" = String, Path, description = "Work id.")),
    request_body = WorkSplitBody,
    responses(
        (status = 200, description = "The new work.", body = SplitJson),
        (status = 400, description = "No expression, too many, an id over 64 bytes, or the work would be left empty.", body = ErrorMessage, example = json!({"error": "a split must leave the work at least one expression"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change how listings resolve"})),
        (status = 404, description = "No such work, or an expression is not the work's.", body = ErrorMessage, example = json!({"error": "work 0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b not found"})),
        (status = 409, description = "The work changed while it was being split.", body = ErrorMessage, example = json!({"error": "the work changed while it was being split; try again"})),
        (status = 500, description = "The split could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn post_work_split(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<WorkSplitBody>,
) -> Result<Json<SplitJson>, OverrideError> {
    let user_id = administrator(&state, &headers, true).await?;
    let bad = |message: String| -> OverrideError {
        (StatusCode::BAD_REQUEST, Json(ErrorMessage::new(message))).into()
    };
    // The same parse the fuzz target drives (`parse_work_split_request`).
    let request = match body.validate() {
        Ok(request) => request,
        Err(LibraryError::Invalid(message)) => return Err(bad(message)),
        Err(err) => return Err(failed("post_work_split", &err)),
    };
    let library = state.library_snapshot().await;
    let outcome = match library
        .split_work(&id, request.expression_ids(), user_id)
        .await
    {
        Ok(outcome) => outcome,
        Err(LibraryError::Conflict(_)) => {
            return Err((
                StatusCode::CONFLICT,
                Json(ErrorMessage::new(
                    "the work changed while it was being split; try again",
                )),
            )
                .into())
        }
        Err(err) => return Err(failed("post_work_split", &err)),
    };
    match outcome {
        SplitOutcome::Split { work_id } => {
            tracing::info!(split_from = %id, %work_id, ?user_id, "work split");
            Ok(Json(SplitJson {
                work_id,
                split_from: id,
            }))
        }
        SplitOutcome::UnknownWork => Err(not_found(EntityKind::Work, &id)),
        SplitOutcome::BadCount => Err(bad(format!(
            "name 1 to {MAX_SPLIT_EXPRESSIONS} expressions to move"
        ))),
        SplitOutcome::NotOfWork(ids) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new(format!(
                "work {id} has no expression {}",
                ids.join(", ")
            ))),
        )
            .into()),
        SplitOutcome::WouldEmpty => Err(bad(
            "a split must leave the work at least one expression".into()
        )),
    }
}

/// The wire form of one conflict.
fn conflict_json(c: ResolutionConflict) -> ConflictJson {
    ConflictJson {
        listing_id: c.listing_id,
        kind: c.kind,
        subject_ids: c.subject_ids,
        plugin_key: c.plugin_key,
        product_id: c.product_id,
        title: c.title,
        edition_id: c.edition_id,
        updated_at: c.updated_at,
    }
}

#[cfg(test)]
#[path = "library_overrides_tests.rs"]
mod tests;
