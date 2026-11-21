//! Signing apps in and out: status, player passwords, the browser
//! sign-in, token refresh, and the patron as apps see them.
//!
//! | Route | What it does |
//! |---|---|
//! | `GET /player/status`, `/player/ping`, `/player/healthcheck` | What an app checks before it signs in |
//! | `POST /player/login` | Signs in with a username and a player password |
//! | `POST /player/auth/refresh` | Trades a refresh token for a new pair |
//! | `POST /player/logout` | Ends the card |
//! | `GET /player/auth/openid` | Starts the browser sign-in: points the app at the consent page |
//! | `GET /player/auth/openid/mobile-redirect` | Sends the browser back to the app with a code |
//! | `GET /player/auth/openid/callback` | Trades the code and PKCE verifier for a card |
//! | `POST /player/api/authorize`, `GET /player/api/me` | The signed-in patron |
//! | `GET`, `POST /api/auth/player-passwords`, `DELETE .../{id}` | A patron's player passwords (Account settings) |
//! | `GET`, `POST /api/auth/player-consent` | The browser sign-in's consent page |

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine as _;
use bookclerk_library::{
    hash_token, LibraryStore, NewPlayerCard, PlayerCardTokens, PlayerPassword, UserRecord,
    UserStatus, MAX_PLAYER_PASSWORD_LABEL_BYTES,
};
use bookclerk_player_api::oauth::{
    app_of_return_address, format_player_password, normalize_player_password,
    parse_mobile_redirect, parse_openid_callback, parse_openid_start, pkce_s256_matches,
    OAuthError, CONSENT_PAGE_PATH, MOBILE_REDIRECT_PATH, PASSWORD_SYMBOLS, PLAYER_CLIENT_ID,
    PLAYER_CODE_TTL_SECS,
};
use bookclerk_player_api::request::{parse_card_token, parse_login_request, MAX_LOGIN_BODY_BYTES};
use bookclerk_player_api::wire::{
    AuthFormData, LoginResponse, Permissions, Ping, ServerSettings, Status, User, AUTH_METHODS,
    STATUS_APP,
};
use bookclerk_player_api::COMPATIBLE_SERVER_VERSION;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroize::Zeroizing;

use super::{ok, Patron, PlayerError, PlayerResult};
use crate::api::AppState;
use crate::auth::UserCaller;
use crate::http_error::ErrorMessage;

/// Most progress rows a patron's sign-in answer carries.
const MAX_USER_PROGRESS: u64 = 500;

/// Most bookmarks a patron's sign-in answer carries.
const MAX_USER_BOOKMARKS: u64 = 1000;

/// The header apps send their refresh token in.
const REFRESH_HEADER: &str = "x-refresh-token";

/// The note under an app's sign-in form.
const SIGN_IN_NOTE: &str =
    "Sign in with a player password from Account settings on the library's website.";

/// The browser sign-in button's label.
const BROWSER_SIGN_IN_LABEL: &str = "Sign in on the library's website";

/// What `/player/status` and the sign-in answers name as the language.
const LANGUAGE: &str = "en-us";

/// 32 random bytes as base64url without padding (43 characters): a card
/// token or a code, the one shape `parse_card_token` reads back.
fn random_token() -> Zeroizing<String> {
    let mut raw = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(raw.as_mut_slice());
    Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_slice()))
}

/// Reads a request body of at most `max` bytes.
///
/// # Errors
///
/// A 413 for a longer body.
pub(crate) async fn read_body(body: Body, max: usize) -> PlayerResult<Bytes> {
    axum::body::to_bytes(body, max)
        .await
        .map_err(|_| PlayerError::new(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large"))
}

/// A response that no cache may keep: sign-in answers carry tokens.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Tells an app what the server is before it offers a sign-in.
///
/// Answers without a card: the version of the API it speaks and the sign-in
/// methods an app may offer (a player password, and the browser sign-in).
#[utoipa::path(
    get,
    path = "/player/status",
    responses(
        (status = 200, description = "What an app checks before it offers a sign-in.", body = Status),
    ),
)]
pub(crate) async fn get_status() -> Json<Status> {
    Json(Status {
        app: STATUS_APP,
        server_version: COMPATIBLE_SERVER_VERSION,
        is_init: true,
        language: LANGUAGE,
        auth_methods: AUTH_METHODS.to_vec(),
        auth_form_data: AuthFormData {
            auth_login_custom_message: Some(SIGN_IN_NOTE.to_owned()),
            auth_openid_button_text: BROWSER_SIGN_IN_LABEL.to_owned(),
            auth_openid_auto_launch: false,
        },
    })
}

/// Answers that the player API is up, without a card.
#[utoipa::path(
    get,
    path = "/player/ping",
    responses((status = 200, description = "The API answers.", body = Ping)),
)]
pub(crate) async fn get_ping() -> Json<Ping> {
    Json(Ping { success: true })
}

/// Answers `OK` in plain text, without a card, for apps that check health.
#[utoipa::path(
    get,
    path = "/player/healthcheck",
    responses((status = 200, description = "`OK`.", body = String, content_type = "text/plain")),
)]
pub(crate) async fn get_healthcheck() -> Response {
    ok()
}

/// The server settings apps read at sign-in.
fn server_settings() -> ServerSettings {
    ServerSettings {
        id: "server-settings",
        version: COMPATIBLE_SERVER_VERSION,
        build_number: 1,
        language: LANGUAGE,
        date_format: "MM/dd/yyyy",
        time_format: "HH:mm",
        sorting_ignore_prefix: false,
        sorting_prefixes: vec!["the", "a"],
        chromecast_enabled: false,
        home_bookshelf_view: 1,
        bookshelf_view: 1,
        auth_active_auth_methods: AUTH_METHODS.to_vec(),
        auth_openid_button_text: BROWSER_SIGN_IN_LABEL.to_owned(),
        auth_openid_auto_launch: false,
        auth_login_custom_message: Some(SIGN_IN_NOTE.to_owned()),
        time_zone: "UTC".to_owned(),
    }
}

/// A card's new tokens, held only until the answer is built.
pub(crate) struct NewTokens {
    /// The access token.
    access: Zeroizing<String>,
    /// The refresh token.
    refresh: Zeroizing<String>,
}

impl NewTokens {
    /// A fresh pair, registered for redaction.
    fn generate() -> Self {
        let tokens = Self {
            access: random_token(),
            refresh: random_token(),
        };
        bookclerk_config::register_secret(tokens.access.as_str());
        bookclerk_config::register_secret(tokens.refresh.as_str());
        tokens
    }
}

/// The patron as apps see them, with their progress and bookmarks.
///
/// # Arguments
///
/// * `library` - The library handle to read from.
/// * `user` - The patron.
/// * `identity_has_sso` - Whether the card came from the browser sign-in.
/// * `token` - What `token` carries: the card's access token.
/// * `tokens` - The card's new pair, on a sign-in or refresh answer.
///
/// # Errors
///
/// A 500 when the library cannot be read.
pub(crate) async fn user_wire(
    library: &LibraryStore,
    user: &UserRecord,
    token: &str,
    tokens: Option<&NewTokens>,
) -> PlayerResult<User> {
    const ROUTE: &str = "player user";
    let (progress, _) = library
        .list_playback_progress(user.id, true, None, MAX_USER_PROGRESS)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    let bookmarks = library
        .list_user_bookmarks(user.id, MAX_USER_BOOKMARKS)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    let download = bookclerk_library::control_plane::user_may(
        library,
        user.id,
        bookclerk_library::control_plane::MemberControl::Download,
    )
    .await
    .unwrap_or(false);
    Ok(User {
        id: user.id.to_string(),
        username: super::username(user),
        email: user.email.clone(),
        kind: "user",
        token: token.to_owned(),
        access_token: tokens.map(|t| t.access.to_string()),
        refresh_token: tokens.map(|t| t.refresh.to_string()),
        is_old_token: false,
        media_progress: progress
            .into_iter()
            .map(|p| super::listening::progress_wire(user.id, p))
            .collect(),
        series_hide_from_continue_listening: Vec::new(),
        bookmarks: bookmarks
            .into_iter()
            .map(super::listening::bookmark_wire)
            .collect(),
        is_active: true,
        is_locked: false,
        last_seen: user.last_seen_at.map(|at| at.timestamp_millis()),
        created_at: user.created_at.timestamp_millis(),
        permissions: Permissions {
            download,
            update: false,
            delete: false,
            upload: false,
            create_ereader: false,
            access_all_libraries: true,
            access_all_tags: true,
            access_explicit_content: true,
            selected_tags_not_accessible: false,
        },
        libraries_accessible: vec![bookclerk_player_api::ids::WHOLE_LIBRARY_ID.to_owned()],
        item_tags_selected: Vec::new(),
        has_openid_link: false,
    })
}

/// The answer to a sign-in, a refresh or `POST /player/api/authorize`.
///
/// # Errors
///
/// A 500 when the library cannot be read.
async fn login_response(
    library: &LibraryStore,
    user: &UserRecord,
    token: &str,
    tokens: Option<&NewTokens>,
) -> PlayerResult<LoginResponse> {
    Ok(LoginResponse {
        user: user_wire(library, user, token, tokens).await?,
        user_default_library_id: bookclerk_player_api::ids::WHOLE_LIBRARY_ID.to_owned(),
        server_settings: server_settings(),
        ereader_devices: Vec::new(),
        source: "bookclerk",
    })
}

/// Records a refused sign-in in the security audit log.
async fn audit_refusal(library: &LibraryStore, actor: &str, reason: &str) {
    let detail = json!({ "reason": reason }).to_string();
    let _ = library
        .insert_security_audit_event(actor, "player_sign_in_refused", Some(&detail))
        .await;
}

/// The patron behind a sign-in, when they may sign in now.
///
/// # Errors
///
/// A 401 for a missing or inactive user, a 403 when a second factor must
/// be enrolled first, a 500 when the library cannot be read.
async fn signing_in_user(
    state: &AppState,
    library: &LibraryStore,
    user_id: i64,
) -> PlayerResult<UserRecord> {
    let user = match library.get_user(user_id).await {
        Ok(Some(user)) if user.status == UserStatus::Active => user,
        Ok(_) => return Err(PlayerError::unauthorized()),
        Err(err) => return Err(PlayerError::failed("player sign-in", &err)),
    };
    if crate::totp::enrollment_blocks(state, library, user.id, "/player/login").await {
        return Err(PlayerError::new(StatusCode::FORBIDDEN, super::ENROLL_FIRST));
    }
    Ok(user)
}

/// Makes a card for `user` and answers the sign-in.
///
/// # Errors
///
/// A 401 when the password the card names was removed meanwhile, a 500
/// when the library cannot be written.
async fn issue_card(
    library: &LibraryStore,
    user: &UserRecord,
    password_id: Option<&str>,
    label: &str,
    headers: &HeaderMap,
) -> PlayerResult<Response> {
    const ROUTE: &str = "player sign-in";
    let identity = library
        .ensure_local_portal_identity(user.id, user.display_name.as_deref())
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    let tokens = NewTokens::generate();
    let session_hash = hash_token(&random_token());
    let access_hash = hash_token(&tokens.access);
    let refresh_hash = hash_token(&tokens.refresh);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let card = library
        .mint_player_card(NewPlayerCard {
            identity_id: identity.id,
            session_hash: &session_hash,
            password_id,
            label,
            user_agent,
            device_type: "mobile",
            tokens: PlayerCardTokens {
                access_hash: &access_hash,
                refresh_hash: &refresh_hash,
            },
        })
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?
        .ok_or_else(PlayerError::unauthorized)?;
    let detail =
        json!({ "card": card, "label": label, "browser": password_id.is_none() }).to_string();
    let _ = library
        .insert_security_audit_event(
            &format!("user:{}", user.id),
            "player_sign_in",
            Some(&detail),
        )
        .await;
    let answer = login_response(library, user, &tokens.access, Some(&tokens)).await?;
    Ok(no_store(Json(answer).into_response()))
}

/// Signs an app in with a username and a player password.
///
/// The username is the patron's sign-in name or email; the password is a
/// player password from Account settings, never the account password.
#[utoipa::path(
    post,
    path = "/player/login",
    request_body(content = super::bodies::LoginBody, description = "The sign-in name (or email) and a player password."),
    responses(
        (status = 200, description = "The patron, the card's tokens and settings.", body = LoginResponse),
        (status = 400, description = "The body is not a JSON object with string `username` and `password`.", body = String, content_type = "text/plain"),
        (status = 401, description = "No such player password, or it belongs to someone else.", body = String, content_type = "text/plain"),
        (status = 403, description = "The patron must enroll a second factor first.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is over 4 KiB.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read or written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, PlayerError> {
    const ROUTE: &str = "POST /player/login";
    let bytes = read_body(body, MAX_LOGIN_BODY_BYTES).await?;
    let request = parse_login_request(&bytes).map_err(|_| PlayerError::bad_request())?;
    let library = state.library_snapshot().await;
    let Some(password) = normalize_player_password(&request.password) else {
        audit_refusal(&library, "player", "not_a_player_password").await;
        return Err(PlayerError::unauthorized());
    };
    let Some(found) = library
        .use_player_password(&hash_token(&password))
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?
    else {
        audit_refusal(&library, "player", "unknown_password").await;
        return Err(PlayerError::unauthorized());
    };
    let user = signing_in_user(&state, &library, found.user_id).await?;
    let named = |value: &Option<String>| {
        value
            .as_deref()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case(&request.username))
    };
    if !named(&user.login_name) && !named(&user.email) {
        audit_refusal(&library, &format!("user:{}", user.id), "username_mismatch").await;
        return Err(PlayerError::unauthorized());
    }
    issue_card(&library, &user, Some(&found.id), &found.label, &headers).await
}

/// Trades a refresh token for a new access and refresh token.
#[utoipa::path(
    post,
    path = "/player/auth/refresh",
    params(("x-refresh-token" = String, Header, description = "The card's refresh token.")),
    responses(
        (status = 200, description = "The patron and the card's new tokens.", body = LoginResponse),
        (status = 401, description = "No such live refresh token.", body = String, content_type = "text/plain"),
        (status = 403, description = "The patron must enroll a second factor first.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read or written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PlayerError> {
    const ROUTE: &str = "POST /player/auth/refresh";
    let presented = headers
        .get(REFRESH_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_card_token)
        .ok_or_else(PlayerError::unauthorized)?;
    let library = state.library_snapshot().await;
    let tokens = NewTokens::generate();
    let access_hash = hash_token(&tokens.access);
    let refresh_hash = hash_token(&tokens.refresh);
    let card = library
        .refresh_player_card(
            &hash_token(presented),
            PlayerCardTokens {
                access_hash: &access_hash,
                refresh_hash: &refresh_hash,
            },
        )
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?
        .ok_or_else(PlayerError::unauthorized)?;
    let user_id = card
        .identity
        .user_id
        .ok_or_else(PlayerError::unauthorized)?;
    let user = signing_in_user(&state, &library, user_id).await?;
    let answer = login_response(&library, &user, &tokens.access, Some(&tokens)).await?;
    Ok(no_store(Json(answer).into_response()))
}

/// Ends the card a refresh token (or the bearer access token) belongs to.
#[utoipa::path(
    post,
    path = "/player/logout",
    params(("x-refresh-token" = Option<String>, Header, description = "The card's refresh token; without it, the bearer access token names the card.")),
    responses(
        (status = 200, description = "`OK`, whether or not a card was live.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PlayerError> {
    const ROUTE: &str = "POST /player/logout";
    let token = headers
        .get(REFRESH_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_card_token)
        .map(str::to_owned)
        .or_else(|| super::token_from_request(&headers, None));
    if let Some(token) = token {
        let library = state.library_snapshot().await;
        library
            .end_player_card(&hash_token(&token))
            .await
            .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    }
    Ok(ok())
}

/// The signed-in patron, as at sign-in, without new tokens.
#[utoipa::path(
    post,
    path = "/player/api/authorize",
    responses(
        (status = 200, description = "The patron and settings; `user.token` is the card's access token.", body = LoginResponse),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_authorize(
    Extension(patron): Extension<Patron>,
) -> Result<Json<LoginResponse>, PlayerError> {
    Ok(Json(
        login_response(&patron.library, &patron.user, patron.token.as_str(), None).await?,
    ))
}

/// The signed-in patron.
#[utoipa::path(
    get,
    path = "/player/api/me",
    responses(
        (status = 200, description = "The patron, their progress and bookmarks.", body = User),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_me(
    Extension(patron): Extension<Patron>,
) -> Result<Json<User>, PlayerError> {
    Ok(Json(
        user_wire(&patron.library, &patron.user, patron.token.as_str(), None).await?,
    ))
}

/// The answer to a refused browser sign-in start: apps look for
/// `Invalid redirect_uri` when the app is not one the library returns to.
fn oauth_refusal(err: OAuthError) -> PlayerError {
    match err {
        OAuthError::UnknownApp => PlayerError::new(StatusCode::BAD_REQUEST, "Invalid redirect_uri"),
        OAuthError::TooLong | OAuthError::Invalid(_) => PlayerError::bad_request(),
    }
}

/// A 302 to `location` that no cache keeps.
fn found(location: &str) -> PlayerResult<Response> {
    let value = HeaderValue::from_str(location).map_err(|_| PlayerError::bad_request())?;
    Ok(no_store(
        (StatusCode::FOUND, [(header::LOCATION, value)]).into_response(),
    ))
}

/// Starts the browser sign-in: answers a 302 to the library's consent
/// page, which the app opens in a browser.
#[utoipa::path(
    get,
    path = "/player/auth/openid",
    params(
        ("code_challenge" = String, Query, description = "PKCE S256 challenge."),
        ("code_challenge_method" = String, Query, description = "`S256`."),
        ("redirect_uri" = String, Query, description = "The app's address: `audiobookshelf://oauth`, `absorb://oauth` or `lissen://oauth`."),
        ("client_id" = Option<String>, Query, description = "The app's own name; echoed."),
        ("response_type" = Option<String>, Query, description = "`code`."),
        ("state" = Option<String>, Query, description = "Handed back to the app with the code."),
    ),
    responses(
        (status = 302, description = "`Location` is the consent page, with the parameters the app passes on."),
        (status = 400, description = "A parameter is missing or malformed, or the app is not one the library returns to (`Invalid redirect_uri`).", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_openid(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> PlayerResult<Response> {
    let start = parse_openid_start(uri.query().unwrap_or_default()).map_err(oauth_refusal)?;
    let origin = {
        let cfg = state.config.read().await;
        crate::origin::effective_origin_from_config(&cfg, Some(&headers))
    };
    let origin = origin.trim_end_matches('/');
    let app_state = start
        .state
        .clone()
        .unwrap_or_else(|| random_token().to_string());
    let back = format!(
        "{origin}{MOBILE_REDIRECT_PATH}?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("app", &start.redirect_uri)
            .finish()
    );
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", &start.client_id)
        .append_pair("scope", "openid")
        .append_pair("state", &app_state)
        .append_pair("redirect_uri", &back)
        .append_pair("code_challenge", &start.code_challenge)
        .append_pair("code_challenge_method", "S256")
        .finish();
    found(&format!("{origin}{CONSENT_PAGE_PATH}?{query}"))
}

/// Sends the browser back to the app with the code (or the refusal).
#[utoipa::path(
    get,
    path = "/player/auth/openid/mobile-redirect",
    params(
        ("app" = String, Query, description = "The app's address, one the library returns to."),
        ("code" = Option<String>, Query, description = "The code; absent when the patron declined."),
        ("state" = Option<String>, Query, description = "The app's state."),
    ),
    responses(
        (status = 302, description = "`Location` is the app's address with `code` and `state` (or `error=access_denied`)."),
        (status = 400, description = "The app is not one the library returns to, or a parameter is malformed.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_openid_mobile_redirect(uri: Uri) -> PlayerResult<Response> {
    let back = parse_mobile_redirect(uri.query().unwrap_or_default()).map_err(oauth_refusal)?;
    let mut url = url::Url::parse(&back.app_redirect).map_err(|_| PlayerError::bad_request())?;
    {
        let mut query = url.query_pairs_mut();
        match &back.code {
            Some(code) => {
                query.append_pair("code", code);
            }
            None => {
                query.append_pair("error", "access_denied");
            }
        }
        if let Some(state) = &back.state {
            query.append_pair("state", state);
        }
    }
    found(url.as_str())
}

/// Trades a browser sign-in's code and PKCE verifier for a card.
#[utoipa::path(
    get,
    path = "/player/auth/openid/callback",
    params(
        ("code" = String, Query, description = "The code the app was handed."),
        ("code_verifier" = String, Query, description = "The PKCE verifier."),
        ("state" = Option<String>, Query, description = "The app's state; not read."),
    ),
    responses(
        (status = 200, description = "The patron, the card's tokens and settings.", body = LoginResponse),
        (status = 400, description = "The code or verifier is missing or malformed.", body = String, content_type = "text/plain"),
        (status = 401, description = "The code is unknown, spent, expired, or does not match the verifier.", body = String, content_type = "text/plain"),
        (status = 403, description = "The patron must enroll a second factor first.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read or written.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_openid_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> PlayerResult<Response> {
    const ROUTE: &str = "GET /player/auth/openid/callback";
    let exchange = parse_openid_callback(uri.query().unwrap_or_default()).map_err(oauth_refusal)?;
    let library = state.library_snapshot().await;
    let Some((client_id, user_id, redirect_uri, challenge, method, _scope)) = library
        .consume_oidc_auth_code(&hash_token(&exchange.code))
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?
    else {
        audit_refusal(&library, "player", "unknown_code").await;
        return Err(PlayerError::unauthorized());
    };
    let app = bookclerk_player_api::oauth::app_for_redirect(&redirect_uri);
    let matches = client_id == PLAYER_CLIENT_ID
        && method == "S256"
        && app.is_some()
        && pkce_s256_matches(&exchange.code_verifier, &challenge);
    let Some(app) = app.filter(|_| matches) else {
        // Any other client's code is spent too, as every failed exchange is.
        audit_refusal(&library, &format!("user:{user_id}"), "code_mismatch").await;
        return Err(PlayerError::unauthorized());
    };
    bookclerk_config::register_secret(&exchange.code_verifier);
    let user = signing_in_user(&state, &library, user_id).await?;
    let label = app.strip_prefix("the ").unwrap_or(app);
    issue_card(&library, &user, None, label, &headers).await
}

/// A player password as Account settings lists it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PlayerPasswordJson {
    /// Its id, for removing it.
    #[schema(example = "8f14e45f-ceea-467f-9a1b-2c3d4e5f6a7b")]
    id: String,
    /// The name the patron gave it.
    #[schema(example = "Phone")]
    label: String,
    /// When it was made (RFC 3339).
    #[schema(example = "2026-11-20T09:00:00Z")]
    created_at: String,
    /// When it last signed an app in (RFC 3339), if ever.
    #[schema(example = "2026-11-20T09:05:00Z")]
    last_used_at: Option<String>,
}

impl From<PlayerPassword> for PlayerPasswordJson {
    fn from(value: PlayerPassword) -> Self {
        Self {
            id: value.id,
            label: value.label,
            created_at: value.created_at.to_rfc3339(),
            last_used_at: value.last_used_at.map(|at| at.to_rfc3339()),
        }
    }
}

/// `GET /api/auth/player-passwords` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PlayerPasswordsJson {
    /// The patron's player passwords, newest first.
    passwords: Vec<PlayerPasswordJson>,
}

/// `POST /api/auth/player-passwords` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct NewPlayerPasswordBody {
    /// A name for it, such as the app or device (1 to 64 bytes once
    /// trimmed).
    #[schema(example = "Phone")]
    label: String,
}

/// `POST /api/auth/player-passwords` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct NewPlayerPasswordJson {
    /// The stored password.
    password: PlayerPasswordJson,
    /// The password itself, shown this once: the library keeps only its
    /// digest.
    #[schema(example = "7KQ2M-XR4TD-9HWPA-3NEFB")]
    secret: String,
}

/// The signed-in user of an Account settings route; the operator has no
/// player passwords.
fn account_user(caller: &UserCaller) -> Result<i64, Response> {
    match caller {
        UserCaller::User { user_id } => Ok(*user_id),
        UserCaller::Operator => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorMessage::new(
                "player passwords belong to a library user",
            )),
        )
            .into_response()),
    }
}

/// An Account settings route's 500.
fn account_failed(route: &str, err: &bookclerk_library::LibraryError) -> Response {
    tracing::error!(error = %err, route, "player password write failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorMessage::new("the library could not be read")),
    )
        .into_response()
}

/// Lists the caller's player passwords.
#[utoipa::path(
    get,
    path = "/api/auth/player-passwords",
    responses(
        (status = 200, description = "The caller's player passwords, newest first.", body = PlayerPasswordsJson),
        (status = 403, description = "The caller is the operator, who has none.", body = ErrorMessage, example = json!({"error": "player passwords belong to a library user"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn list_player_passwords(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
) -> Response {
    const ROUTE: &str = "GET /api/auth/player-passwords";
    let user_id = match account_user(&caller) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let library = state.library_snapshot().await;
    match library.list_player_passwords(user_id).await {
        Ok(passwords) => Json(PlayerPasswordsJson {
            passwords: passwords.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(err) => account_failed(ROUTE, &err),
    }
}

/// Makes a player password for the caller and shows it once.
#[utoipa::path(
    post,
    path = "/api/auth/player-passwords",
    request_body = NewPlayerPasswordBody,
    responses(
        (status = 201, description = "The new password; `secret` is shown this once.", body = NewPlayerPasswordJson),
        (status = 400, description = "The label is empty or over 64 bytes.", body = ErrorMessage, example = json!({"error": "label must be 1 to 64 bytes"})),
        (status = 403, description = "The caller is the operator, who has none.", body = ErrorMessage, example = json!({"error": "player passwords belong to a library user"})),
        (status = 409, description = "The caller already keeps twenty; remove one first.", body = ErrorMessage, example = json!({"error": "too_many_player_passwords"})),
        (status = 500, description = "The library could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn post_player_password(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<NewPlayerPasswordBody>,
) -> Response {
    const ROUTE: &str = "POST /api/auth/player-passwords";
    let user_id = match account_user(&caller) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let label = body.label.trim();
    if label.is_empty()
        || label.len() > MAX_PLAYER_PASSWORD_LABEL_BYTES
        || label.chars().any(char::is_control)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorMessage::new("label must be 1 to 64 bytes")),
        )
            .into_response();
    }
    let mut random = Zeroizing::new([0u8; PASSWORD_SYMBOLS]);
    rand::rngs::OsRng.fill_bytes(random.as_mut_slice());
    let secret = Zeroizing::new(format_player_password(&random));
    let Some(normalized) = normalize_player_password(&secret).map(Zeroizing::new) else {
        // A generated password always reads back.
        return account_failed(
            ROUTE,
            &bookclerk_library::LibraryError::Invalid("password".into()),
        );
    };
    let library = state.library_snapshot().await;
    match library
        .insert_player_password(user_id, label, &hash_token(&normalized))
        .await
    {
        Ok(Some(password)) => {
            let detail = json!({ "password_id": password.id }).to_string();
            let _ = library
                .insert_security_audit_event(
                    &format!("user:{user_id}"),
                    "player_password_created",
                    Some(&detail),
                )
                .await;
            no_store(
                (
                    StatusCode::CREATED,
                    Json(NewPlayerPasswordJson {
                        password: password.into(),
                        secret: secret.to_string(),
                    }),
                )
                    .into_response(),
            )
        }
        Ok(None) => (
            StatusCode::CONFLICT,
            Json(ErrorMessage::new("too_many_player_passwords")),
        )
            .into_response(),
        Err(err) => account_failed(ROUTE, &err),
    }
}

/// Removes one of the caller's player passwords and signs out every app
/// it signed in.
#[utoipa::path(
    delete,
    path = "/api/auth/player-passwords/{id}",
    params(("id" = String, Path, description = "The password's id.")),
    responses(
        (status = 204, description = "Removed; its apps are signed out."),
        (status = 403, description = "The caller is the operator, who has none.", body = ErrorMessage, example = json!({"error": "player passwords belong to a library user"})),
        (status = 404, description = "No such password of the caller's.", body = ErrorMessage, example = json!({"error": "no such player password"})),
        (status = 500, description = "The library could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn delete_player_password(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    const ROUTE: &str = "DELETE /api/auth/player-passwords/{id}";
    let user_id = match account_user(&caller) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let library = state.library_snapshot().await;
    match library.delete_player_password(user_id, &id).await {
        Ok(true) => {
            let detail = json!({ "password_id": id }).to_string();
            let _ = library
                .insert_security_audit_event(
                    &format!("user:{user_id}"),
                    "player_password_removed",
                    Some(&detail),
                )
                .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new("no such player password")),
        )
            .into_response(),
        Err(err) => account_failed(ROUTE, &err),
    }
}

/// The consent page's parameters, as the app passed them on.
#[derive(Debug, Clone, Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
pub(crate) struct PlayerConsentParams {
    /// The return address from `/player/auth/openid`: the library's
    /// mobile-redirect path with the app's address as `app`.
    #[schema(example = "/player/auth/openid/mobile-redirect?app=absorb%3A%2F%2Foauth")]
    redirect_uri: String,
    /// The PKCE S256 challenge.
    #[schema(example = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")]
    code_challenge: String,
    /// The app's state.
    #[serde(default)]
    #[schema(example = "xyz")]
    state: Option<String>,
}

/// What a consent request is for, once checked.
struct ConsentFor {
    /// The app's address.
    app_redirect: &'static str,
    /// The app's name.
    app_name: &'static str,
}

/// Checks a consent request.
fn consent_for(params: &PlayerConsentParams) -> Result<ConsentFor, Response> {
    let refused = || {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorMessage::new("invalid_request")),
        )
            .into_response()
    };
    if !bookclerk_player_api::oauth::is_s256_challenge(&params.code_challenge)
        || params
            .state
            .as_deref()
            .is_some_and(|s| s.len() > bookclerk_player_api::oauth::MAX_STATE_BYTES)
    {
        return Err(refused());
    }
    let (app_redirect, app_name) =
        app_of_return_address(&params.redirect_uri).ok_or_else(refused)?;
    Ok(ConsentFor {
        app_redirect,
        app_name,
    })
}

/// `GET /api/auth/player-consent` answer: what the consent page shows.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PlayerConsentPage {
    /// The app asking, as the page names it.
    #[schema(example = "Absorb")]
    app_name: String,
    /// The signed-in patron's display name.
    #[schema(example = "Riley")]
    display_name: Option<String>,
}

/// What the consent page shows.
#[utoipa::path(
    get,
    path = "/api/auth/player-consent",
    params(PlayerConsentParams),
    responses(
        (status = 200, description = "The app asking and the signed-in patron.", body = PlayerConsentPage),
        (status = 400, description = "The parameters are malformed or name an app the library does not return to (`invalid_request`).", body = ErrorMessage, example = json!({"error": "invalid_request"})),
        (status = 403, description = "The caller is the operator.", body = ErrorMessage, example = json!({"error": "player passwords belong to a library user"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn get_player_consent(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    axum::extract::Query(params): axum::extract::Query<PlayerConsentParams>,
) -> Response {
    const ROUTE: &str = "GET /api/auth/player-consent";
    let consent = match consent_for(&params) {
        Ok(consent) => consent,
        Err(refused) => return refused,
    };
    let user_id = match account_user(&caller) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let library = state.library_snapshot().await;
    match library.get_user(user_id).await {
        Ok(user) => Json(PlayerConsentPage {
            app_name: consent.app_name.to_owned(),
            display_name: user.and_then(|u| u.display_name),
        })
        .into_response(),
        Err(err) => account_failed(ROUTE, &err),
    }
}

/// `POST /api/auth/player-consent` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PlayerConsentBody {
    /// The consent page's parameters.
    #[serde(flatten)]
    params: PlayerConsentParams,
    /// True to let the app sign in; false to decline.
    #[schema(example = true)]
    approve: bool,
}

/// `POST /api/auth/player-consent` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PlayerConsentRedirect {
    /// Where to send the browser: the library's own way back to the app,
    /// with a code or the refusal.
    #[schema(
        example = "/player/auth/openid/mobile-redirect?app=absorb%3A%2F%2Foauth&code=...&state=..."
    )]
    redirect: String,
}

/// Approves or declines an app's browser sign-in.
#[utoipa::path(
    post,
    path = "/api/auth/player-consent",
    request_body = PlayerConsentBody,
    responses(
        (status = 200, description = "Where to send the browser next.", body = PlayerConsentRedirect),
        (status = 400, description = "The parameters are malformed or name an app the library does not return to (`invalid_request`).", body = ErrorMessage, example = json!({"error": "invalid_request"})),
        (status = 403, description = "The caller is the operator, or not an active user.", body = ErrorMessage, example = json!({"error": "player passwords belong to a library user"})),
        (status = 500, description = "The code could not be stored.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn post_player_consent(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<PlayerConsentBody>,
) -> Response {
    const ROUTE: &str = "POST /api/auth/player-consent";
    let consent = match consent_for(&body.params) {
        Ok(consent) => consent,
        Err(refused) => return refused,
    };
    let user_id = match account_user(&caller) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    let library = state.library_snapshot().await;
    match library.get_user(user_id).await {
        Ok(Some(user)) if user.status == UserStatus::Active => {}
        Ok(_) => {
            return (
                StatusCode::FORBIDDEN,
                Json(ErrorMessage::new("this account is not active")),
            )
                .into_response()
        }
        Err(err) => return account_failed(ROUTE, &err),
    }
    // The serializer is not `Send`: it is built after the last await.
    let mut issued: Option<Zeroizing<String>> = None;
    if body.approve {
        let code = random_token();
        bookclerk_config::register_secret(code.as_str());
        let expires = chrono::Utc::now() + chrono::Duration::seconds(PLAYER_CODE_TTL_SECS);
        if let Err(err) = library
            .insert_oidc_auth_code(
                &hash_token(&code),
                PLAYER_CLIENT_ID,
                user_id,
                consent.app_redirect,
                &body.params.code_challenge,
                "S256",
                "player",
                expires,
            )
            .await
        {
            return account_failed(ROUTE, &err);
        }
        let detail = json!({ "app": consent.app_name }).to_string();
        let _ = library
            .insert_security_audit_event(
                &format!("user:{user_id}"),
                "player_authorize",
                Some(&detail),
            )
            .await;
        issued = Some(code);
    }
    let mut back = url::form_urlencoded::Serializer::new(String::new());
    back.append_pair("app", consent.app_redirect);
    if let Some(code) = issued.as_deref() {
        back.append_pair("code", code);
    }
    if let Some(state) = body.params.state.as_deref().filter(|s| !s.is_empty()) {
        back.append_pair("state", state);
    }
    no_store(
        Json(PlayerConsentRedirect {
            redirect: format!("{MOBILE_REDIRECT_PATH}?{}", back.finish()),
        })
        .into_response(),
    )
}
