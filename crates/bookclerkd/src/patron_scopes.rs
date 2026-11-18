//! Patron scope routes: an Administrator reads and sets which part of the
//! library a patron sees, per patron and as the library's default (see
//! `docs/adr/patron-scopes.md`).
//!
//! The scope itself lives in `bookclerk_library::control_plane`; the
//! library pages apply it (see `library_works`).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::control_plane::{
    assigned_patron_scope, default_patron_scope, role_is_scoped, set_default_patron_scope,
    set_patron_scope, verify_patron_administrator, AssignedScope, ConfigActor, PatronScope,
    PutUserScope,
};
use bookclerk_library::LibraryError;
use serde::Serialize;

use crate::api::AppState;
use crate::auth::{authorize_provisioner, Provisioner};
use crate::http_error::ErrorMessage;

/// Error answer of the scope routes: a status and an `error` message.
type ScopeError = (StatusCode, Json<ErrorMessage>);

/// A patron's scope as the scope routes report it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct UserScopeJson {
    /// The user (`users.id`).
    #[schema(example = 7)]
    user_id: i64,
    /// The scope that applies to them: their own, or the library's default.
    scope: PatronScope,
    /// True when an Administrator gave them this scope; false when they
    /// follow the library's default.
    #[schema(example = false)]
    own: bool,
    /// Whether the scope applies to the user's role: false for an Owner or
    /// Administrator, who always see the whole library.
    #[schema(example = true)]
    applies: bool,
}

/// The library's default scope as the scope routes report it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DefaultScopeJson {
    /// The scope patrons without their own follow.
    scope: PatronScope,
}

/// The 500 answer for a failed read or write, logged with its route.
fn failed(route: &str, err: &LibraryError) -> ScopeError {
    tracing::error!(error = %err, route, "patron scope route failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorMessage::new(
            "the patron scope could not be read or written",
        )),
    )
}

/// An answer with only a status and a message.
fn refused(status: StatusCode, message: impl std::fmt::Display) -> ScopeError {
    (status, Json(ErrorMessage::new(message)))
}

/// Maps a library error to the route's answer: 400 for a scope outside its
/// bounds, 404 for an unknown user, 403 for a refused write, 500 otherwise.
fn library_error(route: &str, err: LibraryError) -> ScopeError {
    match err {
        LibraryError::Invalid(message) => refused(StatusCode::BAD_REQUEST, message),
        LibraryError::NotFound(missing) => refused(StatusCode::NOT_FOUND, missing),
        LibraryError::Forbidden(message) => refused(StatusCode::FORBIDDEN, message),
        other => failed(route, &other),
    }
}

/// The provisioner behind `headers`, or the refusal as an error answer.
async fn provisioner(state: &AppState, headers: &HeaderMap) -> Result<Provisioner, ScopeError> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    authorize_provisioner(state, &auth, headers, &library)
        .await
        .map_err(|status| {
            refused(
                status,
                status
                    .canonical_reason()
                    .unwrap_or("refused")
                    .to_lowercase(),
            )
        })
}

/// Loads `user_id` and checks that `actor` may manage them.
///
/// # Errors
///
/// 404 for an unknown user, 403 when the actor may not manage their role,
/// 500 when the read fails.
async fn managed_user(
    library: &bookclerk_library::LibraryStore,
    actor: Provisioner,
    user_id: i64,
    route: &str,
) -> Result<bookclerk_library::UserRecord, ScopeError> {
    let user = library
        .get_user(user_id)
        .await
        .map_err(|err| failed(route, &err))?
        .ok_or_else(|| {
            refused(
                StatusCode::NOT_FOUND,
                LibraryError::not_found(bookclerk_library::EntityKind::User, user_id),
            )
        })?;
    if !actor.can_manage_target(user.role, actor.user_id() == Some(user_id)) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "you may not manage this user",
        ));
    }
    Ok(user)
}

/// The wire form of an assigned scope.
fn user_scope_json(user: &bookclerk_library::UserRecord, assigned: AssignedScope) -> UserScopeJson {
    UserScopeJson {
        user_id: user.id,
        scope: assigned.scope,
        own: assigned.own,
        applies: role_is_scoped(user.role),
    }
}

/// Reads which part of the library a patron sees.
#[utoipa::path(
    get,
    path = "/api/users/{id}/scope",
    params(("id" = i64, Path, description = "User id.")),
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The patron's scope.", body = UserScopeJson),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller may not manage this user.", body = ErrorMessage, example = json!({"error": "you may not manage this user"})),
        (status = 404, description = "No such user.", body = ErrorMessage, example = json!({"error": "user 7 not found"})),
        (status = 500, description = "The scope could not be read.", body = ErrorMessage, example = json!({"error": "the patron scope could not be read or written"})),
    ),
)]
pub(crate) async fn get_user_scope(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> Result<Json<UserScopeJson>, ScopeError> {
    const ROUTE: &str = "GET /api/users/{id}/scope";
    let actor = provisioner(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let user = managed_user(&library, actor, user_id, ROUTE).await?;
    let assigned = assigned_patron_scope(&library, user_id)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    Ok(Json(user_scope_json(&user, assigned)))
}

/// Gives a patron their own scope, or returns them to the library's
/// default with `"scope": null`.
///
/// Nobody sets their own scope. Owners and Administrators always see the
/// whole library, so a scope stored for one only applies if their role
/// changes to Member or Host.
#[utoipa::path(
    put,
    path = "/api/users/{id}/scope",
    params(("id" = i64, Path, description = "User id.")),
    request_body = PutUserScope,
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The patron's scope after the change.", body = UserScopeJson),
        (status = 400, description = "The scope is invalid.", body = ErrorMessage, example = json!({"error": "invalid patron scope: a scope rule needs a value"})),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller may not manage this user, or tried to set their own scope.", body = ErrorMessage, example = json!({"error": "you may not set your own scope"})),
        (status = 404, description = "No such user.", body = ErrorMessage, example = json!({"error": "user 7 not found"})),
        (status = 500, description = "The scope could not be written.", body = ErrorMessage, example = json!({"error": "the patron scope could not be read or written"})),
    ),
)]
pub(crate) async fn put_user_scope(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(body): Json<PutUserScope>,
) -> Result<Json<UserScopeJson>, ScopeError> {
    const ROUTE: &str = "PUT /api/users/{id}/scope";
    let actor = provisioner(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let user = managed_user(&library, actor, user_id, ROUTE).await?;
    if actor.user_id() == Some(user_id) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "you may not set your own scope",
        ));
    }
    let own = body.scope.is_some();
    let assigned = set_patron_scope(&library, user_id, body.scope)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    let rules = assigned.scope.rules.len();
    let _ = library
        .insert_security_audit_event(
            &actor.audit_actor(),
            "user_patron_scope",
            Some(&format!(
                r#"{{"user_id":{user_id},"own":{own},"rules":{rules}}}"#
            )),
        )
        .await;
    Ok(Json(user_scope_json(&user, assigned)))
}

/// Reads the library's default scope, which patrons without their own
/// follow.
#[utoipa::path(
    get,
    path = "/api/users/default-scope",
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The default scope.", body = DefaultScopeJson),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller is not an Owner or Administrator.", body = ErrorMessage, example = json!({"error": "forbidden"})),
        (status = 500, description = "The scope could not be read.", body = ErrorMessage, example = json!({"error": "the patron scope could not be read or written"})),
    ),
)]
pub(crate) async fn get_default_scope(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<DefaultScopeJson>, ScopeError> {
    const ROUTE: &str = "GET /api/users/default-scope";
    provisioner(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let scope = default_patron_scope(&library)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    Ok(Json(DefaultScopeJson { scope }))
}

/// Replaces the library's default scope. Every patron without their own
/// scope follows the change at once.
#[utoipa::path(
    put,
    path = "/api/users/default-scope",
    request_body = PatronScope,
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The default scope after the change.", body = DefaultScopeJson),
        (status = 400, description = "The scope is invalid.", body = ErrorMessage, example = json!({"error": "invalid patron scope: a scope holds at most 64 rules"})),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller is not an Owner or Administrator.", body = ErrorMessage, example = json!({"error": "forbidden"})),
        (status = 409, description = "The default kept changing underneath the write; retry.", body = ErrorMessage, example = json!({"error": "core.patron_scope kept changing; retry"})),
        (status = 500, description = "The scope could not be written.", body = ErrorMessage, example = json!({"error": "the patron scope could not be read or written"})),
    ),
)]
pub(crate) async fn put_default_scope(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(scope): Json<PatronScope>,
) -> Result<Json<DefaultScopeJson>, ScopeError> {
    const ROUTE: &str = "PUT /api/users/default-scope";
    let actor = provisioner(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let config_actor = match actor.user_id() {
        None => ConfigActor::Operator {
            id: "operator".into(),
        },
        Some(user_id) => verify_patron_administrator(&library, user_id)
            .await
            .map_err(|err| library_error(ROUTE, err))?,
    };
    let stored = set_default_patron_scope(&library, &config_actor, scope)
        .await
        .map_err(|err| match err {
            LibraryError::Conflict(message) => refused(StatusCode::CONFLICT, message),
            other => library_error(ROUTE, other),
        })?;
    let rules = stored.body.rules.len();
    let _ = library
        .insert_security_audit_event(
            &actor.audit_actor(),
            "patron_scope_default",
            Some(&format!(r#"{{"rules":{rules}}}"#)),
        )
        .await;
    Ok(Json(DefaultScopeJson { scope: stored.body }))
}

#[cfg(test)]
#[path = "patron_scopes_tests.rs"]
mod tests;
