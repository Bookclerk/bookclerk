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
//! - `POST /api/config/join/start` `{address, fingerprint?, branch_name?,
//!   clerk?}`: reads the library again, checks the confirmed fingerprint,
//!   makes the PKCE verifier, `state` and node keys, keeps them sealed in a
//!   pending join (10 minutes), and answers the authorize URL to send the
//!   browser to. The redirect URI is this install's own `/join/callback`.
//!   With `clerk: true` the sign-in also offers to join as a clerk; only an
//!   empty install may ([`bookclerk_node_client::install_problem`]).
//! - `POST /api/config/join/callback` `{code, state, connection?}`: the
//!   browser came back. The daemon matches `state` to its pending join and
//!   redeems the code with the verifier only it holds. As a branch it
//!   records the link and starts its client at once. As a clerk (only for
//!   a pending join that offered it, and only while the install is still
//!   empty) it opens the sealed bootstrap with the node key, checks the
//!   library's database guest is installed here, writes the bootstrap as
//!   this install's own (`[database]`, `master.key` wrapped with this
//!   install's password, `node-identity.json`), answers, and restarts on it
//!   ([`crate::restart`]): this install is then an owner node of the
//!   library.
//! - `POST /api/config/join/invite` `{link}`: joins with a pasted branch
//!   invite; the link is used once and not kept.
//! - `POST /api/config/join/leave` `{cluster_id}`: Leave library.
//! - `GET /api/config/join/sharing`: the libraries this install is a
//!   branch of, with their two sharing settings.
//! - `POST /api/config/join/sharing` `{cluster_id, share_books,
//!   include_branches}`: Settings → Library's "Share this branch's books
//!   with *library*" and "Include books from my own branches"; the
//!   catalog sync reports them to the library at once.
//!
//! Inspect and start refuse a library whose ancestry names this library's
//! own cluster id (409 `join_cycle`), so a tree of libraries never loops;
//! a branch's redeem checks it again before the code is spent.
//!
//! The sharing routes are for the operator or an Owner (the stored role is
//! read on every call; anyone else gets 403); all the others are
//! operator-only. Answers carrying a URL with `state` are
//! `Cache-Control: no-store`; nothing secret is logged.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use bookclerk_config::DatabasePluginKind;
use bookclerk_library::control_plane::{
    create_pending_join, finish_pending_join, list_cluster_links, open_pending_join, NewPendingJoin,
};
use bookclerk_node_client::{
    apply_clerk_bootstrap, authorize_join, bootstrap_database_kind, inspect_library,
    install_problem, join_cluster, redeem_branch_code, redeem_clerk_code, refuse_cycle,
    BranchRedeem, InspectedLibrary, JoinError,
};
use bookclerk_node_proto::{is_branch_name, ConnectionType, JOIN_CALLBACK_PATH};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

use super::api::link_json;
use crate::api::AppState;
use crate::auth::UserCaller;
use crate::node_api::branch::no_store;
use crate::node_api::branch::require_branch_admin;

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
        JoinError::Cycle => join_error(
            StatusCode::CONFLICT,
            "join_cycle",
            "that library is this library or one of its own branches; joining it would make a loop",
        ),
        JoinError::AncestryTooDeep => join_error(
            StatusCode::CONFLICT,
            "ancestry_too_deep",
            "that library sits too deep in its tree of libraries; joining it could hide a loop",
        ),
        // Enrolled on the library but not saved here: the invite is spent,
        // and the message says what the owner has to revoke.
        JoinError::Unsaved { .. } => {
            join_error(StatusCode::BAD_GATEWAY, "join_unsaved", &err.to_string())
        }
        // Only a sign-in's redeem gives this, and it is retried
        // (`JoinError::is_retryable`) before it gets here.
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
#[utoipa::path(
    get,
    path = "/api/config/join",
    responses(
        (status = 200, description = "Whether this install has joined a library, and its state.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
pub async fn get_join_status(State(state): State<Arc<AppState>>) -> Response {
    let library = state.library_snapshot().await;
    let links = match list_cluster_links(&library).await {
        Ok(links) => links,
        Err(err) => return crate::config_authority::events_error(err),
    };
    let owners = library.count_owners().await.unwrap_or(1);
    let admins = library.count_administrators().await.unwrap_or(1);
    let clerk_problem = match install_problem(&library).await {
        Ok(problem) => problem,
        Err(err) => return join_failed(&err),
    };
    Json(json!({
        "fresh": owners == 0 && admins == 0 && links.is_empty(),
        "has_owner": owners > 0 || admins > 0,
        "clerk_eligible": clerk_problem.is_none(),
        "clerk_problem": clerk_problem,
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectBody {
    /// The library's address (`https://owner.example`, `owner.lan:8443`).
    address: String,
}

/// `POST /api/config/join/inspect`.
#[utoipa::path(
    post,
    path = "/api/config/join/inspect",
    request_body = InspectBody,
    responses(
        (status = 200, description = "What the library says about itself.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 502, description = "The library could not be reached."),
    ),
)]
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
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
    /// Offer to join as a clerk (an owner node that shares the library's
    /// database): only an empty install may, and its data is replaced.
    #[serde(default)]
    clerk: bool,
}

/// `POST /api/config/join/start`.
#[utoipa::path(
    post,
    path = "/api/config/join/start",
    request_body = StartBody,
    responses(
        (status = 200, description = "The library's sign-in URL to open.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 502, description = "The library could not be reached."),
        (status = 409, description = "A join is already in progress or done."),
    ),
)]
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
    let store = state.library_snapshot().await;
    if let Err(err) = refuse_cycle(&store, &library.info.ancestry).await {
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
    if body.clerk {
        match install_problem(&store).await {
            Ok(None) => {}
            Ok(Some(problem)) => return join_error(StatusCode::CONFLICT, "not_empty", &problem),
            Err(err) => return join_failed(&err),
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
    let authorization = match authorize_join(&library, &redirect_uri, &branch_name, body.clerk) {
        Ok(authorization) => authorization,
        Err(err) => return join_failed(&err),
    };
    let pending = NewPendingJoin {
        owner_origin: library.origin.clone(),
        spki_pin: library.spki_pin.clone(),
        owner_cluster_id: library.info.cluster_id.clone(),
        library_name: library.info.library_name.clone(),
        branch_name: branch_name.clone(),
        redirect_uri: authorization.redirect_uri.clone(),
        allow_clerk: body.clerk,
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
        clerk_offered = body.clerk,
        "branch sign-in started"
    );
    no_store(
        Json(json!({
            "authorize_url": authorization.authorize_url,
            "library": library_json(&library),
            "branch_name": branch_name,
            "clerk_offered": body.clerk,
            "expires_in_secs": bookclerk_library::control_plane::PENDING_JOIN_TTL.as_secs(),
        }))
        .into_response(),
    )
}

/// `POST /api/config/join/callback` body.
#[derive(Deserialize, utoipa::ToSchema)]
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
    /// The connection type the library's user chose (`clerk`; a branch
    /// when left out). The code is bound to it, so a changed value only
    /// fails the redeem.
    #[serde(default)]
    connection: Option<String>,
}

/// A sign-in whose redeem the library answered but this install could not
/// save ([`JoinError::SaveFailed`]): the node the library enrolled for it.
struct SaveFailedSignIn {
    /// `hash_token` of the sign-in's `state`; the raw value is not kept.
    state_hash: String,
    /// The node id the library minted for it.
    node_id: String,
}

/// Serializes sign-in callbacks, so two attempts for one sign-in (a retry
/// racing the first, or a page loaded twice) never redeem at once. It
/// holds the latest sign-in whose redeem could not be saved, so its Try
/// again names that node ([`BranchRedeem::enrolled_node_id`]) when it
/// then fails for good; kept in memory only, one at a time (a restart or
/// another sign-in's save failure forgets it, and the retry then reports
/// its own error).
static CALLBACKS: tokio::sync::Mutex<Option<SaveFailedSignIn>> =
    tokio::sync::Mutex::const_new(None);

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
#[utoipa::path(
    post,
    path = "/api/config/join/callback",
    request_body = CallbackBody,
    responses(
        (status = 200, description = "The join completed.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 502, description = "The library could not be reached."),
        (status = 410, description = "The sign-in is unknown, already used, or older than ten minutes; start again (`join_expired`)."),
        (status = 403, description = "The caller's role does not allow this route, or the library refused the sign-in (`denied`)."),
    ),
)]
pub async fn post_join_callback(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CallbackBody>,
) -> Response {
    let code = Zeroizing::new(body.code);
    let sign_in = body.state.trim();
    let store = state.library_snapshot().await;
    // A callback waiting on another sign-in's redeem answers as a retry
    // rather than run past the API deadline.
    let Ok(mut one_at_a_time) = tokio::time::timeout(CALLBACK_WAIT, CALLBACKS.lock()).await else {
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
    let finish = || end_sign_in(&store, sign_in);
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
    let connection = match body.connection.as_deref().map(ConnectionType::parse) {
        None => ConnectionType::Branch,
        Some(Some(connection)) => connection,
        Some(None) => {
            return join_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "connection must be branch or clerk",
            )
        }
    };
    if connection == ConnectionType::Clerk {
        let answer = join_as_clerk(&state, &store, sign_in, taken, code.trim()).await;
        drop(one_at_a_time);
        return answer;
    }
    let state_hash = bookclerk_library::hash_token(sign_in);
    let enrolled_node_id = one_at_a_time
        .as_ref()
        .filter(|failed| failed.state_hash == state_hash)
        .map(|failed| failed.node_id.clone());
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
                enrolled_node_id: enrolled_node_id.as_deref(),
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
    if let Err(JoinError::SaveFailed { node_id, .. }) = &joined {
        *one_at_a_time = Some(SaveFailedSignIn {
            state_hash: state_hash.clone(),
            node_id: node_id.clone(),
        });
    } else if enrolled_node_id.is_some() && !matches!(&joined, Err(err) if err.is_retryable()) {
        // This sign-in ends here (joined, or failed for good naming the
        // node); a retryable failure keeps the node for the next retry.
        *one_at_a_time = None;
    }
    let link = match joined {
        Ok(link) => link,
        Err(err) if err.is_retryable() => {
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

/// Ends the pending sign-in for `sign_in` once its outcome is final.
async fn end_sign_in(store: &bookclerk_library::LibraryStore, sign_in: &str) {
    if let Err(err) = finish_pending_join(store, sign_in).await {
        tracing::warn!(error = %err, "could not delete a finished sign-in; it expires on its own");
    }
}

/// True when the guest for `kind` is installed here, so the daemon can
/// open the library's database after the restart
/// ([`bookclerk_node_client::database_guest_installed`]; tests may assume
/// it).
fn database_guest_installed(cfg: &bookclerk_config::Config, kind: DatabasePluginKind) -> bool {
    #[cfg(test)]
    if tests::ASSUME_DATABASE_GUESTS.load(std::sync::atomic::Ordering::SeqCst) {
        return true;
    }
    bookclerk_node_client::database_guest_installed(cfg, kind)
}

/// The callback for a clerk: redeem as a clerk, open the sealed bootstrap,
/// write it as this install's own, and restart on it. The caller holds the
/// callback lock. Like a branch's, the pending sign-in is kept when the
/// redeem fails for a reason a retry can fix (502 `join_retry`), and ended
/// on success, refusal, or anything else. It is also kept when this server
/// lacks the database guest or D1 token the bootstrap needs (409
/// `database_guest_missing`, `d1_token_missing`), so Try again works once
/// that is fixed.
async fn join_as_clerk(
    state: &Arc<AppState>,
    store: &bookclerk_library::LibraryStore,
    sign_in: &str,
    taken: bookclerk_library::control_plane::TakenJoin,
    code: &str,
) -> Response {
    if !taken.join.allow_clerk {
        end_sign_in(store, sign_in).await;
        return join_error(
            StatusCode::BAD_REQUEST,
            "clerk_not_offered",
            "this sign-in did not offer to join as a clerk; start again and choose to join as a clerk",
        );
    }
    // Checked again here: nothing may have been added since the start.
    match install_problem(store).await {
        Ok(None) => {}
        Ok(Some(problem)) => {
            end_sign_in(store, sign_in).await;
            return join_error(StatusCode::CONFLICT, "not_empty", &problem);
        }
        // A read failure here: the code is not spent yet, so a retry can
        // finish the join.
        Err(err) => {
            return join_error(
                StatusCode::BAD_GATEWAY,
                "join_retry",
                &format!("{err}; try again"),
            )
        }
    }
    let joined = tokio::time::timeout(
        REDEEM_TIMEOUT,
        redeem_clerk_code(
            &BranchRedeem {
                origin: &taken.join.owner_origin,
                spki_pin: taken.join.spki_pin.as_deref(),
                cluster_id: &taken.join.owner_cluster_id,
                code,
                verifier: &taken.verifier,
                redirect_uri: &taken.join.redirect_uri,
                branch_name: &taken.join.branch_name,
                software_version: env!("CARGO_PKG_VERSION"),
                enrolled_node_id: None,
            },
            &taken.keys,
        ),
    )
    .await;
    // The redeem registered both for redaction, as a branch's does.
    bookclerk_config::unregister_secret(code);
    bookclerk_config::unregister_secret(taken.verifier.as_str());
    drop(taken.verifier);
    drop(taken.keys);
    let Ok(joined) = joined else {
        return join_error(
            StatusCode::BAD_GATEWAY,
            "join_retry",
            "the library did not answer in time; try again",
        );
    };
    let joined = match joined {
        Ok(joined) => joined,
        // A repeat of the same exchange gets the same node and a freshly
        // sealed bootstrap.
        Err(err) if err.is_retryable() => {
            return join_error(
                StatusCode::BAD_GATEWAY,
                "join_retry",
                &format!("{err}; try again"),
            )
        }
        Err(err) => {
            end_sign_in(store, sign_in).await;
            return join_failed(&err);
        }
    };
    // The library holds the node now. What it needs here is checked while
    // the sign-in is still pending: once the operator fixes it, Try again
    // repeats the same exchange, and the library answers it with the same
    // node and a freshly sealed bootstrap, so no orphan node is left.
    let cfg = state.config.read().await.clone();
    let kind = bootstrap_database_kind(&joined.bootstrap.database);
    if !database_guest_installed(&cfg, kind) {
        return join_error(
            StatusCode::CONFLICT,
            "database_guest_missing",
            &format!(
                "the library's database is {}, and its database plugin is not installed on this server; install it, then try again within 10 minutes (the library holds this server as node {})",
                kind.as_str(),
                joined.node_id
            ),
        );
    }
    if kind == DatabasePluginKind::D1 && bookclerk_config::resolve_d1_api_token().is_err() {
        return join_error(
            StatusCode::CONFLICT,
            "d1_token_missing",
            &format!(
                "the library's database is Cloudflare D1: set BOOKCLERK_D1_API_TOKEN for this server and restart it (the token is never sent between servers), then try again within 10 minutes (the library holds this server as node {})",
                joined.node_id
            ),
        );
    }
    // The bootstrap is in hand and can be used here: what follows does not
    // redeem again.
    end_sign_in(store, sign_in).await;
    let files_dir = cfg.paths().files_dir.clone();
    let config_file = cfg.paths().config_file.clone();
    let password = cfg.auth_password().map(Zeroizing::new);
    let applied = apply_clerk_bootstrap(
        &files_dir,
        &config_file,
        &joined.bootstrap,
        password.as_ref().map(|p| p.as_str()),
    );
    drop(joined.bootstrap);
    let applied = match applied {
        Ok(applied) => applied,
        // The library holds the node now: name it, so the operator can
        // have that clerk removed there before joining again.
        Err(err) => {
            return join_failed(&JoinError::Unsaved {
                node_id: Some(joined.node_id.clone()),
                reason: err.to_string(),
            })
        }
    };
    let _ = store
        .insert_security_audit_event(
            "operator",
            "clerk_joined",
            Some(
                &json!({
                    "cluster_id": joined.cluster_id,
                    "node_id": joined.node_id,
                    "database": applied.database.as_str(),
                })
                .to_string(),
            ),
        )
        .await;
    tracing::info!(
        cluster_id = %joined.cluster_id,
        node_id = %joined.node_id,
        "joined the library as a clerk; restarting as an owner node"
    );
    // Answer first; the restart waits for in-flight requests.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        crate::restart::request("clerk_join");
    });
    Json(json!({
        "connection": "clerk",
        "cluster_id": joined.cluster_id,
        "node_id": joined.node_id,
        "library_name": taken.join.library_name,
        "database": applied.database.as_str(),
        "master_key_wrapped": applied.wrapped,
        "restarting": true,
    }))
    .into_response()
}

/// `POST /api/config/join/invite` body.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InviteBody {
    /// The whole branch invite (`https://<library>/enroll#bcn1...`).
    link: String,
}

/// `POST /api/config/join/invite`: join with a pasted branch invite. The
/// link is redeemed once and dropped; it is never logged or echoed. A
/// library slower than [`REDEEM_TIMEOUT`] is answered 504 `join_pending`
/// while the join carries on and starts its client when it lands.
#[utoipa::path(
    post,
    path = "/api/config/join/invite",
    request_body = InviteBody,
    responses(
        (status = 200, description = "The join completed.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 502, description = "The library could not be reached."),
        (status = 504, description = "The library is slow to answer; the join goes on in the background (`join_pending`). Poll the join state instead of retrying."),
    ),
)]
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
            // The page has already answered, so a join that fails after
            // all is reported in this daemon's log.
            tokio::spawn(async move {
                match joining.await {
                    Ok(Ok(link)) => {
                        super::start_link_now(&state, &link.owner_cluster_id).await;
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, "a slow invite join failed");
                    }
                    Err(err) => tracing::warn!(error = %err, "a slow invite join stopped"),
                }
            });
            return join_error(
                // Not a success: the page shows the detail, not a link.
                StatusCode::GATEWAY_TIMEOUT,
                "join_pending",
                "the library is slow to answer; this branch keeps joining and appears under Libraries once it has (if it never does, this install's log says why)",
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaveBody {
    /// The library (owner cluster id) to leave.
    cluster_id: String,
}

/// `POST /api/config/join/leave`: Leave library. The leave runs on its own
/// task, so a request cut short (the client gone, or the API deadline)
/// still records it once the library has been told.
#[utoipa::path(
    post,
    path = "/api/config/join/leave",
    request_body = LeaveBody,
    responses(
        (status = 200, description = "Leaving started.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 409, description = "This install has not joined a library."),
        (status = 404, description = "This install is not a branch of that library (`not_found`)."),
    ),
)]
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

/// `POST /api/config/join/sharing` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SharingBody {
    /// The library (owner cluster id).
    cluster_id: String,
    /// "Share this branch's books with *library*".
    share_books: bool,
    /// "Include books from my own branches".
    include_branches: bool,
}

/// `GET /api/config/join/sharing`: Settings → Library for the operator or
/// an Owner: every library this install is a branch of, with its two
/// sharing settings (`{clusters: [...]}`).
#[utoipa::path(
    get,
    path = "/api/config/join/sharing",
    responses(
        (status = 200, description = "The branch's sharing settings.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
pub async fn get_join_sharing(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    let store = state.library_snapshot().await;
    match list_cluster_links(&store).await {
        Ok(links) => Json(json!({ "clusters": links.iter().map(link_json).collect::<Vec<_>>() }))
            .into_response(),
        Err(err) => crate::config_authority::events_error(err),
    }
}

/// `POST /api/config/join/sharing`: Settings → Library's two sharing
/// settings for one library this install is a branch of. The operator or
/// an Owner (verified from the stored role); anyone else gets 403.
#[utoipa::path(
    post,
    path = "/api/config/join/sharing",
    request_body = SharingBody,
    responses(
        (status = 200, description = "The sharing settings after the change.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "This install is not a branch of that library (`not_found`)."),
    ),
)]
pub async fn post_join_sharing(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<SharingBody>,
) -> Response {
    let actor = match require_branch_admin(&state, caller).await {
        Ok(actor) => actor,
        Err(refused) => return refused,
    };
    let actor = match &actor {
        bookclerk_library::control_plane::ConfigActor::ClusterOwner(owner) => {
            format!("user:{}", owner.user_id())
        }
        _ => "operator".to_string(),
    };
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
        %actor,
        share_books = body.share_books,
        include_branches = body.include_branches,
        "branch sharing settings changed"
    );
    let detail = json!({
        "cluster_id": cluster_id,
        "share_books": body.share_books,
        "include_branches": body.include_branches,
    })
    .to_string();
    if let Err(err) = store
        .insert_security_audit_event(&actor, "branch_sharing_changed", Some(&detail))
        .await
    {
        tracing::warn!(
            %cluster_id,
            %actor,
            error = %err,
            "could not record the branch sharing change in the security audit log"
        );
    }
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
