//! Operator authentication for the daemon HTTP API.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
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
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    ok: bool,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, StatusCode> {
    let auth = state.auth.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    if !auth.enabled {
        return Ok((
            StatusCode::OK,
            [(
                header::SET_COOKIE,
                format!("{SESSION_COOKIE}=disabled; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
            )],
            Json(LoginResponse { ok: true }),
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
        Json(LoginResponse { ok: true }),
    )
        .into_response())
}

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(auth) = state.auth.as_ref() {
        if let Some(session_id) = session_id_from_headers(&headers) {
            auth.sessions.lock().await.remove(&session_id);
        }
    }
    let cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
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
            }),
        );
    };
    if !auth.enabled {
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
            }),
        );
    }
    if authorize(auth, &headers).await {
        (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
            }),
        )
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(AuthMeResponse {
                authenticated: false,
            }),
        )
    }
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
    if authorize(auth, req.headers()).await {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn authorize(auth: &OperatorAuthState, headers: &HeaderMap) -> bool {
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
