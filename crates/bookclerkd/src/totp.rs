//! TOTP authenticator-app MFA for local password login.
//!
//! Secrets are sealed in `encrypted_secrets` (`kind=totp`, `account_type=user`).
//! Passkey sign-in is a separate second factor and does not require TOTP.

use crate::openapi::{Access, RouteGroup};
use std::sync::Arc;
use utoipa_axum::{router::OpenApiRouter, routes};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret,
    upsert_secret, LibraryStore,
};
use chrono::{Duration as ChronoDuration, Utc};
use qrcode::render::svg;
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use totp_rs::{Algorithm, Builder, Secret, Totp};
use uuid::Uuid;

use crate::api::AppState;
use crate::auth::{
    authorize_operator_bearer_only, issue_portal_session, require_operator_or_recent_owner,
    require_recent_portal_reauth, resolve_operator_session, timed_portal_identity_from_headers,
    ClientIp,
};

/// Pending enroll secret name (replaced on confirm).
const TOTP_PENDING: &str = "pending";
/// Confirmed authenticator secret name.
const TOTP_PRIMARY: &str = "primary";
/// WebAuthn-challenge kind used between password login and TOTP verify.
const TOTP_LOGIN_KIND: &str = "totp_login";

/// TOTP HTTP routes: login is public, enrollment resolves the caller's
/// session in its handlers, and the MFA policy needs the operator or an
/// Owner.
///
/// # Returns
///
/// The login and enrollment group, then the policy group.
pub(crate) fn route_groups() -> Vec<RouteGroup> {
    let public = OpenApiRouter::new()
        .routes(routes!(login_verify))
        .routes(routes!(enroll_begin))
        .routes(routes!(enroll_finish))
        .routes(routes!(disable_totp, totp_status));
    let policy = OpenApiRouter::new().routes(routes!(get_mfa_policy, put_mfa_policy));
    vec![
        RouteGroup::new(public, Access::Public),
        RouteGroup::new(policy, Access::OperatorOrOwner),
    ]
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /api/auth/totp`.
pub struct TotpStatus {
    /// Whether an authenticator app is confirmed for the caller.
    #[schema(example = true)]
    pub enabled: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body after TOTP enrollment began: what the authenticator app needs.
pub struct TotpEnrollment {
    /// Pending base32 secret, for typing into an app by hand.
    #[schema(example = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP")]
    pub secret: String,
    /// `otpauth://` URL the QR code encodes.
    #[schema(
        example = "otpauth://totp/Bookclerk:sam?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=Bookclerk&algorithm=SHA1&digits=6&period=30"
    )]
    pub otpauth_url: String,
    /// The QR code as an SVG document, ready to inline.
    #[schema(example = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 37 37\">…</svg>")]
    pub qr_svg: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// The host-wide second-factor policy.
pub struct MfaPolicy {
    /// Whether every password sign-in must be followed by a passkey or authenticator code.
    #[schema(example = false)]
    pub require_second_factor: bool,
}

/// Failed TOTP login (wrong code, expired challenge, or missing secret).
fn invalid_authenticator_code() -> Response {
    crate::auth::json_auth_error(
        StatusCode::UNAUTHORIZED,
        "invalid_code",
        "Invalid authenticator code.",
    )
}

/// Account id string used as `encrypted_secrets.account_id` for a user TOTP row.
///
/// # Arguments
///
/// * `user_id` - First-party user id.
fn totp_account_id(user_id: i64) -> String {
    user_id.to_string()
}

/// Builds a SHA1 / 6-digit / 30s TOTP from a base32 secret.
///
/// # Errors
///
/// Returns 500 when the secret cannot be decoded or the TOTP constructor fails.
fn totp_from_secret(secret_b32: &str, account: &str) -> Result<Totp, StatusCode> {
    let secret = Secret::try_from_base32(secret_b32.trim())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_secret(secret)
        .with_issuer(Some("Bookclerk"))
        .with_account_name(account)
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Account name shown in authenticator apps (login, email, or a stable fallback).
///
/// # Arguments
///
/// * `user` - First-party user whose login, email, or display name is used.
fn totp_account_label(user: &bookclerk_library::UserRecord) -> String {
    user.login_name
        .as_deref()
        .or(user.email.as_deref())
        .or(user.display_name.as_deref())
        .unwrap_or("bookclerk-user")
        .to_string()
}

/// Loads and unseals a TOTP secret row (`pending` or `primary`).
///
/// # Errors
///
/// Returns 500 when the store or unseal fails.
async fn load_totp_secret(
    library: &LibraryStore,
    user_id: i64,
    name: &str,
) -> Result<Option<String>, StatusCode> {
    let row = get_secret(
        library.db(),
        secret_kind::TOTP,
        Some("local"),
        secret_account_type::USER,
        Some(&totp_account_id(user_id)),
        name,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let bytes = unseal_secret(&row).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Seals and upserts a TOTP secret row for `user_id`.
///
/// # Errors
///
/// Returns 500 when seal or upsert fails.
async fn store_totp_secret(
    library: &LibraryStore,
    user_id: i64,
    name: &str,
    secret_b32: &str,
) -> Result<(), StatusCode> {
    let record = build_sealed_record(
        secret_b32.as_bytes(),
        secret_kind::TOTP,
        "local",
        secret_account_type::USER,
        &totp_account_id(user_id),
        name,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    upsert_secret(library.db(), &record)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Renders `otpauth://` as a compact SVG QR code.
///
/// # Arguments
///
/// * `otpauth` - Authenticator-app URL from [`Totp::to_url`].
///
/// # Errors
///
/// Returns 500 when QR encoding fails.
fn qr_svg(otpauth: &str) -> Result<String, StatusCode> {
    let code = QrCode::new(otpauth.as_bytes()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(code
        .render()
        .min_dimensions(160, 160)
        .dark_color(svg::Color("#0b3553"))
        .light_color(svg::Color("#fbf7ee"))
        .build())
}

/// Resolves the signed-in user from the portal session.
async fn require_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<bookclerk_library::UserRecord, StatusCode> {
    let library = state.library_snapshot().await;
    let identity = timed_portal_identity_from_headers(&library, headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let user_id = identity.user_id.ok_or(StatusCode::UNAUTHORIZED)?;
    library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)
}

/// Whether this host requires a passkey or TOTP for password login.
pub(crate) async fn require_second_factor(state: &AppState) -> bool {
    state.config.read().await.daemon.auth.require_second_factor
}

/// True when a portal caller must enroll TOTP or a passkey before using the library.
///
/// # Arguments
///
/// * `state` - Reads `daemon.auth.require_second_factor`.
/// * `library` - Counts passkeys / TOTP flag.
/// * `user_id` - First-party user on the portal session.
/// * `path` - Request path; enroll/logout routes are allowed.
pub(crate) async fn enrollment_blocks(
    state: &AppState,
    library: &LibraryStore,
    user_id: i64,
    path: &str,
) -> bool {
    if !require_second_factor(state).await {
        return false;
    }
    if path == "/api/auth/logout"
        || path == "/api/portal/logout"
        || path == "/api/auth/me"
        || path == "/api/auth/password"
        || path == "/api/preferences"
        || path.starts_with("/api/auth/totp")
        || path.starts_with("/api/auth/passkeys")
        || path.starts_with("/api/auth/sessions")
        || path.starts_with("/api/auth/profile")
    {
        return false;
    }
    let Ok(Some(user)) = library.get_user(user_id).await else {
        return false;
    };
    if user.totp_enabled {
        return false;
    }
    library
        .count_webauthn_credentials(user_id)
        .await
        .unwrap_or(0)
        == 0
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Password accepted, but an authenticator-app code is needed before a session is issued.
pub struct TotpChallenge {
    /// Always `true`: the password step succeeded.
    #[schema(example = true)]
    pub ok: bool,
    /// The second step the client must complete.
    pub mfa: TotpChallengeDetails,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// The pending second-factor step after a password login.
pub struct TotpChallengeDetails {
    /// Always `totp`: finish with `POST /api/auth/totp/login`.
    #[schema(example = "totp")]
    pub method: &'static str,
    /// Challenge to send with the code; expires after five minutes.
    #[schema(example = "2c7e4f0a-9b1d-4e3c-a5f6-7d8e9f0a1b2c")]
    pub challenge_id: String,
}

/// After a successful password check: TOTP challenge, passkey-required 403, or `None` to issue a session.
///
/// # Errors
///
/// Returns 403 when policy requires a passkey (user has passkeys but no TOTP), or 500 on store errors.
pub(crate) async fn after_password_verified(
    library: &LibraryStore,
    user: &bookclerk_library::UserRecord,
    require_second_factor: bool,
) -> Result<Option<Response>, StatusCode> {
    if user.totp_enabled {
        let challenge_id = Uuid::new_v4().to_string();
        library
            .insert_webauthn_challenge(
                &challenge_id,
                Some(user.id),
                TOTP_LOGIN_KIND,
                "{}",
                Utc::now() + ChronoDuration::minutes(5),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(Some(
            Json(TotpChallenge {
                ok: true,
                mfa: TotpChallengeDetails {
                    method: "totp",
                    challenge_id,
                },
            })
            .into_response(),
        ));
    }
    if require_second_factor {
        let passkeys = library
            .count_webauthn_credentials(user.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if passkeys > 0 {
            return Ok(Some(crate::auth::json_auth_error(
                StatusCode::FORBIDDEN,
                "passkey_required",
                "This server requires a passkey or authenticator app. Sign in with a passkey.",
            )));
        }
    }
    Ok(None)
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Optional current password for enroll/disable step-up.
#[schema(as = TotpReauthBody)]
struct ReauthBody {
    #[serde(default)]
    /// Password used for recent-reauth when the user already has a credential.
    current_password: Option<String>,
}

/// GET `/api/auth/totp` — whether TOTP is confirmed for this user.
#[utoipa::path(
    get,
    path = "/api/auth/totp",
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "Whether TOTP is on for the caller.", body = TotpStatus),
        (status = 401, description = "The caller is not signed in."),
        (status = 500, description = "The user could not be read."),
    ),
)]
async fn totp_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<TotpStatus>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    Ok(Json(TotpStatus {
        enabled: user.totp_enabled,
    }))
}

/// Starts TOTP enrollment: new pending secret, otpauth URL, and QR SVG.
#[utoipa::path(
    post,
    path = "/api/auth/totp/enroll/begin",
    request_body = ReauthBody,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "A pending secret, its otpauth URL and a QR code SVG.", body = TotpEnrollment),
        (status = 401, description = "The caller is not signed in, or the session is older than fifteen minutes and `current_password` is missing or wrong."),
        (status = 409, description = "TOTP is already on."),
        (status = 500, description = "The secret could not be sealed and stored, or the library could not be read."),
    ),
)]
async fn enroll_begin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ReauthBody>,
) -> Result<Json<TotpEnrollment>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    if user.totp_enabled {
        return Err(StatusCode::CONFLICT);
    }
    require_recent_portal_reauth(&state, &headers, user.id, body.current_password.as_deref())
        .await?;
    let library = state.library_snapshot().await;
    let secret = Secret::generate();
    let encoded = secret.to_base32();
    let account = totp_account_label(&user);
    let totp = totp_from_secret(&encoded, &account)?;
    let otpauth_url = totp
        .to_url()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    store_totp_secret(&library, user.id, TOTP_PENDING, &encoded).await?;
    let qr_svg = qr_svg(&otpauth_url)?;
    Ok(Json(TotpEnrollment {
        secret: encoded,
        otpauth_url,
        qr_svg,
    }))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// First TOTP code that confirms the pending secret.
struct EnrollFinish {
    /// Six-digit code from the authenticator app.
    code: String,
}

/// Confirms the pending secret with a valid code and sets `users.totp_enabled`.
#[utoipa::path(
    post,
    path = "/api/auth/totp/enroll/finish",
    request_body = EnrollFinish,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "TOTP is on.", body = crate::auth::OkResponse),
        (status = 400, description = "No enrollment is pending; start one first."),
        (status = 401, description = "The caller is not signed in (the standard error body), or the code is wrong (`invalid_code`).", body = crate::http_error::ErrorJson, examples(
            ("invalid_code" = (summary = "The code is wrong.", value = json!({"error": "invalid_code", "message": "Invalid authenticator code.", "status": 401}))),
            ("not_signed_in" = (summary = "The caller is not signed in.", value = json!({"error": "unauthorized", "message": "Operator authentication is required. Sign in with your operator token.", "status": 401}))),
        )),
        (status = 409, description = "TOTP is already on."),
        (status = 500, description = "The pending secret could not be read, or the enrollment could not be confirmed."),
    ),
)]
async fn enroll_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<EnrollFinish>,
) -> Result<Response, StatusCode> {
    let user = require_user(&state, &headers).await?;
    if user.totp_enabled {
        return Err(StatusCode::CONFLICT);
    }
    let library = state.library_snapshot().await;
    let Some(secret) = load_totp_secret(&library, user.id, TOTP_PENDING).await? else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let account = totp_account_label(&user);
    let totp = totp_from_secret(&secret, &account)?;
    let code = body.code.trim();
    if totp.check_current(code).is_none() {
        return Ok(crate::auth::json_auth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_code",
            "Invalid authenticator code.",
        ));
    }
    library
        .confirm_totp_enrollment(user.id, &secret)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = library
        .insert_security_audit_event(&format!("user:{}", user.id), "totp_enroll", None)
        .await;
    Ok(Json(crate::auth::OkResponse::OK).into_response())
}

/// Disables TOTP after recent reauth and deletes sealed secrets.
#[utoipa::path(
    delete,
    path = "/api/auth/totp",
    request_body = ReauthBody,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "TOTP is off.", body = crate::auth::OkResponse),
        (status = 401, description = "The caller is not signed in, or the session is older than fifteen minutes and `current_password` is missing or wrong."),
        (status = 500, description = "The library could not be read or written."),
    ),
)]
async fn disable_totp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ReauthBody>,
) -> Result<Json<crate::auth::OkResponse>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    require_recent_portal_reauth(&state, &headers, user.id, body.current_password.as_deref())
        .await?;
    let library = state.library_snapshot().await;
    library
        .disable_user_totp(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = library
        .insert_security_audit_event(&format!("user:{}", user.id), "totp_disable", None)
        .await;
    Ok(Json(crate::auth::OkResponse::OK))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// TOTP code plus the challenge id from password login.
struct LoginVerify {
    /// Server-issued challenge id (5-minute TTL, single use).
    challenge_id: String,
    /// Six-digit authenticator code.
    code: String,
}

/// Completes password+TOTP login and issues a portal session cookie.
#[utoipa::path(
    post,
    path = "/api/auth/totp/login",
    request_body = LoginVerify,
    responses(
        (status = 200, description = "Signed in; the response sets the portal session cookie.", body = crate::auth::PortalSessionIssued),
        (status = 401, description = "The challenge is unknown or expired, TOTP is off for the user, or the code is wrong (`invalid_code`); or the challenged user no longer exists (the standard error body).", body = crate::http_error::ErrorJson, examples(
            ("invalid_code" = (summary = "The challenge or code is not accepted.", value = json!({"error": "invalid_code", "message": "Invalid authenticator code.", "status": 401}))),
            ("user_gone" = (summary = "The challenged user no longer exists.", value = json!({"error": "unauthorized", "message": "Operator authentication is required. Sign in with your operator token.", "status": 401}))),
        )),
        (status = 429, description = "Too many failed attempts from this client; try later."),
        (status = 500, description = "The user or secret could not be read, or the session could not be stored."),
    ),
)]
async fn login_verify(
    State(state): State<Arc<AppState>>,
    ClientIp(client_key): ClientIp,
    headers: HeaderMap,
    Json(body): Json<LoginVerify>,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    if auth.login_throttle_check(&client_key).await.is_some() {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let library = state.library_snapshot().await;
    let Some((Some(uid), _)) = library
        .take_webauthn_challenge(&body.challenge_id, TOTP_LOGIN_KIND)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        let _ = auth.record_login_failure(&client_key).await;
        return Ok(invalid_authenticator_code());
    };
    let user = library
        .get_user(uid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !user.totp_enabled {
        let _ = auth.record_login_failure(&client_key).await;
        return Ok(invalid_authenticator_code());
    }
    let Some(secret) = load_totp_secret(&library, uid, TOTP_PRIMARY).await? else {
        let _ = auth.record_login_failure(&client_key).await;
        return Ok(invalid_authenticator_code());
    };
    let totp = totp_from_secret(&secret, &totp_account_label(&user))?;
    if totp.check_current(body.code.trim()).is_none() {
        let _ = auth.record_login_failure(&client_key).await;
        return Ok(invalid_authenticator_code());
    }
    auth.clear_login_failures(&client_key).await;
    issue_portal_session(&state, &library, &user, &headers, "totp_login").await
}

/// Checks `code` against `user`'s confirmed authenticator secret: the
/// step-up a clerk join asks for on the consent page. False when the user
/// has no confirmed TOTP or the code is wrong.
///
/// # Errors
///
/// Returns 500 when the secret cannot be read or decoded.
pub(crate) async fn verify_user_totp(
    library: &LibraryStore,
    user: &bookclerk_library::UserRecord,
    code: &str,
) -> Result<bool, StatusCode> {
    if !user.totp_enabled {
        return Ok(false);
    }
    let Some(secret) = load_totp_secret(library, user.id, TOTP_PRIMARY).await? else {
        return Ok(false);
    };
    let secret = zeroize::Zeroizing::new(secret);
    let totp = totp_from_secret(&secret, &totp_account_label(user))?;
    Ok(totp.check_current(code.trim()).is_some())
}

/// A current code for `secret_b32` (tests that step up with TOTP).
#[cfg(test)]
pub(crate) fn current_test_code(secret_b32: &str) -> String {
    totp_from_secret(secret_b32, "test")
        .expect("totp")
        .generate_current()
        .to_string()
}

/// Current MFA policy (`require_second_factor`).
#[utoipa::path(
    get,
    path = "/api/auth/mfa-policy",
    responses(
        (status = 200, description = "Whether a second factor is required.", body = MfaPolicy),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is neither the operator nor an Owner (`forbidden`), or must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorJson, examples(
            ("forbidden" = (summary = "The caller is neither the operator nor an Owner.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The Owner must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
    ),
)]
async fn get_mfa_policy(State(state): State<Arc<AppState>>) -> Json<MfaPolicy> {
    let require_second_factor = require_second_factor(&state).await;
    Json(MfaPolicy {
        require_second_factor,
    })
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// PUT body for the host-wide second-factor policy.
struct MfaPolicyPut {
    /// When true, password login requires TOTP or the user must use a passkey.
    require_second_factor: bool,
    #[serde(default)]
    /// Current password for Owner step-up (operators omit this).
    current_password: Option<String>,
}

/// Persists `daemon.auth.require_second_factor` to `config.toml`.
#[utoipa::path(
    put,
    path = "/api/auth/mfa-policy",
    request_body = MfaPolicyPut,
    responses(
        (status = 200, description = "The policy after the change.", body = MfaPolicy),
        (status = 401, description = "The caller is not signed in (the standard error body), or an Owner's session is older than fifteen minutes and `current_password` is missing or wrong (`reauth_required`).", body = crate::http_error::ErrorJson, examples(
            ("reauth_required" = (summary = "The Owner must re-authenticate.", value = json!({"error": "reauth_required", "message": "Recent authentication is required to change the MFA policy.", "status": 401}))),
            ("not_signed_in" = (summary = "The caller is not signed in.", value = json!({"error": "unauthorized", "message": "Operator authentication is required. Sign in with your operator token.", "status": 401}))),
        )),
        (status = 403, description = "The caller is neither the operator nor an Owner (`forbidden`), or must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorJson, examples(
            ("forbidden" = (summary = "The caller is neither the operator nor an Owner.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The Owner must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
        (status = 500, description = "The configuration path is unknown or the configuration file could not be written, or an Owner's current password could not be read or checked (`internal_error`).", body = crate::http_error::ErrorJson, examples(
            ("config_write" = (summary = "The configuration file could not be written.", value = json!({"error": "internal_error", "message": "failed to write config.toml: Permission denied (os error 13)", "status": 500}))),
            ("password_check" = (summary = "The Owner's current password could not be checked.", value = json!({"error": "internal_error", "message": "The current password could not be checked.", "status": 500}))),
        )),
    ),
)]
async fn put_mfa_policy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<MfaPolicyPut>,
) -> Result<Json<MfaPolicy>, Response> {
    require_operator_or_recent_owner(&state, &headers, body.current_password.as_deref())
        .await
        .map_err(|status| {
            let (error, message) = if status == StatusCode::UNAUTHORIZED {
                (
                    "reauth_required",
                    "Recent authentication is required to change the MFA policy.",
                )
            } else if status == StatusCode::INTERNAL_SERVER_ERROR {
                (
                    "internal_error",
                    "The current password could not be checked.",
                )
            } else {
                ("forbidden", "You cannot change the MFA policy.")
            };
            crate::auth::json_auth_error(status, error, message)
        })?;
    let auth = state.auth_snapshot().await;
    let operator_recovery = authorize_operator_bearer_only(&auth, &headers)
        || resolve_operator_session(&state, &auth, &headers)
            .await
            .is_some_and(|op| op.impersonating_user_id.is_none());
    if !operator_recovery {
        let library = state.library_snapshot().await;
        if let Some(identity) = timed_portal_identity_from_headers(&library, &headers).await {
            if let Some(user_id) = identity.user_id {
                if enrollment_blocks(&state, &library, user_id, "/api/auth/mfa-policy").await {
                    return Err(crate::auth::json_auth_error(
                        StatusCode::FORBIDDEN,
                        "mfa_enrollment_required",
                        "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.",
                    ));
                }
            }
        }
    }
    let _reload_guard = state.reload_lock.lock().await;
    let config_path = {
        let cfg = state.config.read().await;
        cfg.paths.as_ref().map(|p| p.config_file.clone())
    };
    let Some(config_path) = config_path else {
        return Err(crate::auth::json_auth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "config path is not available",
        ));
    };
    {
        let mut staged = state.config.read().await.clone();
        staged.daemon.auth.require_second_factor = body.require_second_factor;
        staged.write_toml_file(&config_path).map_err(|err| {
            crate::auth::json_auth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("failed to write config.toml: {err}"),
            )
        })?;
    }
    {
        let mut cfg = state.config.write().await;
        cfg.daemon.auth.require_second_factor = body.require_second_factor;
    }
    Ok(Json(MfaPolicy {
        require_second_factor: body.require_second_factor,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::auth::tests::{phase2_harness, phase2_owner_password};

    /// Collects status plus JSON from an axum response.
    async fn json_of(res: axum::http::Response<Body>) -> (StatusCode, Value) {
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// Six-digit code for the pending/primary secret (issuer Bookclerk).
    fn totp_code(secret_b32: &str, account: &str) -> String {
        totp_from_secret(secret_b32, account)
            .expect("totp")
            .generate_current()
            .to_string()
    }

    /// Password login after TOTP enroll returns an MFA challenge, then a session.
    #[tokio::test]
    async fn totp_enroll_then_password_login_challenge() {
        let (_state, app, _library) = phase2_harness("op-token-totp").await;
        let password = phase2_owner_password();
        let login_body = format!(r#"{{"login":"owner","password":"{password}"}}"#);

        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(login_body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .expect("session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let _ = login.into_body().collect().await;

        let begin = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/totp/enroll/begin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (begin_status, begin_body) = json_of(begin).await;
        assert_eq!(begin_status, StatusCode::OK);
        let secret = begin_body["secret"].as_str().expect("secret");
        assert!(begin_body["otpauth_url"]
            .as_str()
            .unwrap()
            .contains("otpauth://"));
        assert!(begin_body["qr_svg"].as_str().unwrap().contains("<svg"));
        let code = totp_code(secret, "owner");

        let finish = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/totp/enroll/finish")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(r#"{{"code":"{code}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (finish_status, finish_body) = json_of(finish).await;
        assert_eq!(finish_status, StatusCode::OK);
        assert_eq!(finish_body["ok"], true);

        let challenged = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(login_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (challenged_status, challenged_body) = json_of(challenged).await;
        assert_eq!(challenged_status, StatusCode::OK);
        assert_eq!(challenged_body["mfa"]["method"], "totp");
        let challenge_id = challenged_body["mfa"]["challenge_id"]
            .as_str()
            .expect("challenge_id");
        let code = totp_code(secret, "owner");

        let verify = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/totp/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"challenge_id":"{challenge_id}","code":"{code}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(verify.status(), StatusCode::OK);
        assert!(verify.headers().get(header::SET_COOKIE).is_some());
        let _ = verify.into_body().collect().await;
    }

    /// Policy plus an existing passkey refuses password login with `passkey_required`.
    #[tokio::test]
    async fn require_second_factor_blocks_password_when_passkeys_exist() {
        let (state, app, library) = phase2_harness("op-token-mfa-policy").await;
        {
            let mut cfg = state.config.write().await;
            cfg.daemon.auth.require_second_factor = true;
        }
        let owner = library
            .get_user_by_login_name("owner")
            .await
            .unwrap()
            .unwrap();
        library
            .insert_webauthn_credential(owner.id, "cred-policy", "{}", Some("Laptop"))
            .await
            .unwrap();
        let password = phase2_owner_password();
        let login = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"login":"owner","password":"{password}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json_of(login).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "passkey_required");
    }

    /// Owners can read and persist `daemon.auth.require_second_factor`.
    #[tokio::test]
    async fn mfa_policy_get_and_put() {
        let (state, app, library) = phase2_harness("op-token-mfa-put").await;
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cfg = state.config.write().await;
            cfg.paths = Some(bookclerk_config::Paths::from_files_dir(
                dir.path().to_path_buf(),
            ));
            cfg.write_toml_file(&cfg.paths().config_file).unwrap();
        }
        let cookie = crate::auth::tests::portal_cookie_for(&library, "test", "admin-ext").await;
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/mfa-policy")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (get_status, get_body) = json_of(get).await;
        assert_eq!(get_status, StatusCode::OK);
        assert_eq!(get_body["require_second_factor"], false);

        let put = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/mfa-policy")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"require_second_factor":true,"current_password":"{}"}}"#,
                        phase2_owner_password()
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (put_status, put_body) = json_of(put).await;
        assert_eq!(put_status, StatusCode::OK);
        assert_eq!(put_body["require_second_factor"], true);
        assert!(state.config.read().await.daemon.auth.require_second_factor);
    }

    /// Wrong password is a JSON 401, not the branded operator-token copy.
    #[tokio::test]
    async fn password_login_wrong_password_returns_invalid_credentials() {
        let (_state, app, _library) = phase2_harness("op-token-bad-pw").await;
        let login = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"login":"owner","password":"wrong-password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json_of(login).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "invalid_credentials");
        assert_eq!(body["message"], "Invalid login or password.");
        let message = body["message"].as_str().unwrap_or("");
        assert!(!message.to_ascii_lowercase().contains("operator token"));
    }

    /// MFA-required hosts still issue a session so an unenrolled user can enroll or log out.
    #[tokio::test]
    async fn require_second_factor_unenrolled_login_allows_enroll_and_logout() {
        let (state, app, library) = phase2_harness("op-token-mfa-enroll").await;
        {
            let mut cfg = state.config.write().await;
            cfg.daemon.auth.require_second_factor = true;
        }
        let owner = library
            .get_user_by_login_name("owner")
            .await
            .unwrap()
            .unwrap();
        assert!(!enrollment_blocks(&state, &library, owner.id, "/api/auth/logout").await);
        assert!(!enrollment_blocks(&state, &library, owner.id, "/api/portal/logout").await);
        assert!(
            !enrollment_blocks(&state, &library, owner.id, "/api/auth/totp/enroll/begin").await
        );
        assert!(
            !enrollment_blocks(
                &state,
                &library,
                owner.id,
                "/api/auth/passkeys/register/begin"
            )
            .await
        );
        assert!(enrollment_blocks(&state, &library, owner.id, "/api/wishlist").await);
        assert!(enrollment_blocks(&state, &library, owner.id, "/api/auth/mfa-policy").await);

        let password = phase2_owner_password();
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"login":"owner","password":"{password}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .expect("session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let _ = login.into_body().collect().await;

        let me = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (me_status, me_body) = json_of(me).await;
        assert_eq!(me_status, StatusCode::OK);
        assert_eq!(me_body["authenticated"], true);
        assert_eq!(me_body["second_factor"]["required"], true);
        assert_eq!(me_body["second_factor"]["enrolled"], false);

        let blocked = app
            .oneshot(
                Request::builder()
                    .uri("/api/wishlist")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (blocked_status, blocked_body) = json_of(blocked).await;
        assert_eq!(blocked_status, StatusCode::FORBIDDEN);
        assert_eq!(blocked_body["error"], "mfa_enrollment_required");
    }

    /// Unenrolled Owners cannot weaken host MFA policy; operator Bearer still can.
    #[tokio::test]
    async fn unenrolled_owner_cannot_disable_required_mfa_policy() {
        let (state, app, _library) = phase2_harness("op-token-mfa-policy-gate").await;
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cfg = state.config.write().await;
            cfg.daemon.auth.require_second_factor = true;
            cfg.paths = Some(bookclerk_config::Paths::from_files_dir(
                dir.path().to_path_buf(),
            ));
            cfg.write_toml_file(&cfg.paths().config_file).unwrap();
        }
        let password = phase2_owner_password();
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"login":"owner","password":"{password}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .expect("session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let _ = login.into_body().collect().await;

        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/mfa-policy")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (get_status, get_body) = json_of(get).await;
        assert_eq!(get_status, StatusCode::FORBIDDEN);
        assert_eq!(get_body["error"], "mfa_enrollment_required");

        let put = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/mfa-policy")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"require_second_factor":false,"current_password":"{password}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (put_status, put_body) = json_of(put).await;
        assert_eq!(put_status, StatusCode::FORBIDDEN);
        assert_eq!(put_body["error"], "mfa_enrollment_required");
        assert!(state.config.read().await.daemon.auth.require_second_factor);

        let recovery = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/mfa-policy")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer op-token-mfa-policy-gate")
                    .body(Body::from(r#"{"require_second_factor":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (recovery_status, recovery_body) = json_of(recovery).await;
        assert_eq!(recovery_status, StatusCode::OK);
        assert_eq!(recovery_body["require_second_factor"], false);
        assert!(!state.config.read().await.daemon.auth.require_second_factor);
    }

    /// Enrollment-restricted Owners cannot scan, manage OIDC clients, or provision users.
    #[tokio::test]
    async fn unenrolled_owner_cannot_use_admin_routes() {
        let (state, app, _library) = phase2_harness("op-token-mfa-admin-gate").await;
        {
            let mut cfg = state.config.write().await;
            cfg.daemon.auth.require_second_factor = true;
        }
        let password = phase2_owner_password();
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"login":"owner","password":"{password}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .expect("session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let _ = login.into_body().collect().await;

        let scan = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/library/scan")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (scan_status, scan_body) = json_of(scan).await;
        assert_eq!(scan_status, StatusCode::FORBIDDEN);
        assert_eq!(scan_body["error"], "mfa_enrollment_required");

        let oidc = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/clients")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (oidc_status, oidc_body) = json_of(oidc).await;
        assert_eq!(oidc_status, StatusCode::FORBIDDEN);
        assert_eq!(oidc_body["error"], "mfa_enrollment_required");

        let users = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(users.status(), StatusCode::FORBIDDEN);
        let _ = users.into_body().collect().await;

        let recovery = app
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header(header::AUTHORIZATION, "Bearer op-token-mfa-admin-gate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(recovery.status(), StatusCode::OK);
        let _ = recovery.into_body().collect().await;
    }
}
