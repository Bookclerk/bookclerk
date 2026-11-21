//! On a clerk: asking a library this install holds a card for to get a
//! copy of a book, from one of its branches or by downloading it on one of
//! its own store accounts.
//!
//! `POST /api/config/clerk/request` `{library_id, work_id, from, node_id?}`
//! sends one of the library's own member routes with the library card, as
//! the person's browser session there would: `POST /api/branch-requests`
//! when `from` is `branch`, `POST /api/download-requests` when it is
//! `library`. The library decides everything: whether the person's member
//! controls allow it, whether a branch it trusts lists the work (and which
//! branch is asked), or whether one of its accounts holds it.
//! The clerk reads only the status of the answer, never its body, so
//! nothing the library writes is parsed here; the request's progress shows
//! on the library like any other member request.
//!
//! The work id is the library's own (the grouping key its pages and its
//! Discover and search answers name), never one of this install's.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_library::control_plane::{is_valid_node_id, load_clerk_card, load_clerk_link};
use bookclerk_node_client::OwnerEndpoint;
use bookclerk_node_proto::{is_work_ref, CLERK_AUTH_SCHEME};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::api::AppState;
use crate::auth::UserCaller;
use crate::library_link::join::join_error;
use crate::node_api::branch::require_branch_admin;

/// The library's member route for asking a branch for a book.
const BRANCH_REQUESTS_PATH: &str = "/api/branch-requests";

/// The library's member route for asking it to download a book it holds.
const DOWNLOAD_REQUESTS_PATH: &str = "/api/download-requests";

/// Where a clerk's request asks the library to get a copy from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClerkRequestFrom {
    /// One of the library's branches lists the book; the library asks it
    /// to send the book.
    Branch,
    /// One of the library's own store accounts holds the book; the library
    /// downloads it.
    Library,
}

/// How long the clerk waits for the library to take the request.
const LIBRARY_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// `POST /api/config/clerk/request` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClerkRequestBody {
    /// The library to ask: one this install holds an active card for.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    library_id: String,
    /// The library's own work id: 1 to 64 ASCII letters, digits and `-`.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    work_id: String,
    /// Where the copy comes from: the edition's `request.from` in the
    /// window books.
    from: ClerkRequestFrom,
    /// The library's branch to ask, from the work's `available_through`
    /// there; absent asks the first branch that lists it. Only with
    /// `from` `branch`.
    #[serde(default)]
    #[schema(example = "9b1c7d2e-4f3a-4b6c-8d1e-2f3a4b5c6d7e")]
    node_id: Option<String>,
}

/// JSON body for `POST /api/config/clerk/request`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ClerkRequestSent {
    /// The library that took the request.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub library_id: String,
}

/// The body the library's `POST /api/branch-requests` and
/// `POST /api/download-requests` read (the second takes no `node_id`).
#[derive(Serialize)]
struct LibraryRequest<'a> {
    /// The library's work id.
    work_id: &'a str,
    /// The branch to ask, when the person named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    node_id: Option<&'a str>,
}

/// What the clerk answers for the library's status `status` to a request
/// sent `from` there.
fn answer_for(from: ClerkRequestFrom, status: u16, library_id: String) -> Response {
    match (status, from) {
        (200..=299, _) => Json(ClerkRequestSent { library_id }).into_response(),
        (401, _) => join_error(
            StatusCode::BAD_GATEWAY,
            "card_refused",
            "the library no longer accepts this library card; sign in to it again",
        ),
        (403, _) => join_error(
            StatusCode::FORBIDDEN,
            "not_allowed",
            "the library's administrator hasn't allowed you to ask it for books",
        ),
        (404, ClerkRequestFrom::Branch) => join_error(
            StatusCode::NOT_FOUND,
            "not_listed",
            "no branch of that library lists this book now",
        ),
        (404, ClerkRequestFrom::Library) => join_error(
            StatusCode::NOT_FOUND,
            "not_listed",
            "no store account of that library holds this book now",
        ),
        (409, ClerkRequestFrom::Library) => join_error(
            StatusCode::CONFLICT,
            "already_there",
            "the library already has a copy; it shows here after the next refresh",
        ),
        (503, _) => join_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "library_busy",
            "the library's job queue is full; try again later",
        ),
        _ => join_error(
            StatusCode::BAD_GATEWAY,
            "library_error",
            "the library could not take the request",
        ),
    }
}

/// `POST /api/config/clerk/request`: asks a library this install holds a
/// card for, with the card, to get a copy of a book: from one of its
/// branches (`from` `branch`) or by downloading it on one of its own store
/// accounts (`from` `library`).
///
/// Only an active Owner of this install, or its operator, may send it: the
/// request goes out with this install's library card.
#[utoipa::path(
    post,
    path = "/api/config/clerk/request",
    request_body = ClerkRequestBody,
    responses(
        (status = 200, description = "The library took the request; it shows there with the person's other requests, or as a download in its jobs.", body = ClerkRequestSent),
        (status = 400, description = "The work or node id is malformed, or a node id was given with `from` `library` (`invalid_request`).", body = crate::http_error::ErrorDetail, example = json!({"error": "invalid_request", "detail": "invalid work_id"})),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The user is not an active Owner (`forbidden`), or the library's member controls turn requesting books off for the person (`not_allowed`).", body = crate::http_error::ErrorAnswer, example = json!({"error": "not_allowed", "detail": "the library's administrator hasn't allowed you to ask its branches for books"})),
        (status = 404, description = "This install holds no active card for that library (`not_found`), or no branch of the library lists the work now, or none of its store accounts holds it (`not_listed`).", body = crate::http_error::ErrorDetail, example = json!({"error": "not_listed", "detail": "no branch of that library lists this book now"})),
        (status = 409, description = "The library already has a copy of the work (`already_there`).", body = crate::http_error::ErrorDetail, example = json!({"error": "already_there", "detail": "the library already has a copy; it shows here after the next refresh"})),
        (status = 502, description = "The library could not be reached (`unreachable`), no longer accepts the card (`card_refused`), or answered with another error (`library_error`).", body = crate::http_error::ErrorDetail, example = json!({"error": "unreachable", "detail": "the library could not be reached"})),
        (status = 503, description = "The library's job queue is full (`library_busy`).", body = crate::http_error::ErrorDetail, example = json!({"error": "library_busy", "detail": "the library's job queue is full; try again later"})),
        (status = 500, description = "The library database could not be read.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn post_clerk_request(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<ClerkRequestBody>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    let work_id = body.work_id.trim();
    if !is_work_ref(work_id) {
        return join_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid work_id",
        );
    }
    let node_id = body.node_id.as_deref().map(str::trim);
    if node_id.is_some_and(|node| !is_valid_node_id(node)) {
        return join_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid node_id",
        );
    }
    if body.from == ClerkRequestFrom::Library && node_id.is_some() {
        return join_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "node_id names a branch; a download by the library takes none",
        );
    }
    let store = state.library_snapshot().await;
    let link = match load_clerk_link(&store, body.library_id.trim()).await {
        Ok(Some(link)) if link.is_active() => link,
        Ok(_) => return join_error(StatusCode::NOT_FOUND, "not_found", "no such library card"),
        Err(err) => return crate::config_authority::events_error(err),
    };
    let card = match load_clerk_card(&store, &link).await {
        Ok(card) => card,
        Err(err) => return crate::config_authority::events_error(err),
    };
    bookclerk_config::register_secret(card.expose());
    let unreachable = || {
        join_error(
            StatusCode::BAD_GATEWAY,
            "unreachable",
            "the library could not be reached",
        )
    };
    let Ok(endpoint) = OwnerEndpoint::new(&link.origin, link.spki_pin.as_deref()) else {
        return unreachable();
    };
    let Ok(request) = serde_json::to_vec(&LibraryRequest { work_id, node_id }) else {
        return unreachable();
    };
    let authorization = Zeroizing::new(format!("{CLERK_AUTH_SCHEME} {}", card.expose()));
    let path = match body.from {
        ClerkRequestFrom::Branch => BRANCH_REQUESTS_PATH,
        ClerkRequestFrom::Library => DOWNLOAD_REQUESTS_PATH,
    };
    let sent = tokio::time::timeout(
        LIBRARY_REQUEST_TIMEOUT,
        endpoint.post_json_authorized(path, &authorization, &request),
    )
    .await;
    match sent {
        // Only the status is read: the library's body is not parsed here.
        Ok(Ok((status, _))) => {
            tracing::info!(library_id = %link.library_id, status, from = ?body.from, "asked the library for a copy of a book");
            answer_for(body.from, status, link.library_id)
        }
        Ok(Err(err)) => {
            tracing::warn!(library_id = %link.library_id, error = %err, "could not reach the library to ask for a book");
            unreachable()
        }
        Err(_) => {
            tracing::warn!(library_id = %link.library_id, "the library did not answer the book request in time");
            unreachable()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn each_library_status_gets_its_own_answer() {
        use ClerkRequestFrom::{Branch, Library};
        for (from, status, want, error) in [
            (Branch, 200, StatusCode::OK, None),
            (Library, 200, StatusCode::OK, None),
            (Branch, 401, StatusCode::BAD_GATEWAY, Some("card_refused")),
            (Library, 403, StatusCode::FORBIDDEN, Some("not_allowed")),
            (Branch, 404, StatusCode::NOT_FOUND, Some("not_listed")),
            (Library, 404, StatusCode::NOT_FOUND, Some("not_listed")),
            (Library, 409, StatusCode::CONFLICT, Some("already_there")),
            (Branch, 409, StatusCode::BAD_GATEWAY, Some("library_error")),
            (
                Branch,
                503,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("library_busy"),
            ),
            (Branch, 500, StatusCode::BAD_GATEWAY, Some("library_error")),
            (Library, 302, StatusCode::BAD_GATEWAY, Some("library_error")),
        ] {
            let response = answer_for(from, status, "lib-1".into());
            assert_eq!(
                response.status(),
                want,
                "library answered {status} to {from:?}"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .expect("body");
            let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
            match error {
                Some(error) => assert_eq!(body["error"], error),
                None => assert_eq!(body["library_id"], "lib-1"),
            }
        }
    }
}
