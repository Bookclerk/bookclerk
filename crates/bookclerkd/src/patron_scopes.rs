//! Patron scope routes: an Administrator reads and sets which part of the
//! library a patron sees and what they may do in it (the member controls),
//! per patron and as the library's default (see
//! `docs/adr/patron-scopes.md`).
//!
//! The scope and the controls live in `bookclerk_library::control_plane`;
//! the library pages apply the scope (see `library_works`), and each
//! guarded route checks its control.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::control_plane::{
    assigned_member_controls, assigned_patron_scope, default_member_controls, default_patron_scope,
    role_follows_default_controls, role_has_controls, role_is_scoped, set_default_member_controls,
    set_default_patron_scope, set_member_control_overrides_for, set_patron_scope,
    verify_patron_administrator, AssignedControls, AssignedScope, ConfigActor, ControlOverrides,
    MemberControl, MemberControls, PatronScope, PutUserControls, PutUserScope,
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
            "the patron's scope or controls could not be read or written",
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
        (status = 500, description = "The scope could not be read.", body = ErrorMessage, example = json!({"error": "the patron's scope or controls could not be read or written"})),
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
        (status = 500, description = "The scope could not be written.", body = ErrorMessage, example = json!({"error": "the patron's scope or controls could not be read or written"})),
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
        (status = 500, description = "The scope could not be read.", body = ErrorMessage, example = json!({"error": "the patron's scope or controls could not be read or written"})),
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
        (status = 500, description = "The scope could not be written.", body = ErrorMessage, example = json!({"error": "the patron's scope or controls could not be read or written"})),
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

/// A patron's member controls as the control routes report them.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct UserControlsJson {
    /// The user (`users.id`).
    #[schema(example = 7)]
    user_id: i64,
    /// The library's default, which Members follow.
    defaults: MemberControls,
    /// The controls set for this patron alone; the others follow the
    /// default for a Member and are allowed for an Administrator.
    overrides: ControlOverrides,
    /// What applies to the user now: nothing for a disabled user,
    /// everything for an Owner, the overrides alone for an Administrator,
    /// and the default with the overrides applied for a Member.
    effective: MemberControls,
    /// Whether controls can limit the user's role: false for an Owner, who
    /// may always do everything.
    #[schema(example = true)]
    applies: bool,
    /// Whether the user follows the library's default: true for a Member,
    /// false for an Administrator, whom only controls set for them alone
    /// limit (and only an Owner or the operator sets those).
    #[schema(example = true)]
    follows_default: bool,
}

/// The library's default member controls as the control routes report
/// them.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DefaultControlsJson {
    /// The controls every Member follows unless set for them alone.
    /// Administrators do not follow it.
    controls: MemberControls,
}

/// The wire form of a user's controls.
fn user_controls_json(
    user: &bookclerk_library::UserRecord,
    assigned: AssignedControls,
) -> UserControlsJson {
    UserControlsJson {
        user_id: user.id,
        defaults: assigned.defaults,
        overrides: assigned.overrides,
        effective: assigned.effective,
        applies: role_has_controls(user.role),
        follows_default: role_follows_default_controls(user.role),
    }
}

/// The controls `overrides` sets, as `name=value` pairs for the audit log.
fn audit_overrides(overrides: &ControlOverrides) -> String {
    let set: Vec<String> = MemberControl::ALL
        .into_iter()
        .filter_map(|control| {
            overrides
                .get(control)
                .map(|value| format!(r#""{}":{value}"#, control.as_str()))
        })
        .collect();
    format!("{{{}}}", set.join(","))
}

/// Reads what a patron may do: the library's default, their own
/// overrides, and what applies.
#[utoipa::path(
    get,
    path = "/api/users/{id}/controls",
    params(("id" = i64, Path, description = "User id.")),
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The patron's controls.", body = UserControlsJson),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller may not manage this user.", body = ErrorMessage, example = json!({"error": "you may not manage this user"})),
        (status = 404, description = "No such user.", body = ErrorMessage, example = json!({"error": "user 7 not found"})),
        (status = 500, description = "The controls could not be read.", body = ErrorMessage, example = json!({"error": "the patron's scope or controls could not be read or written"})),
    ),
)]
pub(crate) async fn get_user_controls(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> Result<Json<UserControlsJson>, ScopeError> {
    const ROUTE: &str = "GET /api/users/{id}/controls";
    let actor = provisioner(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let user = managed_user(&library, actor, user_id, ROUTE).await?;
    let assigned = assigned_member_controls(&library, &user)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    Ok(Json(user_controls_json(&user, assigned)))
}

/// Sets the controls that differ for one patron; every other control
/// follows the library's default for a Member and is allowed for an
/// Administrator.
///
/// Nobody sets their own. An Owner may always do everything, so
/// overrides stored for one only apply if their role changes. An
/// Administrator's overrides limit them while they stay an Administrator;
/// only an Owner or the operator sets those.
#[utoipa::path(
    put,
    path = "/api/users/{id}/controls",
    params(("id" = i64, Path, description = "User id.")),
    request_body = PutUserControls,
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The patron's controls after the change.", body = UserControlsJson),
        (status = 400, description = "The body names a control that does not exist.", body = ErrorMessage, example = json!({"error": "unknown field `fly`, expected one of `link_accounts`, `download`, `edit_metadata`, `upload`, `request_books`, `see_prices`"})),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller may not manage this user, or tried to set their own controls.", body = ErrorMessage, example = json!({"error": "you may not set your own controls"})),
        (status = 404, description = "No such user.", body = ErrorMessage, example = json!({"error": "user 7 not found"})),
        (status = 409, description = "The user's role changed since the caller was authorized for them; nothing was written.", body = ErrorMessage, example = json!({"error": "user 7 became administrator while their controls were being set; read them again"})),
        (status = 500, description = "The controls could not be written.", body = ErrorMessage, example = json!({"error": "the patron's scope or controls could not be read or written"})),
    ),
)]
pub(crate) async fn put_user_controls(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(body): Json<PutUserControls>,
) -> Result<Json<UserControlsJson>, ScopeError> {
    const ROUTE: &str = "PUT /api/users/{id}/controls";
    let actor = provisioner(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let user = managed_user(&library, actor, user_id, ROUTE).await?;
    if actor.user_id() == Some(user_id) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "you may not set your own controls",
        ));
    }
    // Guarded on the role the authorization above read, so a promotion
    // since then stops the write.
    let assigned = set_member_control_overrides_for(&library, &user, body.overrides)
        .await
        .map_err(|err| match err {
            LibraryError::Conflict(message) => refused(StatusCode::CONFLICT, message),
            other => library_error(ROUTE, other),
        })?;
    let _ = library
        .insert_security_audit_event(
            &actor.audit_actor(),
            "user_member_controls",
            Some(&format!(
                r#"{{"user_id":{user_id},"overrides":{}}}"#,
                audit_overrides(&assigned.overrides)
            )),
        )
        .await;
    Ok(Json(user_controls_json(&user, assigned)))
}

/// Reads the library's default member controls, which every Member
/// follows unless a control is set for them alone.
#[utoipa::path(
    get,
    path = "/api/users/default-controls",
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The default controls.", body = DefaultControlsJson),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller is not an Owner or Administrator.", body = ErrorMessage, example = json!({"error": "forbidden"})),
        (status = 500, description = "The controls could not be read.", body = ErrorMessage, example = json!({"error": "the patron's scope or controls could not be read or written"})),
    ),
)]
pub(crate) async fn get_default_controls(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<DefaultControlsJson>, ScopeError> {
    const ROUTE: &str = "GET /api/users/default-controls";
    provisioner(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let controls = default_member_controls(&library)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    Ok(Json(DefaultControlsJson { controls }))
}

/// Replaces the library's default member controls. Every Member follows
/// the change at once, except in the controls set for them alone;
/// Administrators do not follow the default.
#[utoipa::path(
    put,
    path = "/api/users/default-controls",
    request_body = MemberControls,
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The default controls after the change.", body = DefaultControlsJson),
        (status = 400, description = "The body names a control that does not exist.", body = ErrorMessage, example = json!({"error": "unknown field `fly`, expected one of `link_accounts`, `download`, `edit_metadata`, `upload`, `request_books`, `see_prices`"})),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller is not an Owner or Administrator.", body = ErrorMessage, example = json!({"error": "forbidden"})),
        (status = 409, description = "The default kept changing underneath the write; retry.", body = ErrorMessage, example = json!({"error": "core.member_controls kept changing; retry"})),
        (status = 500, description = "The controls could not be written.", body = ErrorMessage, example = json!({"error": "the patron's scope or controls could not be read or written"})),
    ),
)]
pub(crate) async fn put_default_controls(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(controls): Json<MemberControls>,
) -> Result<Json<DefaultControlsJson>, ScopeError> {
    const ROUTE: &str = "PUT /api/users/default-controls";
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
    let stored = set_default_member_controls(&library, &config_actor, controls)
        .await
        .map_err(|err| match err {
            LibraryError::Conflict(message) => refused(StatusCode::CONFLICT, message),
            other => library_error(ROUTE, other),
        })?;
    let _ = library
        .insert_security_audit_event(
            &actor.audit_actor(),
            "member_controls_default",
            Some(&serde_json::to_string(&stored.body).unwrap_or_default()),
        )
        .await;
    Ok(Json(DefaultControlsJson {
        controls: stored.body,
    }))
}

/// Whether the caller's member controls allow `control`, resolving the
/// caller fail-closed ([`crate::auth::caller_subject`]).
///
/// Only a caller positively identified as the operator passes without a
/// check. A user is checked by their stored role, status and controls
/// (an unknown user may do nothing); a session linked to no user follows
/// the library default.
///
/// # Errors
///
/// 401 when the session ended since it was admitted, 500 when the caller
/// or the controls cannot be read.
async fn caller_allowed(
    state: &AppState,
    headers: &HeaderMap,
    control: MemberControl,
) -> Result<bool, ScopeError> {
    let library = state.library_snapshot().await;
    let user_id = match crate::auth::caller_subject(state, &library, headers).await {
        crate::auth::CallerSubject::Operator => return Ok(true),
        crate::auth::CallerSubject::Person(user_id) => user_id,
        crate::auth::CallerSubject::SignedOut => {
            return Err(refused(StatusCode::UNAUTHORIZED, "sign in again"))
        }
        crate::auth::CallerSubject::Unavailable => {
            tracing::error!(
                control = control.as_str(),
                "the caller could not be identified"
            );
            return Err(refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the patron's scope or controls could not be read or written",
            ));
        }
    };
    let allowed = match user_id {
        Some(user_id) => {
            bookclerk_library::control_plane::user_may(&library, user_id, control).await
        }
        None => default_member_controls(&library)
            .await
            .map(|controls| controls.allows(control)),
    };
    allowed.map_err(|err| failed("member control check", &err))
}

/// Refuses with 403 when the caller's member controls turn `control` off.
/// Only a caller positively identified as the operator passes unchecked.
///
/// # Errors
///
/// 403 with `message` when the control is off, 401 when the session ended,
/// 500 when the caller or the controls cannot be read (the route fails
/// closed).
pub(crate) async fn require_control(
    state: &AppState,
    headers: &HeaderMap,
    control: MemberControl,
    message: &str,
) -> Result<(), ScopeError> {
    if caller_allowed(state, headers, control).await? {
        Ok(())
    } else {
        Err(refused(StatusCode::FORBIDDEN, message))
    }
}

/// Whether the caller's member controls allow `control`. Only a caller
/// positively identified as the operator may without a check. A caller
/// that cannot be identified, or whose controls cannot be read, answers
/// `false`, so a route that only trims its answer fails closed without
/// failing the request.
pub(crate) async fn caller_may(
    state: &AppState,
    headers: &HeaderMap,
    control: MemberControl,
) -> bool {
    caller_allowed(state, headers, control)
        .await
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "patron_scopes_tests.rs"]
mod tests;
