//! Minimal OIDC authorization server (auth code + PKCE) for Audiobookshelf.
//!
//! Tokens are always bound to a first-party User — never minted from the
//! operator token alone. Discovery lives at `/.well-known/openid-configuration`.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bookclerk_library::hash_token;
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::api::AppState;
use crate::auth::{constant_time_eq, PORTAL_SESSION_COOKIE};

/// Access-token and ID-token lifetime in seconds (1 hour).
const ACCESS_TTL_SECS: i64 = 3600;
/// Refresh-token lifetime in days; only the hash is stored.
const REFRESH_TTL_DAYS: i64 = 30;
/// Authorization-code lifetime in seconds; codes are single-use.
const CODE_TTL_SECS: i64 = 300;

/// Mounts discovery, authorize, token, userinfo, and revoke on the daemon.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(openid_configuration),
        )
        .route("/oidc/authorize", get(authorize).post(authorize_consent))
        .route("/oidc/token", post(token))
        .route("/oidc/userinfo", get(userinfo))
        .route("/oidc/revoke", post(revoke))
        .with_state(state)
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
    /// Scopes advertised to clients (`openid` and `profile`).
    scopes_supported: &'static [&'static str],
    /// Subject identifier types; only public (stable user id) is used.
    subject_types_supported: &'static [&'static str],
    /// ID-token signing algorithms; tokens are HS256 with a derived key.
    id_token_signing_alg_values_supported: &'static [&'static str],
    /// Client auth methods: public PKCE (`none`) or `client_secret_post`.
    token_endpoint_auth_methods_supported: &'static [&'static str],
}

/// Serves the OIDC discovery document for Audiobookshelf and other RPs.
async fn openid_configuration(State(state): State<Arc<AppState>>) -> Json<DiscoveryDocument> {
    let issuer = issuer_base(&state).await;
    Json(DiscoveryDocument {
        issuer: issuer.clone(),
        authorization_endpoint: format!("{issuer}/oidc/authorize"),
        token_endpoint: format!("{issuer}/oidc/token"),
        userinfo_endpoint: format!("{issuer}/oidc/userinfo"),
        revocation_endpoint: format!("{issuer}/oidc/revoke"),
        response_types_supported: &["code"],
        grant_types_supported: &["authorization_code", "refresh_token"],
        code_challenge_methods_supported: &["S256"],
        scopes_supported: &["openid", "profile"],
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
    let Some((_id, _secret, uris, _name)) = library
        .get_oidc_client(&q.client_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if !uris.iter().any(|u| u == &q.redirect_uri) {
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
    let code = Uuid::new_v4().to_string();
    let scope = q
        .scope
        .clone()
        .unwrap_or_else(|| String::from("openid profile"));
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
/// Successful token-endpoint JSON (access, refresh, and ID tokens).
struct TokenResponse {
    /// HS256 JWT access token bound to the User (`token_use=access`).
    access_token: String,
    /// Always `Bearer` for this server.
    token_type: &'static str,
    /// Access-token lifetime in seconds (same as [`ACCESS_TTL_SECS`]).
    expires_in: i64,
    /// Opaque refresh token; only the hash is stored.
    refresh_token: String,
    /// HS256 ID token with `name` and `preferred_username` claims.
    id_token: String,
    /// Space-separated scopes granted to this token pair.
    scope: String,
}

/// Dispatches token grants; unknown `grant_type` values return 400.
async fn token(
    State(state): State<Arc<AppState>>,
    Form(form): Form<TokenForm>,
) -> Result<Json<TokenResponse>, StatusCode> {
    match form.grant_type.as_str() {
        "authorization_code" => token_authorization_code(&state, &form).await,
        "refresh_token" => token_refresh(&state, &form).await,
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

/// Exchanges a one-time code after PKCE and optional client-secret checks.
async fn token_authorization_code(
    state: &AppState,
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
    if let Some(secret_hash) = &client.1 {
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
    mint_tokens(state, client_id, user_id, &scope).await
}

/// Issues a new token pair from a stored refresh token (confidential clients re-auth).
async fn token_refresh(
    state: &AppState,
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
    if let Some(secret_hash) = &client.1 {
        let provided = form.client_secret.as_deref().unwrap_or("");
        if !constant_time_eq(hash_token(provided).as_bytes(), secret_hash.as_bytes()) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    mint_tokens(state, &client_id, user_id, &scope).await
}

/// Signs access/ID JWTs and persists a hashed refresh token for an enabled User.
async fn mint_tokens(
    state: &AppState,
    client_id: &str,
    user_id: i64,
    scope: &str,
) -> Result<Json<TokenResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    let user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::BAD_REQUEST)?;
    if matches!(user.status, bookclerk_library::UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    let issuer = issuer_base(state).await;
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
    let id_token = sign_jwt(
        &hmac_key,
        &serde_json::json!({
            "iss": issuer,
            "sub": user_id.to_string(),
            "aud": client_id,
            "iat": now,
            "exp": now + ACCESS_TTL_SECS,
            "name": user.display_name,
            "preferred_username": user.login_name,
        }),
    )?;
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
    Ok(Json(TokenResponse {
        access_token: access,
        token_type: "Bearer",
        expires_in: ACCESS_TTL_SECS,
        refresh_token: refresh_raw,
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
    Ok(Json(serde_json::json!({
        "sub": sub,
        "name": user.display_name,
        "preferred_username": user.login_name,
    })))
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

/// Issuer origin from `integrations.public_origin`, else loopback `:8787`.
async fn issuer_base(state: &AppState) -> String {
    let cfg = state.config.read().await;
    if let Some(origin) = cfg.integrations.public_origin.as_deref() {
        return origin.trim_end_matches('/').to_string();
    }
    String::from("http://127.0.0.1:8787")
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

/// Register a public (PKCE) ABS client — used at startup and in tests.
pub async fn ensure_default_abs_client(state: &AppState) -> bookclerk_library::Result<()> {
    let library = state.library_snapshot().await;
    let origin = issuer_base(state).await;
    let redirects = vec![
        format!("{origin}/"),
        String::from("http://127.0.0.1:13378/auth/openid/callback"),
        String::from("http://localhost:13378/auth/openid/callback"),
    ];
    library
        .upsert_oidc_client("audiobookshelf", None, &redirects, Some("Audiobookshelf"))
        .await
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
            jobs: Arc::new(RwLock::new(Vec::new())),
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
        });
        ensure_default_abs_client(&state)
            .await
            .expect("oidc client");
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
}
