//! Operator API for library cards: Settings → Library → Library cards and
//! the "Sign in with a library card" wizard.
//!
//! - `GET /api/config/clerk`: the libraries this install holds (or held)
//!   a library card for, and the name it signs in as by default.
//! - `POST /api/config/clerk/inspect` `{address}`: reads the library's
//!   `GET /api/node/v1/info` from this daemon (never from the browser)
//!   under the transport policy; a self-signed library's fingerprint comes
//!   back for the person to confirm.
//! - `POST /api/config/clerk/start` `{address, fingerprint?, clerk_name?}`:
//!   reads the library again, checks the confirmed fingerprint, makes the
//!   PKCE verifier and `state`, keeps them in memory for
//!   [`PENDING_CLERK_TTL`], and answers the authorize URL to send the
//!   browser to. The redirect URI is this install's own `/clerk/callback`.
//!   A library this install is already an active branch or window of is
//!   refused (409 `already_joined`): its books are read through that link.
//! - `POST /api/config/clerk/callback` `{code, state, error?}`: the browser
//!   came back. The daemon matches `state` to its pending sign-in, redeems
//!   the code with the verifier only it holds, seals the card, and starts
//!   reading the person's books at once.
//! - `POST /api/config/clerk/sign-out` `{cluster_id}`: ends the card at the
//!   library (`POST /api/auth/logout` with the card) when it can be
//!   reached, then drops the card and the books read with it, and removes
//!   the library from the list, whatever the library answered.
//!
//! Every route is for the operator or an Owner (the stored role is read on
//! every call; anyone else gets 403). Answers carrying a URL with `state`
//! are `Cache-Control: no-store`; nothing secret is logged.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_library::control_plane::{
    forget_clerk_link, list_clerk_links, load_clerk_card, load_clerk_link, load_cluster_link,
    sign_out_clerk_link, ClerkLink, ClerkLinkState,
};
use bookclerk_library::hash_token;
use bookclerk_node_client::{
    authorize_clerk, inspect_library, redeem_clerk_code, ClerkRedeem, InspectedLibrary, JoinError,
    OwnerEndpoint,
};
use bookclerk_node_proto::{CLERK_AUTH_SCHEME, CLERK_CALLBACK_PATH};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::api::AppState;
use crate::auth::UserCaller;
use crate::cluster_link::join::{
    callback_origin, default_branch_name, join_error, join_failed, library_json,
    InspectedLibraryResponse, InspectedLibraryView,
};
use crate::node_api::branch::{no_store, require_branch_admin};

/// How long a started sign-in waits for the browser to come back.
pub const PENDING_CLERK_TTL: Duration = Duration::from_secs(600);

/// The library's sign-out route, which ends the session of the card it is
/// sent with.
const LOGOUT_PATH: &str = "/api/auth/logout";

/// Most sign-ins pending at once; starting another drops the oldest.
pub(crate) const MAX_PENDING_CLERKS: usize = 16;

/// A sign-in waiting for the browser to come back. Kept in memory only: a
/// restart forgets it, and the person starts again.
struct PendingClerk {
    /// The library's canonical origin.
    origin: String,
    /// The pin confirmed for a self-signed library.
    spki_pin: Option<String>,
    /// The cluster id the library's info reported.
    cluster_id: String,
    /// The library's name.
    library_name: String,
    /// The PKCE verifier.
    verifier: Zeroizing<String>,
    /// The redirect URI the authorize URL named.
    redirect_uri: String,
    /// The name this install signs in as.
    clerk_name: String,
    /// When the sign-in started.
    started: Instant,
}

/// Pending sign-ins by `hash_token` of their `state`; the raw value is not
/// kept.
static PENDING: LazyLock<std::sync::Mutex<HashMap<String, PendingClerk>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Locks the pending sign-ins, dropping expired ones.
fn pending() -> std::sync::MutexGuard<'static, HashMap<String, PendingClerk>> {
    let mut pending = PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.retain(|_, sign_in| sign_in.started.elapsed() < PENDING_CLERK_TTL);
    pending
}

/// Keeps `sign_in` under `state_hash`, dropping the oldest when full.
fn keep_pending(state_hash: String, sign_in: PendingClerk) {
    let mut pending = pending();
    while pending.len() >= MAX_PENDING_CLERKS {
        let oldest = pending
            .iter()
            .min_by_key(|(_, sign_in)| sign_in.started)
            .map(|(hash, _)| hash.clone());
        match oldest {
            Some(hash) => {
                pending.remove(&hash);
            }
            None => break,
        }
    }
    pending.insert(state_hash, sign_in);
}

/// A library this install holds (or held) a library card for, without the
/// card.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ClerkLinkView {
    /// The library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub cluster_id: String,
    /// The library's canonical origin.
    #[schema(example = "https://books.example.com")]
    pub origin: String,
    /// The library's name at sign-in.
    #[schema(example = "books.example.com")]
    pub library_name: String,
    /// Whether the card's connection pins the library's self-signed
    /// certificate.
    #[schema(example = false)]
    pub pinned: bool,
    /// The person's display name on the library, when it has one.
    #[schema(example = "Riley")]
    pub display_name: Option<String>,
    /// The name this install signed in as.
    #[schema(example = "Riley's laptop")]
    pub clerk_name: String,
    /// `active`, or `signed_out` once the library ended the card.
    #[schema(example = "active")]
    pub state: String,
    /// RFC 3339 sign-in time.
    #[schema(example = "2026-11-17T09:02:41Z")]
    pub signed_in_at: String,
}

/// JSON for one clerk link.
pub(crate) fn clerk_link_json(link: &ClerkLink) -> ClerkLinkView {
    ClerkLinkView {
        cluster_id: link.library_cluster_id.clone(),
        origin: link.origin.clone(),
        library_name: link.library_name.clone(),
        pinned: link.spki_pin.is_some(),
        display_name: link.display_name.clone(),
        clerk_name: link.clerk_name.clone(),
        state: match link.state {
            ClerkLinkState::Active => "active".into(),
            ClerkLinkState::SignedOut => "signed_out".into(),
        },
        signed_in_at: link.created_at.clone(),
    }
}

/// JSON body for `GET /api/config/clerk`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ClerkCards {
    /// The libraries this install holds (or held) a card for, oldest
    /// sign-in first.
    pub cards: Vec<ClerkLinkView>,
    /// The name this install signs in as unless another is given.
    #[schema(example = "study-mini")]
    pub default_clerk_name: String,
}

/// `GET /api/config/clerk`.
#[utoipa::path(
    get,
    path = "/api/config/clerk",
    responses(
        (status = 200, description = "The libraries this install holds a library card for.", body = ClerkCards),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The user is not an active Owner, or the session has no user account.", body = crate::http_error::ErrorAnswer, example = json!({"error": "forbidden"})),
        (status = 500, description = "The library database could not be read.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn get_clerk_cards(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    let store = state.library_snapshot().await;
    match list_clerk_links(&store).await {
        Ok(links) => Json(ClerkCards {
            cards: links.iter().map(clerk_link_json).collect(),
            default_clerk_name: default_branch_name(),
        })
        .into_response(),
        Err(err) => crate::config_authority::events_error(err),
    }
}

/// `POST /api/config/clerk/inspect` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClerkInspectBody {
    /// The library's address (`https://books.example.com`, `books.lan:8443`).
    #[schema(example = "https://books.example.com")]
    address: String,
}

/// `POST /api/config/clerk/inspect`.
#[utoipa::path(
    post,
    path = "/api/config/clerk/inspect",
    request_body = ClerkInspectBody,
    responses(
        (status = 200, description = "What the library says about itself.", body = InspectedLibraryResponse),
        (status = 400, description = "The address is not a library (`invalid_address`), or is plain HTTP off this machine (`transport_insecure`).", body = crate::http_error::ErrorDetail, example = json!({"error": "invalid_address", "detail": "not an enrollment link: no host"})),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The user is not an active Owner, or the session has no user account.", body = crate::http_error::ErrorAnswer, example = json!({"error": "forbidden"})),
        (status = 409, description = "The library's address is not the one given (`origin_mismatch`, with the address to use in `origin`).", body = crate::cluster_link::join::JoinConflict, example = json!({"error": "origin_mismatch", "detail": "this library's address is https://books.example.com; join with that address", "origin": "https://books.example.com"})),
        (status = 502, description = "The library could not be reached (`unreachable`), or answered with an error (`library_error`).", body = crate::http_error::ErrorDetail, example = json!({"error": "unreachable", "detail": "owner unreachable: Connection refused (os error 111)"})),
    ),
)]
pub async fn post_clerk_inspect(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<ClerkInspectBody>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    match inspect_library(&body.address).await {
        Ok(library) => Json(InspectedLibraryResponse {
            library: library_json(&library),
        })
        .into_response(),
        Err(err) => join_failed(&err),
    }
}

/// `POST /api/config/clerk/start` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClerkStartBody {
    /// The library's address.
    #[schema(example = "https://books.example.com")]
    address: String,
    /// The fingerprint the person confirmed (self-signed libraries).
    #[serde(default)]
    #[schema(example = "kQ3nUj9Z2vY4xW8pT6rS1aB5cD7eF0gH2iJ4kL6mN8o")]
    fingerprint: Option<String>,
    /// The name to sign in as (default: this machine's name).
    #[serde(default)]
    #[schema(example = "Riley's laptop")]
    clerk_name: Option<String>,
}

/// JSON body for `POST /api/config/clerk/start`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ClerkStarted {
    /// The library's sign-in URL to open in the browser.
    #[schema(
        example = "https://books.example.com/oidc/authorize?client_id=bookclerk-clerk&redirect_uri=…"
    )]
    pub authorize_url: String,
    /// What the library said about itself.
    pub library: InspectedLibraryView,
    /// The name this install signs in as.
    #[schema(example = "Riley's laptop")]
    pub clerk_name: String,
    /// Seconds the pending sign-in stays valid.
    #[schema(example = 600)]
    pub expires_in_secs: u64,
}

/// `POST /api/config/clerk/start`.
#[utoipa::path(
    post,
    path = "/api/config/clerk/start",
    request_body = ClerkStartBody,
    responses(
        (status = 200, description = "The library's sign-in URL to open.", body = ClerkStarted),
        (status = 400, description = "The address is not a library, is plain HTTP off this machine, or the name or this install's address cannot sign in (`invalid_address`, `transport_insecure`).", body = crate::http_error::ErrorDetail, example = json!({"error": "invalid_address", "detail": "this computer's name must be 1 to 64 characters"})),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The user is not an active Owner, or the session has no user account.", body = crate::http_error::ErrorAnswer, example = json!({"error": "forbidden"})),
        (status = 409, description = "The library's fingerprint is not confirmed (`fingerprint_unconfirmed`), this install already joined that library as a branch (`already_joined`), or the library's address is not the one given (`origin_mismatch`).", body = crate::cluster_link::join::JoinConflict, examples(
            ("fingerprint_unconfirmed" = (summary = "A self-signed library's fingerprint was not confirmed.", value = json!({"error": "fingerprint_unconfirmed", "detail": "confirm the library's fingerprint first; it must match the one on its Settings → Branches page"}))),
            ("already_joined" = (summary = "This install is already a branch of that library.", value = json!({"error": "already_joined", "detail": "this install already joined that library; its books are read through that link"}))),
        )),
        (status = 502, description = "The library could not be reached (`unreachable`), or answered with an error (`library_error`).", body = crate::http_error::ErrorDetail, example = json!({"error": "unreachable", "detail": "owner unreachable: Connection refused (os error 111)"})),
        (status = 500, description = "The library database could not be read.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn post_clerk_start(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    headers: HeaderMap,
    Json(body): Json<ClerkStartBody>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    // Read the library again: the browser's copy of the fingerprint is
    // only what the person confirmed, never what is trusted.
    let library = match inspect_library(&body.address).await {
        Ok(library) => library,
        Err(err) => return join_failed(&err),
    };
    if let Some(refused) = refuse_start(&state, &library, body.fingerprint.as_deref()).await {
        return refused;
    }
    let clerk_name = body
        .clerk_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(default_branch_name, str::to_string);
    let redirect_uri = {
        let cfg = state.config.read().await;
        format!(
            "{}{CLERK_CALLBACK_PATH}",
            callback_origin(&cfg, &headers).trim_end_matches('/')
        )
    };
    let authorization = match authorize_clerk(&library, &redirect_uri, &clerk_name) {
        Ok(authorization) => authorization,
        Err(err) => return join_failed(&err),
    };
    keep_pending(
        hash_token(&authorization.state),
        PendingClerk {
            origin: library.origin.clone(),
            spki_pin: library.spki_pin.clone(),
            cluster_id: library.info.cluster_id.clone(),
            library_name: library.info.library_name.clone(),
            verifier: authorization.verifier,
            redirect_uri: authorization.redirect_uri,
            clerk_name: authorization.clerk_name.clone(),
            started: Instant::now(),
        },
    );
    tracing::info!(
        origin = %library.origin,
        cluster_id = %library.info.cluster_id,
        "library card sign-in started"
    );
    no_store(
        Json(ClerkStarted {
            authorize_url: authorization.authorize_url,
            library: library_json(&library),
            clerk_name: authorization.clerk_name,
            expires_in_secs: PENDING_CLERK_TTL.as_secs(),
        })
        .into_response(),
    )
}

/// Why a sign-in at `library` may not start, or `None`.
async fn refuse_start(
    state: &Arc<AppState>,
    library: &InspectedLibrary,
    fingerprint: Option<&str>,
) -> Option<Response> {
    if let Some(pin) = library.spki_pin.as_deref() {
        if fingerprint.map(str::trim) != Some(pin) {
            return Some(join_error(
                StatusCode::CONFLICT,
                "fingerprint_unconfirmed",
                "confirm the library's fingerprint first; it must match the one on its Settings → Branches page",
            ));
        }
    }
    let store = state.library_snapshot().await;
    match load_cluster_link(&store, &library.info.cluster_id).await {
        Ok(Some(link)) if link.is_active() => Some(already_joined()),
        Ok(_) => None,
        Err(err) => Some(crate::config_authority::events_error(err)),
    }
}

/// The answer for a library this install already joined as a branch.
fn already_joined() -> Response {
    join_error(
        StatusCode::CONFLICT,
        "already_joined",
        "this install already joined that library; its books are read through that link",
    )
}

/// `POST /api/config/clerk/callback` body.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClerkCallbackBody {
    /// The code from the library.
    #[serde(default)]
    #[schema(example = "BhD5jGe21A8WEIm7SrdBZ-KXhs5B9HkeUSEc58grX94")]
    code: String,
    /// The `state` it came back with.
    #[serde(default)]
    #[schema(example = "c3RhdGUtOGY2ZTJkMWE")]
    state: String,
    /// The library's `error` (`access_denied`), when it sent one.
    #[serde(default)]
    #[schema(example = "access_denied")]
    error: Option<String>,
}

/// JSON body for `POST /api/config/clerk/callback`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ClerkSignedIn {
    /// The library this install now holds a card for.
    pub card: ClerkLinkView,
}

/// `POST /api/config/clerk/callback`: redeems the library's code with the
/// sign-in's verifier, seals the card and starts reading the person's
/// books. The pending sign-in is ended whatever the outcome; a failed one
/// is started again.
#[utoipa::path(
    post,
    path = "/api/config/clerk/callback",
    request_body = ClerkCallbackBody,
    responses(
        (status = 200, description = "This install holds a library card for the library now.", body = ClerkSignedIn),
        (status = 400, description = "The callback carries no code (`invalid_request`).", body = crate::http_error::ErrorDetail, example = json!({"error": "invalid_request", "detail": "no code"})),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The library's user did not approve (`denied`), the library refused the code (`refused`), or the user is not an active Owner (`forbidden`).", body = crate::http_error::ErrorAnswer, examples(
            ("denied" = (summary = "The library's user did not approve.", value = json!({"error": "denied", "detail": "the library's user did not let this computer show their books"}))),
            ("refused" = (summary = "The library refused the code.", value = json!({"error": "refused", "detail": "the library refused the sign-in: it was already used or took too long; start again"}))),
        )),
        (status = 409, description = "This install joined that library as a branch meanwhile (`already_joined`).", body = crate::http_error::ErrorDetail, example = json!({"error": "already_joined", "detail": "this install already joined that library; its books are read through that link"})),
        (status = 410, description = "The sign-in is unknown, already used, or older than ten minutes; start again (`join_expired`).", body = crate::http_error::ErrorDetail, example = json!({"error": "join_expired", "detail": "this sign-in is unknown, already used, or took longer than 10 minutes; start again"})),
        (status = 502, description = "The library could not be reached (`unreachable`), answered with an error (`library_error`), its answer did not parse (`join_failed`), or the card could not be saved here (`join_unsaved`).", body = crate::http_error::ErrorDetail, example = json!({"error": "unreachable", "detail": "owner unreachable: Connection refused (os error 111)"})),
    ),
)]
pub async fn post_clerk_callback(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<ClerkCallbackBody>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    let code = Zeroizing::new(body.code);
    let Some(sign_in) = pending().remove(&hash_token(body.state.trim())) else {
        return join_error(
            StatusCode::GONE,
            "join_expired",
            "this sign-in is unknown, already used, or took longer than 10 minutes; start again",
        );
    };
    if body.error.is_some() {
        return join_error(
            StatusCode::FORBIDDEN,
            "denied",
            "the library's user did not let this computer show their books",
        );
    }
    if code.trim().is_empty() {
        return join_error(StatusCode::BAD_REQUEST, "invalid_request", "no code");
    }
    let store = state.library_snapshot().await;
    match load_cluster_link(&store, &sign_in.cluster_id).await {
        Ok(Some(link)) if link.is_active() => return already_joined(),
        Ok(_) => {}
        Err(err) => return crate::config_authority::events_error(err),
    }
    let redeemed = redeem_clerk_code(
        &store,
        &ClerkRedeem {
            origin: &sign_in.origin,
            spki_pin: sign_in.spki_pin.as_deref(),
            cluster_id: &sign_in.cluster_id,
            library_name: &sign_in.library_name,
            code: code.trim(),
            verifier: &sign_in.verifier,
            redirect_uri: &sign_in.redirect_uri,
            clerk_name: &sign_in.clerk_name,
        },
    )
    .await;
    match redeemed {
        Ok(link) => {
            super::notify_clerk_links_changed();
            crate::live::poke(bookclerk_library::control_plane::LiveTopic::Clusters);
            Json(ClerkSignedIn {
                card: clerk_link_json(&link),
            })
            .into_response()
        }
        Err(JoinError::Refused) => join_error(
            StatusCode::FORBIDDEN,
            "refused",
            "the library refused the sign-in: it was already used or took too long; start again",
        ),
        Err(err) => join_failed(&err),
    }
}

/// `POST /api/config/clerk/sign-out` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClerkSignOutBody {
    /// The library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    cluster_id: String,
}

/// JSON body for `POST /api/config/clerk/sign-out`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ClerkSignedOut {
    /// The library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub cluster_id: String,
    /// Whether the library ended the card's session too. False when it
    /// could not be reached (or the card had already ended): the session
    /// then stays in the person's session list there until it idles out
    /// or they revoke it.
    #[schema(example = true)]
    pub library_signed_out: bool,
}

/// How long sign-out waits for the library to end the card's session.
const LIBRARY_SIGN_OUT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ends `link`'s card at the library (`POST /api/auth/logout` with the
/// card). Answers whether the library confirmed it; a failure is logged.
async fn end_card_at_library(store: &bookclerk_library::LibraryStore, link: &ClerkLink) -> bool {
    if !link.is_active() {
        return false;
    }
    let card = match load_clerk_card(store, link).await {
        Ok(card) => card,
        Err(err) => {
            tracing::warn!(cluster_id = %link.library_cluster_id, error = %err, "could not read the library card to end it at the library");
            return false;
        }
    };
    bookclerk_config::register_secret(card.expose());
    let endpoint = match OwnerEndpoint::new(&link.origin, link.spki_pin.as_deref()) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            tracing::warn!(cluster_id = %link.library_cluster_id, error = %err, "could not reach the library to end the library card");
            return false;
        }
    };
    let authorization = Zeroizing::new(format!("{CLERK_AUTH_SCHEME} {}", card.expose()));
    let ended = tokio::time::timeout(
        LIBRARY_SIGN_OUT_TIMEOUT,
        endpoint.post_json_authorized(LOGOUT_PATH, &authorization, b"{}"),
    )
    .await;
    match ended {
        Ok(Ok((status, _))) if (200..300).contains(&status) => true,
        Ok(Ok((status, _))) => {
            tracing::warn!(cluster_id = %link.library_cluster_id, status, "the library did not end the library card");
            false
        }
        Ok(Err(err)) => {
            tracing::warn!(cluster_id = %link.library_cluster_id, error = %err, "could not reach the library to end the library card");
            false
        }
        Err(_) => {
            tracing::warn!(cluster_id = %link.library_cluster_id, "the library did not answer in time to end the library card");
            false
        }
    }
}

/// `POST /api/config/clerk/sign-out`: ends a library card at the library
/// when it can be reached, then drops the card, the books read with it,
/// and the library from the list here whatever the library answered.
#[utoipa::path(
    post,
    path = "/api/config/clerk/sign-out",
    request_body = ClerkSignOutBody,
    responses(
        (status = 200, description = "The card and its books are gone here; `library_signed_out` says whether the library ended its session too.", body = ClerkSignedOut),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The user is not an active Owner, or the session has no user account.", body = crate::http_error::ErrorAnswer, example = json!({"error": "forbidden"})),
        (status = 404, description = "This install holds no card for that library (`not_found`).", body = crate::http_error::ErrorDetail, example = json!({"error": "not_found", "detail": "no such library card"})),
        (status = 500, description = "The library database could not be written.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn post_clerk_sign_out(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<ClerkSignOutBody>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    let store = state.library_snapshot().await;
    let link = match load_clerk_link(&store, &body.cluster_id).await {
        Ok(Some(link)) => link,
        Ok(None) => {
            return join_error(StatusCode::NOT_FOUND, "not_found", "no such library card");
        }
        Err(err) => return crate::config_authority::events_error(err),
    };
    let library_signed_out = end_card_at_library(&store, &link).await;
    let signed_out = match sign_out_clerk_link(&store, &body.cluster_id).await {
        Ok(_) => forget_clerk_link(&store, &body.cluster_id).await,
        Err(err) => Err(err),
    };
    super::notify_clerk_links_changed();
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Clusters);
    match signed_out {
        Ok(_) => {
            tracing::info!(cluster_id = %body.cluster_id, library_signed_out, "signed out of the library card");
            Json(ClerkSignedOut {
                cluster_id: body.cluster_id,
                library_signed_out,
            })
            .into_response()
        }
        Err(err) => crate::config_authority::events_error(err),
    }
}
