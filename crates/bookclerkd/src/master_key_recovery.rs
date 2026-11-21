//! The operator's recovery copy of the library key, from the web app.
//!
//! A key kept in the OS keychain is not in any file a backup picks up, so
//! the desktop window's first run asks its operator to save a recovery copy
//! (or skip it after a warning) and Settings reminds them until they do.
//! These routes are the web form of `bookclerk config master-key export`:
//!
//! - `GET /api/config/master-key` says where the key lives and what the
//!   operator recorded about a recovery copy of it.
//! - `POST /api/config/master-key/recovery-copy` answers a recovery copy,
//!   wrapped with a password from the body (never the URL).
//! - `PUT /api/config/master-key/recovery-copy/state` records that a copy
//!   was saved, or that the step was skipped.
//!
//! An Owner may read the status, so the web app can remind them to
//! elevate; making a copy or recording an answer needs the operator (or an
//! elevated Owner), since holding the key is the operator's (see the
//! owner-and-operator ADR).

use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use zeroize::Zeroizing;

use bookclerk_library::{MasterKeyFormat, RecoveryCopyChoice, RecoveryCopyState};

use crate::api::AppState;
use crate::http_error::ErrorDetail;
use crate::openapi::{Access, RouteGroup};

/// The status group (the operator or an Owner), then the copy and answer
/// group (the operator only).
// Out of line so it is optimized apart from `api::route_groups`.
#[inline(never)]
pub(crate) fn route_groups() -> Vec<RouteGroup> {
    use utoipa_axum::{router::OpenApiRouter, routes};
    let read = OpenApiRouter::new().routes(routes!(get_master_key_status));
    let write = OpenApiRouter::new()
        .routes(routes!(post_recovery_copy))
        .routes(routes!(put_recovery_copy_state));
    vec![
        RouteGroup::new(read, Access::OperatorOrOwner),
        RouteGroup::new(write, Access::Operator),
    ]
}

/// The shortest recovery password the web app accepts, in characters.
///
/// The copy is an offline Argon2id envelope: whoever holds the file can
/// guess at its password forever, so a short one is refused.
pub(crate) const MIN_RECOVERY_PASSWORD_CHARS: usize = 12;

/// Where this install keeps its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MasterKeyStore {
    /// In `master.key`, unwrapped (`BCK1`).
    File,
    /// In `master.key`, wrapped with the auth password (`BCK2`).
    PasswordFile,
    /// In the OS keychain; `master.key` only names the item (`BCK3`).
    Keychain,
    /// Sealed with `systemd-creds` (TPM2 or the host key); `master.key`
    /// only names the credential (`BCK4`).
    SystemdCreds,
}

/// What the operator recorded about a recovery copy of the key in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryCopyAnswer {
    /// Nothing recorded for this key yet.
    Unrecorded,
    /// The operator saved a recovery copy.
    Saved,
    /// The operator skipped saving one, after a warning.
    Skipped,
}

/// `GET /api/config/master-key` and the `PUT` answer.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct MasterKeyStatus {
    /// Where the key lives.
    store: MasterKeyStore,
    /// What the operator recorded about a recovery copy of this key. A
    /// record for an earlier key (since restored or replaced) reads as
    /// `unrecorded`.
    recovery_copy: RecoveryCopyAnswer,
    /// When that was recorded (RFC 3339, UTC); absent when `unrecorded`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "2026-11-21T09:30:00+00:00")]
    recorded_at: Option<String>,
    /// Whether the web app should remind the operator to save a copy: the
    /// key is in the OS keychain or sealed with `systemd-creds` (so backups
    /// of the files directory don't include it) and no copy was recorded as
    /// saved.
    #[schema(example = true)]
    needs_recovery_copy: bool,
}

/// `POST /api/config/master-key/recovery-copy` body.
#[derive(Deserialize, ToSchema)]
pub(crate) struct RecoveryCopyRequest {
    /// The password the copy is wrapped with: at least 12 characters. Keep
    /// it apart from the copy; without it the copy can't be restored.
    #[schema(example = "correct horse battery staple")]
    password: String,
}

impl std::fmt::Debug for RecoveryCopyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryCopyRequest")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// `PUT /api/config/master-key/recovery-copy/state` body.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct RecoveryCopyStateRequest {
    /// What the operator did.
    state: RecoveryCopyChoiceBody,
}

/// What the operator did about a recovery copy.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryCopyChoiceBody {
    /// Saved a recovery copy.
    Saved,
    /// Skipped saving one, after the warning.
    Skipped,
}

impl From<RecoveryCopyChoiceBody> for RecoveryCopyChoice {
    fn from(body: RecoveryCopyChoiceBody) -> Self {
        match body {
            RecoveryCopyChoiceBody::Saved => Self::Saved,
            RecoveryCopyChoiceBody::Skipped => Self::Skipped,
        }
    }
}

/// A `500` with the `master_key_failed` slug and `detail`.
fn failed(detail: impl std::fmt::Display) -> Response {
    tracing::warn!("master key request failed: {detail}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorDetail::new("master_key_failed", detail.to_string())),
    )
        .into_response()
}

/// Builds the status for the key in use in `files_dir`.
///
/// # Errors
///
/// Returns the failure's message when `master.key` can't be inspected or the
/// key in use can't be read.
fn status_for(files_dir: &Path) -> Result<MasterKeyStatus, String> {
    let store = match bookclerk_library::inspect_master_key(files_dir) {
        Ok(Some(MasterKeyFormat::Raw)) => MasterKeyStore::File,
        Ok(Some(MasterKeyFormat::Wrapped)) => MasterKeyStore::PasswordFile,
        Ok(Some(MasterKeyFormat::Keychain)) => MasterKeyStore::Keychain,
        Ok(Some(MasterKeyFormat::SystemdCreds)) => MasterKeyStore::SystemdCreds,
        Ok(None) => return Err("this install has no master.key".into()),
        Err(err) => return Err(err.to_string()),
    };
    // The key in use, from the process cache: no keychain read or Argon2id
    // unwrap per request.
    let key =
        bookclerk_library::require_master_key(Some(files_dir)).map_err(|err| err.to_string())?;
    Ok(status_from(
        store,
        bookclerk_library::recovery_copy_state(files_dir, &key),
    ))
}

/// The status for a key in `store` whose recorded answer is `state`.
fn status_from(store: MasterKeyStore, state: RecoveryCopyState) -> MasterKeyStatus {
    let (recovery_copy, recorded_at) = match state {
        RecoveryCopyState::Unrecorded => (RecoveryCopyAnswer::Unrecorded, None),
        RecoveryCopyState::Recorded {
            choice,
            recorded_at,
        } => {
            let answer = match choice {
                RecoveryCopyChoice::Saved => RecoveryCopyAnswer::Saved,
                RecoveryCopyChoice::Skipped => RecoveryCopyAnswer::Skipped,
            };
            let at = chrono::DateTime::from_timestamp(recorded_at, 0).map(|at| at.to_rfc3339());
            (answer, at)
        }
    };
    MasterKeyStatus {
        store,
        recovery_copy,
        recorded_at,
        needs_recovery_copy: matches!(
            store,
            MasterKeyStore::Keychain | MasterKeyStore::SystemdCreds
        ) && recovery_copy != RecoveryCopyAnswer::Saved,
    }
}

/// `GET /api/config/master-key`.
#[utoipa::path(
    get,
    path = "/api/config/master-key",
    responses(
        (status = 200, description = "Where the key lives and what was recorded about a recovery copy.", body = MasterKeyStatus, example = json!({"store": "keychain", "recovery_copy": "skipped", "recorded_at": "2026-11-21T09:30:00+00:00", "needs_recovery_copy": true})),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is neither the operator nor an Owner (`forbidden`), or must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorJson, examples(
            ("forbidden" = (summary = "The caller is neither the operator nor an Owner.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The Owner must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
        (status = 500, description = "The key could not be read (`master_key_failed`).", body = ErrorDetail, example = json!({"error": "master_key_failed", "detail": "this install has no master.key"})),
    ),
)]
pub(crate) async fn get_master_key_status(State(state): State<Arc<AppState>>) -> Response {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    match tokio::task::spawn_blocking(move || status_for(&files_dir)).await {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(detail)) => failed(detail),
        Err(err) => failed(err),
    }
}

/// `POST /api/config/master-key/recovery-copy`.
#[utoipa::path(
    post,
    path = "/api/config/master-key/recovery-copy",
    request_body = RecoveryCopyRequest,
    responses(
        (status = 200, description = "The recovery copy: a `BCK2` envelope wrapped with the password, for `bookclerk config master-key import`. Sent as an attachment; nothing is recorded until `PUT …/state`.", content(
            (crate::openapi::BinaryBody = "application/octet-stream"),
        )),
        (status = 400, description = "The password is shorter than 12 characters (`password_too_short`).", body = ErrorDetail, example = json!({"error": "password_too_short", "detail": "a recovery password needs at least 12 characters"})),
        (status = 401, description = "The caller is not signed in as the operator."),
        (status = 403, description = "The session is impersonating a user."),
        (status = 500, description = "The key could not be read or wrapped (`master_key_failed`).", body = ErrorDetail, example = json!({"error": "master_key_failed", "detail": "master key file /home/bookclerk/BookclerkFiles/master.key keeps the key in OS keychain item 6f1c…, which could not be read: the keychain is locked"})),
    ),
)]
pub(crate) async fn post_recovery_copy(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RecoveryCopyRequest>,
) -> Response {
    let recovery = Zeroizing::new(body.password);
    if recovery.trim().chars().count() < MIN_RECOVERY_PASSWORD_CHARS {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorDetail::new(
                "password_too_short",
                format!(
                    "a recovery password needs at least {MIN_RECOVERY_PASSWORD_CHARS} characters"
                ),
            )),
        )
            .into_response();
    }
    let (files_dir, password) = {
        let cfg = state.config.read().await;
        (
            cfg.paths().files_dir.clone(),
            cfg.auth_password().map(Zeroizing::new),
        )
    };
    let made = tokio::task::spawn_blocking(move || {
        bookclerk_library::export_master_key(
            &files_dir,
            password.as_ref().map(|p| p.as_str()),
            &recovery,
        )
    })
    .await;
    match made {
        Ok(Ok(bytes)) => recovery_copy_response(bytes.to_vec()),
        Ok(Err(err)) => failed(err),
        Err(err) => failed(err),
    }
}

/// The attachment response carrying `bytes`.
fn recovery_copy_response(bytes: Vec<u8>) -> Response {
    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        // The file name the web app suggests for a recovery copy.
        HeaderValue::from_static("attachment; filename=\"bookclerk-recovery-key.bck\""),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `PUT /api/config/master-key/recovery-copy/state`.
#[utoipa::path(
    put,
    path = "/api/config/master-key/recovery-copy/state",
    request_body = RecoveryCopyStateRequest,
    responses(
        (status = 200, description = "The answer was recorded for the key in use.", body = MasterKeyStatus, example = json!({"store": "keychain", "recovery_copy": "saved", "recorded_at": "2026-11-21T09:30:00+00:00", "needs_recovery_copy": false})),
        (status = 401, description = "The caller is not signed in as the operator."),
        (status = 403, description = "The session is impersonating a user."),
        (status = 500, description = "The key could not be read or the answer could not be written (`master_key_failed`).", body = ErrorDetail, example = json!({"error": "master_key_failed", "detail": "failed to write /home/bookclerk/BookclerkFiles/master-key-recovery.json: No space left on device (os error 28)"})),
    ),
)]
pub(crate) async fn put_recovery_copy_state(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RecoveryCopyStateRequest>,
) -> Response {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let choice = RecoveryCopyChoice::from(body.state);
    let recorded = tokio::task::spawn_blocking(move || {
        // Checked first, so nothing is recorded for a key the install lacks.
        let before = status_for(&files_dir)?;
        let key = bookclerk_library::require_master_key(Some(&files_dir))
            .map_err(|err| err.to_string())?;
        let state = bookclerk_library::record_recovery_copy(&files_dir, &key, choice)
            .map_err(|err| err.to_string())?;
        Ok::<_, String>(status_from(before.store, state))
    })
    .await;
    match recorded {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(detail)) => failed(detail),
        Err(err) => failed(err),
    }
}

#[cfg(test)]
#[path = "master_key_recovery_tests.rs"]
mod tests;
