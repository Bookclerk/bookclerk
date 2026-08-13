//! WebAuthn passkeys for User login and Owner elevation.
//!
//! Passkeys are the User-plane hatch when an upstream IdP is down. The Operator
//! token remains host break-glass and is never a passkey subject.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bookclerk_library::{UserRole, UserStatus};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use uuid::Uuid;
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Webauthn, WebauthnBuilder,
};

use crate::api::AppState;
use crate::auth::{
    issue_elevation, issue_portal_session, require_recent_portal_reauth,
    resolve_portal_caller_identity, timed_portal_identity_from_headers, ClientIp,
};

/// Passkey HTTP routes.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/auth/passkeys", get(list_passkeys))
        .route("/api/auth/passkeys/register/begin", post(register_begin))
        .route("/api/auth/passkeys/register/finish", post(register_finish))
        .route("/api/auth/passkeys/{id}", delete(delete_passkey))
        .route("/api/auth/passkeys/login/begin", post(login_begin))
        .route("/api/auth/passkeys/login/finish", post(login_finish))
        .route("/api/auth/passkeys/elevate/begin", post(elevate_begin))
        .route("/api/auth/passkeys/elevate/finish", post(elevate_finish))
        .with_state(state)
}

fn build_webauthn(origin: &str) -> Result<Webauthn, StatusCode> {
    let origin = origin.trim().trim_end_matches('/');
    let url = Url::parse(origin).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rp_id = url.host_str().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    WebauthnBuilder::new(rp_id, &url)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .rp_name("Bookclerk")
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn origin_webauthn(state: &AppState) -> Result<Webauthn, StatusCode> {
    let cfg = state.config.read().await;
    let origin = cfg
        .integrations
        .public_origin
        .as_deref()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("http://127.0.0.1:8787"));
    drop(cfg);
    build_webauthn(&origin)
}

fn ceremony_json<T: serde::Serialize>(
    challenge_id: &str,
    inner: &T,
) -> Result<Json<Value>, StatusCode> {
    let mut body = serde_json::to_value(inner).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let obj = body
        .as_object_mut()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    obj.insert(
        "challenge_id".into(),
        Value::String(challenge_id.to_string()),
    );
    Ok(Json(body))
}

fn cred_id_b64(id: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(id.as_ref())
}

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

async fn list_passkeys(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let rows = library
        .list_webauthn_credentials(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let passkeys: Vec<Value> = rows
        .into_iter()
        .map(|(id, cred_id, _)| serde_json::json!({ "id": id, "credential_id": cred_id }))
        .collect();
    Ok(Json(serde_json::json!({ "passkeys": passkeys })))
}

#[derive(Debug, Deserialize)]
struct ReauthBody {
    #[serde(default)]
    current_password: Option<String>,
}

async fn register_begin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ReauthBody>,
) -> Result<Json<Value>, StatusCode> {
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
    let webauthn = origin_webauthn(&state).await?;
    let existing = library
        .list_webauthn_credentials(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let exclude: Vec<Passkey> = existing
        .iter()
        .filter_map(|(_, _, json)| serde_json::from_str(json).ok())
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
    ceremony_json(&challenge_id, &ccr)
}

#[derive(Debug, Deserialize)]
struct CeremonyFinish {
    challenge_id: String,
    credential: Value,
}

async fn register_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CeremonyFinish>,
) -> Result<Json<Value>, StatusCode> {
    let user = require_user(&state, &headers).await?;
    let webauthn = origin_webauthn(&state).await?;
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
    library
        .insert_webauthn_credential(user.id, &cred_id, &passkey_json)
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    let _ = library
        .delete_elevated_operator_sessions_for_user(user.id)
        .await;
    let _ = library
        .insert_security_audit_event(&format!("user:{}", user.id), "passkey_register", None)
        .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn delete_passkey(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ReauthBody>,
) -> Result<Json<Value>, StatusCode> {
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
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct LoginBegin {
    login: String,
}

async fn login_begin(
    State(state): State<Arc<AppState>>,
    ClientIp(client_key): ClientIp,
    Json(body): Json<LoginBegin>,
) -> Result<Json<Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    if auth.login_throttle_check(&client_key).await.is_some() {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    match login_begin_inner(&state, body).await {
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

async fn login_begin_inner(state: &AppState, body: LoginBegin) -> Result<Json<Value>, StatusCode> {
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
        .filter_map(|(_, _, json)| serde_json::from_str(json).ok())
        .collect();
    if passkeys.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let webauthn = origin_webauthn(state).await?;
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
    ceremony_json(&challenge_id, &rcr)
}

async fn login_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CeremonyFinish>,
) -> Result<Response, StatusCode> {
    let webauthn = origin_webauthn(&state).await?;
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

async fn elevate_begin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
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
        .filter_map(|(_, _, json)| serde_json::from_str(json).ok())
        .collect();
    let webauthn = origin_webauthn(&state).await?;
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
    ceremony_json(&challenge_id, &rcr)
}

async fn elevate_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CeremonyFinish>,
) -> Result<Response, StatusCode> {
    let user = require_user(&state, &headers).await?;
    if user.role != UserRole::Owner {
        return Err(StatusCode::FORBIDDEN);
    }
    let webauthn = origin_webauthn(&state).await?;
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

fn uuid_for_user(user_id: i64) -> Uuid {
    Uuid::from_u64_pair(0x626f_6f6b_636c_6572, user_id as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_uuid_is_stable() {
        assert_eq!(uuid_for_user(1), uuid_for_user(1));
        assert_ne!(uuid_for_user(1), uuid_for_user(2));
    }
}
