//! WebAuthn passkeys for User login and Owner elevation.
//!
//! Passkeys are the User-plane hatch when an upstream IdP is down. The Operator
//! token remains host break-glass and is never a passkey subject.

use crate::openapi::{Access, RouteGroup};
use std::sync::Arc;
use utoipa_axum::{router::OpenApiRouter, routes};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bookclerk_library::{UserRole, UserStatus};
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use uuid::Uuid;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Webauthn,
    WebauthnBuilder,
};

use crate::api::AppState;
use crate::auth::{
    issue_elevation, issue_portal_session, require_recent_portal_reauth,
    resolve_portal_caller_identity, timed_portal_identity_from_headers, ClientIp, Elevated,
    OkResponse, PortalSessionIssued,
};

/// Passkey HTTP routes. They carry no auth layer: each handler resolves the
/// session it needs.
///
/// # Returns
///
/// One public group.
pub(crate) fn route_groups() -> Vec<RouteGroup> {
    let routes = OpenApiRouter::new()
        .routes(routes!(list_passkeys))
        .routes(routes!(register_begin))
        .routes(routes!(register_finish))
        .routes(routes!(delete_passkey))
        .routes(routes!(login_begin))
        .routes(routes!(login_finish))
        .routes(routes!(elevate_begin))
        .routes(routes!(elevate_finish));
    vec![RouteGroup::new(routes, Access::Public)]
}

/// Builds a WebAuthn relying party from `origin` (`rp_id` = host).
///
/// # Arguments
///
/// * `origin` - Absolute origin (`scheme://host[:port]`). Loopback IPs should
///   already have been rewritten to `localhost` by [`crate::origin::rewrite_loopback_host`].
///
/// # Errors
///
/// Returns 400 when the URL has no registrable domain (raw IPs are not valid
/// WebAuthn RP IDs). Returns 500 when the builder otherwise fails.
fn build_webauthn(origin: &str) -> Result<Webauthn, StatusCode> {
    let origin = origin.trim().trim_end_matches('/');
    let url = Url::parse(origin).map_err(|_| StatusCode::BAD_REQUEST)?;
    let rp_id = url.host_str().ok_or(StatusCode::BAD_REQUEST)?;
    if url.domain().is_none() {
        tracing::warn!(
            origin,
            rp_id,
            "webauthn RP origin has no domain (use localhost or integrations.public_origin)"
        );
        return Err(StatusCode::BAD_REQUEST);
    }
    WebauthnBuilder::new(rp_id, &url)
        .map_err(|err| {
            tracing::error!(origin, rp_id, %err, "webauthn relying party rejected origin");
            StatusCode::BAD_REQUEST
        })?
        .rp_name("Bookclerk")
        .build()
        .map_err(|err| {
            tracing::error!(origin, %err, "webauthn builder failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Rewrites loopback IP origins to `localhost` so webauthn-rs can treat them as
/// an effective domain (`Url::domain()` is `None` for `127.0.0.1` / `::1`).
///
/// Uses `integrations.public_origin`, else the request `Origin`, else localhost.
///
/// Loopback IPs are rewritten to `localhost`. The tray and `cargo dev` open
/// `http://localhost:8787`; `http://127.0.0.1` is not a valid WebAuthn RP ID.
///
/// # Arguments
///
/// * `state` - Daemon state (reads `integrations.public_origin`).
/// * `headers` - Used for `Origin` when `public_origin` is unset.
///
/// # Errors
///
/// Returns 400 when the resolved origin is not a valid WebAuthn RP.
async fn origin_webauthn(state: &AppState, headers: &HeaderMap) -> Result<Webauthn, StatusCode> {
    let cfg = state.config.read().await;
    let origin = crate::origin::effective_origin_from_config(&cfg, Some(headers));
    drop(cfg);
    build_webauthn(&origin)
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// WebAuthn creation options for `navigator.credentials.create()`, plus the challenge id for the finish step.
pub struct PasskeyCreationOptions {
    /// Server-issued challenge id to send back with the attestation; consumed once, five-minute lifetime.
    #[schema(example = "6d2f0c1e-8a4b-4f3d-9e7c-5b1a2c3d4e5f")]
    pub challenge_id: String,
    /// The W3C `PublicKeyCredentialCreationOptions`, with binary fields base64url-encoded.
    #[serde(rename = "publicKey")]
    #[schema(schema_with = public_key_creation_schema)]
    pub public_key: Value,
}

/// Schema for the `publicKey` member of [`PasskeyCreationOptions`].
fn public_key_creation_schema() -> utoipa::openapi::schema::Schema {
    utoipa::openapi::ObjectBuilder::new()
        .schema_type(utoipa::openapi::schema::SchemaType::AnyValue)
        .description(Some(
            "W3C PublicKeyCredentialCreationOptions (relying party, user, challenge, algorithms, excluded credentials), binary fields base64url-encoded.",
        ))
        .examples([serde_json::json!({
            "rp": { "name": "Bookclerk", "id": "books.example.com" },
            "user": { "id": "AAAAAAAAAAAAAAAAAAAABw", "name": "sam", "displayName": "Sam" },
            "challenge": "k9u2Xo3cZ1Qp7mT4vW8yA2bC5dE6fG7hI8jK9lM0nO1",
            "pubKeyCredParams": [{ "type": "public-key", "alg": -7 }, { "type": "public-key", "alg": -257 }],
            "timeout": 60000,
            "attestation": "none",
            "excludeCredentials": [],
            "authenticatorSelection": { "residentKey": "required", "userVerification": "preferred" }
        })])
        .build()
        .into()
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// WebAuthn request options for `navigator.credentials.get()`, plus the challenge id for the finish step.
pub struct PasskeyRequestOptions {
    /// Server-issued challenge id to send back with the assertion; consumed once, five-minute lifetime.
    #[schema(example = "6d2f0c1e-8a4b-4f3d-9e7c-5b1a2c3d4e5f")]
    pub challenge_id: String,
    /// The W3C `PublicKeyCredentialRequestOptions`, with binary fields base64url-encoded.
    #[serde(rename = "publicKey")]
    #[schema(schema_with = public_key_request_schema)]
    pub public_key: Value,
    /// Credential mediation the daemon asks the browser for; absent unless
    /// conditional mediation was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "conditional")]
    pub mediation: Option<String>,
}

/// Schema for the `publicKey` member of [`PasskeyRequestOptions`].
fn public_key_request_schema() -> utoipa::openapi::schema::Schema {
    utoipa::openapi::ObjectBuilder::new()
        .schema_type(utoipa::openapi::schema::SchemaType::AnyValue)
        .description(Some(
            "W3C PublicKeyCredentialRequestOptions (challenge, relying party id, allowed credentials), binary fields base64url-encoded.",
        ))
        .examples([serde_json::json!({
            "challenge": "k9u2Xo3cZ1Qp7mT4vW8yA2bC5dE6fG7hI8jK9lM0nO1",
            "rpId": "books.example.com",
            "timeout": 60000,
            "userVerification": "preferred",
            "allowCredentials": [{ "type": "public-key", "id": "pQECAyYgASFYIE" }]
        })])
        .build()
        .into()
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /api/auth/passkeys`.
pub struct PasskeysResponse {
    /// The caller's passkeys, oldest first.
    pub passkeys: Vec<PasskeySummary>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One stored passkey.
pub struct PasskeySummary {
    /// Row id, for `DELETE /api/auth/passkeys/{id}`.
    #[schema(example = 4)]
    pub id: i64,
    /// Base64url credential id the authenticator reports.
    #[schema(example = "pQECAyYgASFYIE9ZJ2kLm3nO4pQ5rS6tU7vW8xY9zA0")]
    pub credential_id: String,
    /// Label chosen at registration (`Passkey` when none was given).
    #[schema(example = "YubiKey 5C")]
    pub name: String,
}

/// Splits a webauthn-rs challenge response into its `publicKey` member and
/// the optional `mediation` it carries, so the typed ceremony bodies serialize
/// exactly what the library produced.
///
/// # Errors
///
/// Returns 500 when the options do not serialize to an object with a `publicKey` member.
fn split_ceremony<T: serde::Serialize>(inner: &T) -> Result<(Value, Option<String>), StatusCode> {
    let Value::Object(mut obj) =
        serde_json::to_value(inner).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let public_key = obj
        .remove("publicKey")
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let mediation = obj
        .remove("mediation")
        .and_then(|value| value.as_str().map(str::to_owned));
    Ok((public_key, mediation))
}

/// Builds the creation-ceremony body with the challenge id for the finish step.
///
/// # Errors
///
/// Returns 500 when the options do not serialize (see [`split_ceremony`]).
fn creation_options(
    challenge_id: &str,
    ccr: &CreationChallengeResponse,
) -> Result<Json<PasskeyCreationOptions>, StatusCode> {
    let (public_key, _) = split_ceremony(ccr)?;
    Ok(Json(PasskeyCreationOptions {
        challenge_id: challenge_id.to_owned(),
        public_key,
    }))
}

/// Builds the request-ceremony body with the challenge id for the finish step.
///
/// # Errors
///
/// Returns 500 when the options do not serialize (see [`split_ceremony`]).
fn request_options(
    challenge_id: &str,
    rcr: &RequestChallengeResponse,
) -> Result<Json<PasskeyRequestOptions>, StatusCode> {
    let (public_key, mediation) = split_ceremony(rcr)?;
    Ok(Json(PasskeyRequestOptions {
        challenge_id: challenge_id.to_owned(),
        public_key,
        mediation,
    }))
}

/// Encodes a credential id as URL-safe base64 without padding.
fn cred_id_b64(id: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(id.as_ref())
}

/// Trims a passkey label and falls back to `Passkey` when empty (max 80 chars).
///
/// # Arguments
///
/// * `name` - Optional label from the register-finish body.
///
/// # Returns
///
/// A non-empty display name of at most 80 characters.
fn normalize_passkey_name(name: Option<&str>) -> String {
    let trimmed: String = name.unwrap_or("").trim().chars().take(80).collect();
    if trimmed.is_empty() {
        String::from("Passkey")
    } else {
        trimmed
    }
}

/// Resolves the signed-in local user from the portal session, or 401.
async fn require_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<bookclerk_library::UserRecord, StatusCode> {
    let library = state.library_snapshot().await;
    let identity = timed_portal_identity_from_headers(&library, headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let (_, _, user, _) = resolve_portal_caller_identity(&library, &identity).await;
    let Some(me) = user else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    library
        .get_user(me.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)
}

/// Lists this user's stored passkeys as `{ id, credential_id }` rows.
#[utoipa::path(
    get,
    path = "/api/auth/passkeys",
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The caller's passkeys.", body = PasskeysResponse),
        (status = 401, description = "The caller is not signed in."),
        (status = 500, description = "The passkeys could not be read."),
    ),
)]
async fn list_passkeys(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<PasskeysResponse>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let rows = library
        .list_webauthn_credentials(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let passkeys = rows
        .into_iter()
        .map(|row| PasskeySummary {
            id: row.id,
            credential_id: row.credential_id,
            name: row.name.unwrap_or_else(|| String::from("Passkey")),
        })
        .collect();
    Ok(Json(PasskeysResponse { passkeys }))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Optional current password for step-up before register or delete.
#[schema(as = PasskeyReauthBody)]
struct ReauthBody {
    #[serde(default)]
    /// Password used for recent-reauth when the user already has a credential.
    #[schema(example = "correct horse battery staple")]
    current_password: Option<String>,
}

/// Starts passkey registration (skips reauth only for first local-only setup).
#[utoipa::path(
    post,
    path = "/api/auth/passkeys/register/begin",
    request_body = ReauthBody,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "WebAuthn creation options.", body = PasskeyCreationOptions),
        (status = 400, description = "The daemon's origin is not a valid WebAuthn relying party (set `integrations.public_origin`)."),
        (status = 401, description = "The caller is not signed in, or the session is older than fifteen minutes and `current_password` is missing or wrong."),
        (status = 429, description = "Too many wrong `current_password` attempts for this account; try again after the lockout."),
        (status = 500, description = "The library could not be read, or the challenge could not be stored."),
    ),
)]
async fn register_begin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ReauthBody>,
) -> Result<Json<PasskeyCreationOptions>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let existing_count = library
        .count_webauthn_credentials(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let initial_setup = existing_count == 0
        && library
            .get_user_password_hash(user.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .is_none()
        && library
            .list_portal_identities_for_user(user.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .iter()
            .all(|p| p.provider == "local");
    if !initial_setup {
        require_recent_portal_reauth(&state, &headers, user.id, body.current_password.as_deref())
            .await?;
    }
    let webauthn = origin_webauthn(&state, &headers).await?;
    let existing = library
        .list_webauthn_credentials(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let exclude: Vec<Passkey> = existing
        .iter()
        .filter_map(|row| serde_json::from_str(&row.passkey_json).ok())
        .collect();
    let exclude_ids = exclude.iter().map(|pk| pk.cred_id().clone()).collect();
    let user_uuid = uuid_for_user(user.id);
    let name = user
        .login_name
        .as_deref()
        .or(user.email.as_deref())
        .unwrap_or("bookclerk-user");
    let display = user.display_name.as_deref().unwrap_or(name);
    let (ccr, reg_state) = webauthn
        .start_passkey_registration(user_uuid, name, display, Some(exclude_ids))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let challenge_id = Uuid::new_v4().to_string();
    let state_json =
        serde_json::to_string(&reg_state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    library
        .insert_webauthn_challenge(
            &challenge_id,
            Some(user.id),
            "register",
            &state_json,
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    creation_options(&challenge_id, &ccr)
}

/// Schema for the `credential` member of [`CeremonyFinish`]: the browser's
/// `PublicKeyCredential` serialized to JSON, an attestation after
/// `navigator.credentials.create()` or an assertion after
/// `navigator.credentials.get()`, binary fields base64url-encoded.
pub(crate) fn credential_schema() -> utoipa::openapi::schema::Schema {
    utoipa::openapi::ObjectBuilder::new()
        .schema_type(utoipa::openapi::schema::SchemaType::AnyValue)
        .description(Some(
            "W3C PublicKeyCredential as the browser serializes it: a registration attestation (`response.attestationObject`) or an authentication assertion (`response.authenticatorData`, `signature`, `userHandle`), binary fields base64url-encoded.",
        ))
        .examples([
            serde_json::json!({
                "id": "pQECAyYgASFYIE",
                "rawId": "pQECAyYgASFYIE",
                "type": "public-key",
                "response": {
                    "attestationObject": "o2NmbXRkbm9uZWdhdHRTdG10oGhhdXRoRGF0YViU",
                    "clientDataJSON": "eyJ0eXBlIjoid2ViYXV0aG4uY3JlYXRlIiwiY2hhbGxlbmdlIjoiazl1MlhvM2NaMVFwN21UNHZXOHlBMmJDNWRFNmZHN2hJOGpLOWxNMG5PMSIsIm9yaWdpbiI6Imh0dHBzOi8vYm9va3MuZXhhbXBsZS5jb20ifQ"
                },
                "extensions": {}
            }),
            serde_json::json!({
                "id": "pQECAyYgASFYIE",
                "rawId": "pQECAyYgASFYIE",
                "type": "public-key",
                "response": {
                    "authenticatorData": "SZYN5YgOjGh0NBcPZHZgW4_krrmihjLHmVzzuoMdl2MFAAAAAQ",
                    "clientDataJSON": "eyJ0eXBlIjoid2ViYXV0aG4uZ2V0IiwiY2hhbGxlbmdlIjoiazl1MlhvM2NaMVFwN21UNHZXOHlBMmJDNWRFNmZHN2hJOGpLOWxNMG5PMSIsIm9yaWdpbiI6Imh0dHBzOi8vYm9va3MuZXhhbXBsZS5jb20ifQ",
                    "signature": "MEUCIQDx8n2Y3kP1qT7vW9zA4bC6dE8fG0hJ2kL4mN6oP8qR0wIgS2tU4vW6xY8zA0bC2dE4fG6hJ8kL0mN2oP4qR6sT8uY",
                    "userHandle": "AAAAAAAAAAAAAAAAAAAABw"
                },
                "extensions": {}
            }),
        ])
        .build()
        .into()
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Browser attestation/assertion plus the challenge id from `begin`.
struct CeremonyFinish {
    /// Server-issued challenge id (consumed once; 5-minute TTL).
    #[schema(example = "6d2f0c1e-8a4b-4f3d-9e7c-5b1a2c3d4e5f")]
    challenge_id: String,
    /// Browser `PublicKeyCredential` JSON for `finish_*`.
    #[schema(schema_with = credential_schema)]
    credential: Value,
    #[serde(default)]
    /// Label stored with the credential (`Passkey` when empty).
    #[schema(example = "YubiKey 5C")]
    name: Option<String>,
}

/// Completes registration, stores the passkey, and revokes elevated operator sessions.
#[utoipa::path(
    post,
    path = "/api/auth/passkeys/register/finish",
    request_body = CeremonyFinish,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The passkey was stored.", body = OkResponse),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The challenge was issued to another user."),
        (status = 400, description = "The challenge is unknown or expired, the attestation is invalid, or the daemon's origin is not a valid WebAuthn relying party."),
        (status = 409, description = "The passkey is already registered."),
        (status = 500, description = "The library could not be read or written."),
    ),
)]
async fn register_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CeremonyFinish>,
) -> Result<Json<OkResponse>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    let webauthn = origin_webauthn(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let Some((Some(uid), state_json)) = library
        .take_webauthn_challenge(&body.challenge_id, "register")
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if uid != user.id {
        return Err(StatusCode::FORBIDDEN);
    }
    let reg_state: PasskeyRegistration =
        serde_json::from_str(&state_json).map_err(|_| StatusCode::BAD_REQUEST)?;
    let cred: RegisterPublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| StatusCode::BAD_REQUEST)?;
    let passkey = webauthn
        .finish_passkey_registration(&cred, &reg_state)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let cred_id = cred_id_b64(passkey.cred_id());
    let passkey_json =
        serde_json::to_string(&passkey).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let name = normalize_passkey_name(body.name.as_deref());
    library
        .insert_webauthn_credential(user.id, &cred_id, &passkey_json, Some(&name))
        .await
        .map_err(|err| {
            if crate::profile::is_unique_violation(&err) {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;
    let _ = library
        .delete_elevated_operator_sessions_for_user(user.id)
        .await;
    let _ = library
        .insert_security_audit_event(&format!("user:{}", user.id), "passkey_register", None)
        .await;
    Ok(Json(OkResponse::OK))
}

/// Deletes one passkey after recent reauth; 404 when the id is not this user's.
#[utoipa::path(
    delete,
    path = "/api/auth/passkeys/{id}",
    params(("id" = i64, Path, description = "Passkey id.")),
    request_body = ReauthBody,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The passkey was deleted.", body = OkResponse),
        (status = 401, description = "The caller is not signed in, or the session is older than fifteen minutes and `current_password` is missing or wrong."),
        (status = 429, description = "Too many wrong `current_password` attempts for this account; try again after the lockout."),
        (status = 404, description = "No such passkey for this user."),
        (status = 500, description = "The library could not be read or written."),
    ),
)]
async fn delete_passkey(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ReauthBody>,
) -> Result<Json<OkResponse>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    require_recent_portal_reauth(&state, &headers, user.id, body.current_password.as_deref())
        .await?;
    let library = state.library_snapshot().await;
    let ok = library
        .delete_webauthn_credential(user.id, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !ok {
        return Err(StatusCode::NOT_FOUND);
    }
    let _ = library
        .delete_elevated_operator_sessions_for_user(user.id)
        .await;
    Ok(Json(OkResponse::OK))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Login-name or email used to start a discoverable-less passkey login.
struct LoginBegin {
    /// Login name or email looked up before issuing an authentication challenge.
    #[schema(example = "dana")]
    login: String,
}

/// Rate-limits and starts passkey login; records a failure on any error.
#[utoipa::path(
    post,
    path = "/api/auth/passkeys/login/begin",
    request_body = LoginBegin,
    responses(
        (status = 200, description = "WebAuthn request options.", body = PasskeyRequestOptions),
        (status = 400, description = "The daemon's origin is not a valid WebAuthn relying party (set `integrations.public_origin`)."),
        (status = 401, description = "No user has this login name or email."),
        (status = 403, description = "The user is disabled."),
        (status = 404, description = "The user has no usable passkeys."),
        (status = 429, description = "Too many failed attempts from this client; try later."),
        (status = 500, description = "The library could not be read, or the challenge could not be stored."),
    ),
)]
async fn login_begin(
    State(state): State<Arc<AppState>>,
    ClientIp(client_key): ClientIp,
    headers: HeaderMap,
    Json(body): Json<LoginBegin>,
) -> Result<Json<PasskeyRequestOptions>, StatusCode> {
    let auth = state.auth_snapshot().await;
    if auth.login_throttle_check(&client_key).await.is_some() {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    match login_begin_inner(&state, &headers, body).await {
        Ok(json) => {
            auth.clear_login_failures(&client_key).await;
            Ok(json)
        }
        Err(err) => {
            let _ = auth.record_login_failure(&client_key).await;
            Err(err)
        }
    }
}

/// Issues a login challenge for an enabled user that already has passkeys (404 if none).
///
/// # Arguments
///
/// * `state` - Daemon state for library + WebAuthn origin.
/// * `headers` - Request headers (Origin used when `public_origin` is unset).
/// * `body` - Login name or email from the SPA.
///
/// # Errors
///
/// Returns 401/403/404 when the user cannot start a ceremony, or 400/500 from
/// WebAuthn origin construction.
async fn login_begin_inner(
    state: &AppState,
    headers: &HeaderMap,
    body: LoginBegin,
) -> Result<Json<PasskeyRequestOptions>, StatusCode> {
    let library = state.library_snapshot().await;
    let user = match library
        .get_user_by_login_name(&body.login)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Some(u) => u,
        None => library
            .get_user_by_email(&body.login)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::UNAUTHORIZED)?,
    };
    if matches!(user.status, UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    let rows = library
        .list_webauthn_credentials(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if rows.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let passkeys: Vec<Passkey> = rows
        .iter()
        .filter_map(|row| serde_json::from_str(&row.passkey_json).ok())
        .collect();
    if passkeys.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let webauthn = origin_webauthn(state, headers).await?;
    let (rcr, auth_state) = webauthn
        .start_passkey_authentication(&passkeys)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let challenge_id = Uuid::new_v4().to_string();
    let state_json =
        serde_json::to_string(&auth_state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    library
        .insert_webauthn_challenge(
            &challenge_id,
            Some(user.id),
            "login",
            &state_json,
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    request_options(&challenge_id, &rcr)
}

/// Verifies the assertion, updates the credential counter, and issues a portal session.
#[utoipa::path(
    post,
    path = "/api/auth/passkeys/login/finish",
    request_body = CeremonyFinish,
    responses(
        (status = 200, description = "Signed in; the response sets the portal session cookie.", body = PortalSessionIssued),
        (status = 400, description = "The challenge is unknown or expired, the credential is malformed, or the daemon's origin is not a valid WebAuthn relying party."),
        (status = 401, description = "The assertion does not verify, or the credential is not one of the challenged user's."),
        (status = 403, description = "The user is disabled."),
        (status = 500, description = "The library could not be read or written."),
    ),
)]
async fn login_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CeremonyFinish>,
) -> Result<Response, StatusCode> {
    let webauthn = origin_webauthn(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let Some((Some(uid), state_json)) = library
        .take_webauthn_challenge(&body.challenge_id, "login")
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let auth_state: PasskeyAuthentication =
        serde_json::from_str(&state_json).map_err(|_| StatusCode::BAD_REQUEST)?;
    let cred: PublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| StatusCode::BAD_REQUEST)?;
    let result = webauthn
        .finish_passkey_authentication(&cred, &auth_state)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let cred_id = cred_id_b64(result.cred_id());
    let Some((row_id, owner, json)) = library
        .get_webauthn_credential_by_cred_id(&cred_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if owner != uid {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut passkey: Passkey = serde_json::from_str(&json).map_err(|_| StatusCode::UNAUTHORIZED)?;
    if passkey.update_credential(&result).is_none() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let updated = serde_json::to_string(&passkey).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    library
        .update_webauthn_credential(row_id, &updated)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let user = library
        .get_user(uid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if matches!(user.status, UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    issue_portal_session(&state, &library, &user, &headers, "passkey_login").await
}

/// Starts Owner elevation; 403 unless the signed-in user is an Owner with passkeys.
#[utoipa::path(
    post,
    path = "/api/auth/passkeys/elevate/begin",
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "WebAuthn request options.", body = PasskeyRequestOptions),
        (status = 400, description = "The daemon's origin is not a valid WebAuthn relying party (set `integrations.public_origin`)."),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is not an Owner."),
        (status = 404, description = "The caller has no passkeys."),
        (status = 500, description = "The library could not be read, or the challenge could not be stored."),
    ),
)]
async fn elevate_begin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<PasskeyRequestOptions>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    if user.role != UserRole::Owner {
        return Err(StatusCode::FORBIDDEN);
    }
    let library = state.library_snapshot().await;
    let rows = library
        .list_webauthn_credentials(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if rows.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let passkeys: Vec<Passkey> = rows
        .iter()
        .filter_map(|row| serde_json::from_str(&row.passkey_json).ok())
        .collect();
    let webauthn = origin_webauthn(&state, &headers).await?;
    let (rcr, auth_state) = webauthn
        .start_passkey_authentication(&passkeys)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let challenge_id = Uuid::new_v4().to_string();
    let state_json =
        serde_json::to_string(&auth_state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    library
        .insert_webauthn_challenge(
            &challenge_id,
            Some(user.id),
            "elevate",
            &state_json,
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    request_options(&challenge_id, &rcr)
}

/// Completes Owner elevation only when the assertion is user-verified.
#[utoipa::path(
    post,
    path = "/api/auth/passkeys/elevate/finish",
    request_body = CeremonyFinish,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "Elevated; the response sets a short-lived operator cookie.", body = Elevated),
        (status = 400, description = "The challenge is unknown or expired, the credential is malformed, or the daemon's origin is not a valid WebAuthn relying party."),
        (status = 401, description = "The caller is not signed in, the assertion does not verify or was not user-verified, or the credential is not the caller's."),
        (status = 403, description = "The caller is not an Owner, or the challenge was issued to another user."),
        (status = 500, description = "The library could not be read or written."),
    ),
)]
async fn elevate_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CeremonyFinish>,
) -> Result<Response, StatusCode> {
    let user = require_user(&state, &headers).await?;
    if user.role != UserRole::Owner {
        return Err(StatusCode::FORBIDDEN);
    }
    let webauthn = origin_webauthn(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let Some((Some(uid), state_json)) = library
        .take_webauthn_challenge(&body.challenge_id, "elevate")
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if uid != user.id {
        return Err(StatusCode::FORBIDDEN);
    }
    let auth_state: PasskeyAuthentication =
        serde_json::from_str(&state_json).map_err(|_| StatusCode::BAD_REQUEST)?;
    let cred: PublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| StatusCode::BAD_REQUEST)?;
    let result = webauthn
        .finish_passkey_authentication(&cred, &auth_state)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    if !result.user_verified() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let cred_id = cred_id_b64(result.cred_id());
    let Some((row_id, owner, json)) = library
        .get_webauthn_credential_by_cred_id(&cred_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if owner != uid {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut passkey: Passkey = serde_json::from_str(&json).map_err(|_| StatusCode::UNAUTHORIZED)?;
    if passkey.update_credential(&result).is_none() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let updated = serde_json::to_string(&passkey).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    library
        .update_webauthn_credential(row_id, &updated)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    issue_elevation(&state, &library, user.id, &headers).await
}

/// Starts a user-verified passkey step-up for `user_id` (its own challenge
/// `kind`, five minutes): the shared branch consent's fresh second factor. 404 when
/// the user has no passkey.
///
/// # Errors
///
/// Returns 404 without passkeys, 400 when the origin is not a valid
/// WebAuthn RP, and 500 on store errors.
pub(crate) async fn begin_step_up(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
    kind: &str,
) -> Result<Json<PasskeyRequestOptions>, StatusCode> {
    let library = state.library_snapshot().await;
    let rows = library
        .list_webauthn_credentials(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let passkeys: Vec<Passkey> = rows
        .iter()
        .filter_map(|row| serde_json::from_str(&row.passkey_json).ok())
        .collect();
    if passkeys.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let webauthn = origin_webauthn(state, headers).await?;
    let (rcr, auth_state) = webauthn
        .start_passkey_authentication(&passkeys)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let challenge_id = Uuid::new_v4().to_string();
    let state_json =
        serde_json::to_string(&auth_state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    library
        .insert_webauthn_challenge(
            &challenge_id,
            Some(user_id),
            kind,
            &state_json,
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    request_options(&challenge_id, &rcr)
}

/// Completes a step-up begun with [`begin_step_up`]: the challenge is
/// consumed once, must be `user_id`'s, and the assertion must be
/// user-verified and from one of that user's passkeys.
///
/// # Errors
///
/// Returns 400 for an unknown or used challenge, 401 for an assertion that
/// does not verify, 403 for another user's challenge, and 500 on store
/// errors.
pub(crate) async fn finish_step_up(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
    kind: &str,
    challenge_id: &str,
    credential: Value,
) -> Result<(), StatusCode> {
    let webauthn = origin_webauthn(state, headers).await?;
    let library = state.library_snapshot().await;
    let Some((Some(uid), state_json)) = library
        .take_webauthn_challenge(challenge_id, kind)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if uid != user_id {
        return Err(StatusCode::FORBIDDEN);
    }
    let auth_state: PasskeyAuthentication =
        serde_json::from_str(&state_json).map_err(|_| StatusCode::BAD_REQUEST)?;
    let cred: PublicKeyCredential =
        serde_json::from_value(credential).map_err(|_| StatusCode::BAD_REQUEST)?;
    let result = webauthn
        .finish_passkey_authentication(&cred, &auth_state)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    if !result.user_verified() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let cred_id = cred_id_b64(result.cred_id());
    let Some((row_id, owner, json)) = library
        .get_webauthn_credential_by_cred_id(&cred_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if owner != uid {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut passkey: Passkey = serde_json::from_str(&json).map_err(|_| StatusCode::UNAUTHORIZED)?;
    if passkey.update_credential(&result).is_none() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let updated = serde_json::to_string(&passkey).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    library
        .update_webauthn_credential(row_id, &updated)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(())
}

/// Stable WebAuthn user handle: Bookclerk ASCII in the high 64 bits, `user_id` in the low.
fn uuid_for_user(user_id: i64) -> Uuid {
    Uuid::from_u64_pair(0x626f_6f6b_636c_6572, user_id as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use webauthn_authenticator_rs::softpasskey::SoftPasskey;
    use webauthn_authenticator_rs::WebauthnAuthenticator;

    #[test]
    fn user_uuid_is_stable() {
        assert_eq!(uuid_for_user(1), uuid_for_user(1));
        assert_ne!(uuid_for_user(1), uuid_for_user(2));
    }

    #[test]
    fn localhost_origin_builds() {
        assert!(build_webauthn("http://localhost:8787").is_ok());
        assert!(build_webauthn("https://bookclerk.example.com").is_ok());
    }

    #[test]
    fn loopback_ip_origin_does_not_build_until_rewritten() {
        assert!(build_webauthn("http://127.0.0.1:8787").is_err());
        assert!(build_webauthn("http://[::1]:8787").is_err());
        assert!(build_webauthn(&crate::origin::rewrite_loopback_host(
            "http://127.0.0.1:8787"
        ))
        .is_ok());
        assert!(build_webauthn(&crate::origin::rewrite_loopback_host("http://[::1]:8787")).is_ok());
    }

    /// Empty labels become `Passkey`; names are trimmed and capped at 80 characters.
    #[test]
    fn passkey_name_trims_and_falls_back() {
        assert_eq!(normalize_passkey_name(None), "Passkey");
        assert_eq!(normalize_passkey_name(Some("  ")), "Passkey");
        assert_eq!(normalize_passkey_name(Some(" Laptop ")), "Laptop");
        assert_eq!(normalize_passkey_name(Some(&"x".repeat(90))).len(), 80);
    }

    /// A full passkey ceremony against a software authenticator: register,
    /// store the credential as JSON the way the handlers do, then sign in
    /// with it and advance its counter.
    #[test]
    fn passkey_registers_and_authenticates_through_stored_json() {
        let origin = "https://bookclerk.test";
        let webauthn = build_webauthn(origin).expect("relying party builds");
        let url = Url::parse(origin).expect("origin parses");
        let mut authenticator = SoftPasskey::new(true);

        let (creation, registration) = webauthn
            .start_passkey_registration(uuid_for_user(7), "owner", "Owner", None)
            .expect("registration starts");
        let registration: PasskeyRegistration = serde_json::from_str(
            &serde_json::to_string(&registration).expect("registration state serializes"),
        )
        .expect("registration state deserializes");
        let attestation = authenticator
            .do_registration(url.clone(), creation)
            .expect("authenticator registers");
        let passkey = webauthn
            .finish_passkey_registration(&attestation, &registration)
            .expect("registration verifies");
        let stored = serde_json::to_string(&passkey).expect("passkey serializes");
        let stored: Passkey = serde_json::from_str(&stored).expect("passkey deserializes");
        assert_eq!(
            cred_id_b64(stored.cred_id()),
            URL_SAFE_NO_PAD.encode(attestation.raw_id.as_ref() as &[u8])
        );

        let (request, authentication) = webauthn
            .start_passkey_authentication(std::slice::from_ref(&stored))
            .expect("authentication starts");
        let authentication: PasskeyAuthentication = serde_json::from_str(
            &serde_json::to_string(&authentication).expect("authentication state serializes"),
        )
        .expect("authentication state deserializes");
        let assertion = authenticator
            .do_authentication(url, request)
            .expect("authenticator signs");
        let result = webauthn
            .finish_passkey_authentication(&assertion, &authentication)
            .expect("assertion verifies");
        assert_eq!(cred_id_b64(result.cred_id()), cred_id_b64(stored.cred_id()));
        let mut stored = stored;
        assert_eq!(stored.update_credential(&result), Some(true));
    }

    /// A passkey, an in-flight sign-in challenge and the browser's assertion
    /// stored by webauthn-rs 0.5 still deserialize and verify, so existing
    /// passkeys and sign-ins started before an upgrade keep working.
    #[test]
    fn passkey_stored_by_webauthn_0_5_still_signs_in() {
        let fixture: Value =
            serde_json::from_str(include_str!("testdata/webauthn-0.5-passkey.json"))
                .expect("fixture parses");
        let mut passkey: Passkey =
            serde_json::from_value(fixture["passkey"].clone()).expect("0.5 passkey loads");
        let authentication: PasskeyAuthentication =
            serde_json::from_value(fixture["authentication_state"].clone())
                .expect("0.5 authentication state loads");
        let assertion: PublicKeyCredential =
            serde_json::from_value(fixture["assertion"].clone()).expect("0.5 assertion loads");

        let webauthn = build_webauthn("https://bookclerk.test").expect("relying party builds");
        let result = webauthn
            .finish_passkey_authentication(&assertion, &authentication)
            .expect("0.5 assertion verifies");
        assert_eq!(
            cred_id_b64(result.cred_id()),
            cred_id_b64(passkey.cred_id())
        );
        assert_eq!(passkey.update_credential(&result), Some(true));

        let restored: Passkey = serde_json::from_str(
            &serde_json::to_string(&passkey).expect("upgraded passkey serializes"),
        )
        .expect("upgraded passkey reloads");
        assert_eq!(
            cred_id_b64(restored.cred_id()),
            cred_id_b64(passkey.cred_id())
        );
    }

    #[test]
    fn normalize_rewrites_loopback_ips_to_localhost() {
        assert_eq!(
            crate::origin::rewrite_loopback_host("http://127.0.0.1:8787"),
            "http://localhost:8787"
        );
        assert_eq!(
            crate::origin::rewrite_loopback_host("http://[::1]:8787/"),
            "http://localhost:8787"
        );
        assert_eq!(
            crate::origin::rewrite_loopback_host("https://bookclerk.example.com"),
            "https://bookclerk.example.com"
        );
    }
}
