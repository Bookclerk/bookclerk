//! Clerk sign-in: the library side of the OAuth code + PKCE flow of the
//! built-in public client [`CLERK_CLIENT_ID`].
//!
//! A clerk is an install that shows a person's books from this library
//! without joining it as a node. It signs in as the person and holds a
//! session token, never node keys:
//!
//! - `/oidc/authorize` sends a [`CLERK_CLIENT_ID`] request (checked by
//!   [`clerk_request_problem`]) to the SPA page `/clerk/approve`, which
//!   signs the user in and asks "Let *clerk* show your books from
//!   *library*?".
//! - `GET|POST /api/auth/clerk-consent` ([`get_clerk_consent`],
//!   [`post_clerk_consent`]): the consent page's API. Approving needs an
//!   active signed-in user and a connection the node API transport policy
//!   allows, then issues a single-use code that lives
//!   [`CLERK_CODE_TTL_SECS`]. It does not need "Let new branches join":
//!   a clerk adds no machine to the library.
//! - `POST /api/clerk/v1/token` ([`post_clerk_token`]): the clerk's daemon
//!   redeems the code with its PKCE verifier for a `clerk` session
//!   ([`bookclerk_library::PortalSessionKind::Clerk`]) in the user's
//!   session list. Each use keeps it signed in for another
//!   [`CLERK_IDLE_DAYS`] days.
//!
//! The token is accepted only as `Authorization: Bookclerk-Clerk` and only
//! on the route group a library session may use for the person's own
//! books (see [`crate::clerk_token`]).

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine as _;
use bookclerk_library::hash_token;
use bookclerk_node_proto::{
    canonical_origin, is_clerk_name, ClerkAuthorizeParams, ClerkTokenAnswer, ClerkTokenRequest,
    ClerkUser, CLERK_CLIENT_ID, CLERK_CODE_TTL_SECS, CLERK_IDLE_DAYS, CLERK_SCOPE,
};
pub(crate) use bookclerk_node_proto::{clerk_request_problem, parse_clerk_query};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroize::Zeroizing;

use super::branch::{
    consent_error, consenting_user, no_store, owner_info, redirect_with, ConsentRedirect,
    ConsentingUser,
};
use super::check_transport;
use crate::api::AppState;
use crate::auth::UserCaller;

/// Largest `POST /api/auth/clerk-consent` body.
const MAX_CONSENT_BODY_BYTES: usize = 8 * 1024;

/// Largest `POST /api/clerk/v1/token` body.
const MAX_TOKEN_BODY_BYTES: usize = 8 * 1024;

/// What the stored scope of an approved code adds after [`CLERK_SCOPE`]
/// for the approving sign-in: the portal identity id, so the card's session
/// sits beside the browser session that approved it (an integration user's
/// Sessions list shows and revokes it there).
const CONSENTED_IDENTITY_MARK: &str = " identity:";

/// What the stored scope of an approved code adds after the identity: the
/// consented clerk name, base64url, so the session is labelled with the
/// name the user saw whatever the redeem says.
const CONSENTED_NAME_MARK: &str = " name:";

/// What an approved code was consented for: the clerk name and the portal
/// identity the card's session belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Consent {
    /// The clerk name the user approved.
    name: String,
    /// The approving portal identity, or `None` when the card gets the
    /// user's local identity.
    identity_id: Option<i64>,
}

/// The scope stored with an approved code: [`CLERK_SCOPE`], the approving
/// portal identity when there is one, and the consented clerk name.
fn consented_scope(clerk_name: &str, identity_id: Option<i64>) -> String {
    let name = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(clerk_name.as_bytes());
    let identity = identity_id
        .map(|id| format!("{CONSENTED_IDENTITY_MARK}{id}"))
        .unwrap_or_default();
    format!("{CLERK_SCOPE}{identity}{CONSENTED_NAME_MARK}{name}")
}

/// What [`consented_scope`] wrote, or `None` when the scope is not a clerk
/// consent.
fn consented(scope: &str) -> Option<Consent> {
    let rest = scope.strip_prefix(CLERK_SCOPE)?;
    let (identity_id, rest) = match rest.strip_prefix(CONSENTED_IDENTITY_MARK) {
        Some(tail) => {
            let (id, tail) = tail.split_once(' ')?;
            (Some(id.parse::<i64>().ok()?), format!(" {tail}"))
        }
        None => (None, rest.to_string()),
    };
    let encoded = rest.strip_prefix(CONSENTED_NAME_MARK)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    let name = String::from_utf8(bytes).ok()?;
    is_clerk_name(&name).then_some(Consent { name, identity_id })
}

/// The portal identity a card approved by this request belongs to: the
/// browser session's own identity when it is the user's, else (an operator
/// impersonating the user) the identity the Sessions list shows for them.
/// `None` leaves the card on the user's local identity.
///
/// # Arguments
///
/// * `library` - The library store.
/// * `headers` - The consent request's headers (its session cookie).
/// * `user_id` - The approving user.
///
/// # Errors
///
/// [`crate::auth::CallerRefusal::SessionCheckUnavailable`] when the
/// session could not be checked, so the card is not tied to the wrong
/// identity.
async fn approving_identity(
    library: &bookclerk_library::LibraryStore,
    headers: &axum::http::HeaderMap,
    user_id: i64,
) -> Result<Option<i64>, crate::auth::CallerRefusal> {
    if let Some(identity) = crate::auth::optional_portal_identity(library, headers).await? {
        if identity.user_id == Some(user_id) {
            return Ok(Some(identity.id));
        }
    }
    Ok(library
        .first_portal_identity_for_user(user_id)
        .await
        .ok()
        .flatten()
        .map(|identity| identity.id))
}

/// The portal identity a redeemed card's session is stored under: the
/// consented identity while it still belongs to the user, otherwise the
/// user's local identity.
///
/// # Arguments
///
/// * `library` - The library store.
/// * `user` - The user the code was approved for.
/// * `identity_id` - The identity recorded at consent.
///
/// # Errors
///
/// When the library cannot be read.
async fn card_identity(
    library: &bookclerk_library::LibraryStore,
    user: &bookclerk_library::UserRecord,
    identity_id: Option<i64>,
) -> bookclerk_library::Result<bookclerk_library::PortalIdentity> {
    if let Some(id) = identity_id {
        if let Some(identity) = library.get_portal_identity_by_id(id).await? {
            if identity.user_id == Some(user.id) {
                return Ok(identity);
            }
        }
    }
    library
        .ensure_local_portal_identity(user.id, user.display_name.as_deref())
        .await
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /api/auth/clerk-consent`: what the consent page shows.
pub struct ClerkConsentPage {
    /// This library's name.
    #[schema(example = "books.example.com")]
    pub library_name: String,
    /// The name the clerk gives itself.
    #[schema(example = "Riley's laptop")]
    pub clerk_name: String,
    /// Origin of the clerk's redirect URI, for the page to name.
    #[schema(example = "http://127.0.0.1:41923")]
    pub redirect_origin: String,
    /// The signed-in user the clerk would sign in as.
    pub user: ConsentingUser,
}

/// `GET /api/auth/clerk-consent?<authorize params>`: what the consent page
/// shows. 400 `invalid_request` for a malformed request (the page then
/// shows the problem and never redirects).
#[utoipa::path(
    get,
    path = "/api/auth/clerk-consent",
    params(ClerkAuthorizeParams),
    security(("portalSession" = []), ("operatorSession" = [])),
    responses(
        (status = 200, description = "What the consent page shows.", body = ClerkConsentPage),
        (status = 400, description = "The authorize parameters are invalid (`invalid_request`).", body = crate::http_error::ErrorDetail, examples(
            ("unknown_client" = (summary = "The client id is not the clerk's built-in client.", value = json!({"error": "invalid_request", "detail": "unknown client"}))),
            ("code_challenge" = (summary = "No S256 code challenge.", value = json!({"error": "invalid_request", "detail": "an S256 code challenge is required"}))),
            ("redirect_uri" = (summary = "The redirect URI is not loopback or https.", value = json!({"error": "invalid_request", "detail": "the redirect URI must be a loopback or https URL"}))),
            ("clerk_name" = (summary = "The clerk name is empty or too long.", value = json!({"error": "invalid_request", "detail": "the clerk needs a name of 1 to 64 characters"}))),
        )),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is not a library user (`user_required`; an operator session counts only while impersonating), the account is inactive (`user_inactive`), the session has no user account (the standard error body), or the user must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorAnswer, examples(
            ("user_required" = (summary = "The session is not a library user.", value = json!({"error": "user_required", "detail": "a branch belongs to a user: sign in with your account on this library"}))),
            ("user_inactive" = (summary = "The account is not active.", value = json!({"error": "user_inactive", "detail": "this account is not active"}))),
        )),
        (status = 503, description = "The library could not be read; try again (`unavailable`), or this library's identity row or origin is not available yet (`library_unavailable`, `origin_unavailable`).", body = crate::http_error::ErrorAnswer, examples(
            ("unavailable" = (summary = "A read failed; try again.", value = json!({"error": "unavailable", "detail": "try again"}))),
            ("library_unavailable" = (summary = "This library's identity row is not available yet.", value = json!({"error": "library_unavailable"}))),
        )),
    ),
)]
pub async fn get_clerk_consent(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Query(params): Query<ClerkAuthorizeParams>,
) -> Response {
    if let Some(problem) = clerk_request_problem(&params) {
        return consent_error(StatusCode::BAD_REQUEST, "invalid_request", problem);
    }
    let user = match consenting_user(&state, caller).await {
        Ok(user) => user,
        Err(refused) => return refused,
    };
    let info = match owner_info(&state).await {
        Ok(info) => info,
        Err(response) => return response,
    };
    Json(ClerkConsentPage {
        library_name: info.library_name,
        clerk_name: params.clerk_name.unwrap_or_default().trim().to_owned(),
        redirect_origin: canonical_origin(&params.redirect_uri).unwrap_or_default(),
        user: ConsentingUser {
            id: user.id,
            display_name: user.display_name,
        },
    })
    .into_response()
}

/// `POST /api/auth/clerk-consent` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ClerkConsentBody {
    /// The authorization request.
    #[serde(flatten)]
    pub params: ClerkAuthorizeParams,
    /// True to let the clerk sign in; false to deny.
    #[schema(example = true)]
    pub approve: bool,
}

/// `POST /api/auth/clerk-consent`: approve or deny. Answers `{redirect}`,
/// the URL to send the browser to: the clerk's callback with a code (or
/// `error=access_denied`).
#[utoipa::path(
    post,
    path = "/api/auth/clerk-consent",
    request_body = ClerkConsentBody,
    security(("portalSession" = []), ("operatorSession" = [])),
    responses(
        (status = 200, description = "`{redirect}` back to the clerk.", body = ConsentRedirect),
        (status = 400, description = "The body or its authorize parameters are invalid (`invalid_request`).", body = crate::http_error::ErrorDetail, examples(
            ("malformed" = (summary = "The body does not parse.", value = json!({"error": "invalid_request", "detail": "malformed consent"}))),
            ("bad_redirect" = (summary = "The redirect URI cannot carry the answer.", value = json!({"error": "invalid_request", "detail": "bad redirect"}))),
            ("unknown_client" = (summary = "The client id is not the clerk's built-in client.", value = json!({"error": "invalid_request", "detail": "unknown client"}))),
        )),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is not an active library user (`user_required`, `user_inactive`), the connection failed the transport policy (`transport_insecure`), the session has no user account (the standard error body), or the user must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::NodeRefusal, examples(
            ("user_required" = (summary = "The session is not a library user.", value = json!({"error": "user_required", "detail": "a branch belongs to a user: sign in with your account on this library"}))),
            ("user_inactive" = (summary = "The account is not active.", value = json!({"error": "user_inactive", "detail": "this account is not active"}))),
            ("transport_insecure" = (summary = "Plain HTTP from another machine.", value = json!({"error": "transport_insecure", "reason": "plain_http"}))),
        )),
        (status = 404, description = "The node API is not mounted on this listener (`node_api_unmounted`).", body = crate::http_error::ErrorMessage, example = json!({"error": "node_api_unmounted"})),
        (status = 413, description = "The body is larger than 8 KiB.", body = crate::http_error::ErrorMessage, example = json!({"error": "request too large"})),
        (status = 415, description = "The request is not `application/json`.", body = crate::http_error::ErrorMessage, example = json!({"error": "expected application/json"})),
        (status = 503, description = "The library could not be read or the code could not be stored; try again (`unavailable`).", body = crate::http_error::ErrorDetail, example = json!({"error": "unavailable", "detail": "try again"})),
    ),
)]
pub async fn post_clerk_consent(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let body = match super::read_json_body(&parts.headers, body, MAX_CONSENT_BODY_BYTES).await {
        Ok(body) => body,
        Err(refused) => return refused,
    };
    let Ok(body) = serde_json::from_slice::<ClerkConsentBody>(&body) else {
        return consent_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "malformed consent",
        );
    };
    let params = body.params;
    if let Some(problem) = clerk_request_problem(&params) {
        return consent_error(StatusCode::BAD_REQUEST, "invalid_request", problem);
    }
    let user = match consenting_user(&state, caller).await {
        Ok(user) => user,
        Err(refused) => return refused,
    };
    let state_param = params.state.clone().unwrap_or_default();
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if !body.approve {
        pairs.push(("error", "access_denied"));
        if !state_param.is_empty() {
            pairs.push(("state", &state_param));
        }
        return match redirect_with(&params.redirect_uri, &pairs) {
            Some(redirect) => no_store(Json(ConsentRedirect { redirect }).into_response()),
            None => consent_error(StatusCode::BAD_REQUEST, "invalid_request", "bad redirect"),
        };
    }
    // The session token this leads to is used from another machine, so it
    // is issued only under the node API transport policy: TLS except on
    // loopback.
    if let Err(refused) = check_transport(&state, &mut parts, "clerk_consent").await {
        return refused;
    }
    let clerk_name = params.clerk_name.as_deref().unwrap_or("").trim();
    let library = state.library_snapshot().await;
    let identity_id = match approving_identity(&library, &parts.headers, user.id).await {
        Ok(identity_id) => identity_id,
        Err(refused) => return refused.into_response(),
    };
    let code = random_token();
    bookclerk_config::register_secret(code.as_str());
    let expires = chrono::Utc::now() + chrono::Duration::seconds(CLERK_CODE_TTL_SECS);
    if let Err(err) = library
        .insert_oidc_auth_code(
            &hash_token(&code),
            CLERK_CLIENT_ID,
            user.id,
            &params.redirect_uri,
            &params.code_challenge,
            "S256",
            &consented_scope(clerk_name, identity_id),
            expires,
        )
        .await
    {
        tracing::warn!(error = %err, "clerk consent could not store its code");
        return consent_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "try again");
    }
    let detail = json!({
        "client_id": CLERK_CLIENT_ID,
        "clerk_name": clerk_name,
        "redirect_origin": canonical_origin(&params.redirect_uri),
    })
    .to_string();
    let _ = library
        .insert_security_audit_event(
            &format!("user:{}", user.id),
            "clerk_authorize",
            Some(&detail),
        )
        .await;
    pairs.push(("code", &code));
    if !state_param.is_empty() {
        pairs.push(("state", &state_param));
    }
    match redirect_with(&params.redirect_uri, &pairs) {
        Some(redirect) => no_store(Json(ConsentRedirect { redirect }).into_response()),
        None => consent_error(StatusCode::BAD_REQUEST, "invalid_request", "bad redirect"),
    }
}

/// 32 random bytes as base64url without padding (43 characters): a code or
/// a session token.
fn random_token() -> Zeroizing<String> {
    let mut raw = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(raw.as_mut_slice());
    Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_slice()))
}

/// A `POST /api/clerk/v1/token` refusal: `{error}`.
fn token_error(status: StatusCode, error: &'static str) -> Response {
    no_store((status, Json(crate::http_error::ErrorMessage::new(error))).into_response())
}

/// `POST /api/clerk/v1/token`: redeem an approved clerk sign-in for a
/// session token. The code is spent by any attempt, matching or not.
#[utoipa::path(
    post,
    path = "/api/clerk/v1/token",
    request_body = ClerkTokenRequest,
    responses(
        (status = 200, description = "The session token (shown once), this library's id, the signed-in user and how long the session stays signed in unused.", body = ClerkTokenAnswer),
        (status = 400, description = "The body is not a token request (`invalid_request`).", body = crate::http_error::ErrorMessage, example = json!({"error": "invalid_request"})),
        (status = 403, description = "The connection failed the transport policy (`transport_insecure`, with the check in `reason`), or the code does not redeem: blank, unknown, used, expired, another client's, a wrong verifier or redirect URI, or the user is no longer active (`sign_in_refused`).", body = crate::http_error::NodeRefusal, examples(
            ("transport_insecure" = (summary = "Plain HTTP from another machine.", value = json!({"error": "transport_insecure", "reason": "plain_http"}))),
            ("sign_in_refused" = (summary = "The code does not redeem.", value = json!({"error": "sign_in_refused"}))),
        )),
        (status = 404, description = "The node API is not mounted on this listener (`node_api_unmounted`).", body = crate::http_error::ErrorMessage, example = json!({"error": "node_api_unmounted"})),
        (status = 413, description = "The body is larger than 8 KiB.", body = crate::http_error::ErrorMessage, example = json!({"error": "request too large"})),
        (status = 415, description = "The request is not `application/json`.", body = crate::http_error::ErrorMessage, example = json!({"error": "expected application/json"})),
        (status = 503, description = "The sign-in could not be read or recorded; try again (`unavailable`).", body = crate::http_error::ErrorMessage, example = json!({"error": "unavailable"})),
    ),
)]
pub async fn post_clerk_token(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    if let Err(refused) = check_transport(&state, &mut parts, "clerk_token").await {
        return refused;
    }
    let body = match super::read_json_body(&parts.headers, body, MAX_TOKEN_BODY_BYTES).await {
        Ok(body) => body,
        Err(refused) => return refused,
    };
    let Some(body) = bookclerk_node_proto::parse_clerk_token_request(&body) else {
        return token_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let code = body.code.trim();
    let verifier = body.code_verifier.trim();
    if code.is_empty() || verifier.is_empty() {
        return token_error(StatusCode::FORBIDDEN, "sign_in_refused");
    }
    let library = state.library_snapshot().await;
    let consumed = match library.consume_oidc_auth_code(&hash_token(code)).await {
        Ok(consumed) => consumed,
        Err(err) => {
            tracing::warn!(error = %err, "clerk sign-in could not read its code");
            return token_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
        }
    };
    let Some((client_id, user_id, redirect_uri, challenge, method, scope)) = consumed else {
        return token_error(StatusCode::FORBIDDEN, "sign_in_refused");
    };
    let consent = consented(&scope);
    let stored = bookclerk_node_proto::StoredClerkCode {
        client_id: &client_id,
        redirect_uri: &redirect_uri,
        method: &method,
        challenge: &challenge,
    };
    let matches = consent.is_some() && bookclerk_node_proto::clerk_exchange_matches(&stored, &body);
    let Some(Consent { name, identity_id }) = consent.filter(|_| matches) else {
        // Another OAuth client's code is spent too, as any failed exchange is: the
        // branch client's codes are redeemed only at enroll/oauth, and a
        // registered client's only at the token endpoint.
        tracing::warn!(
            user_id,
            "clerk sign-in refused: the code does not match its exchange"
        );
        let _ = library
            .insert_security_audit_event(
                &format!("user:{user_id}"),
                "clerk_sign_in_refused",
                Some(r#"{"reason":"code_mismatch"}"#),
            )
            .await;
        return token_error(StatusCode::FORBIDDEN, "sign_in_refused");
    };
    // Registered only once it has proven itself, so a stranger's guesses
    // never grow the redaction registry.
    bookclerk_config::register_secret(verifier);
    let user = match library.get_user(user_id).await {
        Ok(Some(user)) if user.status == bookclerk_library::UserStatus::Active => user,
        Ok(_) => return token_error(StatusCode::FORBIDDEN, "sign_in_refused"),
        Err(err) => {
            tracing::warn!(error = %err, "clerk sign-in could not read the user");
            return token_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
        }
    };
    let info = match owner_info(&state).await {
        Ok(info) => info,
        Err(response) => return response,
    };
    let identity = match card_identity(&library, &user, identity_id).await {
        Ok(identity) => identity,
        Err(err) => {
            tracing::warn!(error = %err, "clerk sign-in could not read the identity");
            return token_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
        }
    };
    let token = random_token();
    bookclerk_config::register_secret(token.as_str());
    let idle = chrono::Duration::days(CLERK_IDLE_DAYS);
    if let Err(err) = library
        .insert_clerk_session(
            &hash_token(&token),
            identity.id,
            chrono::Utc::now() + idle,
            &name,
        )
        .await
    {
        tracing::warn!(error = %err, "clerk sign-in could not record its session");
        return token_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    }
    let detail = json!({ "clerk_name": name }).to_string();
    let _ = library
        .insert_security_audit_event(&format!("user:{}", user.id), "clerk_sign_in", Some(&detail))
        .await;
    no_store(
        Json(ClerkTokenAnswer {
            token: token.to_string(),
            library_id: info.library_id,
            user: ClerkUser {
                id: user.id,
                display_name: user.display_name,
            },
            idle_secs: idle.num_seconds(),
        })
        .into_response(),
    )
}

#[cfg(test)]
#[path = "clerk_tests.rs"]
mod tests;
