//! Self-service password reset (`docs/adr/notifications.md` decision 7,
//! `docs/server/notifications.md#password-reset`).
//!
//! A patron who forgot their password asks for a link by address. The
//! request always answers `202` with the same body and its work runs in a
//! job, so neither the answer nor its timing says whether an account has
//! the address. A link goes only to a verified address, works once and
//! for 30 minutes, and a new one voids the last. The password stays until
//! the link is used, so asking for links cannot lock anyone out.
//!
//! Using the link asks for the account's second factor when it has one
//! (an authenticator code, otherwise a passkey), then sets the password,
//! signs every device out and mails a password-changed notice. The flow
//! is off until account mail is ready and a test message through its
//! notifier was delivered. Requests and link checks count toward the
//! client's sign-in throttle.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::notifications::{
    PasswordResetAsk, PasswordResetFinish, PasswordResetLink, PasswordResetToken, ResetSecondFactor,
};
use bookclerk_library::{hash_password, LibraryStore, UserRecord, UserStatus};
use chrono::Utc;
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::api::AppState;
use crate::auth::{json_auth_error, password_throttle_key, too_many_requests, ClientIp};
use crate::openapi::{Access, RouteGroup};

/// The WebAuthn challenge kind of a reset's passkey step.
const RESET_PASSKEY_KIND: &str = "password_reset";

/// The answer to a link that does not work, whatever the reason.
const INVALID_LINK: &str = "This reset link is invalid or has expired. Ask for a new one.";

/// The public password-reset routes.
///
/// # Returns
///
/// One public group: the link is the proof, so no sign-in is needed.
#[inline(never)]
pub(crate) fn route_groups() -> Vec<RouteGroup> {
    let public = OpenApiRouter::new()
        .routes(routes!(get_password_reset, post_password_reset))
        .routes(routes!(post_password_reset_check))
        .routes(routes!(post_password_reset_passkey))
        .routes(routes!(post_password_reset_finish));
    vec![RouteGroup::new(public, Access::Public)]
}

/// Whether self-service password reset is on.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PasswordResetAvailability {
    /// True when the sign-in page may offer "Forgot password": account
    /// mail is set up and a test message through it was delivered.
    #[schema(example = true)]
    available: bool,
}

/// The answer to a reset request, the same whether or not an account has
/// the address.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PasswordResetAccepted {
    /// Always true.
    #[schema(example = true)]
    accepted: bool,
}

/// The second factor a reset link asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResetFactor {
    /// The account has no second factor.
    None,
    /// An authenticator-app code (`totp_code` in the finish body).
    Totp,
    /// A passkey: begin with `POST /api/auth/password-reset/passkey`.
    Passkey,
}

/// What a working reset link needs to finish.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PasswordResetLinkState {
    /// The second factor the finish step asks for.
    second_factor: ResetFactor,
}

/// The answer to a finished reset.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PasswordResetDone {
    /// Always true.
    #[schema(example = true)]
    ok: bool,
    /// Sessions of the account that were signed out.
    #[schema(example = 2)]
    revoked_sessions: u64,
}

/// The body of `POST /api/auth/password-reset`.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[allow(dead_code)] // Documents the body; the route reads `PasswordResetAsk`.
pub(crate) struct PasswordResetAskBody {
    /// The address to send a link to.
    #[schema(example = "dana@example.com")]
    email: String,
}

/// The body of `POST /api/auth/password-reset/check` and
/// `POST /api/auth/password-reset/passkey`.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[allow(dead_code)] // Documents the body; the routes read `PasswordResetLink`.
pub(crate) struct PasswordResetLinkBody {
    /// The token from the link: 64 lowercase hex characters.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    token: String,
}

/// A passkey assertion in the finish body.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[allow(dead_code)] // Documents the body; the route reads `PasswordResetFinish`.
pub(crate) struct PasswordResetPasskeyBody {
    /// The challenge from `POST /api/auth/password-reset/passkey`.
    #[schema(example = "2c7e4f0a-9b1d-4e3c-a5f6-7d8e9f0a1b2c")]
    challenge_id: String,
    /// The `PublicKeyCredential` from `navigator.credentials.get()`.
    #[schema(schema_with = crate::passkeys::credential_schema)]
    credential: serde_json::Value,
}

/// The body of `POST /api/auth/password-reset/finish`.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[allow(dead_code)] // Documents the body; the route reads `PasswordResetFinish`.
pub(crate) struct PasswordResetFinishBody {
    /// The token from the link: 64 lowercase hex characters.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    token: String,
    /// The new password: 1 to 1024 bytes after trimming.
    #[schema(example = "new horse battery staple")]
    password: String,
    /// Six-digit authenticator code, when the link asks for `totp`.
    #[schema(example = "492817")]
    totp_code: Option<String>,
    /// Passkey assertion, when the link asks for `passkey`.
    passkey: Option<PasswordResetPasskeyBody>,
}

/// The `400` for a body the JSON extractor or the checks refused, and for
/// a link that does not work.
fn invalid_link() -> Response {
    json_auth_error(StatusCode::BAD_REQUEST, "invalid_link", INVALID_LINK)
}

/// The answer to a body the JSON extractor refused: `415` without the JSON
/// content type, otherwise `400`.
fn refused(rejection: &JsonRejection) -> Response {
    match rejection {
        JsonRejection::MissingJsonContentType(_) => json_auth_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Send the body with Content-Type: application/json.",
        ),
        _ => json_auth_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "The request body is not valid.",
        ),
    }
}

/// The `500` for a library that could not be read or written.
fn unavailable() -> Response {
    json_auth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "The library could not be read or changed.",
    )
}

/// The client's throttle key for reset requests and link checks.
fn throttle_key(client: &str) -> String {
    format!("password-reset:{client}")
}

/// The second factor `user` must show to finish a reset.
///
/// # Errors
///
/// An error when the user's passkeys cannot be counted.
async fn factor_for(
    library: &LibraryStore,
    user: &UserRecord,
) -> bookclerk_library::Result<ResetFactor> {
    if user.totp_enabled {
        return Ok(ResetFactor::Totp);
    }
    Ok(if library.count_webauthn_credentials(user.id).await? > 0 {
        ResetFactor::Passkey
    } else {
        ResetFactor::None
    })
}

/// The enabled user a working link is for; `None` for a link that does
/// not work or a disabled user.
///
/// # Errors
///
/// An error when the library cannot be read.
async fn link_user(
    library: &LibraryStore,
    token: &PasswordResetToken,
) -> bookclerk_library::Result<Option<UserRecord>> {
    let Some(user_id) = library.password_reset_user(token, Utc::now()).await? else {
        return Ok(None);
    };
    Ok(library
        .get_user(user_id)
        .await?
        .filter(|user| !matches!(user.status, UserStatus::Disabled)))
}

/// Whether self-service password reset is on, for the sign-in page.
///
/// Access: anyone.
#[utoipa::path(
    get,
    path = "/api/auth/password-reset",
    responses(
        (status = 200, description = "Whether the sign-in page may offer a reset.", body = PasswordResetAvailability),
        (status = 500, description = "The library or the plugin catalog could not be read."),
    ),
)]
pub(crate) async fn get_password_reset(
    State(state): State<Arc<AppState>>,
) -> Result<Json<PasswordResetAvailability>, axum::response::ErrorResponse> {
    let library = state.library_snapshot().await;
    let available = crate::account_mail::reset_available(&state, &library).await?;
    Ok(Json(PasswordResetAvailability { available }))
}

/// Asks for a password-reset link. When an enabled account has the
/// address verified, a link that works once for 30 minutes is mailed to
/// it, at most one every five minutes. The answer and its timing are the
/// same whether or not an account has the address.
///
/// Access: anyone; each request counts toward the client's sign-in
/// throttle.
#[utoipa::path(
    post,
    path = "/api/auth/password-reset",
    request_body = PasswordResetAskBody,
    responses(
        (status = 202, description = "Accepted; a link is mailed when an account has the address verified.", body = PasswordResetAccepted),
        (status = 400, description = "The body is not one object whose only key is a valid `email`.", body = crate::http_error::ErrorJson),
        (status = 409, description = "Self-service reset is not set up (`reset_unavailable`).", body = crate::http_error::ErrorJson, example = json!({"error": "reset_unavailable", "message": "Password reset by email is not set up. Ask an administrator.", "status": 409})),
        (status = 415, description = "The body is not sent as `application/json`.", body = crate::http_error::ErrorJson),
        (status = 429, description = "Too many requests from this client; retry after `Retry-After` seconds.", body = crate::auth::LoginThrottled, headers(("Retry-After" = u64, description = "Seconds to wait before retrying.")), example = json!({"error": "too_many_requests", "message": "Too many failed login attempts. Try again in 45 seconds.", "status": 429, "retry_after_secs": 45})),
        (status = 500, description = "The library could not be read or written.", body = crate::http_error::ErrorJson),
    ),
)]
pub(crate) async fn post_password_reset(
    State(state): State<Arc<AppState>>,
    ClientIp(client): ClientIp,
    body: Result<Json<PasswordResetAsk>, JsonRejection>,
) -> Response {
    let auth = state.auth_snapshot().await;
    let key = throttle_key(&client);
    if let Some(retry_after) = auth.login_throttle_check(&key).await {
        return too_many_requests(retry_after);
    }
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return refused(&rejection),
    };
    let Ok(address) = body.validate() else {
        return json_auth_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Enter a valid email address.",
        );
    };
    let _ = auth.record_login_failure(&key).await;
    match crate::account_mail::queue_reset_request(&state, &address).await {
        Ok(true) => (
            StatusCode::ACCEPTED,
            Json(PasswordResetAccepted { accepted: true }),
        )
            .into_response(),
        Ok(false) => json_auth_error(
            StatusCode::CONFLICT,
            "reset_unavailable",
            "Password reset by email is not set up. Ask an administrator.",
        ),
        Err(_) => unavailable(),
    }
}

/// Checks a reset link without using it, and says which second factor
/// finishing it asks for.
///
/// Access: anyone holding the link; a link that does not work counts
/// toward the client's sign-in throttle.
#[utoipa::path(
    post,
    path = "/api/auth/password-reset/check",
    request_body = PasswordResetLinkBody,
    responses(
        (status = 200, description = "The link works.", body = PasswordResetLinkState),
        (status = 400, description = "The body is malformed, or the link is unknown, used, expired, for an address the account no longer has verified, or for a disabled account (`invalid_link`).", body = crate::http_error::ErrorJson),
        (status = 415, description = "The body is not sent as `application/json`.", body = crate::http_error::ErrorJson),
        (status = 429, description = "Too many failed attempts from this client; retry after `Retry-After` seconds.", body = crate::auth::LoginThrottled, headers(("Retry-After" = u64, description = "Seconds to wait before retrying.")), example = json!({"error": "too_many_requests", "message": "Too many failed login attempts. Try again in 45 seconds.", "status": 429, "retry_after_secs": 45})),
        (status = 500, description = "The library could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
pub(crate) async fn post_password_reset_check(
    State(state): State<Arc<AppState>>,
    ClientIp(client): ClientIp,
    body: Result<Json<PasswordResetLink>, JsonRejection>,
) -> Response {
    let auth = state.auth_snapshot().await;
    let key = throttle_key(&client);
    if let Some(retry_after) = auth.login_throttle_check(&key).await {
        return too_many_requests(retry_after);
    }
    let token = match body {
        Ok(Json(body)) => body.validate(),
        Err(rejection) => return refused(&rejection),
    };
    let library = state.library_snapshot().await;
    let user = match token {
        Ok(token) => match link_user(&library, &token).await {
            Ok(user) => user,
            Err(_) => return unavailable(),
        },
        Err(_) => None,
    };
    let Some(user) = user else {
        let _ = auth.record_login_failure(&key).await;
        return invalid_link();
    };
    match factor_for(&library, &user).await {
        Ok(second_factor) => Json(PasswordResetLinkState { second_factor }).into_response(),
        Err(_) => unavailable(),
    }
}

/// Begins the passkey step of a reset, for an account whose link asks for
/// a passkey. The challenge works once, for five minutes.
///
/// Access: anyone holding the link.
#[utoipa::path(
    post,
    path = "/api/auth/password-reset/passkey",
    request_body = PasswordResetLinkBody,
    responses(
        (status = 200, description = "WebAuthn request options for the account's passkeys.", body = crate::passkeys::PasskeyRequestOptions),
        (status = 400, description = "The body is malformed or the link does not work (`invalid_link`), or the daemon's origin is not a valid WebAuthn relying party.", body = crate::http_error::ErrorJson),
        (status = 409, description = "The link does not ask for a passkey (`passkey_not_needed`).", body = crate::http_error::ErrorJson),
        (status = 415, description = "The body is not sent as `application/json`.", body = crate::http_error::ErrorJson),
        (status = 429, description = "Too many failed attempts from this client; retry after `Retry-After` seconds.", body = crate::auth::LoginThrottled, headers(("Retry-After" = u64, description = "Seconds to wait before retrying.")), example = json!({"error": "too_many_requests", "message": "Too many failed login attempts. Try again in 45 seconds.", "status": 429, "retry_after_secs": 45})),
        (status = 500, description = "The library could not be read, or the challenge could not be stored.", body = crate::http_error::ErrorJson),
    ),
)]
pub(crate) async fn post_password_reset_passkey(
    State(state): State<Arc<AppState>>,
    ClientIp(client): ClientIp,
    headers: HeaderMap,
    body: Result<Json<PasswordResetLink>, JsonRejection>,
) -> Response {
    let auth = state.auth_snapshot().await;
    let key = throttle_key(&client);
    if let Some(retry_after) = auth.login_throttle_check(&key).await {
        return too_many_requests(retry_after);
    }
    let token = match body {
        Ok(Json(body)) => body.validate(),
        Err(rejection) => return refused(&rejection),
    };
    let library = state.library_snapshot().await;
    let user = match token {
        Ok(token) => match link_user(&library, &token).await {
            Ok(user) => user,
            Err(_) => return unavailable(),
        },
        Err(_) => None,
    };
    let Some(user) = user else {
        let _ = auth.record_login_failure(&key).await;
        return invalid_link();
    };
    match factor_for(&library, &user).await {
        Ok(ResetFactor::Passkey) => {}
        Ok(ResetFactor::None | ResetFactor::Totp) => {
            return json_auth_error(
                StatusCode::CONFLICT,
                "passkey_not_needed",
                "This reset does not ask for a passkey.",
            )
        }
        Err(_) => return unavailable(),
    }
    match crate::passkeys::begin_step_up(&state, &headers, user.id, RESET_PASSKEY_KIND).await {
        Ok(options) => options.into_response(),
        Err(StatusCode::BAD_REQUEST) => json_auth_error(
            StatusCode::BAD_REQUEST,
            "bad_origin",
            "The library's address is not a valid passkey origin. Set its public address.",
        ),
        Err(StatusCode::NOT_FOUND) => json_auth_error(
            StatusCode::CONFLICT,
            "passkey_not_needed",
            "This reset does not ask for a passkey.",
        ),
        Err(_) => unavailable(),
    }
}

/// Finishes a reset: checks the account's second factor when it has one,
/// uses the link up, sets the password, signs every device out and mails
/// a password-changed notice to the verified address.
///
/// Access: anyone holding the link; a wrong second factor or a link that
/// does not work counts toward the client's sign-in throttle.
#[utoipa::path(
    post,
    path = "/api/auth/password-reset/finish",
    request_body = PasswordResetFinishBody,
    responses(
        (status = 200, description = "The password is set and every session signed out.", body = PasswordResetDone),
        (status = 400, description = "The body is malformed, or the link is unknown, used, expired, for an address the account no longer has verified, or for a disabled account (`invalid_link`).", body = crate::http_error::ErrorJson),
        (status = 401, description = "The account's second factor is missing (`second_factor_required`) or wrong (`invalid_code`).", body = crate::http_error::ErrorJson, example = json!({"error": "second_factor_required", "message": "Enter the code from your authenticator app.", "status": 401})),
        (status = 415, description = "The body is not sent as `application/json`.", body = crate::http_error::ErrorJson),
        (status = 429, description = "Too many failed attempts from this client; retry after `Retry-After` seconds.", body = crate::auth::LoginThrottled, headers(("Retry-After" = u64, description = "Seconds to wait before retrying.")), example = json!({"error": "too_many_requests", "message": "Too many failed login attempts. Try again in 45 seconds.", "status": 429, "retry_after_secs": 45})),
        (status = 500, description = "The password could not be hashed, or the library could not be read or written.", body = crate::http_error::ErrorJson),
    ),
)]
pub(crate) async fn post_password_reset_finish(
    State(state): State<Arc<AppState>>,
    ClientIp(client): ClientIp,
    headers: HeaderMap,
    body: Result<Json<PasswordResetFinish>, JsonRejection>,
) -> Response {
    let auth = state.auth_snapshot().await;
    let key = throttle_key(&client);
    if let Some(retry_after) = auth.login_throttle_check(&key).await {
        return too_many_requests(retry_after);
    }
    let finish = match body {
        Ok(Json(body)) => match body.validate() {
            Ok(finish) => finish,
            Err(bookclerk_library::LibraryError::Invalid(message)) => {
                return json_auth_error(StatusCode::BAD_REQUEST, "bad_request", message)
            }
            Err(_) => return invalid_link(),
        },
        Err(rejection) => return refused(&rejection),
    };
    let library = state.library_snapshot().await;
    let user = match link_user(&library, &finish.token).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            let _ = auth.record_login_failure(&key).await;
            return invalid_link();
        }
        Err(_) => return unavailable(),
    };
    let required = match factor_for(&library, &user).await {
        Ok(required) => required,
        Err(_) => return unavailable(),
    };
    let wrong_factor = || {
        json_auth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_code",
            "That second factor is not right. Try again.",
        )
    };
    match (required, &finish.second_factor) {
        (ResetFactor::None, ResetSecondFactor::None) => {}
        (ResetFactor::Totp, ResetSecondFactor::Totp(code)) => {
            match crate::totp::verify_user_totp(&library, &user, code).await {
                Ok(true) => {}
                Ok(false) => {
                    let _ = auth.record_login_failure(&key).await;
                    return wrong_factor();
                }
                Err(_) => return unavailable(),
            }
        }
        (
            ResetFactor::Passkey,
            ResetSecondFactor::Passkey {
                challenge_id,
                credential,
            },
        ) => {
            if crate::passkeys::finish_step_up(
                &state,
                &headers,
                user.id,
                RESET_PASSKEY_KIND,
                challenge_id,
                credential.clone(),
            )
            .await
            .is_err()
            {
                let _ = auth.record_login_failure(&key).await;
                return wrong_factor();
            }
        }
        (ResetFactor::Totp, ResetSecondFactor::None | ResetSecondFactor::Passkey { .. }) => {
            return json_auth_error(
                StatusCode::UNAUTHORIZED,
                "second_factor_required",
                "Enter the code from your authenticator app.",
            )
        }
        (ResetFactor::Passkey, ResetSecondFactor::None | ResetSecondFactor::Totp(_)) => {
            return json_auth_error(
                StatusCode::UNAUTHORIZED,
                "second_factor_required",
                "Confirm with one of your passkeys.",
            )
        }
        (ResetFactor::None, ResetSecondFactor::Totp(_) | ResetSecondFactor::Passkey { .. }) => {
            return json_auth_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "This account has no second factor; send none.",
            )
        }
    }
    let Ok(hash) = hash_password(&finish.password) else {
        return unavailable();
    };
    match library
        .take_password_reset(&finish.token, user.id, Utc::now())
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            let _ = auth.record_login_failure(&key).await;
            return invalid_link();
        }
        Err(_) => return unavailable(),
    }
    if library
        .set_user_password_hash(user.id, Some(&hash))
        .await
        .is_err()
    {
        return unavailable();
    }
    auth.clear_login_failures(&password_throttle_key(user.id))
        .await;
    let revoked = library
        .delete_portal_sessions_for_user(user.id)
        .await
        .unwrap_or(0);
    let _ = library
        .insert_security_audit_event(
            &format!("user:{}", user.id),
            "password_reset_self",
            Some(&format!(r#"{{"revoked_sessions":{revoked}}}"#)),
        )
        .await;
    if let Some(address) = user.email.as_deref() {
        crate::account_mail::queue_password_changed(&state, user.id, address).await;
    }
    Json(PasswordResetDone {
        ok: true,
        revoked_sessions: revoked,
    })
    .into_response()
}

#[cfg(test)]
#[path = "password_reset_tests.rs"]
mod tests;
