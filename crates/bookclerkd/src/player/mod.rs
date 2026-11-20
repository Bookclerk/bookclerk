//! The player API at `/player` (`docs/adr/player-api.md`): the routes
//! audiobook player apps written for an Audiobookshelf server call, served
//! by the host from its playback core and library store.
//!
//! The wire half (every parser, response shape and the socket framing)
//! lives in `bookclerk_player_api`, sans-IO, so the fuzz targets drive the
//! same code; this module is the I/O half: who is calling, what they may
//! see and play, and the answers.
//!
//! **Who.** An app signs in with a player password
//! ([`sign_in::post_login`]) or through the library's own sign-in page
//! ([`sign_in::get_openid`]) and gets a **player card**: an access token
//! that lives an hour and a refresh token that lives thirty days, each use
//! starting the thirty days again (`bookclerk_library::PlayerCard`). The
//! card shows in the patron's session list and can be revoked there. The
//! account password is never accepted here.
//!
//! **Admission.** Routes past sign-in sit behind [`admit_player_card`]:
//! it drops any cookie and `Authorization` value, reads the card's access
//! token from `Authorization: Bearer` or `?token=` (apps put it in audio
//! URLs), resolves it to an active patron, and hands the handler a
//! [`Patron`] with the patron's scope. A browser session never reaches a
//! `/player` route, and a card never reaches any other route.
//!
//! **What.** A library is the whole library the patron may see; an item
//! is an audio edition the library acquired, listed through the patron's
//! scope exactly as the library's own pages are; streams go through the
//! playback core's one check
//! ([`bookclerk_library::control_plane::plan_playback`]) on every request,
//! with the Download control covering streaming.
//!
//! Errors are short plain-text bodies with the status apps check, as the
//! apps expect.

mod bodies;
pub(crate) mod catalog;
pub(crate) mod listening;
pub(crate) mod sign_in;
pub(crate) mod socket;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bookclerk_library::{hash_token, LibraryStore, UserRecord, UserStatus, WorkScope};
use bookclerk_player_api::request::{parse_bearer, parse_card_token};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use zeroize::Zeroizing;

use crate::api::AppState;
use crate::openapi::{Access, RouteGroup};

/// The route groups of the player API: sign-in and the socket (no card
/// needed before the handler), and everything else behind
/// [`admit_player_card`].
pub(crate) fn route_groups() -> Vec<RouteGroup> {
    let open = OpenApiRouter::new()
        .routes(routes!(sign_in::get_status))
        .routes(routes!(sign_in::get_ping))
        .routes(routes!(sign_in::get_healthcheck))
        .routes(routes!(sign_in::post_login))
        .routes(routes!(sign_in::post_refresh))
        .routes(routes!(sign_in::post_logout))
        .routes(routes!(sign_in::get_openid))
        .routes(routes!(sign_in::get_openid_mobile_redirect))
        .routes(routes!(sign_in::get_openid_callback))
        .routes(routes!(socket::get_socket))
        .routes(routes!(socket::get_socket_without_slash));
    let carded = OpenApiRouter::new()
        .routes(routes!(sign_in::post_authorize))
        .routes(routes!(sign_in::get_me))
        .routes(routes!(catalog::list_libraries))
        .routes(routes!(catalog::get_library))
        .routes(routes!(catalog::list_library_items))
        .routes(routes!(catalog::list_library_series))
        .routes(routes!(catalog::list_library_authors))
        .routes(routes!(catalog::list_library_narrators))
        .routes(routes!(catalog::list_library_collections))
        .routes(routes!(catalog::list_library_playlists))
        .routes(routes!(catalog::get_library_personalized))
        .routes(routes!(catalog::search_library))
        .routes(routes!(catalog::get_library_stats))
        .routes(routes!(catalog::get_library_filterdata))
        .routes(routes!(catalog::get_series))
        .routes(routes!(catalog::get_author))
        .routes(routes!(catalog::get_item))
        .routes(routes!(catalog::post_items_batch_get))
        .routes(routes!(catalog::get_item_cover))
        .routes(routes!(listening::post_play))
        .routes(routes!(listening::get_item_file))
        .routes(routes!(listening::get_item_file_download))
        .routes(routes!(listening::post_session_sync))
        .routes(routes!(listening::post_session_close))
        .routes(routes!(listening::post_local_session))
        .routes(routes!(listening::post_local_sessions))
        .routes(routes!(
            listening::get_progress,
            listening::patch_progress,
            listening::delete_progress
        ))
        .routes(routes!(listening::patch_progress_batch))
        .routes(routes!(listening::get_items_in_progress))
        .routes(routes!(listening::get_listening_sessions))
        .routes(routes!(listening::get_listening_stats))
        .routes(routes!(listening::post_bookmark, listening::patch_bookmark))
        .routes(routes!(listening::delete_bookmark));
    let account = OpenApiRouter::new()
        .routes(routes!(
            sign_in::list_player_passwords,
            sign_in::post_player_password
        ))
        .routes(routes!(sign_in::delete_player_password))
        .routes(routes!(
            sign_in::get_player_consent,
            sign_in::post_player_consent
        ));
    vec![
        RouteGroup::new(open, Access::Public),
        RouteGroup::new(carded, Access::Player),
        RouteGroup::new(account, Access::OperatorOrUser),
    ]
}

/// A plain-text error answer, as apps written for an Audiobookshelf server
/// expect one.
#[derive(Debug)]
pub(crate) struct PlayerError {
    /// The status apps act on.
    status: StatusCode,
    /// A short English reason.
    message: &'static str,
}

impl PlayerError {
    /// An error with `status` and `message`.
    pub(crate) const fn new(status: StatusCode, message: &'static str) -> Self {
        Self { status, message }
    }

    /// 400: the request is malformed.
    pub(crate) const fn bad_request() -> Self {
        Self::new(StatusCode::BAD_REQUEST, "Bad Request")
    }

    /// 401: no live card.
    pub(crate) const fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Unauthorized")
    }

    /// 404: no such thing, or not one the patron may see.
    pub(crate) const fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Not Found")
    }

    /// 500 after logging a failed read or write of the library.
    pub(crate) fn failed(route: &str, err: &bookclerk_library::LibraryError) -> Self {
        tracing::error!(error = %err, route, "player API library access failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
    }

    /// 503: storage or the library cannot be asked right now.
    pub(crate) const fn unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable")
    }

    /// The status.
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for PlayerError {
    fn into_response(self) -> Response {
        text(self.status, self.message)
    }
}

/// A plain-text answer.
pub(crate) fn text(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// `200 OK` as plain text: what apps get for a write with no answer.
pub(crate) fn ok() -> Response {
    text(StatusCode::OK, "OK")
}

/// What the player API's answers are: a value or a [`PlayerError`].
pub(crate) type PlayerResult<T> = Result<T, PlayerError>;

/// A card's access token, held only as long as the request.
#[derive(Clone)]
pub(crate) struct CardToken(Arc<Zeroizing<String>>);

impl CardToken {
    /// The token text.
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for CardToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CardToken(..)")
    }
}

/// The patron a live card signs in as, with what they may see, resolved by
/// [`admit_player_card`] for one request.
#[derive(Clone)]
pub(crate) struct Patron {
    /// The library handle the patron, their scope and the route's reads
    /// all come from, so a reload between them cannot mix two stores.
    pub(crate) library: LibraryStore,
    /// The patron.
    pub(crate) user: UserRecord,
    /// The card's `portal_sessions.id`.
    pub(crate) card_id: i64,
    /// The patron's scope; `None` when it hides nothing.
    pub(crate) scope: Option<WorkScope>,
    /// The access token the request carried.
    pub(crate) token: CardToken,
}

impl std::fmt::Debug for Patron {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Patron")
            .field("user_id", &self.user.id)
            .field("card_id", &self.card_id)
            .finish_non_exhaustive()
    }
}

impl Patron {
    /// The patron's `users.id`.
    pub(crate) fn user_id(&self) -> i64 {
        self.user.id
    }
}

/// The card token a request carries: `Authorization: Bearer <token>`, or
/// the `token` query parameter, when it has the one shape cards have.
///
/// # Arguments
///
/// * `headers` - The request's headers.
/// * `query` - The raw query string, if any.
pub(crate) fn token_from_request(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    let mut bearer = headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .filter_map(|value| parse_bearer(value.as_bytes()));
    if let Some(token) = bearer.next() {
        return Some(token.to_owned());
    }
    let query = query?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "token")
        .and_then(|(_, value)| parse_card_token(&value).map(str::to_owned))
}

/// Why a card did not resolve to a patron.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardRefusal {
    /// No such live card, or its patron is gone or disabled.
    SignedOut,
    /// The patron must enroll a second factor before using the library.
    EnrollFirst,
    /// The library could not be read.
    Unavailable,
}

/// Resolves an access token to its patron and their scope.
///
/// # Arguments
///
/// * `state` - Daemon state.
/// * `token` - The access token.
/// * `path` - The request path, for the second-factor rule's exemptions.
///
/// # Errors
///
/// [`CardRefusal`] naming why not.
pub(crate) async fn resolve_card(
    state: &AppState,
    token: &str,
    path: &str,
) -> Result<Patron, CardRefusal> {
    let library = state.library_snapshot().await;
    let card = match library.player_card_by_access(&hash_token(token)).await {
        Ok(Some(card)) => card,
        Ok(None) => return Err(CardRefusal::SignedOut),
        Err(err) => {
            tracing::warn!(error = %err, "player card lookup failed");
            return Err(CardRefusal::Unavailable);
        }
    };
    let Some(user_id) = card.identity.user_id else {
        return Err(CardRefusal::SignedOut);
    };
    let user = match library.get_user(user_id).await {
        Ok(Some(user)) if user.status == UserStatus::Active => user,
        Ok(_) => return Err(CardRefusal::SignedOut),
        Err(err) => {
            tracing::warn!(error = %err, user_id, "player card user lookup failed");
            return Err(CardRefusal::Unavailable);
        }
    };
    if crate::totp::enrollment_blocks(state, &library, user_id, path).await {
        return Err(CardRefusal::EnrollFirst);
    }
    let scope = bookclerk_library::control_plane::work_scope_for_user(&library, &user)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, user_id, "player card scope lookup failed");
            CardRefusal::Unavailable
        })?;
    Ok(Patron {
        library,
        user,
        card_id: card.session_id,
        scope,
        token: CardToken(Arc::new(Zeroizing::new(token.to_owned()))),
    })
}

/// What a card's sign-in or a carded route answers (403) while the patron
/// must enroll a second factor first.
pub(crate) const ENROLL_FIRST: &str = "Enroll a second factor on the library's website first";

/// Shows the body of every plain-text refusal on the player API's routes in
/// the OpenAPI document, where the route does not show one itself.
///
/// The player API answers refusals as the apps expect: the status's reason
/// phrase as plain text ([`PlayerError`]), except a 403 (a second factor to
/// enroll, [`ENROLL_FIRST`]) and a 409 (the session ended).
///
/// # Arguments
///
/// * `doc` - The document, after the routes are collected.
pub(crate) fn add_error_examples(doc: &mut utoipa::openapi::OpenApi) {
    use utoipa::openapi::RefOr;
    for (path, item) in doc.paths.paths.iter_mut() {
        if !path.starts_with("/player/") {
            continue;
        }
        let operations = [
            item.get.as_mut(),
            item.put.as_mut(),
            item.post.as_mut(),
            item.delete.as_mut(),
            item.patch.as_mut(),
        ];
        for op in operations.into_iter().flatten() {
            for (status, response) in op.responses.responses.iter_mut() {
                let RefOr::T(response) = response else {
                    continue;
                };
                let Some(status) = status
                    .parse::<u16>()
                    .ok()
                    .and_then(|code| StatusCode::from_u16(code).ok())
                    .filter(|code| code.is_client_error() || code.is_server_error())
                else {
                    continue;
                };
                let body = match status {
                    StatusCode::FORBIDDEN => ENROLL_FIRST,
                    StatusCode::CONFLICT => "Session ended",
                    other => other.canonical_reason().unwrap_or("Error"),
                };
                for (media, content) in response.content.iter_mut() {
                    let RefOr::T(content) = content else { continue };
                    if media == "text/plain"
                        && content.example.is_none()
                        && content.examples.is_empty()
                    {
                        content.example = Some(body.into());
                    }
                }
            }
        }
    }
}

/// The plain-text body the card layer answers with a refusal, for the
/// OpenAPI document.
///
/// # Arguments
///
/// * `status` - The refusal's status: `401`, `403` or `503`.
///
/// # Returns
///
/// A `text/plain` content with the body the layer sends for that status.
pub(crate) fn refusal_content(status: &str) -> utoipa::openapi::Content {
    use utoipa::openapi::schema::{ObjectBuilder, Type};
    let body = match status {
        "403" => ENROLL_FIRST,
        "503" => "Service Unavailable",
        _ => "Unauthorized",
    };
    utoipa::openapi::ContentBuilder::new()
        .schema(Some(ObjectBuilder::new().schema_type(Type::String)))
        .example(Some(body.into()))
        .build()
}

/// The answer to a card that did not resolve.
pub(crate) fn card_refusal(refusal: CardRefusal) -> Response {
    match refusal {
        CardRefusal::SignedOut => PlayerError::unauthorized().into_response(),
        CardRefusal::EnrollFirst => text(StatusCode::FORBIDDEN, ENROLL_FIRST),
        CardRefusal::Unavailable => PlayerError::unavailable().into_response(),
    }
}

/// Admits a player card in front of the carded route group: drops every
/// cookie and `Authorization` value, resolves the card's access token
/// ([`token_from_request`]) to a [`Patron`] extension, and answers 401
/// (no live card), 403 (second factor to enroll) or 503 itself otherwise.
///
/// # Arguments
///
/// * `state` - Daemon state.
/// * `req` - The request, routed to a carded route.
/// * `next` - The handler.
pub(crate) async fn admit_player_card(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let token = token_from_request(req.headers(), req.uri().query());
    let headers = req.headers_mut();
    headers.remove(header::COOKIE);
    headers.remove(header::AUTHORIZATION);
    let Some(token) = token.map(Zeroizing::new) else {
        return PlayerError::unauthorized().into_response();
    };
    let path = req.uri().path().to_owned();
    match resolve_card(&state, &token, &path).await {
        Ok(patron) => {
            req.extensions_mut().insert(patron);
            next.run(req).await
        }
        Err(refusal) => card_refusal(refusal),
    }
}

/// The name a patron signs in with, as apps show it: their login name,
/// else their email, else their display name, else `patron-<id>`.
pub(crate) fn username(user: &UserRecord) -> String {
    user.login_name
        .clone()
        .or_else(|| user.email.clone())
        .or_else(|| user.display_name.clone())
        .unwrap_or_else(|| format!("patron-{}", user.id))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
