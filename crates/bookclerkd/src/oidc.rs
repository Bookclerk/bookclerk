//! Minimal OIDC authorization server (auth code + PKCE) for Audiobookshelf.
//!
//! Tokens are always bound to a first-party User — never minted from the
//! operator token alone. Discovery lives at `/.well-known/openid-configuration`.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bookclerk_library::hash_token;
use chrono::{Duration as ChronoDuration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use url::Url;

use crate::api::AppState;
use crate::auth::{constant_time_eq, require_operator_or_recent_owner, PORTAL_SESSION_COOKIE};

/// Access-token and ID-token lifetime in seconds (1 hour).
const ACCESS_TTL_SECS: i64 = 3600;
/// Refresh-token lifetime in days; only the hash is stored.
const REFRESH_TTL_DAYS: i64 = 30;
/// Authorization-code lifetime in seconds; codes are single-use.
const CODE_TTL_SECS: i64 = 300;

/// Mounts discovery, authorize, token, userinfo, revoke, and client admin APIs.
pub fn router(state: Arc<AppState>) -> Router {
    let protocol = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(openid_configuration),
        )
        .route("/oidc/authorize", get(authorize).post(authorize_consent))
        .route("/oidc/token", post(token))
        .route("/oidc/userinfo", get(userinfo))
        .route("/oidc/revoke", post(revoke))
        .with_state(state.clone());
    let admin = Router::new()
        .route(
            "/api/auth/oidc/clients",
            get(list_clients).post(create_client),
        )
        .route(
            "/api/auth/oidc/clients/{client_id}",
            axum::routing::put(update_client).delete(delete_client),
        )
        .route(
            "/api/auth/oidc/clients/{client_id}/rotate-secret",
            post(rotate_client_secret),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_operator_or_owner_auth,
        ))
        .with_state(state);
    protocol.merge(admin)
}

#[derive(Debug, Serialize)]
/// OIDC discovery document served at `/.well-known/openid-configuration`.
struct DiscoveryDocument {
    /// Issuer origin used as `iss` in minted JWTs (`public_origin` or loopback).
    issuer: String,
    /// Absolute URL of the authorization endpoint (`/oidc/authorize`).
    authorization_endpoint: String,
    /// Absolute URL of the token endpoint (`/oidc/token`).
    token_endpoint: String,
    /// Absolute URL of the userinfo endpoint (`/oidc/userinfo`).
    userinfo_endpoint: String,
    /// Absolute URL of the refresh-token revocation endpoint (`/oidc/revoke`).
    revocation_endpoint: String,
    /// Supported `response_type` values; this server only issues `code`.
    response_types_supported: &'static [&'static str],
    /// Supported token grants: `authorization_code` and `refresh_token`.
    grant_types_supported: &'static [&'static str],
    /// PKCE methods; only S256 is accepted (`plain` is rejected).
    code_challenge_methods_supported: &'static [&'static str],
    /// Scopes advertised to clients (`openid`, `profile`, and `email`).
    scopes_supported: &'static [&'static str],
    /// Subject identifier types; only public (stable user id) is used.
    subject_types_supported: &'static [&'static str],
    /// ID-token signing algorithms; tokens are HS256 with a derived key.
    id_token_signing_alg_values_supported: &'static [&'static str],
    /// Client auth methods: public PKCE (`none`) or `client_secret_post`.
    token_endpoint_auth_methods_supported: &'static [&'static str],
}

/// Serves the OIDC discovery document for registered clients.
async fn openid_configuration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<DiscoveryDocument> {
    let issuer = issuer_base(&state, Some(&headers)).await;
    Json(DiscoveryDocument {
        issuer: issuer.clone(),
        authorization_endpoint: format!("{issuer}/oidc/authorize"),
        token_endpoint: format!("{issuer}/oidc/token"),
        userinfo_endpoint: format!("{issuer}/oidc/userinfo"),
        revocation_endpoint: format!("{issuer}/oidc/revoke"),
        response_types_supported: &["code"],
        grant_types_supported: &["authorization_code", "refresh_token"],
        code_challenge_methods_supported: &["S256"],
        scopes_supported: &["openid", "profile", "email"],
        subject_types_supported: &["public"],
        id_token_signing_alg_values_supported: &["HS256"],
        token_endpoint_auth_methods_supported: &["none", "client_secret_post"],
    })
}

#[derive(Debug, Deserialize)]
/// Query parameters for `GET /oidc/authorize` (authorization-code + PKCE).
pub struct AuthorizeQuery {
    /// Registered OIDC client id (must exist in the library store).
    pub client_id: String,
    /// Redirect URI; must match a URI registered for the client.
    pub redirect_uri: String,
    /// OAuth `response_type`; only `code` is accepted.
    pub response_type: String,
    /// Space-separated scopes; defaults to `openid profile` when omitted.
    pub scope: Option<String>,
    /// Opaque CSRF value echoed back on the redirect.
    pub state: Option<String>,
    /// PKCE S256 challenge (base64url SHA-256 of the verifier).
    pub code_challenge: String,
    /// PKCE method; omitted is treated as `plain` and then rejected.
    pub code_challenge_method: Option<String>,
}

/// Starts the authorization-code flow; unauthenticated users are sent to SPA login.
async fn authorize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthorizeQuery>,
) -> Result<Response, StatusCode> {
    validate_authorize_request(&state, &q).await?;
    let Some(user_id) = require_user_session(&state, &headers).await? else {
        // Operator token alone is not enough — send to SPA login.
        let next = format!(
            "/oidc/authorize?{}",
            serde_urlencoded_query(&q).unwrap_or_default()
        );
        return Ok(
            Redirect::temporary(&format!("/?next={}", urlencoding_encode(&next))).into_response(),
        );
    };
    issue_code_redirect(&state, user_id, &q).await
}

#[derive(Debug, Deserialize)]
/// Form body for `POST /oidc/authorize` after the user grants or denies consent.
pub struct ConsentBody {
    /// Registered OIDC client id (must exist in the library store).
    pub client_id: String,
    /// Redirect URI; must match a URI registered for the client.
    pub redirect_uri: String,
    /// OAuth `response_type`; only `code` is accepted.
    pub response_type: String,
    /// Space-separated scopes; defaults to `openid profile` when omitted.
    pub scope: Option<String>,
    /// Opaque CSRF value echoed back on the redirect.
    pub state: Option<String>,
    /// PKCE S256 challenge (base64url SHA-256 of the verifier).
    pub code_challenge: String,
    /// PKCE method; omitted is treated as `plain` and then rejected.
    pub code_challenge_method: Option<String>,
    /// `deny` aborts with `access_denied`; any other value proceeds to issue a code.
    pub consent: Option<String>,
}

/// Completes consent: deny redirects with `access_denied`, grant issues a code.
async fn authorize_consent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(body): Form<ConsentBody>,
) -> Result<Response, StatusCode> {
    let q = AuthorizeQuery {
        client_id: body.client_id,
        redirect_uri: body.redirect_uri,
        response_type: body.response_type,
        scope: body.scope,
        state: body.state,
        code_challenge: body.code_challenge,
        code_challenge_method: body.code_challenge_method,
    };
    validate_authorize_request(&state, &q).await?;
    if body.consent.as_deref() == Some("deny") {
        return Ok(redirect_error(
            &q.redirect_uri,
            q.state.as_deref(),
            "access_denied",
        ));
    }
    let Some(user_id) = require_user_session(&state, &headers).await? else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    issue_code_redirect(&state, user_id, &q).await
}

/// Rejects non-`code` responses, non-S256 PKCE, unknown clients, or unregistered redirects.
async fn validate_authorize_request(
    state: &AppState,
    q: &AuthorizeQuery,
) -> Result<(), StatusCode> {
    if q.response_type != "code" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let method = q.code_challenge_method.as_deref().unwrap_or("plain");
    if method != "S256" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let library = state.library_snapshot().await;
    let Some(client) = library
        .get_oidc_client(&q.client_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if !client.enabled {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !client.redirect_uris.iter().any(|u| u == &q.redirect_uri) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(())
}

/// Persists a hashed one-time code and redirects the RP with `code` (and `state`).
async fn issue_code_redirect(
    state: &AppState,
    user_id: i64,
    q: &AuthorizeQuery,
) -> Result<Response, StatusCode> {
    let library = state.library_snapshot().await;
    let client = library
        .get_oidc_client(&q.client_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::BAD_REQUEST)?;
    let requested = q
        .scope
        .clone()
        .unwrap_or_else(|| String::from("openid profile"));
    let scope = intersect_scopes(&requested, &client.allowed_scopes);
    let code = Uuid::new_v4().to_string();
    let expires = Utc::now() + ChronoDuration::seconds(CODE_TTL_SECS);
    library
        .insert_oidc_auth_code(
            &hash_token(&code),
            &q.client_id,
            user_id,
            &q.redirect_uri,
            &q.code_challenge,
            q.code_challenge_method.as_deref().unwrap_or("S256"),
            &scope,
            expires,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = library
        .insert_security_audit_event(
            &format!("user:{user_id}"),
            "oidc_authorize",
            Some(&format!(r#"{{"client_id":"{}"}}"#, q.client_id)),
        )
        .await;
    let mut loc = format!("{}?code={code}", q.redirect_uri);
    if let Some(state_param) = &q.state {
        loc.push_str(&format!("&state={state_param}"));
    }
    Ok(Redirect::temporary(&loc).into_response())
}

/// Require a **User** portal session. Operator bearer/session alone → None / reject.
/// Disabled users are rejected (same as cookie `/api/*` paths).
async fn require_user_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<i64>, StatusCode> {
    // Explicitly reject operator-only credentials for user token minting.
    let auth = state.auth_snapshot().await;
    if auth.enabled {
        if let Some(token) = bearer_token(headers) {
            if auth.token_matches(token) {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }
    let library = state.library_snapshot().await;
    let Some(raw) = cookie_value(headers, PORTAL_SESSION_COOKIE) else {
        return Ok(None);
    };
    let identity = library
        .get_portal_session_identity(&hash_token(&raw))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(identity) = identity else {
        return Ok(None);
    };
    let Some(user_id) = identity.user_id else {
        return Ok(None);
    };
    let user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if matches!(user.status, bookclerk_library::UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Some(user_id))
}

#[derive(Debug, Deserialize)]
/// `application/x-www-form-urlencoded` body for `POST /oidc/token`.
pub struct TokenForm {
    /// Grant: `authorization_code` or `refresh_token`; anything else is 400.
    pub grant_type: String,
    /// Authorization code from the redirect (`authorization_code` grant).
    pub code: Option<String>,
    /// Must match the URI bound to the consumed authorization code.
    pub redirect_uri: Option<String>,
    /// Client id; required for code exchange, optional (but checked) on refresh.
    pub client_id: Option<String>,
    /// Client secret for confidential clients (`client_secret_post`).
    pub client_secret: Option<String>,
    /// PKCE verifier; SHA-256 must match the stored challenge.
    pub code_verifier: Option<String>,
    /// Refresh token to rotate (`refresh_token` grant).
    pub refresh_token: Option<String>,
}

#[derive(Debug, Serialize)]
/// Successful token-endpoint JSON (access, optional refresh, and ID tokens).
struct TokenResponse {
    /// HS256 JWT access token bound to the User (`token_use=access`).
    access_token: String,
    /// Always `Bearer` for this server.
    token_type: &'static str,
    /// Access-token lifetime in seconds (same as [`ACCESS_TTL_SECS`]).
    expires_in: i64,
    /// Opaque refresh token when the client is allowed to receive one.
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// HS256 ID token; profile/email claims follow the granted scope.
    id_token: String,
    /// Space-separated scopes granted to this token pair.
    scope: String,
}

/// Dispatches token grants; unknown `grant_type` values return 400.
async fn token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Result<Json<TokenResponse>, StatusCode> {
    match form.grant_type.as_str() {
        "authorization_code" => token_authorization_code(&state, &headers, &form).await,
        "refresh_token" => token_refresh(&state, &headers, &form).await,
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

/// Exchanges a one-time code after PKCE and optional client-secret checks.
async fn token_authorization_code(
    state: &AppState,
    headers: &HeaderMap,
    form: &TokenForm,
) -> Result<Json<TokenResponse>, StatusCode> {
    let code = form.code.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
    let redirect_uri = form
        .redirect_uri
        .as_deref()
        .ok_or(StatusCode::BAD_REQUEST)?;
    let client_id = form.client_id.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
    let verifier = form
        .code_verifier
        .as_deref()
        .ok_or(StatusCode::BAD_REQUEST)?;
    let library = state.library_snapshot().await;
    let client = library
        .get_oidc_client(client_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !client.enabled {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if let Some(secret_hash) = &client.client_secret_hash {
        let provided = form.client_secret.as_deref().unwrap_or("");
        if !constant_time_eq(hash_token(provided).as_bytes(), secret_hash.as_bytes()) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let Some((stored_client, user_id, stored_redirect, challenge, method, scope)) = library
        .consume_oidc_auth_code(&hash_token(code))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if stored_client != client_id || stored_redirect != redirect_uri {
        return Err(StatusCode::BAD_REQUEST);
    }
    if method == "S256" {
        let expected = pkce_s256_challenge(verifier);
        if expected != challenge {
            return Err(StatusCode::BAD_REQUEST);
        }
    } else {
        return Err(StatusCode::BAD_REQUEST);
    }
    mint_tokens(state, headers, client_id, user_id, &scope).await
}

/// Issues a new token pair from a stored refresh token (confidential clients re-auth).
async fn token_refresh(
    state: &AppState,
    headers: &HeaderMap,
    form: &TokenForm,
) -> Result<Json<TokenResponse>, StatusCode> {
    let refresh = form
        .refresh_token
        .as_deref()
        .ok_or(StatusCode::BAD_REQUEST)?;
    let library = state.library_snapshot().await;
    let Some((client_id, user_id, scope)) = library
        .get_oidc_refresh_token(&hash_token(refresh))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if let Some(req_client) = form.client_id.as_deref() {
        if req_client != client_id {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    // Confidential clients must present client_secret on refresh (client_secret_post).
    let client = library
        .get_oidc_client(&client_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !client.enabled {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !client.issue_refresh_token {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(secret_hash) = &client.client_secret_hash {
        let provided = form.client_secret.as_deref().unwrap_or("");
        if !constant_time_eq(hash_token(provided).as_bytes(), secret_hash.as_bytes()) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    mint_tokens(state, headers, &client_id, user_id, &scope).await
}

/// Signs access/ID JWTs and optionally persists a hashed refresh token.
async fn mint_tokens(
    state: &AppState,
    headers: &HeaderMap,
    client_id: &str,
    user_id: i64,
    scope: &str,
) -> Result<Json<TokenResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    let client = library
        .get_oidc_client(client_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !client.enabled {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::BAD_REQUEST)?;
    if matches!(user.status, bookclerk_library::UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    let issuer = issuer_base(state, Some(headers)).await;
    let hmac_key = signing_key(state).await;
    let now = Utc::now().timestamp();
    let access = sign_jwt(
        &hmac_key,
        &serde_json::json!({
            "iss": issuer,
            "sub": user_id.to_string(),
            "aud": client_id,
            "iat": now,
            "exp": now + ACCESS_TTL_SECS,
            "scope": scope,
            "token_use": "access",
        }),
    )?;
    let mut id_claims = serde_json::json!({
        "iss": issuer,
        "sub": user_id.to_string(),
        "aud": client_id,
        "iat": now,
        "exp": now + ACCESS_TTL_SECS,
    });
    if scope_has(scope, "profile") {
        id_claims["name"] = serde_json::json!(user.display_name);
        id_claims["preferred_username"] = serde_json::json!(user.login_name);
    }
    if scope_has(scope, "email") {
        if let Some(email) = user
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            id_claims["email"] = serde_json::json!(email);
            id_claims["email_verified"] = serde_json::json!(true);
        }
    }
    let id_token = sign_jwt(&hmac_key, &id_claims)?;
    let refresh_token = if client.issue_refresh_token {
        let refresh_raw = Uuid::new_v4().to_string();
        library
            .insert_oidc_refresh_token(
                &hash_token(&refresh_raw),
                client_id,
                user_id,
                scope,
                Utc::now() + ChronoDuration::days(REFRESH_TTL_DAYS),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Some(refresh_raw)
    } else {
        None
    };
    Ok(Json(TokenResponse {
        access_token: access,
        token_type: "Bearer",
        expires_in: ACCESS_TTL_SECS,
        refresh_token,
        id_token,
        scope: scope.to_string(),
    }))
}

/// Returns `sub` / `name` / `preferred_username` for a valid access JWT.
async fn userinfo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let token = bearer_token(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let hmac_key = signing_key(&state).await;
    let claims = verify_jwt(&hmac_key, token).ok_or(StatusCode::UNAUTHORIZED)?;
    if claims.get("token_use").and_then(|v| v.as_str()) != Some("access") {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let sub = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let user_id: i64 = sub.parse().map_err(|_| StatusCode::UNAUTHORIZED)?;
    let library = state.library_snapshot().await;
    let user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if matches!(user.status, bookclerk_library::UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    let scope = claims
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("openid profile");
    let mut body = serde_json::json!({ "sub": sub });
    if scope_has(scope, "profile") {
        body["name"] = serde_json::json!(user.display_name);
        body["preferred_username"] = serde_json::json!(user.login_name);
    }
    if scope_has(scope, "email") {
        if let Some(email) = user
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            body["email"] = serde_json::json!(email);
            body["email_verified"] = serde_json::json!(true);
        }
    }
    Ok(Json(body))
}

#[derive(Debug, Deserialize)]
/// Form body for `POST /oidc/revoke` (RFC 7009-style refresh-token revoke).
pub struct RevokeForm {
    /// Token to revoke; treated as a refresh token (hashed before lookup).
    pub token: String,
    /// Optional hint ignored; this server only stores refresh tokens.
    pub token_type_hint: Option<String>,
}

/// Best-effort refresh-token revoke; always returns 200 per RFC 7009.
async fn revoke(State(state): State<Arc<AppState>>, Form(form): Form<RevokeForm>) -> StatusCode {
    let library = state.library_snapshot().await;
    let _ = library
        .revoke_oidc_refresh_token(&hash_token(&form.token))
        .await;
    let _ = form.token_type_hint;
    StatusCode::OK
}

/// Issuer origin from `integrations.public_origin`, else this request, else localhost.
async fn issuer_base(state: &AppState, headers: Option<&HeaderMap>) -> String {
    let cfg = state.config.read().await;
    crate::origin::effective_origin_from_config(&cfg, headers)
}

/// Derives the HS256 key from the operator token (not the User session).
async fn signing_key(state: &AppState) -> [u8; 32] {
    let auth = state.auth_snapshot().await;
    let mut hasher = Sha256::new();
    hasher.update(b"bookclerk-oidc-hs256-v1:");
    hasher.update(auth.token.as_bytes());
    hasher.finalize().into()
}

/// Encodes an HS256 JWT; fails with 500 if claims cannot be serialized.
fn sign_jwt(key: &[u8; 32], claims: &serde_json::Value) -> Result<String, StatusCode> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
    let signing_input = format!("{header}.{payload}");
    let sig = URL_SAFE_NO_PAD.encode(hmac_sha256(key, signing_input.as_bytes()));
    Ok(format!("{signing_input}.{sig}"))
}

/// Verifies HS256 signature and `exp`; returns `None` on any failure.
fn verify_jwt(key: &[u8; 32], token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let sig = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let signing_input = format!("{header}.{payload}");
    let expected = hmac_sha256(key, signing_input.as_bytes());
    let got = URL_SAFE_NO_PAD.decode(sig).ok()?;
    if !constant_time_eq(expected.as_slice(), got.as_slice()) {
        return None;
    }
    let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let now = Utc::now().timestamp();
    if let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) {
        if now >= exp {
            return None;
        }
    }
    Some(claims)
}

/// HMAC-SHA256 (RFC 2104) without pulling `hmac` (digest 0.10 vs sha2 0.11).
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hashed = Sha256::digest(key);
        key_block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().into()
}

/// Computes the RFC 7636 S256 challenge (base64url SHA-256 of the verifier).
fn pkce_s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Redirects the RP with `error` (and `state` when present).
fn redirect_error(redirect_uri: &str, state: Option<&str>, error: &str) -> Response {
    let mut loc = format!("{redirect_uri}?error={error}");
    if let Some(s) = state {
        loc.push_str(&format!("&state={s}"));
    }
    Redirect::temporary(&loc).into_response()
}

/// Extracts a non-empty Bearer token from `Authorization`, if present.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Reads a named cookie value from the request `Cookie` header.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{name}=")) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Rebuilds the authorize query string so login can return the user here.
fn serde_urlencoded_query(q: &AuthorizeQuery) -> Option<String> {
    let mut pairs = vec![
        ("client_id", q.client_id.as_str()),
        ("redirect_uri", q.redirect_uri.as_str()),
        ("response_type", q.response_type.as_str()),
        ("code_challenge", q.code_challenge.as_str()),
    ];
    if let Some(s) = q.scope.as_deref() {
        pairs.push(("scope", s));
    }
    if let Some(s) = q.state.as_deref() {
        pairs.push(("state", s));
    }
    if let Some(s) = q.code_challenge_method.as_deref() {
        pairs.push(("code_challenge_method", s));
    }
    Some(
        pairs
            .into_iter()
            .map(|(k, v)| format!("{}={}", urlencoding_encode(k), urlencoding_encode(v)))
            .collect::<Vec<_>>()
            .join("&"),
    )
}

/// Percent-encodes a string for query values (RFC 3986 unreserved left intact).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One plugin-owned OIDC client to materialize (from `oidcClients` or plugin.toml).
#[derive(Debug, Clone)]
struct PluginOidcSync {
    /// Plugin id that owns this client.
    plugin_id: String,
    /// OAuth `client_id`.
    client_id: String,
    /// Operator-facing card title.
    display_name: String,
    /// Path appended to the plugin origin.
    callback_path: String,
    /// Dotted config key for the player origin.
    origin_config_key: String,
    /// Refresh-token default for **new** rows only.
    issue_refresh_token: bool,
    /// Scope default for **new** rows only.
    default_scopes: Vec<String>,
}

/// Materialize plugin-owned OIDC clients from guest RPC and `plugin.toml`.
///
/// Used at startup and after config reload. New plugin clients start disabled;
/// existing rows keep their enable flag while redirects refresh from plugin
/// settings.
pub async fn sync_plugin_oidc_clients(state: &AppState) -> bookclerk_library::Result<()> {
    let templates = collect_plugin_oidc_templates(state).await;
    sync_plugin_oidc_clients_with(state, &templates).await
}

/// Collect templates from discovered `[[oidc.clients]]` and loaded `oidcClients` RPCs.
async fn collect_plugin_oidc_templates(state: &AppState) -> Vec<PluginOidcSync> {
    let mut by_key = std::collections::BTreeMap::<(String, String), PluginOidcSync>::new();
    let cfg = state.config.read().await.clone();
    let discovered = tokio::task::spawn_blocking(move || {
        bookclerk_plugin_host::discover_plugins(&cfg).unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    for plugin in discovered {
        for client in &plugin.manifest.oidc.clients {
            let sync = PluginOidcSync {
                plugin_id: plugin.manifest.id.clone(),
                client_id: client.client_id.clone(),
                display_name: if client.display_name.trim().is_empty() {
                    client.client_id.clone()
                } else {
                    client.display_name.clone()
                },
                callback_path: client.callback_path.clone(),
                origin_config_key: client.origin_config_key.clone(),
                issue_refresh_token: client.issue_refresh_token,
                default_scopes: if client.default_scopes.is_empty() {
                    vec!["openid".into(), "profile".into()]
                } else {
                    client.default_scopes.clone()
                },
            };
            by_key.insert((sync.plugin_id.clone(), sync.client_id.clone()), sync);
        }
    }
    for integration in state.integrations.read().await.all() {
        let Ok(clients) = integration.provided_oidc_clients().await else {
            continue;
        };
        for client in clients {
            let sync = PluginOidcSync {
                plugin_id: integration.id().to_string(),
                client_id: client.client_id.clone(),
                display_name: if client.display_name.trim().is_empty() {
                    client.client_id.clone()
                } else {
                    client.display_name
                },
                callback_path: client.callback_path,
                origin_config_key: client.origin_config_key,
                issue_refresh_token: client.issue_refresh_token,
                default_scopes: if client.default_scopes.is_empty() {
                    vec!["openid".into(), "profile".into()]
                } else {
                    client.default_scopes
                },
            };
            by_key.insert((sync.plugin_id.clone(), sync.client_id.clone()), sync);
        }
    }
    by_key.into_values().collect()
}

/// Upsert the given plugin templates (tests pass a fixed list).
async fn sync_plugin_oidc_clients_with(
    state: &AppState,
    templates: &[PluginOidcSync],
) -> bookclerk_library::Result<()> {
    let mut seen = std::collections::BTreeMap::<&str, &str>::new();
    for tmpl in templates {
        if let Some(prev) = seen.insert(tmpl.client_id.as_str(), tmpl.plugin_id.as_str()) {
            return Err(bookclerk_library::LibraryError::Conflict(format!(
                "duplicate OIDC client_id `{}` from plugins `{prev}` and `{}`",
                tmpl.client_id, tmpl.plugin_id
            )));
        }
    }
    let library = state.library_snapshot().await;
    let cfg = state.config.read().await;
    for tmpl in templates {
        let base_url = origin_from_config_key(&cfg, &tmpl.origin_config_key);
        let redirects = redirect_uris_from_plugin_base(&base_url, &tmpl.callback_path);
        library
            .upsert_plugin_oidc_client(
                &tmpl.client_id,
                &tmpl.plugin_id,
                &tmpl.display_name,
                &redirects,
                tmpl.issue_refresh_token,
                &tmpl.default_scopes,
            )
            .await?;
    }
    Ok(())
}

/// Resolve a dotted `integrations.<id>.<field>` path to a string setting.
fn origin_from_config_key(cfg: &bookclerk_config::Config, key: &str) -> String {
    let Some(rest) = key.strip_prefix("integrations.") else {
        return String::new();
    };
    let (id, field) = rest.split_once('.').unwrap_or((rest, "base_url"));
    cfg.integrations
        .plugin_table(id)
        .and_then(|table| table.get(field))
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string()
}

/// Build `{origin}{callback_path}` from an operator-set plugin server URL.
///
/// Loopback hosts also register the usual `localhost` / `127.0.0.1` alias.
#[must_use]
pub fn redirect_uris_from_plugin_base(base_url: &str, callback_path: &str) -> Vec<String> {
    let Some(origin) = normalize_plugin_origin(base_url) else {
        return Vec::new();
    };
    let path = if callback_path.starts_with('/') {
        callback_path.to_string()
    } else {
        format!("/{callback_path}")
    };
    let primary = format!("{origin}{path}");
    loopback_redirect_aliases(&primary)
}

/// Strip a trailing slash and keep only an http(s) origin from a plugin `base_url`.
fn normalize_plugin_origin(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let url = Url::parse(trimmed).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?;
    match url.port() {
        Some(port) => Some(format!("{}://{host}:{port}", url.scheme())),
        None => Some(format!("{}://{host}", url.scheme())),
    }
}

/// Duplicate a loopback callback URI with `localhost` / `127.0.0.1` swapped.
fn loopback_redirect_aliases(uri: &str) -> Vec<String> {
    let Ok(mut url) = Url::parse(uri) else {
        return vec![uri.to_string()];
    };
    let host = url.host_str().unwrap_or("").to_string();
    let alt = match host.as_str() {
        "127.0.0.1" => Some("localhost"),
        "localhost" => Some("127.0.0.1"),
        "::1" => Some("localhost"),
        _ => None,
    };
    let mut out = vec![uri.to_string()];
    if let Some(alt_host) = alt {
        if url.set_host(Some(alt_host)).is_ok() {
            let alias = url.as_str().trim_end_matches('/').to_string();
            if !out.iter().any(|u| u == &alias) {
                out.push(alias);
            }
        }
    }
    out
}

/// True when `name` appears as a space-delimited scope token.
fn scope_has(scope: &str, name: &str) -> bool {
    scope
        .split_whitespace()
        .any(|s| s.eq_ignore_ascii_case(name))
}

/// Intersect requested scopes with the client's allowlist; always keep `openid` when allowed.
fn intersect_scopes(requested: &str, allowed: &[String]) -> String {
    let requested: Vec<&str> = requested
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .collect();
    let wanted = if requested.is_empty() {
        vec!["openid", "profile"]
    } else {
        requested
    };
    let mut out: Vec<String> = Vec::new();
    for scope in wanted {
        if allowed.iter().any(|a| a.eq_ignore_ascii_case(scope))
            && !out.iter().any(|s| s.eq_ignore_ascii_case(scope))
        {
            out.push(scope.to_string());
        }
    }
    if !out.iter().any(|s| s.eq_ignore_ascii_case("openid"))
        && allowed.iter().any(|a| a.eq_ignore_ascii_case("openid"))
    {
        out.insert(0, "openid".into());
    }
    if out.is_empty() {
        String::from("openid")
    } else {
        out.join(" ")
    }
}

/// JSON error body for OIDC client admin APIs.
fn oidc_client_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": "oidc_client",
            "message": message.into(),
        })),
    )
        .into_response()
}

/// Reject empty, oversized, or punctuation-heavy client ids.
fn validate_client_id(id: &str) -> Result<(), String> {
    let trimmed = id.trim();
    if trimmed.len() < 2 || trimmed.len() > 128 {
        return Err("client id must be 2–128 characters".into());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
    {
        return Err("client id may only contain letters, digits, '.', '_', '-', and '~'".into());
    }
    Ok(())
}

/// Parse redirect URIs, skipping blanks; require at least one http(s) URL.
fn validate_redirect_uris(uris: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for raw in uris {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = url::Url::parse(trimmed)
            .map_err(|_| format!("redirect URI is not an absolute URL: {trimmed}"))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err("redirect URIs must use http or https".into());
        }
        if parsed.cannot_be_a_base() {
            return Err("redirect URIs must be absolute".into());
        }
        out.push(trimmed.to_string());
    }
    if out.is_empty() {
        return Err("at least one redirect URI is required".into());
    }
    Ok(out)
}

/// Keep `openid` plus optional `profile` / `email` in a stable order.
fn normalize_client_scopes(scopes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for wanted in ["openid", "profile", "email"] {
        if scopes.iter().any(|s| s.trim().eq_ignore_ascii_case(wanted)) {
            out.push(wanted.to_string());
        }
    }
    if !out.iter().any(|s| s == "openid") {
        out.insert(0, "openid".into());
    }
    out
}

/// Random URL-safe secret shown once on create or rotate.
fn generate_client_secret() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// Operator JSON for a client; `secret` is included only when freshly minted.
fn client_view(
    client: &bookclerk_library::OidcClientRecord,
    secret: Option<String>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "client_id": client.client_id,
        "name": client.name,
        "redirect_uris": client.redirect_uris,
        "confidential": client.has_secret(),
        "issue_refresh_token": client.issue_refresh_token,
        "allowed_scopes": client.allowed_scopes,
        "has_secret": client.has_secret(),
        "enabled": client.enabled,
        "plugin_id": client.plugin_id,
    });
    if let Some(secret) = secret {
        body["client_secret"] = serde_json::json!(secret);
    }
    body
}

#[derive(Debug, Deserialize)]
/// Body for creating or updating a Bookclerk-as-IdP client.
struct OidcClientWrite {
    /// Public client_id (create only; path wins on update).
    #[serde(default)]
    client_id: Option<String>,
    /// Operator-facing display name.
    #[serde(default)]
    name: Option<String>,
    /// Allowed OAuth redirect URIs.
    #[serde(default)]
    redirect_uris: Vec<String>,
    /// When true, mint a client secret (confidential). When false, public PKCE.
    #[serde(default)]
    confidential: bool,
    /// When true, token responses include a refresh token.
    #[serde(default = "default_true")]
    issue_refresh_token: bool,
    /// Scopes this client may request (`openid`, `profile`, `email`).
    #[serde(default)]
    allowed_scopes: Vec<String>,
    /// When true, authorize and token endpoints accept this client.
    #[serde(default = "default_true")]
    enabled: bool,
    /// Required for a non-elevated Owner whose portal session is older than 15 minutes.
    #[serde(default)]
    current_password: Option<String>,
}

/// Serde default for `issue_refresh_token` (on unless the operator opts out).
fn default_true() -> bool {
    true
}

/// Owner/operator gate for client mutations (recent portal re-auth or operator).
async fn require_client_admin(
    state: &AppState,
    headers: &HeaderMap,
    password: Option<&str>,
) -> Result<(), Response> {
    require_operator_or_recent_owner(state, headers, password)
        .await
        .map_err(|status| {
            oidc_client_error(
                status,
                if status == StatusCode::UNAUTHORIZED {
                    "recent authentication required to change OIDC clients"
                } else {
                    "forbidden"
                },
            )
        })
}

/// Owner/operator list of Bookclerk-as-IdP clients (secrets never included).
async fn list_clients(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let clients = library
        .list_oidc_clients()
        .await
        .map_err(|err| oidc_client_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(serde_json::json!({
        "clients": clients.iter().map(|c| client_view(c, None)).collect::<Vec<_>>(),
    })))
}

/// Create a client; confidential clients return the generated secret once.
async fn create_client(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<OidcClientWrite>,
) -> Result<Json<serde_json::Value>, Response> {
    require_client_admin(&state, &headers, body.current_password.as_deref()).await?;
    let client_id = body.client_id.unwrap_or_default();
    validate_client_id(&client_id)
        .map_err(|msg| oidc_client_error(StatusCode::BAD_REQUEST, msg))?;
    let redirects = validate_redirect_uris(&body.redirect_uris)
        .map_err(|msg| oidc_client_error(StatusCode::BAD_REQUEST, msg))?;
    let scopes = normalize_client_scopes(&body.allowed_scopes);
    let secret = if body.confidential {
        Some(generate_client_secret())
    } else {
        None
    };
    let hash = secret.as_deref().map(hash_token);
    let library = state.library_snapshot().await;
    if library
        .get_oidc_client(client_id.trim())
        .await
        .map_err(|err| oidc_client_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .is_some()
    {
        return Err(oidc_client_error(
            StatusCode::CONFLICT,
            "a client with this id already exists",
        ));
    }
    let client = library
        .insert_oidc_client(
            client_id.trim(),
            hash.as_deref(),
            &redirects,
            body.name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            body.issue_refresh_token,
            &scopes,
            body.enabled,
            None,
        )
        .await
        .map_err(|err| oidc_client_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(client_view(&client, secret)))
}

/// Update redirects, name, scopes, and public/confidential mode (no secret echo unless rotating).
async fn update_client(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(client_id): Path<String>,
    Json(body): Json<OidcClientWrite>,
) -> Result<Json<serde_json::Value>, Response> {
    require_client_admin(&state, &headers, body.current_password.as_deref()).await?;
    validate_client_id(&client_id)
        .map_err(|msg| oidc_client_error(StatusCode::BAD_REQUEST, msg))?;
    let scopes = normalize_client_scopes(&body.allowed_scopes);
    let library = state.library_snapshot().await;
    let existing = library
        .get_oidc_client(&client_id)
        .await
        .map_err(|err| oidc_client_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .ok_or_else(|| oidc_client_error(StatusCode::NOT_FOUND, "unknown client"))?;
    let redirects = if existing.is_plugin_provided() {
        existing.redirect_uris.clone()
    } else {
        validate_redirect_uris(&body.redirect_uris)
            .map_err(|msg| oidc_client_error(StatusCode::BAD_REQUEST, msg))?
    };
    let (secret_action, generated_secret) = if body.confidential && !existing.has_secret() {
        let raw = generate_client_secret();
        (Some(Some(hash_token(&raw))), Some(raw))
    } else if !body.confidential && existing.has_secret() {
        (Some(None), None)
    } else {
        (None, None)
    };
    let updated = library
        .update_oidc_client(
            &client_id,
            &redirects,
            body.name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            body.issue_refresh_token,
            &scopes,
            body.enabled,
            secret_action,
        )
        .await
        .map_err(|err| oidc_client_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .ok_or_else(|| oidc_client_error(StatusCode::NOT_FOUND, "unknown client"))?;
    Ok(Json(client_view(&updated, generated_secret)))
}

/// Delete a registered client.
async fn delete_client(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(client_id): Path<String>,
    body: Option<Json<OidcClientWrite>>,
) -> Result<StatusCode, Response> {
    let password = body
        .as_ref()
        .and_then(|Json(body)| body.current_password.as_deref());
    require_client_admin(&state, &headers, password).await?;
    let library = state.library_snapshot().await;
    let existing = library
        .get_oidc_client(&client_id)
        .await
        .map_err(|err| oidc_client_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if existing
        .as_ref()
        .is_some_and(bookclerk_library::OidcClientRecord::is_plugin_provided)
    {
        return Err(oidc_client_error(
            StatusCode::CONFLICT,
            "plugin-provided clients cannot be deleted; disable them instead",
        ));
    }
    let deleted = library
        .delete_oidc_client(&client_id)
        .await
        .map_err(|err| oidc_client_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(oidc_client_error(StatusCode::NOT_FOUND, "unknown client"))
    }
}

#[derive(Debug, Deserialize)]
/// Optional password for rotating a confidential client's secret.
struct RotateSecretBody {
    /// Required for a non-elevated Owner whose portal session is older than 15 minutes.
    #[serde(default)]
    current_password: Option<String>,
}

/// Mint a new secret for a confidential client; plaintext is returned once.
async fn rotate_client_secret(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(client_id): Path<String>,
    body: Option<Json<RotateSecretBody>>,
) -> Result<Json<serde_json::Value>, Response> {
    let password = body
        .as_ref()
        .and_then(|Json(body)| body.current_password.as_deref());
    require_client_admin(&state, &headers, password).await?;
    let raw = generate_client_secret();
    let library = state.library_snapshot().await;
    let updated = library
        .set_oidc_client_secret(&client_id, Some(&hash_token(&raw)))
        .await
        .map_err(|err| oidc_client_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .ok_or_else(|| oidc_client_error(StatusCode::NOT_FOUND, "unknown client"))?;
    Ok(Json(client_view(&updated, Some(raw))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_known_vector() {
        // RFC 7636 appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_s256_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn jwt_round_trip() {
        let key = [7u8; 32];
        let token = sign_jwt(
            &key,
            &serde_json::json!({"sub":"1","exp": Utc::now().timestamp() + 60, "token_use":"access"}),
        )
        .unwrap();
        let claims = verify_jwt(&key, &token).unwrap();
        assert_eq!(claims["sub"], "1");
    }

    #[tokio::test]
    async fn pkce_happy_path_and_operator_rejected() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use bookclerk_config::{Config, ListenAddrs};
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::{hash_token, LibraryStore, UserRole};
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_source::SourceRegistry;
        use chrono::Duration as ChronoDuration;
        use http_body_util::BodyExt;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
        use tower::ServiceExt;

        use crate::api::AppState;
        use crate::auth::OperatorAuthState;

        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let mut cfg = Config::default();
        cfg.daemon.listen = ListenAddrs::parse_list("127.0.0.1:8787").unwrap();
        cfg.daemon.auth.enabled = true;
        cfg.integrations.public_origin = Some(String::from("http://127.0.0.1:8787"));
        let state = Arc::new(AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(library.clone())),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                "oidc-op-token".into(),
                12,
                true,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
            tray_handoff: Mutex::new(None),
            event_node_id: std::sync::OnceLock::new(),
        });
        let user = library
            .create_user_with_login(UserRole::Member, Some("Abs User"), Some("absuser"), None)
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(user.id, Some("Abs User"))
            .await
            .unwrap();
        let portal_raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&portal_raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(1),
            )
            .await
            .unwrap();
        let portal_cookie = format!("{PORTAL_SESSION_COOKIE}={portal_raw}");

        library
            .insert_oidc_client(
                "audiobookshelf",
                None,
                &[
                    String::from("http://127.0.0.1:13378/auth/openid/callback"),
                    String::from("http://localhost:13378/auth/openid/callback"),
                ],
                Some("Audiobookshelf"),
                true,
                &["openid".into(), "profile".into(), "email".into()],
                true,
                None,
            )
            .await
            .unwrap();

        let app = crate::api::router(state.clone(), None);

        // Operator bearer cannot authorize.
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/oidc/authorize?client_id=audiobookshelf&redirect_uri=http%3A%2F%2F127.0.0.1%3A13378%2Fauth%2Fopenid%2Fcallback&response_type=code&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256&scope=openid")
                    .header(header::AUTHORIZATION, "Bearer oidc-op-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let authz = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/oidc/authorize?client_id=audiobookshelf&redirect_uri=http%3A%2F%2F127.0.0.1%3A13378%2Fauth%2Fopenid%2Fcallback&response_type=code&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256&scope=openid")
                    .header(header::COOKIE, &portal_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authz.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = authz
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let code = loc
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();

        let token_body = format!(
            "grant_type=authorization_code&code={code}&redirect_uri=http%3A%2F%2F127.0.0.1%3A13378%2Fauth%2Fopenid%2Fcallback&client_id=audiobookshelf&code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        );
        let tok = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oidc/token")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(token_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tok.status(), StatusCode::OK);
        let json = String::from_utf8(tok.into_body().collect().await.unwrap().to_bytes().to_vec())
            .unwrap();
        assert!(json.contains("access_token"));
        assert!(json.contains("id_token"));
        assert!(json.contains("refresh_token"));
    }

    async fn oidc_test_state() -> (Arc<AppState>, bookclerk_library::LibraryStore) {
        use bookclerk_config::{Config, ListenAddrs};
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::LibraryStore;
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_source::SourceRegistry;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

        use crate::api::AppState;
        use crate::auth::OperatorAuthState;

        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let mut cfg = Config::default();
        cfg.daemon.listen = ListenAddrs::parse_list("127.0.0.1:8787").unwrap();
        cfg.daemon.auth.enabled = true;
        let state = Arc::new(AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(library.clone())),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                "oidc-op-token".into(),
                12,
                true,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
            tray_handoff: Mutex::new(None),
            event_node_id: std::sync::OnceLock::new(),
        });
        (state, library)
    }

    async fn json_body(res: axum::http::Response<axum::body::Body>) -> serde_json::Value {
        use http_body_util::BodyExt;
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes) }))
    }

    #[test]
    fn origin_from_config_key_reads_integration_field() {
        let mut cfg = bookclerk_config::Config::default();
        cfg.integrations
            .set_audiobookshelf_string("base_url", "https://abs.example:13378");
        assert_eq!(
            origin_from_config_key(&cfg, "integrations.audiobookshelf.base_url"),
            "https://abs.example:13378"
        );
        assert!(origin_from_config_key(&cfg, "daemon.listen").is_empty());
        assert!(origin_from_config_key(&cfg, "integrations.missing.base_url").is_empty());
    }

    #[test]
    fn plugin_redirects_from_base_url() {
        assert!(redirect_uris_from_plugin_base("", "/auth/openid/callback").is_empty());
        assert_eq!(
            redirect_uris_from_plugin_base("https://abs.example:13378/", "/auth/openid/callback"),
            vec![String::from(
                "https://abs.example:13378/auth/openid/callback"
            )]
        );
        let loopback =
            redirect_uris_from_plugin_base("http://127.0.0.1:13378", "/auth/openid/callback");
        assert_eq!(
            loopback,
            vec![
                String::from("http://127.0.0.1:13378/auth/openid/callback"),
                String::from("http://localhost:13378/auth/openid/callback"),
            ]
        );
    }

    fn abs_oidc_sync() -> PluginOidcSync {
        PluginOidcSync {
            plugin_id: String::from("audiobookshelf"),
            client_id: String::from("audiobookshelf"),
            display_name: String::from("Audiobookshelf"),
            callback_path: String::from("/auth/openid/callback"),
            origin_config_key: String::from("integrations.audiobookshelf.base_url"),
            issue_refresh_token: true,
            default_scopes: vec!["openid".into(), "profile".into()],
        }
    }

    #[tokio::test]
    async fn plugin_oidc_sync_skips_uninstalled_and_starts_disabled() {
        let (state, library) = oidc_test_state().await;
        sync_plugin_oidc_clients_with(&state, &[]).await.unwrap();
        assert!(library
            .get_oidc_client("audiobookshelf")
            .await
            .unwrap()
            .is_none());

        {
            let mut cfg = state.config.write().await;
            cfg.integrations
                .set_audiobookshelf_string("base_url", "http://127.0.0.1:13378");
        }
        sync_plugin_oidc_clients_with(&state, &[abs_oidc_sync()])
            .await
            .unwrap();
        let seeded = library
            .get_oidc_client("audiobookshelf")
            .await
            .unwrap()
            .unwrap();
        assert!(!seeded.enabled);
        assert_eq!(seeded.plugin_id.as_deref(), Some("audiobookshelf"));
        assert!(!seeded.has_secret());
        assert_eq!(
            seeded.redirect_uris,
            vec![
                String::from("http://127.0.0.1:13378/auth/openid/callback"),
                String::from("http://localhost:13378/auth/openid/callback"),
            ]
        );

        library
            .update_oidc_client(
                "audiobookshelf",
                &seeded.redirect_uris,
                Some("Custom name"),
                false,
                &["openid".into()],
                true,
                Some(Some(hash_token("keep-me"))),
            )
            .await
            .unwrap();
        {
            let mut cfg = state.config.write().await;
            cfg.integrations
                .set_audiobookshelf_string("base_url", "https://abs.home:13378");
        }
        sync_plugin_oidc_clients_with(&state, &[abs_oidc_sync()])
            .await
            .unwrap();
        let row = library
            .get_oidc_client("audiobookshelf")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.name.as_deref(), Some("Custom name"));
        assert!(row.enabled);
        assert!(!row.issue_refresh_token);
        assert_eq!(row.allowed_scopes, vec!["openid"]);
        assert_eq!(
            row.redirect_uris,
            vec![String::from("https://abs.home:13378/auth/openid/callback")]
        );
        assert_eq!(
            row.client_secret_hash.as_deref(),
            Some(hash_token("keep-me").as_str())
        );
    }

    #[tokio::test]
    async fn plugin_oidc_sync_rejects_duplicate_client_id_before_writes() {
        let (state, library) = oidc_test_state().await;
        library
            .insert_oidc_client(
                "keep-me",
                None,
                &[String::from("https://player.example/callback")],
                Some("Keep"),
                true,
                &["openid".into()],
                true,
                None,
            )
            .await
            .unwrap();
        let colliding = PluginOidcSync {
            plugin_id: String::from("other-player"),
            client_id: String::from("audiobookshelf"),
            display_name: String::from("Other"),
            callback_path: String::from("/callback"),
            origin_config_key: String::from("integrations.audiobookshelf.base_url"),
            issue_refresh_token: false,
            default_scopes: vec!["openid".into()],
        };
        let err = sync_plugin_oidc_clients_with(&state, &[abs_oidc_sync(), colliding])
            .await
            .unwrap_err();
        assert!(matches!(err, bookclerk_library::LibraryError::Conflict(_)));
        assert!(library
            .get_oidc_client("audiobookshelf")
            .await
            .unwrap()
            .is_none());
        let kept = library.get_oidc_client("keep-me").await.unwrap().unwrap();
        assert_eq!(kept.name.as_deref(), Some("Keep"));
        assert!(kept.plugin_id.is_none());
    }

    #[tokio::test]
    async fn disabled_client_cannot_authorize() {
        use axum::body::Body;
        use axum::http::Request;
        use bookclerk_library::UserRole;
        use tower::ServiceExt;

        let (state, library) = oidc_test_state().await;
        library
            .insert_oidc_client(
                "player",
                None,
                &[String::from("https://player.example/callback")],
                Some("Player"),
                true,
                &["openid".into()],
                false,
                None,
            )
            .await
            .unwrap();
        let user = library
            .create_user_with_login(UserRole::Member, Some("Abs User"), Some("absuser"), None)
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(user.id, Some("Abs User"))
            .await
            .unwrap();
        let portal_raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&portal_raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(1),
            )
            .await
            .unwrap();
        let app = crate::api::router(state, None);
        let authz = app
            .oneshot(
                Request::builder()
                    .uri("/oidc/authorize?client_id=player&redirect_uri=https%3A%2F%2Fplayer.example%2Fcallback&response_type=code&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256&scope=openid")
                    .header(header::COOKIE, format!("{PORTAL_SESSION_COOKIE}={portal_raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authz.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn plugin_client_redirects_are_read_only_and_cannot_be_deleted() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (state, library) = oidc_test_state().await;
        library
            .upsert_plugin_oidc_client(
                "audiobookshelf",
                "audiobookshelf",
                "Audiobookshelf",
                &[String::from("http://127.0.0.1:13378/auth/openid/callback")],
                true,
                &["openid".into(), "profile".into()],
            )
            .await
            .unwrap();
        let app = crate::api::router(state, None);
        let updated = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/oidc/clients/audiobookshelf")
                    .header(header::AUTHORIZATION, "Bearer oidc-op-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "ABS",
                            "redirect_uris": ["https://evil.example/callback"],
                            "confidential": false,
                            "issue_refresh_token": true,
                            "allowed_scopes": ["openid", "profile"],
                            "enabled": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(updated.status(), StatusCode::OK);
        let body = json_body(updated).await;
        assert_eq!(body["enabled"], true);
        assert_eq!(
            body["redirect_uris"][0],
            "http://127.0.0.1:13378/auth/openid/callback"
        );
        assert_eq!(body["plugin_id"], "audiobookshelf");

        let deleted = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/auth/oidc/clients/audiobookshelf")
                    .header(header::AUTHORIZATION, "Bearer oidc-op-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn client_create_returns_secret_once_and_list_omits_it() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (state, _library) = oidc_test_state().await;
        let app = crate::api::router(state, None);
        let create = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/oidc/clients")
                    .header(header::AUTHORIZATION, "Bearer oidc-op-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "client_id": "player",
                            "name": "Player",
                            "redirect_uris": ["https://player.example/callback"],
                            "confidential": true,
                            "issue_refresh_token": true,
                            "allowed_scopes": ["openid", "profile", "email"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created = json_body(create).await;
        assert_eq!(created["client_id"], "player");
        assert_eq!(created["confidential"], true);
        let secret = created["client_secret"].as_str().expect("plaintext once");
        assert!(secret.len() >= 16);

        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/clients")
                    .header(header::AUTHORIZATION, "Bearer oidc-op-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let list_json = json_body(listed).await;
        assert_eq!(list_json["clients"][0]["client_id"], "player");
        assert!(list_json["clients"][0].get("client_secret").is_none());
        assert_eq!(list_json["clients"][0]["has_secret"], true);

        let rotated = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/oidc/clients/player/rotate-secret")
                    .header(header::AUTHORIZATION, "Bearer oidc-op-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rotated.status(), StatusCode::OK);
        let rotated_json = json_body(rotated).await;
        let new_secret = rotated_json["client_secret"].as_str().unwrap();
        assert_ne!(new_secret, secret);
    }

    #[tokio::test]
    async fn mint_tokens_honors_refresh_flag_and_email_scope() {
        use axum::body::Body;
        use axum::http::Request;
        use bookclerk_library::UserRole;
        use chrono::Duration as ChronoDuration;
        use tower::ServiceExt;

        let (state, library) = oidc_test_state().await;
        library
            .insert_oidc_client(
                "no-refresh",
                None,
                &[String::from("https://player.example/callback")],
                Some("No refresh"),
                false,
                &["openid".into(), "email".into()],
                true,
                None,
            )
            .await
            .unwrap();
        let user = library
            .create_user_with_profile(
                UserRole::Member,
                Some("Abs User"),
                Some("absuser"),
                Some("abs@example.com"),
                None,
            )
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(user.id, Some("Abs User"))
            .await
            .unwrap();
        let portal_raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&portal_raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(1),
            )
            .await
            .unwrap();
        let portal_cookie = format!("{PORTAL_SESSION_COOKIE}={portal_raw}");
        let app = crate::api::router(state, None);

        let authz = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/oidc/authorize?client_id=no-refresh&redirect_uri=https%3A%2F%2Fplayer.example%2Fcallback&response_type=code&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256&scope=openid%20email")
                    .header(header::COOKIE, &portal_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authz.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = authz
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let code = loc
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();
        let tok = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oidc/token")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "grant_type=authorization_code&code={code}&redirect_uri=https%3A%2F%2Fplayer.example%2Fcallback&client_id=no-refresh&code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tok.status(), StatusCode::OK);
        let json = json_body(tok).await;
        assert!(json.get("refresh_token").is_none(), "{json:?}");
        assert_eq!(json["scope"], "openid email");
        let access = json["access_token"].as_str().unwrap();
        let info = app
            .oneshot(
                Request::builder()
                    .uri("/oidc/userinfo")
                    .header(header::AUTHORIZATION, format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(info.status(), StatusCode::OK);
        let info_json = json_body(info).await;
        assert_eq!(info_json["email"], "abs@example.com");
        assert!(info_json.get("name").is_none());
    }
}
