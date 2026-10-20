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
//!
//! All of them are operator-only. Answers carrying a URL with `state` are
//! `Cache-Control: no-store`; nothing secret is logged.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    create_pending_join, finish_pending_join, list_cluster_links, open_pending_join, NewPendingJoin,
};
use bookclerk_node_client::{
    authorize_branch, inspect_library, join_cluster, redeem_branch_code, BranchRedeem,
    InspectedLibrary, JoinError,
};
use bookclerk_node_proto::{is_branch_name, JOIN_CALLBACK_PATH};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

use super::api::link_json;
use crate::api::AppState;
use crate::node_api::branch::no_store;

/// Where the library sends the browser back after consent: the browser's
/// own origin when it is a loopback one (`http://127.0.0.1:8787` stays as
/// typed, so the operator's host-only session cookie is sent on return),
/// else this daemon's public origin. The shared origin helper rewrites a
/// loopback IP to `localhost`, a different site for cookies.
fn callback_origin(cfg: &bookclerk_config::Config, headers: &HeaderMap) -> String {
    let loopback_browser = crate::origin::request_origin(headers).filter(|origin| {
        url::Url::parse(origin).is_ok_and(|url| {
            url.scheme() == "http"
                && match url.host() {
                    Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                    Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                    Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
                    None => false,
                }
                && url.path() == "/"
                && url.query().is_none()
        })
    });
    loopback_browser
        .unwrap_or_else(|| crate::origin::effective_origin_from_config(cfg, Some(headers)))
}

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
        // Enrolled on the library but not saved here: the invite is spent,
        // and the message says what the owner has to revoke.
        JoinError::Unsaved { .. } => {
            join_error(StatusCode::BAD_GATEWAY, "join_unsaved", &err.to_string())
        }
        // Only a sign-in's redeem gives this, and it is retried
        // (`is_retryable`) before it gets here.
        JoinError::SaveFailed { .. } => {
            join_error(StatusCode::BAD_GATEWAY, "join_failed", &err.to_string())
        }
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
                value["running"] = json!(super::is_node_running(
                    &link.owner_cluster_id,
                    &link.remote_node_id
                ));
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
pub async fn post_join_inspect(Json(body): Json<InspectBody>) -> Response {
    match inspect_library(&body.address).await {
        Ok(library) => Json(json!({ "library": library_json(&library) })).into_response(),
        Err(err) => join_failed(&err),
    }
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
            callback_origin(&cfg, &headers).trim_end_matches('/')
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

/// Serializes sign-in callbacks, so two attempts for one sign-in (a retry
/// racing the first, or a page loaded twice) never redeem at once.
static CALLBACKS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// True for a redeem failure a retry with the same code and keys can fix:
/// the library unreachable (its answer may have been lost after it
/// enrolled the node, which a retry gets again), or busy.
fn is_retryable(err: &JoinError) -> bool {
    use bookclerk_node_client::DialError;
    match err {
        // A bad address, an insecure transport, or a certificate that does
        // not match fails the same way on every try.
        JoinError::Dial(DialError::Insecure(_) | DialError::Invalid(_) | DialError::Tls(_)) => {
            false
        }
        JoinError::Dial(DialError::Refused(status)) => *status >= 500 || *status == 429,
        JoinError::Dial(_) => true,
        JoinError::Http { status, .. } => *status >= 500 || *status == 429,
        // The library keeps the node for a repeat of the same exchange.
        JoinError::SaveFailed { .. } => true,
        _ => false,
    }
}

/// How long a join waits on the library to redeem: longer than one connect
/// attempt (`bookclerk_node_client` dials each address for up to 10 s) plus
/// TLS, so one dead address does not fail every redeem. With [`CALLBACK_WAIT`]
/// and [`super::START_NOW_TIMEOUT`] it stays under the API deadline for
/// joins (`api::api_timeout_for_path`), so a slow library is answered as a
/// retry rather than cut off.
const REDEEM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a sign-in callback waits for another one to finish its redeem.
const CALLBACK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// `POST /api/config/join/callback`: redeems the library's code with the
/// sign-in's sealed verifier and keys. The pending sign-in is kept when the
/// redeem fails for a reason a retry can fix (answered as 502
/// `join_retry`; the page offers Try again with the same code), and ended
/// on success, refusal, or anything else.
pub async fn post_join_callback(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CallbackBody>,
) -> Response {
    let code = Zeroizing::new(body.code);
    let sign_in = body.state.trim();
    let store = state.library_snapshot().await;
    // A callback waiting on another sign-in's redeem answers as a retry
    // rather than run past the API deadline.
    let Ok(one_at_a_time) = tokio::time::timeout(CALLBACK_WAIT, CALLBACKS.lock()).await else {
        return join_error(
            StatusCode::BAD_GATEWAY,
            "join_retry",
            "another sign-in is finishing; try again",
        );
    };
    let Some(taken) = (match open_pending_join(&store, sign_in).await {
        Ok(taken) => taken,
        Err(err) => return crate::config_authority::events_error(err),
    }) else {
        return join_error(
            StatusCode::GONE,
            "join_expired",
            "this sign-in is unknown, already used, or took longer than 10 minutes; start again",
        );
    };
    let finish = || async {
        if let Err(err) = finish_pending_join(&store, sign_in).await {
            tracing::warn!(error = %err, "could not delete a finished sign-in; it expires on its own");
        }
    };
    if let Some(error) = body.error.as_deref() {
        finish().await;
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
    let joined = tokio::time::timeout(
        REDEEM_TIMEOUT,
        redeem_branch_code(
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
        ),
    )
    .await;
    // The redeem registered both for redaction; this daemon outlives many
    // joins, and nothing logs them past this point (the answer echoes the
    // error to the browser that sent the code).
    bookclerk_config::unregister_secret(code.trim());
    bookclerk_config::unregister_secret(taken.verifier.as_str());
    drop(taken.verifier);
    let Ok(joined) = joined else {
        // Answer before the API deadline does, so the page can offer Try
        // again with the sign-in still pending.
        return join_error(
            StatusCode::BAD_GATEWAY,
            "join_retry",
            "the library did not answer in time; try again",
        );
    };
    let link = match joined {
        Ok(link) => link,
        Err(err) if is_retryable(&err) => {
            return join_error(
                StatusCode::BAD_GATEWAY,
                "join_retry",
                &format!("{err}; try again"),
            )
        }
        Err(err) => {
            finish().await;
            return join_failed(&err);
        }
    };
    finish().await;
    drop(one_at_a_time);
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
/// link is redeemed once and dropped; it is never logged or echoed. A
/// library slower than [`REDEEM_TIMEOUT`] is answered 504 `join_pending`
/// while the join carries on and starts its client when it lands.
pub async fn post_join_invite(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InviteBody>,
) -> Response {
    let link_text = Zeroizing::new(body.link);
    let store = state.library_snapshot().await;
    // The invite is spent once the library has it, and the node keys exist
    // only in this exchange, so the join runs on its own task: a slow
    // library, or a request cut short, does not drop it half way.
    let mut joining = tokio::spawn(async move {
        let joined = join_cluster(&store, &link_text, env!("CARGO_PKG_VERSION")).await;
        drop(link_text);
        joined
    });
    let joined = match tokio::time::timeout(REDEEM_TIMEOUT, &mut joining).await {
        Ok(Ok(joined)) => joined,
        Ok(Err(_)) => {
            return join_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "join_failed",
                "the join stopped unexpectedly",
            )
        }
        Err(_) => {
            // Start the link's client once the join lands.
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                if let Ok(Ok(link)) = joining.await {
                    super::start_link_now(&state, &link.owner_cluster_id).await;
                }
            });
            return join_error(
                // Not a success: the page shows the detail, not a link.
                StatusCode::GATEWAY_TIMEOUT,
                "join_pending",
                "the library is slow to answer; this branch keeps joining and appears under Libraries once it has",
            );
        }
    };
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

/// `POST /api/config/join/leave`: Leave library. The leave runs on its own
/// task, so a request cut short (the client gone, or the API deadline)
/// still records it once the library has been told.
pub async fn post_join_leave(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LeaveBody>,
) -> Response {
    let cluster_id = body.cluster_id.trim().to_string();
    let leaving = tokio::spawn(async move { super::leave_link(&state, &cluster_id).await });
    let Ok(left) = leaving.await else {
        return join_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "leave_failed",
            "the leave stopped before it finished; check the library list",
        );
    };
    match left {
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

#[cfg(test)]
#[path = "join_tests.rs"]
mod tests;
