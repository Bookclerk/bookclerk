//! Operator API for joining a library from the web UI, and for leaving one
//! (the welcome screen, the Join wizard and its "I have an invite" tab, and
//! Settings → Library → Leave library).
//!
//! - `GET /api/config/join`: whether this is a fresh install (no Owner,
//!   no Administrator, no cluster link), and the links it has.
//! - `POST /api/config/join/inspect` `{address}`: reads the library's
//!   `GET /api/node/v1/info` from this daemon (never from the browser)
//!   under the transport policy; a self-signed library's fingerprint comes
//!   back for the operator to confirm.
//! - `POST /api/config/join/start` `{address, fingerprint?, branch_name?}`:
//!   reads the library again, checks the confirmed fingerprint, makes the
//!   PKCE verifier, `state` and node keys, keeps them sealed in a pending
//!   join (10 minutes), and answers the authorize URL to send the browser
//!   to. The redirect URI is this install's own `/join/callback`.
//! - `POST /api/config/join/callback` `{code, state}`: the browser came
//!   back. The daemon matches `state` to its pending join, redeems the
//!   code with the verifier only it holds, records the link, and starts
//!   its client at once.
//! - `POST /api/config/join/invite` `{link}`: joins with a pasted branch
//!   invite; the link is used once and not kept.
//! - `POST /api/config/join/leave` `{cluster_id}`: Leave library.
//! - `POST /api/config/join/sharing` `{cluster_id, share_books,
//!   include_branches}`: Settings → Library's "Share this branch's books
//!   with *library*" and "Include books from my own branches"; the
//!   catalog sync reports them to the library at once.
//!
//! Inspect and start refuse a library whose ancestry names this library's
//! own cluster id (409 `join_cycle`), so a tree of libraries never loops;
//! the redeem checks it again before the code is spent.
//!
//! All of them are operator-only. Answers carrying a URL with `state` are
//! `Cache-Control: no-store`; nothing secret is logged.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    create_pending_join, list_cluster_links, take_pending_join, NewPendingJoin,
};
use bookclerk_node_client::{
    authorize_branch, inspect_library, join_cluster, redeem_branch_code, refuse_cycle,
    BranchRedeem, InspectedLibrary, JoinError,
};
use bookclerk_node_proto::{is_branch_name, JOIN_CALLBACK_PATH};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

use super::api::link_json;
use crate::api::AppState;
use crate::node_api::branch::no_store;

/// A JSON error answer.
fn join_error(status: StatusCode, error: &str, detail: &str) -> Response {
    (status, Json(json!({ "error": error, "detail": detail }))).into_response()
}

/// The answer for a failed join step. Never echoes a link or code.
fn join_failed(err: &JoinError) -> Response {
    match err {
        JoinError::Link(detail) => join_error(StatusCode::BAD_REQUEST, "invalid_address", detail),
        JoinError::OriginMismatch { origin } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "origin_mismatch",
                "detail": err.to_string(),
                "origin": origin,
            })),
        )
            .into_response(),
        JoinError::Dial(bookclerk_node_client::DialError::Insecure(reason)) => join_error(
            StatusCode::BAD_REQUEST,
            "transport_insecure",
            &format!("plain HTTP is allowed only on this machine ({reason})"),
        ),
        JoinError::Dial(dial) => join_error(StatusCode::BAD_GATEWAY, "unreachable", &dial.to_string()),
        JoinError::Refused => join_error(
            StatusCode::FORBIDDEN,
            "refused",
            "the library refused: the sign-in or invite is used, expired, revoked, or the library isn't accepting new branches",
        ),
        JoinError::Http { status, error } => join_error(
            StatusCode::BAD_GATEWAY,
            "library_error",
            &format!("the library answered HTTP {status} {error}"),
        ),
        JoinError::Record(detail) => {
            join_error(StatusCode::BAD_GATEWAY, "join_failed", detail)
        }
        JoinError::Cycle => join_error(
            StatusCode::CONFLICT,
            "join_cycle",
            "that library is this library or one of its own branches; joining it would make a loop",
        ),
    }
}

/// JSON for what a library said about itself.
fn library_json(library: &InspectedLibrary) -> serde_json::Value {
    json!({
        "cluster_id": library.info.cluster_id,
        "library_name": library.info.library_name,
        "origin": library.origin,
        "transport": library.info.transport,
        "enrollment": library.info.enrollment,
        "fingerprint": library.spki_pin,
        "needs_fingerprint": library.spki_pin.is_some(),
    })
}

/// This machine's name, as a default branch name.
fn default_branch_name() -> String {
    let from_env = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok();
    let from_file = || std::fs::read_to_string("/etc/hostname").ok();
    from_env
        .or_else(from_file)
        .map(|name| name.trim().chars().take(64).collect::<String>())
        .filter(|name| is_branch_name(name))
        .unwrap_or_else(|| "branch".into())
}

/// `GET /api/config/join`.
pub async fn get_join_status(State(state): State<Arc<AppState>>) -> Response {
    let library = state.library_snapshot().await;
    let links = match list_cluster_links(&library).await {
        Ok(links) => links,
        Err(err) => return crate::config_authority::events_error(err),
    };
    let owners = library.count_owners().await.unwrap_or(1);
    let admins = library.count_administrators().await.unwrap_or(1);
    Json(json!({
        "fresh": owners == 0 && admins == 0 && links.is_empty(),
        "has_owner": owners > 0 || admins > 0,
        "default_branch_name": default_branch_name(),
        "links": links
            .iter()
            .map(|link| {
                let mut value = link_json(link);
                value["running"] = json!(super::is_link_running(&link.owner_cluster_id));
                value
            })
            .collect::<Vec<_>>(),
    }))
    .into_response()
}

/// `POST /api/config/join/inspect` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectBody {
    /// The library's address (`https://owner.example`, `owner.lan:8443`).
    address: String,
}

/// `POST /api/config/join/inspect`.
pub async fn post_join_inspect(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InspectBody>,
) -> Response {
    let library = match inspect_library(&body.address).await {
        Ok(library) => library,
        Err(err) => return join_failed(&err),
    };
    let store = state.library_snapshot().await;
    if let Err(err) = refuse_cycle(&store, &library.info.ancestry).await {
        return join_failed(&err);
    }
    Json(json!({ "library": library_json(&library) })).into_response()
}

/// `POST /api/config/join/start` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartBody {
    /// The library's address.
    address: String,
    /// The fingerprint the operator confirmed (self-signed libraries).
    #[serde(default)]
    fingerprint: Option<String>,
    /// The name to join as (default: this machine's name).
    #[serde(default)]
    branch_name: Option<String>,
}

/// `POST /api/config/join/start`.
pub async fn post_join_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<StartBody>,
) -> Response {
    // Read the library again: the browser's copy of the fingerprint is
    // only what the operator confirmed, never what is trusted.
    let library = match inspect_library(&body.address).await {
        Ok(library) => library,
        Err(err) => return join_failed(&err),
    };
    if let Err(err) = refuse_cycle(&state.library_snapshot().await, &library.info.ancestry).await {
        return join_failed(&err);
    }
    if !library.info.enrollment {
        return join_error(
            StatusCode::FORBIDDEN,
            "enrollment_disabled",
            "This library isn't accepting new branches",
        );
    }
    if let Some(pin) = library.spki_pin.as_deref() {
        let confirmed = body.fingerprint.as_deref().map(str::trim);
        if confirmed != Some(pin) {
            return join_error(
                StatusCode::CONFLICT,
                "fingerprint_unconfirmed",
                "confirm the library's fingerprint first; it must match the one on its Settings → Branches page",
            );
        }
    }
    let branch_name = body
        .branch_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(default_branch_name, str::to_string);
    let redirect_uri = {
        let cfg = state.config.read().await;
        format!(
            "{}{JOIN_CALLBACK_PATH}",
            crate::origin::effective_origin_from_config(&cfg, Some(&headers)).trim_end_matches('/')
        )
    };
    let authorization = match authorize_branch(&library, &redirect_uri, &branch_name) {
        Ok(authorization) => authorization,
        Err(err) => return join_failed(&err),
    };
    let store = state.library_snapshot().await;
    let pending = NewPendingJoin {
        owner_origin: library.origin.clone(),
        spki_pin: library.spki_pin.clone(),
        owner_cluster_id: library.info.cluster_id.clone(),
        library_name: library.info.library_name.clone(),
        branch_name: branch_name.clone(),
        redirect_uri: authorization.redirect_uri.clone(),
    };
    if let Err(err) = create_pending_join(
        &store,
        &pending,
        &authorization.state,
        &authorization.verifier,
        &authorization.keys,
    )
    .await
    {
        tracing::warn!(error = %err, "could not record the pending join");
        return crate::config_authority::events_error(err);
    }
    tracing::info!(
        origin = %library.origin,
        cluster_id = %library.info.cluster_id,
        "branch sign-in started"
    );
    no_store(
        Json(json!({
            "authorize_url": authorization.authorize_url,
            "library": library_json(&library),
            "branch_name": branch_name,
            "expires_in_secs": bookclerk_library::control_plane::PENDING_JOIN_TTL.as_secs(),
        }))
        .into_response(),
    )
}

/// `POST /api/config/join/callback` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackBody {
    /// The code from the library.
    #[serde(default)]
    code: String,
    /// The `state` it came back with.
    #[serde(default)]
    state: String,
    /// The library's `error` (`access_denied`), when it sent one.
    #[serde(default)]
    error: Option<String>,
}

/// `POST /api/config/join/callback`.
pub async fn post_join_callback(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CallbackBody>,
) -> Response {
    let code = Zeroizing::new(body.code);
    let store = state.library_snapshot().await;
    let Some(taken) = (match take_pending_join(&store, body.state.trim()).await {
        Ok(taken) => taken,
        Err(err) => return crate::config_authority::events_error(err),
    }) else {
        return join_error(
            StatusCode::GONE,
            "join_expired",
            "this sign-in is unknown, already used, or took longer than 10 minutes; start again",
        );
    };
    if let Some(error) = body.error.as_deref() {
        let detail = if error == "access_denied" {
            "the library's user did not let this branch join"
        } else {
            "the library did not complete the sign-in"
        };
        return join_error(StatusCode::FORBIDDEN, "denied", detail);
    }
    if code.trim().is_empty() {
        return join_error(StatusCode::BAD_REQUEST, "invalid_request", "no code");
    }
    let joined = redeem_branch_code(
        &store,
        &BranchRedeem {
            origin: &taken.join.owner_origin,
            spki_pin: taken.join.spki_pin.as_deref(),
            cluster_id: &taken.join.owner_cluster_id,
            code: code.trim(),
            verifier: &taken.verifier,
            redirect_uri: &taken.join.redirect_uri,
            branch_name: &taken.join.branch_name,
            software_version: env!("CARGO_PKG_VERSION"),
        },
        &taken.keys,
    )
    .await;
    drop(taken.verifier);
    let link = match joined {
        Ok(link) => link,
        Err(err) => return join_failed(&err),
    };
    let running = super::start_link_now(&state, &link.owner_cluster_id).await;
    Json(json!({
        "cluster": link_json(&link),
        "library_name": taken.join.library_name,
        "running": running,
    }))
    .into_response()
}

/// `POST /api/config/join/invite` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteBody {
    /// The whole branch invite (`https://<library>/enroll#bcn1...`).
    link: String,
}

/// `POST /api/config/join/invite`: join with a pasted branch invite. The
/// link is redeemed once and dropped; it is never logged or echoed.
pub async fn post_join_invite(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InviteBody>,
) -> Response {
    let link_text = Zeroizing::new(body.link);
    let store = state.library_snapshot().await;
    let joined = join_cluster(&store, &link_text, env!("CARGO_PKG_VERSION")).await;
    drop(link_text);
    let link = match joined {
        Ok(link) => link,
        Err(JoinError::Link(_)) => {
            return join_error(
                StatusCode::BAD_REQUEST,
                "invalid_invite",
                "that is not a branch invite (https://<library>/enroll#bcn1...)",
            )
        }
        Err(err) => return join_failed(&err),
    };
    let running = super::start_link_now(&state, &link.owner_cluster_id).await;
    Json(json!({ "cluster": link_json(&link), "running": running })).into_response()
}

/// `POST /api/config/join/leave` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaveBody {
    /// The library (owner cluster id) to leave.
    cluster_id: String,
}

/// `POST /api/config/join/leave`: Leave library.
pub async fn post_join_leave(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LeaveBody>,
) -> Response {
    match super::leave_link(&state, body.cluster_id.trim()).await {
        Ok(Some((link, notified))) => {
            tracing::info!(
                cluster_id = %link.owner_cluster_id,
                notified,
                "left the library"
            );
            Json(json!({ "cluster": link_json(&link), "owner_notified": notified })).into_response()
        }
        Ok(None) => join_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "this install is not a branch of that library",
        ),
        Err(err) => crate::config_authority::events_error(err),
    }
}

/// `POST /api/config/join/sharing` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharingBody {
    /// The library (owner cluster id).
    cluster_id: String,
    /// "Share this branch's books with *library*".
    share_books: bool,
    /// "Include books from my own branches".
    include_branches: bool,
}

/// `POST /api/config/join/sharing`: Settings → Library's two sharing
/// settings for one library this install is a branch of.
pub async fn post_join_sharing(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SharingBody>,
) -> Response {
    let store = state.library_snapshot().await;
    let cluster_id = body.cluster_id.trim();
    match bookclerk_library::control_plane::set_cluster_link_sharing(
        &store,
        cluster_id,
        body.share_books,
        body.include_branches,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return join_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "this install is not a branch of that library",
            )
        }
        Err(err) => return crate::config_authority::events_error(err),
    }
    tracing::info!(
        %cluster_id,
        share_books = body.share_books,
        include_branches = body.include_branches,
        "branch sharing settings changed"
    );
    super::catalog_sync::notify_sharing_changed();
    match bookclerk_library::control_plane::load_cluster_link(&store, cluster_id).await {
        Ok(Some(link)) => Json(json!({ "cluster": link_json(&link) })).into_response(),
        Ok(None) => join_error(StatusCode::NOT_FOUND, "not_found", "no such library"),
        Err(err) => crate::config_authority::events_error(err),
    }
}

#[cfg(test)]
#[path = "join_tests.rs"]
mod tests;
