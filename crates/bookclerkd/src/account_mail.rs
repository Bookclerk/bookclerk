//! Account mail (`docs/server/notifications.md#account-mail`): the messages
//! the library sends about a patron's own account, never routed and never
//! patron-chosen.
//!
//! An Administrator picks one notifier instance whose manifest sets
//! `[notifier] account_mail = true`. Account mail is ready when that
//! instance is installed and the library's public origin is set, since
//! every link in a message is built from the public origin and never from
//! a request's `Host`. These messages exist:
//!
//! - a test to the Administrator's own address;
//! - an invite, whose claim ticket the send job mints at send time and
//!   which verifies the address once it is used;
//! - a verification link, also minted at send time, for an address a
//!   patron set or changed;
//! - a password-reset link, minted at send time, only to a verified
//!   address ([`crate::password_reset`]);
//! - a notice, with no link, that a reset link changed the password.
//!
//! Only a token's hash is stored, and the link is written nowhere but the
//! message: a retry mints a new token and voids the last one. The send
//! job (`notify_send` with an `account_mail` payload) shares the
//! notification send path's retries and stall handling
//! ([`crate::notify_jobs`]).

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use bookclerk_library::notifications::{
    link_for, AccountMail, AccountMailKind, AccountMailSettingsRequest, DeliveryState,
    FailedAttempt, VerifyEmailRequest, MAX_SEND_ATTEMPTS,
};
use bookclerk_library::{JobPayload, LibraryError, LibraryStore, UserStatus};
use bookclerk_plugin_abi::notifications::{check_send_params, classify_send_reply, SendOutcome};
use bookclerk_plugin_abi::{
    NotificationMessage, NotificationSeverity, NotifyDestination, NotifyDestinationKind,
    NotifySendParams,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::warn;

use crate::api::AppState;
use crate::http_error::ErrorMessage;
use crate::job_handler::JobExecCtx;
use crate::jobs::{job_cancelled, JobFailedForGood, JOB_SUSPENDED_DETAIL_PREFIX};
use crate::notify_jobs::{notify_queue_limits, retry_backoff};

/// An error answer: a status and a JSON body.
type ApiError = axum::response::ErrorResponse;

/// The notification type every account mail carries.
const ACCOUNT_MAIL_TYPE: &str = "account_mail";

/// Shortest wait between two verification messages a patron asks for.
pub(crate) const VERIFY_RESEND_GAP: chrono::Duration = chrono::Duration::minutes(5);

/// The audit label of the tickets the send job mints.
const INVITE_CREATED_BY: &str = "account_mail";

/// Why account mail cannot be sent yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NotReady {
    /// No notifier instance is chosen.
    NotConfigured,
    /// The chosen instance is gone, or its plugin no longer sends account
    /// mail.
    NotifierMissing,
    /// The library's public origin is not set, so no link can be built.
    NoPublicOrigin,
}

/// Account mail's settings and whether it can be sent.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct AccountMailStatus {
    /// The notifier instance that sends account mail; `null` when none is
    /// chosen.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: Option<String>,
    /// Whether invites and verification messages are sent.
    #[schema(example = true)]
    ready: bool,
    /// Why not, when not ready.
    not_ready: Option<NotReady>,
    /// When a test message through this instance was last delivered.
    #[schema(example = "2026-11-23T10:00:00Z")]
    tested_at: Option<DateTime<Utc>>,
}

/// The answer to a queued account mail.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct AccountMailQueued {
    /// The message's id. The send runs as a job; its outcome is kept for
    /// 30 days.
    #[schema(example = "1b2c3d4e-5f6a-4b7c-8d9e-0f1a2b3c4d5e")]
    mail_id: String,
}

/// The answer to a used verification link.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct EmailVerified {
    /// Always true; a link that did not verify is a `400`.
    #[schema(example = true)]
    verified: bool,
}

/// A `400` with `message`.
fn bad(message: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_REQUEST, Json(ErrorMessage::new(message))).into()
}

/// The answer to a body the JSON extractor refused: `415` without the JSON
/// content type, otherwise a `400` with `message` (or the extractor's own
/// reason when `message` is `None`).
fn refused_body(rejection: JsonRejection, message: Option<&str>) -> ApiError {
    match rejection {
        JsonRejection::MissingJsonContentType(_) => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(ErrorMessage::new(
                "send the body with Content-Type: application/json",
            )),
        )
            .into(),
        other => bad(message.map_or_else(|| other.body_text(), str::to_owned)),
    }
}

/// A `409` with `message`.
fn conflict(message: impl std::fmt::Display) -> ApiError {
    (StatusCode::CONFLICT, Json(ErrorMessage::new(message))).into()
}

/// Maps a store error: refusals keep their words, the rest is logged and
/// answered as a 500.
fn store_error(route: &str, err: impl Into<LibraryError>) -> ApiError {
    match err.into() {
        LibraryError::Invalid(message) => bad(message),
        other => {
            tracing::error!(error = %other, route, "account-mail call failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new(
                    "the library could not be read or changed",
                )),
            )
                .into()
        }
    }
}

/// Admits the operator, an Owner or an Administrator.
///
/// # Returns
///
/// The caller's user id; `None` for the operator.
///
/// # Errors
///
/// 401 when no one is signed in, 403 for anyone else.
async fn administrator(state: &AppState, headers: &HeaderMap) -> Result<Option<i64>, ApiError> {
    crate::auth::library_administrator(state, headers, None)
        .await
        .map_err(|refusal| {
            refusal
                .answer(|status| {
                    let message = if status == StatusCode::UNAUTHORIZED {
                        "sign in first"
                    } else {
                        "only an Administrator can manage account mail"
                    };
                    (status, Json(ErrorMessage::new(message)))
                })
                .into()
        })
}

/// Account mail's status, read from the settings, the installed
/// notifiers and the public origin.
///
/// # Errors
///
/// An error answer when the library or the plugin catalog cannot be read.
async fn status(state: &AppState, library: &LibraryStore) -> Result<AccountMailStatus, ApiError> {
    let settings = library
        .account_mail_settings()
        .await
        .map_err(|err| store_error("account_mail_status", err))?;
    let Some(settings) = settings else {
        return Ok(AccountMailStatus {
            plugin_instance_id: None,
            ready: false,
            not_ready: Some(NotReady::NotConfigured),
            tested_at: None,
        });
    };
    let installed = crate::notifications_api::is_account_mail_notifier(
        state,
        library,
        &settings.plugin_instance_id,
    )
    .await?;
    let origin = state.config.read().await.integrations.public_origin.clone();
    let not_ready = if !installed {
        Some(NotReady::NotifierMissing)
    } else if link_for(origin.as_deref(), Some("/")).is_none() {
        Some(NotReady::NoPublicOrigin)
    } else {
        None
    };
    Ok(AccountMailStatus {
        plugin_instance_id: Some(settings.plugin_instance_id),
        ready: not_ready.is_none(),
        not_ready,
        tested_at: settings.tested_at,
    })
}

/// Queues one account mail through the chosen instance. A test needs no
/// public origin, since it carries no link; every other message does.
///
/// # Returns
///
/// The message id, or why account mail is not ready.
///
/// # Errors
///
/// An error answer when the library cannot be read or written.
async fn queue(
    state: &AppState,
    kind: AccountMailKind,
    user_id: i64,
    address: &str,
) -> Result<Result<String, NotReady>, ApiError> {
    match queue_unless_since(state, kind, user_id, address, None).await? {
        Ok(Some(id)) => Ok(Ok(id)),
        Ok(None) => Err(store_error(
            "account_mail_queue",
            LibraryError::Other(anyhow::anyhow!("account mail was not recorded")),
        )),
        Err(why) => Ok(Err(why)),
    }
}

/// [`queue`], refused when a message of the same kind for the user was
/// recorded after `since`; the store checks and records in one statement.
///
/// # Returns
///
/// The message id, `None` when a recent message refused it, or why
/// account mail is not ready.
///
/// # Errors
///
/// An error answer when the library cannot be read or written.
async fn queue_unless_since(
    state: &AppState,
    kind: AccountMailKind,
    user_id: i64,
    address: &str,
    since: Option<DateTime<Utc>>,
) -> Result<Result<Option<String>, NotReady>, ApiError> {
    // Hold off a database reload from the snapshot through the enqueue, so
    // the job never lands on a superseded store.
    let _permit = crate::jobs::admission_permit(state)
        .await
        .map_err(|err| store_error("account_mail_queue", LibraryError::Other(err)))?;
    let library = state.library_snapshot().await;
    let status = status(state, &library).await?;
    let usable = match (status.not_ready, kind) {
        (None, _)
        | (
            Some(NotReady::NoPublicOrigin),
            AccountMailKind::Test | AccountMailKind::PasswordChanged,
        ) => true,
        (
            Some(NotReady::NotConfigured | NotReady::NotifierMissing | NotReady::NoPublicOrigin),
            _,
        ) => false,
    };
    let Some(instance) = status.plugin_instance_id.filter(|_| usable) else {
        return Ok(Err(status.not_ready.unwrap_or(NotReady::NotConfigured)));
    };
    let limits = notify_queue_limits(state).await;
    let id = library
        .queue_account_mail_unless_since(kind, user_id, address, &instance, limits, since)
        .await
        .map_err(|err| store_error("account_mail_queue", err))?;
    Ok(Ok(id))
}

/// Queues an invite for a user when account mail is ready.
///
/// # Returns
///
/// Whether an invite was queued; `false` when account mail is not ready
/// or the library could not be written (logged).
pub(crate) async fn queue_invite_if_ready(state: &AppState, user_id: i64, address: &str) -> bool {
    match queue(state, AccountMailKind::Invite, user_id, address).await {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            warn!(user_id, "could not queue an invite mail");
            false
        }
    }
}

/// Queues a verification message for a user's new address when account
/// mail is ready; a failure is logged, since the patron can ask again from
/// their profile.
pub(crate) async fn queue_verification_if_ready(state: &AppState, user_id: i64, address: &str) {
    if let Err(_) | Ok(Err(_)) = queue(state, AccountMailKind::VerifyEmail, user_id, address).await
    {
        tracing::debug!(user_id, "no verification mail queued");
    }
}

/// Whether self-service password reset is on: account mail is ready and a
/// test message through the chosen instance was delivered (ADR decision
/// 7), so a patron who asks for a link can expect it to arrive.
///
/// # Errors
///
/// An error answer when the library or the plugin catalog cannot be read.
pub(crate) async fn reset_available(
    state: &AppState,
    library: &LibraryStore,
) -> Result<bool, ApiError> {
    let status = status(state, library).await?;
    Ok(status.ready && status.tested_at.is_some())
}

/// Shortest wait between two reset links for one account; a request
/// inside it sends nothing.
pub(crate) const RESET_RESEND_GAP: chrono::Duration = chrono::Duration::minutes(5);

/// Enqueues the job that answers a password-reset request for `address`.
///
/// # Returns
///
/// `false` when self-service reset is off.
///
/// # Errors
///
/// An error answer when the library cannot be read or written.
pub(crate) async fn queue_reset_request(state: &AppState, address: &str) -> Result<bool, ApiError> {
    let _permit = crate::jobs::admission_permit(state)
        .await
        .map_err(|err| store_error("password_reset_request", LibraryError::Other(err)))?;
    let library = state.library_snapshot().await;
    let status = status(state, &library).await?;
    let Some(instance) = status
        .plugin_instance_id
        .filter(|_| status.ready && status.tested_at.is_some())
    else {
        return Ok(false);
    };
    let limits = notify_queue_limits(state).await;
    if !library
        .enqueue_password_reset_request(address, &instance, limits)
        .await
        .map_err(|err| store_error("password_reset_request", err))?
    {
        warn!("the notification queue is full; a password-reset request was not queued");
    }
    Ok(true)
}

/// Queues the notice that a user's password was changed through a reset
/// link, to their verified address; a failure is logged, since the reset
/// itself already happened.
pub(crate) async fn queue_password_changed(state: &AppState, user_id: i64, address: &str) {
    if let Err(_) | Ok(Err(_)) =
        queue(state, AccountMailKind::PasswordChanged, user_id, address).await
    {
        warn!(user_id, "could not queue the password-changed notice");
    }
}

/// Runs one `notify_send` job that answers a self-service password-reset
/// request: when an enabled user has the address verified, it queues the
/// reset mail, at most one per account every five minutes. The job's
/// summary does not say whether an account was found.
///
/// # Errors
///
/// An error when the library could not be read or written.
pub async fn run_password_reset_request(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    const DONE: &str = "password-reset request answered";
    let address = payload
        .password_reset_request
        .as_deref()
        .ok_or_else(|| JobFailedForGood("notify_send names no reset request".into()))?;
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let Some(user) = library.get_user_by_email(address).await? else {
        return Ok(DONE.into());
    };
    if matches!(user.status, UserStatus::Disabled) || !user.email_verified {
        return Ok(DONE.into());
    }
    let Some(email) = user.email.clone() else {
        return Ok(DONE.into());
    };
    match queue_unless_since(
        state,
        AccountMailKind::PasswordReset,
        user.id,
        &email,
        Some(Utc::now() - RESET_RESEND_GAP),
    )
    .await
    {
        Ok(_) => Ok(DONE.into()),
        Err(_) => anyhow::bail!("the reset mail could not be queued"),
    }
}

/// Queues an invite for a user an Administrator chose, replacing their
/// unredeemed tickets when it is sent.
///
/// # Errors
///
/// A `409` when the user has no email address or account mail is not
/// ready; a `500` when the library cannot be written.
pub(crate) async fn queue_invite(
    state: &AppState,
    user_id: i64,
    address: Option<&str>,
) -> Result<AccountMailQueued, ApiError> {
    let address = address.ok_or_else(|| conflict("the user has no email address"))?;
    match queue(state, AccountMailKind::Invite, user_id, address).await? {
        Ok(mail_id) => Ok(AccountMailQueued { mail_id }),
        Err(why) => Err(not_ready(why)),
    }
}

/// The `409` for account mail that is not ready.
fn not_ready(why: NotReady) -> ApiError {
    conflict(match why {
        NotReady::NotConfigured => "account mail is not set up; choose a notifier first",
        NotReady::NotifierMissing => "the account-mail notifier is not installed",
        NotReady::NoPublicOrigin => "set the library's public address first",
    })
}

/// Reads account mail's settings and whether it can be sent.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    get,
    path = "/api/notifications/account-mail",
    responses(
        (status = 200, description = "The settings and status.", body = AccountMailStatus),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage account mail"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn get_account_mail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AccountMailStatus>, ApiError> {
    administrator(&state, &headers).await?;
    let library = state.library_snapshot().await;
    Ok(Json(status(&state, &library).await?))
}

/// The body of `PUT /api/notifications/account-mail`.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[allow(dead_code)] // Documents the body; the route parses the raw bytes.
pub(crate) struct AccountMailSettingsBody {
    /// A notifier instance whose plugin sends account mail, or `null` to
    /// turn account mail off. The key is required.
    #[schema(required = true, example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: Option<String>,
}

/// Chooses the notifier instance that sends account mail, or turns it
/// off. Choosing another instance clears the last test.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    put,
    path = "/api/notifications/account-mail",
    request_body = AccountMailSettingsBody,
    responses(
        (status = 200, description = "The settings and status after the change.", body = AccountMailStatus),
        (status = 400, description = "The body is malformed, or the instance does not send account mail.", body = ErrorMessage, example = json!({"error": "that notifier does not send account mail"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage account mail"})),
        (status = 415, description = "The body is not sent as `application/json`.", body = ErrorMessage, example = json!({"error": "send the body with Content-Type: application/json"})),
        (status = 500, description = "The library could not be changed.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn put_account_mail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<AccountMailSettingsRequest>, JsonRejection>,
) -> Result<Json<AccountMailStatus>, ApiError> {
    administrator(&state, &headers).await?;
    let Json(body) = body.map_err(|rejection| refused_body(rejection, None))?;
    let instance = body
        .validate()
        .map_err(|err| store_error("put_account_mail", err))?;
    let library = state.library_snapshot().await;
    if let Some(id) = instance.as_deref() {
        if !crate::notifications_api::is_account_mail_notifier(&state, &library, id).await? {
            return Err(bad("that notifier does not send account mail"));
        }
    }
    library
        .set_account_mail_instance(instance.as_deref())
        .await
        .map_err(|err| store_error("put_account_mail", err))?;
    Ok(Json(status(&state, &library).await?))
}

/// Sends a test message to the caller's own email address through the
/// account-mail notifier. Its delivery marks the notifier tested, which
/// self-service password reset requires.
///
/// Access: an Owner or an Administrator with an email address.
#[utoipa::path(
    post,
    path = "/api/notifications/account-mail/test",
    responses(
        (status = 202, description = "The test is queued.", body = AccountMailQueued),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator, or is the operator, who has no address.", body = ErrorMessage, example = json!({"error": "sign in as a library user with an email address to test account mail"})),
        (status = 409, description = "The caller has no email address, or account mail is not set up.", body = ErrorMessage, example = json!({"error": "account mail is not set up; choose a notifier first"})),
        (status = 500, description = "The test could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_account_mail_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<AccountMailQueued>), ApiError> {
    let Some(user_id) = administrator(&state, &headers).await? else {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorMessage::new(
                "sign in as a library user with an email address to test account mail",
            )),
        )
            .into());
    };
    let library = state.library_snapshot().await;
    let user = library
        .get_user(user_id)
        .await
        .map_err(|err| store_error("post_account_mail_test", err))?;
    let address = user
        .and_then(|u| u.email)
        .ok_or_else(|| conflict("add an email address to your profile first"))?;
    match queue(&state, AccountMailKind::Test, user_id, &address).await? {
        Ok(mail_id) => Ok((StatusCode::ACCEPTED, Json(AccountMailQueued { mail_id }))),
        Err(why) => Err(not_ready(why)),
    }
}

/// Sends the caller a new link to verify their email address, voiding the
/// last one. A patron may ask once every five minutes.
///
/// Access: any signed-in library user, for themselves.
#[utoipa::path(
    post,
    path = "/api/auth/profile/email/verification",
    responses(
        (status = 202, description = "The message is queued.", body = AccountMailQueued),
        (status = 401, description = "No one is signed in."),
        (status = 403, description = "The session has no library user."),
        (status = 409, description = "There is no address to verify, it is already verified, or account mail is not set up.", body = ErrorMessage, example = json!({"error": "your email address is already verified"})),
        (status = 429, description = "A link was sent less than five minutes ago.", body = ErrorMessage, headers(("Retry-After" = u64, description = "Seconds to wait before asking again.")), example = json!({"error": "a link was sent recently; try again in a few minutes"})),
        (status = 500, description = "The message could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_my_email_verification(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let user_id = crate::profile::require_self_user_id(&state, &headers)
        .await
        .map_err(|refusal| {
            refusal.answer(|status| (status, Json(ErrorMessage::new("sign in first"))))
        })?;
    let library = state.library_snapshot().await;
    let user = library
        .get_user(user_id)
        .await
        .map_err(|err| store_error("post_my_email_verification", err))?
        .ok_or_else(|| conflict("there is no address to verify"))?;
    let address = user
        .email
        .clone()
        .ok_or_else(|| conflict("there is no address to verify"))?;
    if user.email_verified {
        return Err(conflict("your email address is already verified"));
    }
    // The answer to a link asked for too soon; the wait is read back so
    // the client knows when to ask again.
    let too_soon = |last: Option<DateTime<Utc>>| {
        let seconds = last
            .map(|at| (at + VERIFY_RESEND_GAP - Utc::now()).num_seconds())
            .unwrap_or(0)
            .clamp(1, VERIFY_RESEND_GAP.num_seconds());
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorMessage::new(
                "a link was sent recently; try again in a few minutes",
            )),
        )
            .into_response();
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    };
    // The store refuses the insert when a link was recorded within the
    // gap, in the same statement, so overlapping requests queue one link.
    let since = Utc::now() - VERIFY_RESEND_GAP;
    match queue_unless_since(
        &state,
        AccountMailKind::VerifyEmail,
        user_id,
        &address,
        Some(since),
    )
    .await?
    {
        Ok(Some(mail_id)) => {
            Ok((StatusCode::ACCEPTED, Json(AccountMailQueued { mail_id })).into_response())
        }
        Ok(None) => {
            let last = library
                .last_account_mail_at(user_id, AccountMailKind::VerifyEmail)
                .await
                .map_err(|err| store_error("post_my_email_verification", err))?;
            Ok(too_soon(last))
        }
        Err(why) => Err(not_ready(why)),
    }
}

/// The body of `POST /api/auth/verify-email`.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[allow(dead_code)] // Documents the body; the route parses the raw bytes.
pub(crate) struct VerifyEmailBody {
    /// The token from the link: 64 lowercase hex characters.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    token: String,
}

/// Uses a verification link. The link is the proof, so no sign-in is
/// needed; it works once, for 24 hours, and only while the patron still
/// has the address it was sent to.
///
/// Access: anyone holding the link.
#[utoipa::path(
    post,
    path = "/api/auth/verify-email",
    request_body = VerifyEmailBody,
    responses(
        (status = 200, description = "The address is verified.", body = EmailVerified),
        (status = 400, description = "The body is malformed, or the link is unknown, used, expired or for an address the patron no longer has.", body = ErrorMessage, example = json!({"error": "this link is invalid or has expired"})),
        (status = 415, description = "The body is not sent as `application/json`.", body = ErrorMessage, example = json!({"error": "send the body with Content-Type: application/json"})),
        (status = 500, description = "The library could not be read or changed.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_verify_email(
    State(state): State<Arc<AppState>>,
    body: Result<Json<VerifyEmailRequest>, JsonRejection>,
) -> Result<Json<EmailVerified>, ApiError> {
    const INVALID: &str = "this link is invalid or has expired";
    let Json(body) = body.map_err(|rejection| refused_body(rejection, Some(INVALID)))?;
    let token = body.validate().map_err(|_| bad(INVALID))?;
    let library = state.library_snapshot().await;
    match library
        .use_email_verification(&token, Utc::now())
        .await
        .map_err(|err| store_error("post_verify_email", err))?
    {
        Some(_) => Ok(Json(EmailVerified { verified: true })),
        None => Err(bad("this link is invalid or has expired")),
    }
}

// --- The send job ------------------------------------------------------------

/// What one account mail says: its title, text and link.
struct Composed {
    /// Short subject.
    title: String,
    /// Plain-text body; the link is appended by the notifier when absent.
    text: String,
    /// The link, carrying the token; `None` for a test.
    link: Option<String>,
}

/// Ends a send whose link mint changed nothing: the mail is dropped when
/// the user's address changed meanwhile, otherwise the send lost its
/// lease and the job is left to its new owner.
///
/// # Errors
///
/// An error when the send lost its lease, or the library could not be
/// read or written.
async fn unminted(
    library: &LibraryStore,
    mail: &AccountMail,
    fence: Option<&bookclerk_library::JobFence>,
) -> anyhow::Result<String> {
    let current = library.get_user(mail.user_id).await?;
    if current.and_then(|u| u.email).as_deref() != Some(mail.address.as_str()) {
        return dropped(library, &mail.id, "the user's address changed", fence).await;
    }
    anyhow::bail!("the send lost its lease before minting the link")
}

/// Runs one `notify_send` job that sends account mail: mints the link's
/// token, sends the message and records the outcome on the mail.
///
/// # Arguments
///
/// * `state` - Daemon state (library, integrations, config).
/// * `payload` - The job payload; `account_mail` names the mail.
/// * `ctx` - Lease and cancel token; needed to park a retry.
///
/// # Returns
///
/// A one-line summary for the job row, or the suspended marker when the
/// job was parked for a retry.
///
/// # Errors
///
/// [`JobFailedForGood`] when the mail failed for good; another error when
/// the library could not be read or written.
pub async fn run_account_mail_send(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let mail_id = payload
        .account_mail
        .as_deref()
        .ok_or_else(|| JobFailedForGood("notify_send names no account mail".into()))?;
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let fence = ctx.map(|ctx| &ctx.fence);
    let Some(mail) = library.account_mail(mail_id).await? else {
        return Ok(format!("account mail {mail_id} is gone"));
    };
    if mail.state != DeliveryState::Pending {
        return Ok(format!(
            "account mail {mail_id} is already {}",
            mail.state.as_str()
        ));
    }
    if mail.attempts >= MAX_SEND_ATTEMPTS {
        return fail(
            &library,
            &mail,
            &format!("gave up after {} attempts", mail.attempts),
            FailedAttempt::AlreadyCounted,
            fence,
        )
        .await;
    }
    let notifier = {
        let integrations = state.integrations.read().await;
        integrations
            .all()
            .iter()
            .find(|i| i.plugin_instance_id() == Some(mail.plugin_instance_id.as_str()))
            .cloned()
    };
    let Some(notifier) = notifier.filter(|n| n.supports_notifier()) else {
        return dropped(
            &library,
            mail_id,
            "the notifier instance is not running or no longer delivers notifications",
            fence,
        )
        .await;
    };
    // The instance must still opt into account mail: a package under the
    // same key may have dropped `[notifier] account_mail` since queueing.
    let opted_in = crate::notifications_api::is_account_mail_notifier(
        state,
        &library,
        &mail.plugin_instance_id,
    )
    .await
    .map_err(|_| anyhow::anyhow!("the plugin catalog could not be read"))?;
    if !opted_in {
        return dropped(
            &library,
            mail_id,
            "the notifier no longer sends account mail",
            fence,
        )
        .await;
    }
    let Some(user) = library.get_user(mail.user_id).await? else {
        return dropped(&library, mail_id, "the user was deleted", fence).await;
    };
    if matches!(user.status, UserStatus::Disabled) {
        return dropped(&library, mail_id, "the user is disabled", fence).await;
    }
    if mail.kind != AccountMailKind::Test && user.email.as_deref() != Some(mail.address.as_str()) {
        return dropped(&library, mail_id, "the user's address changed", fence).await;
    }
    if mail.kind == AccountMailKind::VerifyEmail && user.email_verified {
        return dropped(&library, mail_id, "the address is already verified", fence).await;
    }
    if matches!(
        mail.kind,
        AccountMailKind::PasswordReset | AccountMailKind::PasswordChanged
    ) && !user.email_verified
    {
        return dropped(&library, mail_id, "the address is not verified", fence).await;
    }
    let origin = state.config.read().await.integrations.public_origin.clone();
    // Cancellation may have arrived while the mail was read; no token is
    // minted after it.
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let composed = match mail.kind {
        AccountMailKind::Test => Composed {
            title: "Bookclerk account mail works".into(),
            text: "This is a test of the account mail your library sends for invites, \
                   email verification and password resets. No action is needed."
                .into(),
            link: None,
        },
        AccountMailKind::Invite => {
            if link_for(origin.as_deref(), Some("/")).is_none() {
                return dropped(
                    &library,
                    mail_id,
                    "the library's public address is not set",
                    fence,
                )
                .await;
            }
            let identity = library
                .ensure_local_portal_identity(user.id, user.display_name.as_deref())
                .await?;
            let Some(ticket) = library
                .mint_invite_ticket(
                    user.id,
                    identity.id,
                    &mail.address,
                    INVITE_CREATED_BY,
                    fence,
                )
                .await?
            else {
                return unminted(&library, &mail, fence).await;
            };
            Composed {
                title: "You're invited to a Bookclerk library".into(),
                text: "Open this link to set up your sign-in. It works once and for 7 days; \
                       if you did not expect it, ignore this message."
                    .into(),
                link: link_for(
                    origin.as_deref(),
                    Some(&format!("/invite?ticket={}", ticket.expose())),
                ),
            }
        }
        AccountMailKind::VerifyEmail => {
            if link_for(origin.as_deref(), Some("/")).is_none() {
                return dropped(
                    &library,
                    mail_id,
                    "the library's public address is not set",
                    fence,
                )
                .await;
            }
            let Some(token) = library
                .mint_email_verification(user.id, &mail.address, fence)
                .await?
            else {
                return unminted(&library, &mail, fence).await;
            };
            Composed {
                title: "Confirm your email address".into(),
                text: "Open this link to confirm this address for your Bookclerk library. \
                       It works once and for 24 hours; if you did not ask for it, ignore \
                       this message."
                    .into(),
                link: link_for(
                    origin.as_deref(),
                    Some(&format!("/verify-email?token={}", token.expose())),
                ),
            }
        }
        AccountMailKind::PasswordReset => {
            if link_for(origin.as_deref(), Some("/")).is_none() {
                return dropped(
                    &library,
                    mail_id,
                    "the library's public address is not set",
                    fence,
                )
                .await;
            }
            let Some(token) = library
                .mint_password_reset(user.id, &mail.address, fence)
                .await?
            else {
                return unminted(&library, &mail, fence).await;
            };
            Composed {
                title: "Reset your password".into(),
                text: "Open this link to choose a new password for your Bookclerk library. \
                       It works once and for 30 minutes, and your password stays the same \
                       until you use it. If you did not ask for it, ignore this message."
                    .into(),
                link: link_for(
                    origin.as_deref(),
                    Some(&format!("/reset-password?token={}", token.expose())),
                ),
            }
        }
        AccountMailKind::PasswordChanged => Composed {
            title: "Your password was changed".into(),
            text: "The password of your Bookclerk library account was just changed through \
                   a reset link, and every device was signed out. If this was not you, \
                   contact your library's administrator right away."
                .into(),
            link: None,
        },
    };
    let needs_link = match mail.kind {
        AccountMailKind::Invite | AccountMailKind::VerifyEmail | AccountMailKind::PasswordReset => {
            true
        }
        AccountMailKind::Test | AccountMailKind::PasswordChanged => false,
    };
    if needs_link && composed.link.is_none() {
        return dropped(
            &library,
            mail_id,
            "the library's public address does not make a usable link",
            fence,
        )
        .await;
    }
    let params = NotifySendParams {
        message: NotificationMessage {
            // A message with a link gets a new id on every attempt: the
            // attempt minted a new link and voided the last, so a service
            // that deduplicates on the id must not drop it as a repeat. A
            // test carries no link and keeps one id.
            notification_id: if composed.link.is_some() {
                format!("{}:{}", mail.id, uuid::Uuid::new_v4().simple())
            } else {
                mail.id.clone()
            },
            notification_type: ACCOUNT_MAIL_TYPE.into(),
            severity: NotificationSeverity::Info,
            title: composed.title,
            text: composed.text,
            link: composed.link,
            sensitive: true,
            ..NotificationMessage::default()
        },
        destination: NotifyDestination {
            kind: NotifyDestinationKind::Personal,
            destination_id: format!("account:{}", user.id),
            address_json: serde_json::json!({ "email": mail.address }).to_string(),
        },
    };
    if let Err(err) = check_send_params(&params) {
        let reason = format!("the account mail is not sendable: {err}");
        library
            .record_account_mail_ended(
                mail_id,
                DeliveryState::Dropped,
                &reason,
                FailedAttempt::AlreadyCounted,
                fence,
            )
            .await?;
        return Err(JobFailedForGood(reason).into());
    }
    let reply = notifier.notify_send(params).await;
    let outcome = match &reply {
        Ok(answer) => classify_send_reply(answer),
        Err(err) => SendOutcome::Retry {
            after: None,
            reason: format!("the notifier could not be reached: {err}"),
        },
    };
    match outcome {
        SendOutcome::Delivered { .. } => {
            library
                .record_account_mail_delivered(mail_id, fence)
                .await?;
            Ok(format!("account mail {mail_id} delivered"))
        }
        SendOutcome::Failed { reason } => {
            fail(&library, &mail, &reason, FailedAttempt::New, fence).await
        }
        SendOutcome::Retry { after, reason } => {
            let Some(attempts) = library
                .record_account_mail_retry(mail_id, &reason, fence)
                .await?
            else {
                return Ok(format!("account mail {mail_id} finished elsewhere"));
            };
            if attempts >= MAX_SEND_ATTEMPTS {
                return fail(
                    &library,
                    &mail,
                    &format!("gave up after {attempts} attempts: {reason}"),
                    FailedAttempt::AlreadyCounted,
                    fence,
                )
                .await;
            }
            let wait = after.unwrap_or_else(|| retry_backoff(attempts));
            let Some(ctx) = ctx else {
                anyhow::bail!("{reason}");
            };
            let until =
                Utc::now() + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::hours(1));
            let progress = format!("attempt {attempts} of {MAX_SEND_ATTEMPTS}: {reason}");
            if !library.defer_job(&ctx.fence, until, &progress).await? {
                anyhow::bail!("could not park the send (lease fence lost)");
            }
            Ok(format!("{JOB_SUSPENDED_DETAIL_PREFIX}{until} ({progress})"))
        }
    }
}

/// Records a mail that was not sent, with no attempt counted.
///
/// # Returns
///
/// The job's summary.
async fn dropped(
    library: &LibraryStore,
    mail_id: &str,
    reason: &str,
    fence: Option<&bookclerk_library::JobFence>,
) -> anyhow::Result<String> {
    library
        .record_account_mail_ended(
            mail_id,
            DeliveryState::Dropped,
            reason,
            FailedAttempt::AlreadyCounted,
            fence,
        )
        .await?;
    Ok(format!("account mail {mail_id} dropped: {reason}"))
}

/// Records a failed mail and ends the job for good.
async fn fail(
    library: &LibraryStore,
    mail: &AccountMail,
    reason: &str,
    attempt: FailedAttempt,
    fence: Option<&bookclerk_library::JobFence>,
) -> anyhow::Result<String> {
    library
        .record_account_mail_ended(&mail.id, DeliveryState::Failed, reason, attempt, fence)
        .await?;
    Err(JobFailedForGood(reason.to_string()).into())
}

/// One sweep pass over account mail: drops mail whose send job ended
/// without a result and enqueues the send of each mail that has none.
///
/// # Errors
///
/// An error when a library read or write fails.
pub(crate) async fn sweep_once(
    state: &AppState,
    library: &LibraryStore,
    batch: u64,
) -> anyhow::Result<()> {
    let dropped = library.drop_orphaned_account_mail().await?;
    if dropped > 0 {
        tracing::info!(
            dropped,
            "dropped account mail whose send job ended without a result"
        );
    }
    let limits = notify_queue_limits(state).await;
    for mail in library.unscheduled_account_mail(batch).await? {
        // A send job that ended before its link was written means the
        // message is finished (cancelled or failed), not unscheduled.
        match library.drop_account_mail_if_job_ended(&mail.id).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(err) => {
                warn!(mail = %mail.id, error = %err, "account-mail sweep skipped a message");
                continue;
            }
        }
        match library
            .enqueue_account_mail_send(&mail.id, &mail.plugin_instance_id, limits)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(err) => {
                warn!(mail = %mail.id, error = %err, "account-mail sweep skipped a message")
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "account_mail_tests.rs"]
pub(crate) mod tests;
