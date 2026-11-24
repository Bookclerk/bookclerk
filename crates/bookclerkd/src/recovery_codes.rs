//! Second-factor recovery codes (`docs/adr/notifications.md` decision 8,
//! `docs/server/sign-in.md#recovery-codes`).
//!
//! A patron gets ten single-use codes when they set up their first second
//! factor (an authenticator app or a passkey), shown once in the answer to
//! that enrollment. They can see how many are left and make a new set,
//! which replaces the old one. A code finishes a password reset in place
//! of the authenticator code or passkey (`crate::password_reset`); the
//! codes go when the account's last second factor does.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::recovery_code::RecoveryCode;
use bookclerk_library::{LibraryStore, RecoveryCodeIssue};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::api::AppState;
use crate::auth::{require_portal_identity, require_recent_portal_reauth, CallerRefusal};
use crate::openapi::{Access, RouteGroup};

/// The recovery-code routes: each resolves the caller's own session in
/// its handler.
///
/// # Returns
///
/// One group of routes for the signed-in user.
#[inline(never)]
pub(crate) fn route_groups() -> Vec<RouteGroup> {
    let own = OpenApiRouter::new().routes(routes!(get_recovery_codes, post_recovery_codes));
    vec![RouteGroup::new(own, Access::Public)]
}

/// How many recovery codes the caller has left.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct RecoveryCodeStatus {
    /// Unused codes; 0 when the caller has no second factor.
    #[schema(example = 8)]
    remaining: u64,
}

/// A new set of recovery codes, shown this once.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct RecoveryCodeSet {
    /// Ten single-use codes, each four groups of four letters and digits.
    /// They are never shown again; a new set replaces them.
    #[schema(example = json!(["7K3M-Q9TB-2XWD-HF6P", "M4RE-8N2V-TQ7K-ZC3J"]))]
    codes: Vec<String>,
}

/// The body of `POST /api/auth/recovery-codes`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = RecoveryCodesReauthBody)]
struct ReauthBody {
    /// The caller's password, needed when their sign-in is older than
    /// fifteen minutes.
    #[serde(default)]
    #[schema(example = "correct horse battery staple")]
    current_password: Option<String>,
}

/// The display form of each code in `codes`.
fn shown(codes: &[RecoveryCode]) -> Vec<String> {
    codes
        .iter()
        .map(|code| code.display().to_string())
        .collect()
}

/// Issues the set a user's first second factor comes with, when they have
/// no codes yet.
///
/// # Arguments
///
/// * `library` - The library to store the set in.
/// * `user_id` - The user who just set up a second factor.
///
/// # Returns
///
/// The codes to show once, or `None` when the user already had codes, no
/// longer has a second factor, or the set could not be stored (logged;
/// the patron can make a set from Settings).
pub(crate) async fn issue_first_set(library: &LibraryStore, user_id: i64) -> Option<Vec<String>> {
    let codes = RecoveryCode::generate_set();
    match library
        .issue_recovery_codes(user_id, &codes, RecoveryCodeIssue::First, Utc::now())
        .await
    {
        Ok(true) => Some(shown(&codes)),
        Ok(false) => None,
        Err(err) => {
            tracing::warn!(user_id, error = %err, "could not store recovery codes");
            None
        }
    }
}

/// Drops a user's recovery codes after a second factor was removed, when
/// none remains. A failure is logged: the codes then stay until the next
/// change, and a code still asks for a password-reset link to use.
pub(crate) async fn drop_without_factor(library: &LibraryStore, user_id: i64) {
    if let Err(err) = library.drop_recovery_codes_without_factor(user_id).await {
        tracing::warn!(user_id, error = %err, "could not drop recovery codes");
    }
}

/// The signed-in user's id.
///
/// # Errors
///
/// 401 when nobody is signed in, and the session-check 503 when the
/// session could not be checked.
async fn caller(state: &AppState, headers: &HeaderMap) -> Result<i64, CallerRefusal> {
    let library = state.library_snapshot().await;
    let identity = require_portal_identity(&library, headers).await?;
    Ok(identity.user_id.ok_or(StatusCode::UNAUTHORIZED)?)
}

/// How many recovery codes the caller has left.
///
/// Access: any signed-in user, for their own account.
#[utoipa::path(
    get,
    path = "/api/auth/recovery-codes",
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "How many unused codes the caller has.", body = RecoveryCodeStatus),
        (status = 401, description = "The caller is not signed in."),
        (status = 500, description = "The library could not be read."),
        (status = 503, description = "The sign-in could not be checked because the library did not answer in time (`session_check_unavailable`); ask again after `Retry-After` seconds."),
    ),
)]
async fn get_recovery_codes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<RecoveryCodeStatus>, CallerRefusal> {
    let user_id = caller(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let remaining = library
        .count_recovery_codes(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(RecoveryCodeStatus { remaining }))
}

/// Makes a new set of ten recovery codes, replacing the caller's current
/// set, and shows it this once.
///
/// Access: any signed-in user with a second factor, for their own account;
/// a sign-in older than fifteen minutes must send `current_password`.
#[utoipa::path(
    post,
    path = "/api/auth/recovery-codes",
    request_body = ReauthBody,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The new set; the old codes no longer work.", body = RecoveryCodeSet),
        (status = 401, description = "The caller is not signed in, or the session is older than fifteen minutes and `current_password` is missing or wrong."),
        (status = 409, description = "The caller has no authenticator app or passkey, so recovery codes have nothing to stand in for."),
        (status = 429, description = "Too many wrong `current_password` attempts for this account; try again after the lockout."),
        (status = 500, description = "The codes could not be hashed or stored."),
        (status = 503, description = "The sign-in could not be checked because the library did not answer in time (`session_check_unavailable`); ask again after `Retry-After` seconds."),
    ),
)]
async fn post_recovery_codes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ReauthBody>,
) -> Result<Json<RecoveryCodeSet>, CallerRefusal> {
    let user_id = caller(&state, &headers).await?;
    require_recent_portal_reauth(&state, &headers, user_id, body.current_password.as_deref())
        .await?;
    let library = state.library_snapshot().await;
    let codes = RecoveryCode::generate_set();
    let stored = library
        .issue_recovery_codes(user_id, &codes, RecoveryCodeIssue::Replace, Utc::now())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !stored {
        return Err(StatusCode::CONFLICT.into());
    }
    Ok(Json(RecoveryCodeSet {
        codes: shown(&codes),
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::auth::tests::{phase2_harness, phase2_owner_password};

    /// Sends `method uri` with the session `cookie` and a JSON `body`;
    /// answers the status and the JSON body.
    async fn call(
        app: &axum::Router,
        method: &str,
        uri: &str,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, cookie)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn codes_are_made_anew_only_with_a_second_factor_and_go_with_the_last_one() {
        let (_state, app, library) = phase2_harness("op-token-recovery").await;
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
        let owner = library
            .get_user_by_login_name("owner")
            .await
            .unwrap()
            .unwrap();
        let uri = "/api/auth/recovery-codes";

        // Without a second factor there is nothing to recover.
        let (status, json) = call(&app, "GET", uri, &cookie, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["remaining"], 0);
        let (status, _) = call(&app, "POST", uri, &cookie, "{}").await;
        assert_eq!(status, StatusCode::CONFLICT);

        library
            .insert_webauthn_credential(owner.id, "cred-owner", "{}", Some("Laptop"))
            .await
            .unwrap();
        let (status, json) = call(&app, "POST", uri, &cookie, "{}").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let first: Vec<String> = serde_json::from_value(json["codes"].clone()).unwrap();
        assert_eq!(first.len(), 10);
        let (status, json) = call(&app, "POST", uri, &cookie, "{}").await;
        assert_eq!(status, StatusCode::OK);
        let second: Vec<String> = serde_json::from_value(json["codes"].clone()).unwrap();
        assert!(first.iter().all(|code| !second.contains(code)));
        let (_, json) = call(&app, "GET", uri, &cookie, "").await;
        assert_eq!(json["remaining"], 10);

        // Removing the only passkey takes the codes with it.
        let passkey = library.list_webauthn_credentials(owner.id).await.unwrap()[0].id;
        let (status, _) = call(
            &app,
            "DELETE",
            &format!("/api/auth/passkeys/{passkey}"),
            &cookie,
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, json) = call(&app, "GET", uri, &cookie, "").await;
        assert_eq!(json["remaining"], 0);

        // Signed out, neither route answers.
        let (status, _) = call(&app, "GET", uri, "bookclerk_portal=nope", "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
