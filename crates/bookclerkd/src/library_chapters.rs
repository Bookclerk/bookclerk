//! An edition's chapters: read the list it plays, and, for an
//! Administrator, read every claim, set or clear the chapters by hand, and
//! pin a claim (`docs/adr/edition-chapters.md`).
//!
//! Anyone who may see the edition's work may read its chapters. Only the
//! operator, an Owner or an Administrator may read the claims or change
//! them, as with every other metadata edit. A change publishes
//! `edition_updated` with `fields: ["chapters"]` when the list the edition
//! plays changes.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::{
    parse_chapter_edit_request, parse_chapter_pin_request, resolve_chapters, Chapter, ChapterClaim,
    ChapterClaimWrite, ChapterEditBody, ChapterPinBody, ChapterSource, ChapterSourceKind,
    EditionChapters, EntityKind, LibraryError, LibraryStore, MAX_CHAPTER_EDIT_BYTES,
    MAX_CHAPTER_PIN_BYTES,
};
use serde::Serialize;

use crate::api::AppState;
use crate::http_error::ErrorMessage;
use crate::library_works::{caller_scope, require_in_scope};

/// An error answer: a status and JSON body.
type ChapterError = (StatusCode, Json<ErrorMessage>);

/// The chapters an edition plays.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct EditionChaptersJson {
    /// The edition.
    #[schema(example = "6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b")]
    edition_id: String,
    /// The chapters in pre-order, starts never decreasing; empty when the
    /// edition has none or an Administrator cleared them.
    chapters: Vec<Chapter>,
    /// The kind of source the chapters came from; absent when the edition
    /// has no chapters yet.
    source_kind: Option<ChapterSourceKind>,
    /// Which source of that kind (a PluginKey; empty for an edit or for
    /// Bookclerk's own processing).
    #[schema(example = "")]
    source_id: Option<String>,
    /// Length of the timeline the starts were measured on, in milliseconds.
    #[schema(example = 41_400_000)]
    runtime_ms: Option<u64>,
}

/// One source's chapter claim for an edition.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ChapterClaimJson {
    /// The kind of source.
    source_kind: ChapterSourceKind,
    /// Which source of that kind (a PluginKey; empty for an edit or for
    /// Bookclerk's own processing).
    #[schema(example = "plugin:audnexus")]
    source_id: String,
    /// Where a machine source ranks within its kind: `strong_accurate`,
    /// `strong_estimated` or `weak` for a provider, `chapters`, `tracks` or
    /// `unknown` for a store listing; empty otherwise.
    #[schema(example = "strong_accurate")]
    quality: String,
    /// SHA-256 of the claimed list, 64 lower-case hex characters.
    #[schema(example = "9f2c4e6a8b0d1f3e5a7c9b1d3f5e7a9c2b4d6f8e0a1c3e5b7d9f1a3c5e7b9d0f")]
    digest: String,
    /// The claimed chapters; absent while the list's write is unfinished.
    chapters: Option<Vec<Chapter>>,
    /// Length of the timeline the list was measured on, in milliseconds.
    #[schema(example = 41_400_000)]
    runtime_ms: Option<u64>,
    /// For a processed claim, the digest of the provider list it was
    /// aligned from.
    #[schema(example = "4b7d9f1a3c5e7b9d0f9f2c4e6a8b0d1f3e5a7c9b1d3f5e7a9c2b4d6f8e0a1c3e")]
    input_digest: Option<String>,
    /// Whether the edition plays this claim now.
    #[schema(example = true)]
    plays: bool,
    /// When an Administrator pinned it (RFC 3339); absent when unpinned.
    #[schema(example = "2026-11-20T19:30:00Z")]
    pinned_at: Option<String>,
    /// The person whose edit it is; absent for a machine source or the
    /// operator.
    #[schema(example = 2)]
    created_by_user_id: Option<i64>,
    /// When it last changed (RFC 3339).
    #[schema(example = "2026-11-20T19:30:00Z")]
    updated_at: String,
}

/// Every chapter claim for an edition.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ChapterClaimsJson {
    /// The claims, most recently changed first.
    claims: Vec<ChapterClaimJson>,
}

/// The Administrator behind `headers`, or the refusal. With `fixes`, the
/// caller is about to change an edition's chapters, which an Owner can turn
/// off for one Administrator (the "Fix the catalog" switch).
async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    fixes: bool,
) -> Result<Option<i64>, axum::response::ErrorResponse> {
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
                        "only an Administrator can change chapters"
                    };
                    (status, Json(ErrorMessage::new(message)))
                })
                .into()
        })
}

/// Maps a store error: 404 for a missing edition or claim, 400 for a
/// refused value, 500 otherwise.
fn store_failed(route: &str, err: LibraryError) -> ChapterError {
    match err {
        LibraryError::NotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new(err.to_string())),
        ),
        LibraryError::Invalid(message) => {
            (StatusCode::BAD_REQUEST, Json(ErrorMessage::new(message)))
        }
        other => {
            tracing::error!(error = %other, route, "library chapter call failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new("the library could not be changed")),
            )
        }
    }
}

/// Reads a JSON body of at most `max` bytes.
///
/// # Errors
///
/// 415 without the JSON content type, 413 for a longer body.
async fn json_bytes(request: Request, max: usize) -> Result<axum::body::Bytes, ChapterError> {
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(ErrorMessage::new(
                "send the body with Content-Type: application/json",
            )),
        ));
    }
    let too_long = || {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorMessage::new(format!(
                "the body is at most {max} bytes"
            ))),
        )
    };
    // One byte past the limit is enough to know the body is too long.
    let bytes = axum::body::to_bytes(request.into_body(), max.saturating_add(1))
        .await
        .map_err(|_| too_long())?;
    if bytes.len() > max {
        return Err(too_long());
    }
    Ok(bytes)
}

/// The wire form of what `edition_id` plays.
fn chapters_json(edition_id: String, found: Option<EditionChapters>) -> EditionChaptersJson {
    match found {
        Some(found) => EditionChaptersJson {
            edition_id,
            chapters: found.list.chapters().to_vec(),
            source_kind: Some(found.source_kind),
            source_id: Some(found.source_id),
            runtime_ms: found.runtime_ms,
        },
        None => EditionChaptersJson {
            edition_id,
            chapters: Vec::new(),
            source_kind: None,
            source_id: None,
            runtime_ms: None,
        },
    }
}

/// Reads what `edition_id` plays after a change, for the answer.
async fn current(
    library: &LibraryStore,
    route: &str,
    edition_id: String,
) -> Result<Json<EditionChaptersJson>, ChapterError> {
    let found = library
        .edition_chapters(&edition_id)
        .await
        .map_err(|err| store_failed(route, err))?;
    Ok(Json(chapters_json(edition_id, found)))
}

/// The wire form of one claim.
async fn claim_json(
    library: &LibraryStore,
    route: &str,
    claim: &ChapterClaim,
    plays: bool,
) -> Result<ChapterClaimJson, ChapterError> {
    let chapters = if claim.chapter_count.is_some() {
        library
            .chapter_list(&claim.digest)
            .await
            .map_err(|err| store_failed(route, err))?
            .map(|list| list.chapters().to_vec())
    } else {
        None
    };
    Ok(ChapterClaimJson {
        source_kind: claim.source_kind,
        source_id: claim.source_id.clone(),
        quality: claim.quality.as_str().to_string(),
        digest: claim.digest.as_str().to_string(),
        chapters,
        runtime_ms: claim.runtime_ms,
        input_digest: claim.input_digest.as_ref().map(|d| d.as_str().to_string()),
        plays,
        pinned_at: claim.pinned_at.clone(),
        created_by_user_id: claim.created_by_user_id,
        updated_at: claim.updated_at.clone(),
    })
}

/// Reads the chapters an edition plays.
///
/// The list comes from the claim the resolver chose: a pinned claim, else
/// an Administrator's edit, else the best machine source (Bookclerk's own
/// aligned chapters first).
#[utoipa::path(
    get,
    path = "/api/library/editions/{id}/chapters",
    params(("id" = String, Path, description = "Edition id (an `id` in a work's `editions`).")),
    responses(
        (status = 200, description = "The chapters; an empty list with no source when the edition has none yet.", body = EditionChaptersJson),
        (status = 401, description = "The session ended.", body = ErrorMessage, example = json!({"error": "sign in again"})),
        (status = 404, description = "No such edition, or one outside the caller's scope.", body = ErrorMessage, example = json!({"error": "edition 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn get_edition_chapters(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<EditionChaptersJson>, ChapterError> {
    const ROUTE: &str = "GET /api/library/editions/{id}/chapters";
    let library = state.library_snapshot().await;
    let scope = caller_scope(&state, &library, &headers, ROUTE).await?;
    let Some(work_id) = library
        .work_of_edition(&id)
        .await
        .map_err(|err| crate::library_works::read_failed(ROUTE, &err))?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new(LibraryError::not_found(
                EntityKind::Edition,
                &id,
            ))),
        ));
    };
    require_in_scope(
        &library,
        scope.as_ref(),
        &work_id,
        EntityKind::Edition,
        &id,
        ROUTE,
    )
    .await?;
    let found = library
        .edition_chapters(&id)
        .await
        .map_err(|err| crate::library_works::read_failed(ROUTE, &err))?;
    Ok(Json(chapters_json(id, found)))
}

/// Lists every source's chapter claim for an edition, with its chapters.
#[utoipa::path(
    get,
    path = "/api/library/editions/{id}/chapters/claims",
    params(("id" = String, Path, description = "Edition id.")),
    responses(
        (status = 200, description = "The claims, most recently changed first.", body = ChapterClaimsJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change chapters"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn list_chapter_claims(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> axum::response::Result<Json<ChapterClaimsJson>> {
    const ROUTE: &str = "GET /api/library/editions/{id}/chapters/claims";
    administrator(&state, &headers, false).await?;
    let library = state.library_snapshot().await;
    if library
        .work_of_edition(&id)
        .await
        .map_err(|err| store_failed(ROUTE, err))?
        .is_none()
    {
        return Err(store_failed(ROUTE, LibraryError::not_found(EntityKind::Edition, &id)).into());
    }
    let claims = library
        .chapter_claims(&id)
        .await
        .map_err(|err| store_failed(ROUTE, err))?;
    let winner = resolve_chapters(&claims);
    let mut out = Vec::with_capacity(claims.len());
    for claim in &claims {
        let plays = winner.is_some_and(|w| std::ptr::eq(w, claim));
        out.push(claim_json(&library, ROUTE, claim, plays).await?);
    }
    Ok(Json(ChapterClaimsJson { claims: out }))
}

/// Sets an edition's chapters by hand.
///
/// The edit is checked strictly: what is stored is exactly what was sent.
/// It wins over every machine source until it is removed or another claim
/// is pinned. An empty list clears the chapters.
#[utoipa::path(
    put,
    path = "/api/library/editions/{id}/chapters",
    params(("id" = String, Path, description = "Edition id.")),
    request_body = ChapterEditBody,
    responses(
        (status = 200, description = "The chapters the edition plays now (a pinned claim still wins over the edit).", body = EditionChaptersJson),
        (status = 400, description = "The body is not a chapter list, or a chapter breaks a rule.", body = ErrorMessage, example = json!({"error": "chapter 3 starts before chapter 2"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change chapters"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 413, description = "The body is over 2 MiB.", body = ErrorMessage, example = json!({"error": "the body is at most 2097152 bytes"})),
        (status = 415, description = "The body is not sent as JSON.", body = ErrorMessage, example = json!({"error": "send the body with Content-Type: application/json"})),
        (status = 500, description = "The chapters could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn put_edition_chapters(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    request: Request,
) -> axum::response::Result<Json<EditionChaptersJson>> {
    const ROUTE: &str = "PUT /api/library/editions/{id}/chapters";
    let user_id = administrator(&state, &headers, true).await?;
    let body = json_bytes(request, MAX_CHAPTER_EDIT_BYTES).await?;
    let list = parse_chapter_edit_request(&body).map_err(|err| store_failed(ROUTE, err))?;
    let library = state.library_snapshot().await;
    let count = list.chapters().len();
    library
        .record_chapter_claim(
            &id,
            &ChapterClaimWrite {
                source: ChapterSource::Person { user_id },
                list,
                runtime_ms: None,
            },
        )
        .await
        .map_err(|err| store_failed(ROUTE, err))?;
    tracing::info!(edition_id = %id, ?user_id, chapters = count, "chapters edited");
    Ok(current(&library, ROUTE, id).await?)
}

/// Removes an Administrator's chapter edit, so the edition plays the best
/// remaining claim again.
#[utoipa::path(
    delete,
    path = "/api/library/editions/{id}/chapters",
    params(("id" = String, Path, description = "Edition id.")),
    responses(
        (status = 200, description = "The chapters the edition plays now.", body = EditionChaptersJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change chapters"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 500, description = "The edit could not be removed.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn delete_edition_chapters(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> axum::response::Result<Json<EditionChaptersJson>> {
    const ROUTE: &str = "DELETE /api/library/editions/{id}/chapters";
    let user_id = administrator(&state, &headers, true).await?;
    let library = state.library_snapshot().await;
    library
        .remove_chapter_edit(&id)
        .await
        .map_err(|err| store_failed(ROUTE, err))?;
    tracing::info!(edition_id = %id, ?user_id, "chapter edit removed");
    Ok(current(&library, ROUTE, id).await?)
}

/// Pins one claim, so the edition plays it whatever the others say.
///
/// Pinning replaces any earlier pin on the edition's chapters.
#[utoipa::path(
    put,
    path = "/api/library/editions/{id}/chapters/pin",
    params(("id" = String, Path, description = "Edition id.")),
    request_body = ChapterPinBody,
    responses(
        (status = 200, description = "The chapters the edition plays now.", body = EditionChaptersJson),
        (status = 400, description = "The body does not name a claim.", body = ErrorMessage, example = json!({"error": "pin body: unknown variant `wizard`"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change chapters"})),
        (status = 404, description = "No such edition, or it has no such claim.", body = ErrorMessage, example = json!({"error": "metadata claim listing:plugin:audible not found"})),
        (status = 413, description = "The body is over 1 KiB.", body = ErrorMessage, example = json!({"error": "the body is at most 1024 bytes"})),
        (status = 415, description = "The body is not sent as JSON.", body = ErrorMessage, example = json!({"error": "send the body with Content-Type: application/json"})),
        (status = 500, description = "The pin could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn put_chapter_pin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    request: Request,
) -> axum::response::Result<Json<EditionChaptersJson>> {
    const ROUTE: &str = "PUT /api/library/editions/{id}/chapters/pin";
    let user_id = administrator(&state, &headers, true).await?;
    let body = json_bytes(request, MAX_CHAPTER_PIN_BYTES).await?;
    let pin = parse_chapter_pin_request(&body).map_err(|err| store_failed(ROUTE, err))?;
    let library = state.library_snapshot().await;
    library
        .pin_chapter_claim(&id, Some((pin.source_kind, &pin.source_id)), user_id)
        .await
        .map_err(|err| store_failed(ROUTE, err))?;
    tracing::info!(
        edition_id = %id,
        ?user_id,
        source_kind = pin.source_kind.as_str(),
        source_id = %pin.source_id,
        "chapter claim pinned"
    );
    Ok(current(&library, ROUTE, id).await?)
}

/// Removes the pin on an edition's chapters; the resolver chooses again.
#[utoipa::path(
    delete,
    path = "/api/library/editions/{id}/chapters/pin",
    params(("id" = String, Path, description = "Edition id.")),
    responses(
        (status = 200, description = "The chapters the edition plays now.", body = EditionChaptersJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change chapters"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 500, description = "The pin could not be removed.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn delete_chapter_pin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> axum::response::Result<Json<EditionChaptersJson>> {
    const ROUTE: &str = "DELETE /api/library/editions/{id}/chapters/pin";
    let user_id = administrator(&state, &headers, true).await?;
    let library = state.library_snapshot().await;
    library
        .pin_chapter_claim(&id, None, user_id)
        .await
        .map_err(|err| store_failed(ROUTE, err))?;
    tracing::info!(edition_id = %id, ?user_id, "chapter pin removed");
    Ok(current(&library, ROUTE, id).await?)
}

/// A queued chapter alignment.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ChapterAlignJson {
    /// The `chapter_align` job that does the work; it answers through the
    /// jobs API. An alignment already queued for the edition is returned
    /// rather than a second one.
    #[schema(example = "0f3c2a9e-5b1d-4c7e-8a6f-2d4b9e1c7a35")]
    job_id: String,
}

/// Queues the alignment of a metadata provider's chapters to the
/// edition's stored copy.
///
/// The job asks the providers for the edition's chapters by its ASIN,
/// reads a stored copy, snaps each chapter start to the spoken title, and
/// records the provider's list and the aligned list as claims; the edition
/// plays the aligned list unless an edit or a pin says otherwise.
#[utoipa::path(
    post,
    path = "/api/library/editions/{id}/chapters/realign",
    params(("id" = String, Path, description = "Edition id.")),
    responses(
        (status = 202, description = "The alignment is queued.", body = ChapterAlignJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can change chapters"})),
        (status = 404, description = "No such edition.", body = ErrorMessage, example = json!({"error": "edition 6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b not found"})),
        (status = 409, description = "The edition has no ASIN to ask providers by.", body = ErrorMessage, example = json!({"error": "the edition has no ASIN to ask metadata providers by"})),
        (status = 503, description = "The job queue is full.", body = ErrorMessage, example = json!({"error": "the job queue is full; try again later"})),
        (status = 500, description = "The job could not be queued.", body = ErrorMessage, example = json!({"error": "the library could not be changed"})),
    ),
)]
pub(crate) async fn post_chapter_realign(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> axum::response::Result<(StatusCode, Json<ChapterAlignJson>)> {
    const ROUTE: &str = "POST /api/library/editions/{id}/chapters/realign";
    let user_id = administrator(&state, &headers, true).await?;
    let library = state.library_snapshot().await;
    if library
        .playback_edition(&id)
        .await
        .map_err(|err| store_failed(ROUTE, err))?
        .is_none()
    {
        return Err(store_failed(ROUTE, LibraryError::not_found(EntityKind::Edition, &id)).into());
    }
    if library
        .edition_asins(&id)
        .await
        .map_err(|err| store_failed(ROUTE, err))?
        .is_empty()
    {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorMessage::new(
                "the edition has no ASIN to ask metadata providers by",
            )),
        )
            .into());
    }
    let admitted = crate::chapter_align::enqueue_chapter_align(
        &state,
        &id,
        bookclerk_library::JobTrigger::Api,
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, route = ROUTE, "could not queue a chapter alignment");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorMessage::new("the library could not be changed")),
        )
    })?;
    let job_id = match admitted {
        crate::jobs::AdmitJob::Created(job_id) | crate::jobs::AdmitJob::Duplicate(job_id) => job_id,
        crate::jobs::AdmitJob::QueueFull => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorMessage::new("the job queue is full; try again later")),
            )
                .into())
        }
    };
    tracing::info!(edition_id = %id, ?user_id, %job_id, "chapter alignment queued");
    Ok((StatusCode::ACCEPTED, Json(ChapterAlignJson { job_id })))
}

#[cfg(test)]
#[path = "library_chapters_tests.rs"]
mod tests;
