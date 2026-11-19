//! Administrator switch routes: an Owner or the operator reads and sets
//! what an Administrator may do, per Administrator and as the library's
//! default (see `docs/adr/owner-and-operator.md`).
//!
//! The switches live in `bookclerk_library::control_plane`; each guarded
//! route checks its switch (`crate::auth::require_admin_control`).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::control_plane::{
    assigned_admin_controls, default_admin_controls, set_admin_control_overrides,
    set_default_admin_controls, verify_library_owner, AdminControl, AdminControlOverrides,
    AdminControls, AssignedAdminControls, ConfigActor,
};
use bookclerk_library::{LibraryError, UserRole};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::auth::{authorize_provisioner, Provisioner};
use crate::http_error::ErrorMessage;

/// Error answer of the switch routes: a status and an `error` message.
type SwitchError = (StatusCode, Json<ErrorMessage>);

/// The message of a failed read or write.
const FAILED: &str = "the administrator's switches could not be read or written";

/// An Administrator's switches as the switch routes report them.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct UserAdminControlsJson {
    /// The user (`users.id`).
    #[schema(example = 4)]
    user_id: i64,
    /// The library's default for every Administrator.
    defaults: AdminControls,
    /// The switches set for this Administrator alone; the others follow
    /// the default.
    overrides: AdminControlOverrides,
    /// What applies to the user now: everything for an Owner, nothing for
    /// a disabled user or a Member, and the default with the
    /// overrides applied for an Administrator.
    effective: AdminControls,
    /// Whether the switches limit the user's role: true only for an
    /// Administrator.
    #[schema(example = true)]
    applies: bool,
}

/// `PUT /api/users/{id}/admin-controls` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PutUserAdminControls {
    /// The switches set for this Administrator alone; an absent or `null`
    /// switch follows the library's default, so `{}` returns them to the
    /// default for all of them.
    overrides: AdminControlOverrides,
}

/// The library's default Administrator switches as the switch routes
/// report them.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DefaultAdminControlsJson {
    /// The switches every Administrator follows unless set for them alone.
    controls: AdminControls,
}

/// The 500 answer for a failed read or write, logged with its route.
fn failed(route: &str, err: &LibraryError) -> SwitchError {
    tracing::error!(error = %err, route, "administrator switch route failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorMessage::new(FAILED)),
    )
}

/// An answer with only a status and a message.
fn refused(status: StatusCode, message: impl std::fmt::Display) -> SwitchError {
    (status, Json(ErrorMessage::new(message)))
}

/// Maps a library error to the route's answer: 404 for an unknown user,
/// 403 for a refused write, 409 for a default that kept changing, 500
/// otherwise.
fn library_error(route: &str, err: LibraryError) -> SwitchError {
    match err {
        LibraryError::NotFound(missing) => refused(StatusCode::NOT_FOUND, missing),
        LibraryError::Forbidden(message) => refused(StatusCode::FORBIDDEN, message),
        LibraryError::Conflict(message) => refused(StatusCode::CONFLICT, message),
        other => failed(route, &other),
    }
}

/// The provisioner behind `headers`, or the refusal as an error answer.
async fn provisioner(state: &AppState, headers: &HeaderMap) -> Result<Provisioner, SwitchError> {
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

/// Refuses anyone but the operator or an Owner, who alone set switches.
fn require_owner(actor: Provisioner) -> Result<(), SwitchError> {
    match actor {
        Provisioner::Operator | Provisioner::ElevatedOwner { .. } | Provisioner::Owner { .. } => {
            Ok(())
        }
        Provisioner::Administrator { .. } => Err(refused(
            StatusCode::FORBIDDEN,
            "only an Owner can change what Administrators may do",
        )),
    }
}

/// The wire form of a user's switches.
fn user_json(
    user: &bookclerk_library::UserRecord,
    assigned: AssignedAdminControls,
) -> UserAdminControlsJson {
    UserAdminControlsJson {
        user_id: user.id,
        defaults: assigned.defaults,
        overrides: assigned.overrides,
        effective: assigned.effective,
        applies: user.role == UserRole::Administrator,
    }
}

/// The switches `overrides` sets, as JSON for the audit log.
fn audit_overrides(overrides: &AdminControlOverrides) -> String {
    let set: Vec<String> = AdminControl::ALL
        .into_iter()
        .filter_map(|control| {
            overrides
                .get(control)
                .map(|value| format!(r#""{}":{value}"#, control.as_str()))
        })
        .collect();
    format!("{{{}}}", set.join(","))
}

/// Loads `user_id`, or the 404 answer.
async fn load_user(
    library: &bookclerk_library::LibraryStore,
    user_id: i64,
    route: &str,
) -> Result<bookclerk_library::UserRecord, SwitchError> {
    library
        .get_user(user_id)
        .await
        .map_err(|err| failed(route, &err))?
        .ok_or_else(|| {
            refused(
                StatusCode::NOT_FOUND,
                LibraryError::not_found(bookclerk_library::EntityKind::User, user_id),
            )
        })
}

/// Reads what an Administrator may do: the library's default, their own
/// overrides, and what applies. An Administrator may read their own.
#[utoipa::path(
    get,
    path = "/api/users/{id}/admin-controls",
    params(("id" = i64, Path, description = "User id.")),
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The user's switches.", body = UserAdminControlsJson),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller is not an Owner, and not the Administrator asking about themselves.", body = ErrorMessage, example = json!({"error": "only an Owner can change what Administrators may do"})),
        (status = 404, description = "No such user.", body = ErrorMessage, example = json!({"error": "user 4 not found"})),
        (status = 500, description = "The switches could not be read.", body = ErrorMessage, example = json!({"error": "the administrator's switches could not be read or written"})),
    ),
)]
pub(crate) async fn get_user_admin_controls(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> Result<Json<UserAdminControlsJson>, SwitchError> {
    const ROUTE: &str = "GET /api/users/{id}/admin-controls";
    let actor = provisioner(&state, &headers).await?;
    if actor.user_id() != Some(user_id) {
        require_owner(actor)?;
    }
    let library = state.library_snapshot().await;
    let user = load_user(&library, user_id, ROUTE).await?;
    let assigned = assigned_admin_controls(&library, &user)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    Ok(Json(user_json(&user, assigned)))
}

/// Sets the switches that differ for one Administrator; every other
/// switch follows the library's default. Only an Owner or the operator
/// may, and switches stored for someone who is not an Administrator apply
/// only if their role changes to Administrator.
#[utoipa::path(
    put,
    path = "/api/users/{id}/admin-controls",
    params(("id" = i64, Path, description = "User id.")),
    request_body = PutUserAdminControls,
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The user's switches after the change.", body = UserAdminControlsJson),
        (status = 400, description = "The body names a switch that does not exist.", body = ErrorMessage, example = json!({"error": "unknown field `fly`"})),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller is not an Owner or the operator.", body = ErrorMessage, example = json!({"error": "only an Owner can change what Administrators may do"})),
        (status = 404, description = "No such user.", body = ErrorMessage, example = json!({"error": "user 4 not found"})),
        (status = 500, description = "The switches could not be written.", body = ErrorMessage, example = json!({"error": "the administrator's switches could not be read or written"})),
    ),
)]
pub(crate) async fn put_user_admin_controls(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(body): Json<PutUserAdminControls>,
) -> Result<Json<UserAdminControlsJson>, SwitchError> {
    const ROUTE: &str = "PUT /api/users/{id}/admin-controls";
    let actor = provisioner(&state, &headers).await?;
    require_owner(actor)?;
    let library = state.library_snapshot().await;
    let user = load_user(&library, user_id, ROUTE).await?;
    let assigned = set_admin_control_overrides(&library, user_id, body.overrides)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    let _ = library
        .insert_security_audit_event(
            &actor.audit_actor(),
            "user_admin_controls",
            Some(&format!(
                r#"{{"user_id":{user_id},"overrides":{}}}"#,
                audit_overrides(&assigned.overrides)
            )),
        )
        .await;
    Ok(Json(user_json(&user, assigned)))
}

/// Reads the library's default Administrator switches.
#[utoipa::path(
    get,
    path = "/api/users/default-admin-controls",
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The default switches.", body = DefaultAdminControlsJson),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller is not an Owner or Administrator.", body = ErrorMessage, example = json!({"error": "forbidden"})),
        (status = 500, description = "The switches could not be read.", body = ErrorMessage, example = json!({"error": "the administrator's switches could not be read or written"})),
    ),
)]
pub(crate) async fn get_default_admin_controls(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<DefaultAdminControlsJson>, SwitchError> {
    const ROUTE: &str = "GET /api/users/default-admin-controls";
    provisioner(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let controls = default_admin_controls(&library)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    Ok(Json(DefaultAdminControlsJson { controls }))
}

/// Replaces the library's default Administrator switches. Every
/// Administrator follows the change at once, except in the switches set
/// for them alone. Only an Owner or the operator may.
#[utoipa::path(
    put,
    path = "/api/users/default-admin-controls",
    request_body = AdminControls,
    security(("operatorToken" = []), ("operatorSession" = []), ("portalSession" = [])),
    responses(
        (status = 200, description = "The default switches after the change.", body = DefaultAdminControlsJson),
        (status = 400, description = "The body names a switch that does not exist.", body = ErrorMessage, example = json!({"error": "unknown field `fly`"})),
        (status = 401, description = "The caller is not signed in.", body = ErrorMessage, example = json!({"error": "unauthorized"})),
        (status = 403, description = "The caller is not an Owner or the operator.", body = ErrorMessage, example = json!({"error": "only an Owner can change what Administrators may do"})),
        (status = 409, description = "The default kept changing underneath the write; retry.", body = ErrorMessage, example = json!({"error": "core.admin_controls kept changing; retry"})),
        (status = 500, description = "The switches could not be written.", body = ErrorMessage, example = json!({"error": "the administrator's switches could not be read or written"})),
    ),
)]
pub(crate) async fn put_default_admin_controls(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(controls): Json<AdminControls>,
) -> Result<Json<DefaultAdminControlsJson>, SwitchError> {
    const ROUTE: &str = "PUT /api/users/default-admin-controls";
    let actor = provisioner(&state, &headers).await?;
    require_owner(actor)?;
    let library = state.library_snapshot().await;
    let config_actor = match actor {
        Provisioner::Operator => ConfigActor::Operator {
            id: "operator".into(),
        },
        Provisioner::ElevatedOwner { user_id }
        | Provisioner::Owner { user_id }
        | Provisioner::Administrator { user_id } => verify_library_owner(&library, user_id)
            .await
            .map_err(|err| library_error(ROUTE, err))?,
    };
    let stored = set_default_admin_controls(&library, &config_actor, controls)
        .await
        .map_err(|err| library_error(ROUTE, err))?;
    let _ = library
        .insert_security_audit_event(
            &actor.audit_actor(),
            "admin_controls_default",
            Some(&serde_json::to_string(&stored.body).unwrap_or_default()),
        )
        .await;
    Ok(Json(DefaultAdminControlsJson {
        controls: stored.body,
    }))
}

#[cfg(test)]
#[path = "admin_controls_tests.rs"]
mod tests;
