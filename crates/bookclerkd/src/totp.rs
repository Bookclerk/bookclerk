//! TOTP authenticator-app MFA for local password login.
//!
//! Secrets are sealed in `encrypted_secrets` (`kind=totp`, `account_type=user`).
//! Passkey sign-in is a separate second factor and does not require TOTP.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use bookclerk_library::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret,
    upsert_secret, LibraryStore,
};
use chrono::{Duration as ChronoDuration, Utc};
use qrcode::render::svg;
use qrcode::QrCode;
use serde::Deserialize;
use serde_json::Value;
use totp_rs::{Algorithm, Secret, TOTP};
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

/// TOTP HTTP routes (enroll is session-authenticated; login is public).
pub fn router(state: Arc<AppState>) -> Router {
    let public = Router::new()
        .route("/api/auth/totp/login", post(login_verify))
        .with_state(state.clone());
    let policy = Router::new()
        .route(
            "/api/auth/mfa-policy",
            get(get_mfa_policy).put(put_mfa_policy),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_operator_or_owner_auth,
        ))
        .with_state(state.clone());
    let enrolled = Router::new()
        .route("/api/auth/totp/enroll/begin", post(enroll_begin))
        .route("/api/auth/totp/enroll/finish", post(enroll_finish))
        .route("/api/auth/totp", delete(disable_totp).get(totp_status))
        .with_state(state);
    public.merge(policy).merge(enrolled)
}

/// JSON error body for TOTP / MFA-policy handlers.
///
/// # Arguments
///
/// * `status` - HTTP status for the response.
/// * `error` - Machine-readable slug (`passkey_required`, `reauth_required`, …).
/// * `message` - Operator-facing explanation.
fn totp_error(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": error,
            "message": message,
            "status": status.as_u16()
        })),
    )
        .into_response()
}

/// Failed TOTP login (wrong code, expired challenge, or missing secret).
fn invalid_authenticator_code() -> Response {
    totp_error(
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
fn totp_from_secret(secret_b32: &str, account: &str) -> Result<TOTP, StatusCode> {
    let secret = Secret::Encoded(secret_b32.trim().to_string())
        .to_bytes()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret,
        Some(String::from("Bookclerk")),
        account.to_string(),
    )
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
/// * `otpauth` - Authenticator-app URL from [`TOTP::get_url`].
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
            Json(serde_json::json!({
                "ok": true,
                "mfa": {
                    "method": "totp",
                    "challenge_id": challenge_id,
                }
            }))
            .into_response(),
        ));
    }
    if require_second_factor {
        let passkeys = library
            .count_webauthn_credentials(user.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if passkeys > 0 {
            return Ok(Some(totp_error(
                StatusCode::FORBIDDEN,
                "passkey_required",
                "This host requires a passkey or authenticator app. Sign in with a passkey.",
            )));
        }
    }
    Ok(None)
}

#[derive(Debug, Deserialize)]
/// Optional current password for enroll/disable step-up.
struct ReauthBody {
    #[serde(default)]
    /// Password used for recent-reauth when the user already has a credential.
    current_password: Option<String>,
}

/// GET `/api/auth/totp` — whether TOTP is confirmed for this user.
async fn totp_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    Ok(Json(serde_json::json!({ "enabled": user.totp_enabled })))
}

/// Starts TOTP enrollment: new pending secret, otpauth URL, and QR SVG.
async fn enroll_begin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ReauthBody>,
) -> Result<Json<Value>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    if user.totp_enabled {
        return Err(StatusCode::CONFLICT);
    }
    require_recent_portal_reauth(&state, &headers, user.id, body.current_password.as_deref())
        .await?;
    let library = state.library_snapshot().await;
    let secret = Secret::generate_secret();
    let encoded = match secret.to_encoded() {
        Secret::Encoded(value) => value,
        Secret::Raw(_) => {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let account = totp_account_label(&user);
    let totp = totp_from_secret(&encoded, &account)?;
    let otpauth_url = totp.get_url();
    store_totp_secret(&library, user.id, TOTP_PENDING, &encoded).await?;
    let qr_svg = qr_svg(&otpauth_url)?;
    Ok(Json(serde_json::json!({
        "secret": encoded,
        "otpauth_url": otpauth_url,
        "qr_svg": qr_svg,
    })))
}

#[derive(Debug, Deserialize)]
/// First TOTP code that confirms the pending secret.
struct EnrollFinish {
    /// Six-digit code from the authenticator app.
    code: String,
}

/// Confirms the pending secret with a valid code and sets `users.totp_enabled`.
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
    if !totp.check_current(code).unwrap_or(false) {
        return Ok(totp_error(
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
    Ok(Json(serde_json::json!({ "ok": true })).into_response())
}

/// Disables TOTP after recent reauth and deletes sealed secrets.
async fn disable_totp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ReauthBody>,
) -> Result<Json<Value>, StatusCode> {
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
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
/// TOTP code plus the challenge id from password login.
struct LoginVerify {
    /// Server-issued challenge id (5-minute TTL, single use).
    challenge_id: String,
    /// Six-digit authenticator code.
    code: String,
}

/// Completes password+TOTP login and issues a portal session cookie.
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
    if !totp.check_current(body.code.trim()).unwrap_or(false) {
        let _ = auth.record_login_failure(&client_key).await;
        return Ok(invalid_authenticator_code());
    }
    auth.clear_login_failures(&client_key).await;
    issue_portal_session(&state, &library, &user, &headers, "totp_login").await
}

/// Current MFA policy (`require_second_factor`).
async fn get_mfa_policy(State(state): State<Arc<AppState>>) -> Json<Value> {
    let required = require_second_factor(&state).await;
    Json(serde_json::json!({ "require_second_factor": required }))
}

#[derive(Debug, Deserialize)]
/// PUT body for the host-wide second-factor policy.
struct MfaPolicyPut {
    /// When true, password login requires TOTP or the user must use a passkey.
    require_second_factor: bool,
    #[serde(default)]
    /// Current password for Owner step-up (operators omit this).
    current_password: Option<String>,
}

/// Persists `daemon.auth.require_second_factor` to `config.toml`.
async fn put_mfa_policy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<MfaPolicyPut>,
) -> Result<Json<Value>, Response> {
    require_operator_or_recent_owner(&state, &headers, body.current_password.as_deref())
        .await
        .map_err(|status| {
            totp_error(
                status,
                if status == StatusCode::UNAUTHORIZED {
                    "reauth_required"
                } else {
                    "forbidden"
                },
                if status == StatusCode::UNAUTHORIZED {
                    "Recent authentication is required to change the MFA policy."
                } else {
                    "You cannot change the MFA policy."
                },
            )
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
                    return Err(totp_error(
                        StatusCode::FORBIDDEN,
                        "mfa_enrollment_required",
                        "This host requires a passkey or authenticator app. Set one up to continue, or log out and finish later.",
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
        return Err(totp_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "config path is not available",
        ));
    };
    {
        let mut staged = state.config.read().await.clone();
        staged.daemon.auth.require_second_factor = body.require_second_factor;
        staged.write_toml_file(&config_path).map_err(|err| {
            totp_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("failed to write config.toml: {err}"),
            )
        })?;
    }
    {
        let mut cfg = state.config.write().await;
        cfg.daemon.auth.require_second_factor = body.require_second_factor;
    }
    Ok(Json(serde_json::json!({
        "require_second_factor": body.require_second_factor
    })))
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
            .expect("code")
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
