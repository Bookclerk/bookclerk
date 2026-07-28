//! Operator and portal session authentication for the daemon HTTP API.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_integrations::portal_identity_from_headers;
use bookclerk_library::{portal_prefs_key, PortalIdentity, OPERATOR_PREFS_KEY};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::api::AppState;

pub const SESSION_COOKIE: &str = "bookclerk_operator_session";

#[derive(Debug)]
pub struct OperatorAuthState {
    pub token: String,
    pub sessions: Mutex<HashMap<String, Instant>>,
    pub session_ttl: Duration,
    pub enabled: bool,
}

impl OperatorAuthState {
    pub fn new(token: String, session_ttl_hours: u64, enabled: bool) -> Self {
        Self {
            token,
            sessions: Mutex::new(HashMap::new()),
            session_ttl: Duration::from_secs(session_ttl_hours.saturating_mul(3600).max(3600)),
            enabled,
        }
    }

    fn token_matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.token.as_bytes(), candidate.as_bytes())
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct AuthMeResponse {
    pub authenticated: bool,
    /// `operator`, `portal`, or omitted when anonymous.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Configured post-auth landing view (`discover` / `library` / `accounts`).
    /// Loaded from per-user DB preferences (not config.toml).
    pub default_view: String,
    /// Whether this session may acquire / scan / manage jobs.
    pub can_acquire: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal: Option<PortalMeInfo>,
}

#[derive(Debug, Serialize)]
pub struct PortalMeInfo {
    pub identity_id: i64,
    pub provider: String,
    pub external_user_id: String,
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    ok: bool,
    role: String,
    default_view: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, StatusCode> {
    let auth = state.auth.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let default_view = default_view_for_subject(&state.library, OPERATOR_PREFS_KEY, None);
    if !auth.enabled {
        return Ok((
            StatusCode::OK,
            [(
                header::SET_COOKIE,
                format!("{SESSION_COOKIE}=disabled; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
            )],
            Json(LoginResponse {
                ok: true,
                role: String::from("operator"),
                default_view,
            }),
        )
            .into_response());
    }
    if !auth.token_matches(body.token.trim()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let session_id = Uuid::new_v4().to_string();
    {
        let mut sessions = auth.sessions.lock().await;
        prune_sessions(&mut sessions, auth.session_ttl);
        sessions.insert(session_id.clone(), Instant::now());
    }
    let max_age = auth.session_ttl.as_secs();
    let cookie =
        format!("{SESSION_COOKIE}={session_id}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(LoginResponse {
            ok: true,
            role: String::from("operator"),
            default_view,
        }),
    )
        .into_response())
}

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(auth) = state.auth.as_ref() {
        if let Some(session_id) = session_id_from_headers(&headers) {
            auth.sessions.lock().await.remove(&session_id);
        }
    }
    let mut hdrs = HeaderMap::new();
    for cookie in [
        format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
        String::from("bookclerk_portal_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    ] {
        if let Ok(v) = header::HeaderValue::from_str(&cookie) {
            hdrs.append(header::SET_COOKIE, v);
        }
    }
    (
        StatusCode::OK,
        hdrs,
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

pub async fn me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let Some(auth) = state.auth.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(AuthMeResponse {
                authenticated: false,
                role: None,
                default_view: String::from("discover"),
                can_acquire: false,
                portal: None,
            }),
        );
    };

    if !auth.enabled {
        let default_view = default_view_for_subject(&state.library, OPERATOR_PREFS_KEY, None);
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(String::from("operator")),
                default_view,
                can_acquire: true,
                portal: None,
            }),
        );
    }

    if authorize_operator(auth, &headers).await {
        let default_view = default_view_for_subject(&state.library, OPERATOR_PREFS_KEY, None);
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(String::from("operator")),
                default_view,
                can_acquire: true,
                portal: None,
            }),
        );
    }

    if let Some(identity) = portal_identity_from_headers(&state.library, &headers) {
        let key = portal_prefs_key(identity.id);
        let default_view = default_view_for_subject(&state.library, &key, Some(identity.id));
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(String::from("portal")),
                default_view,
                can_acquire: false,
                portal: Some(PortalMeInfo {
                    identity_id: identity.id,
                    provider: identity.provider,
                    external_user_id: identity.external_user_id,
                    label: identity.label,
                }),
            }),
        );
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(AuthMeResponse {
            authenticated: false,
            role: None,
            default_view: String::from("discover"),
            can_acquire: false,
            portal: None,
        }),
    )
}

pub async fn require_operator_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(auth) = state.auth.as_ref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    if !auth.enabled {
        return Ok(next.run(req).await);
    }
    if authorize_operator(auth, req.headers()).await {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Allow operator sessions **or** portal sessions (Discover / read-only library).
pub async fn require_operator_or_portal_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(auth) = state.auth.as_ref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    if !auth.enabled {
        return Ok(next.run(req).await);
    }
    if authorize_operator(auth, req.headers()).await {
        return Ok(next.run(req).await);
    }
    if portal_identity_from_headers(&state.library, req.headers()).is_some() {
        return Ok(next.run(req).await);
    }
    Err(StatusCode::UNAUTHORIZED)
}

/// Resolve portal identity when the caller is not an operator.
pub async fn caller_portal_identity(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<PortalIdentity> {
    let auth = state.auth.as_ref()?;
    if !auth.enabled {
        return None;
    }
    if authorize_operator(auth, headers).await {
        return None;
    }
    portal_identity_from_headers(&state.library, headers)
}

/// Subject key + optional portal identity id for the caller's preferences row.
pub async fn prefs_subject_for_caller(
    state: &AppState,
    headers: &HeaderMap,
) -> (String, Option<i64>) {
    if let Some(auth) = state.auth.as_ref() {
        if !auth.enabled || authorize_operator(auth, headers).await {
            return (OPERATOR_PREFS_KEY.to_string(), None);
        }
    } else {
        return (OPERATOR_PREFS_KEY.to_string(), None);
    }
    if let Some(identity) = portal_identity_from_headers(&state.library, headers) {
        return (portal_prefs_key(identity.id), Some(identity.id));
    }
    (OPERATOR_PREFS_KEY.to_string(), None)
}

fn default_view_for_subject(
    library: &bookclerk_library::LibraryStore,
    subject_key: &str,
    identity_id: Option<i64>,
) -> String {
    library
        .get_user_preferences_or_default(subject_key, identity_id)
        .map(|p| normalize_default_view(&p.default_view))
        .unwrap_or_else(|_| String::from("discover"))
}

async fn authorize_operator(auth: &OperatorAuthState, headers: &HeaderMap) -> bool {
    if let Some(token) = bearer_token(headers) {
        if auth.token_matches(token) {
            return true;
        }
    }
    let Some(session_id) = session_id_from_headers(headers) else {
        return false;
    };
    let mut sessions = auth.sessions.lock().await;
    prune_sessions(&mut sessions, auth.session_ttl);
    sessions
        .get(&session_id)
        .is_some_and(|created| created.elapsed() < auth.session_ttl)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn prune_sessions(sessions: &mut HashMap<String, Instant>, ttl: Duration) {
    sessions.retain(|_, created| created.elapsed() < ttl);
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[must_use]
pub fn normalize_default_view(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "library" => String::from("library"),
        "accounts" => String::from("accounts"),
        "wishlist" => String::from("wishlist"),
        _ => String::from("discover"),
    }
}
