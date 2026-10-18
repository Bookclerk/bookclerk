//! Join a library with a sign-in: the owner side of the OAuth code + PKCE
//! flow of the built-in public client [`BRANCH_CLIENT_ID`].
//!
//! - `GET /api/node/v1/info` ([`get_info`]): the cluster id, a library
//!   name, the canonical origin, the node API transport mode, whether new
//!   branches may join, and in `self_signed` mode the SPKI fingerprint the
//!   joining operator confirms. Outside the cookie and auth layers, after
//!   the transport policy.
//! - `GET|POST /api/auth/branch-consent` ([`get_branch_consent`],
//!   [`post_branch_consent`]): the consent page's API. `/oidc/authorize`
//!   sends a [`BRANCH_CLIENT_ID`] request to the SPA page `/join/approve`,
//!   which signs the user in and asks "Let *branch* join *library* as
//!   yours?". Approving checks the signed-in user is active, host
//!   enrollment is on and the connection passes the transport policy, then
//!   issues a single-use code that lives [`BRANCH_CODE_TTL_SECS`].
//! - `POST /api/node/v1/enroll/oauth` ([`post_enroll_oauth`]): the joining
//!   daemon redeems the code with its PKCE verifier and the node's public
//!   keys. The code is consumed and checked (client, redirect URI, scope,
//!   S256 verifier), then the redeem is the same canonical batch a minted
//!   link takes ([`redeem_enrollment_link`]), through a link whose id and
//!   secret derive from the code and verifier
//!   ([`authorized_enrollment_link`]): one node row, `node_credentials`,
//!   a receipt, idempotent retries, and one node under concurrency.
//! - `GET|PUT /api/config/branches` ([`get_branches`], [`put_branches`]):
//!   the owner's Settings → Branches (operator or Owner): the fingerprint,
//!   "Let new branches join", and each branch with its two sharing
//!   settings as it last reported them (read-only here; the branch's
//!   operator sets them) and counts of the titles it shared.
//! - `POST /api/config/branches/titles/remove` `{node_id?}`
//!   ([`post_remove_branch_titles`]): removes, in bulk, the entries of
//!   titles from a branch that stopped sharing or left (of one branch, or
//!   all). Files already delivered stay in this library's outputs.

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_config::NodeApiMode;
use bookclerk_library::control_plane::{
    authorized_enrollment_link, branch_title_counts, library_ancestry, list_nodes,
    load_cluster_row, load_host_enrollment, load_node_credential,
    record_authorized_enrollment_link, redeem_enrollment_link, remove_detached_branch_titles,
    set_host_enrollment_enabled, verify_cluster_owner, ConfigActor, NodeKind, RedeemRequest,
};
use bookclerk_library::{hash_token, LibraryError, UserStatus};
use bookclerk_node_proto::{
    canonical_origin, is_branch_name, is_branch_redirect_uri, pkce_challenge, OAuthEnrollRequest,
    OwnerInfo, BRANCH_CLIENT_ID, BRANCH_CODE_TTL_SECS, BRANCH_SCOPE,
};
use serde::Deserialize;
use serde_json::json;

use super::{check_transport, decode_key, enroll_error, redeem_answer};
use crate::api::AppState;
use crate::auth::{constant_time_eq, UserCaller};

/// A name to show for this library: the host of its canonical origin.
pub(crate) fn library_name(origin: &str) -> String {
    url::Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .map(|host| host.trim_matches(['[', ']']).to_string())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "Bookclerk library".into())
}

/// What `info` and Settings → Branches report.
async fn owner_info(state: &Arc<AppState>) -> Result<OwnerInfo, Response> {
    let library = state.library_snapshot().await;
    let cluster_id = load_cluster_row(&library)
        .await
        .ok()
        .flatten()
        .map(|row| row.cluster_id)
        .ok_or_else(|| enroll_error(StatusCode::SERVICE_UNAVAILABLE, "cluster_unavailable"))?;
    let enrollment = load_host_enrollment(&library)
        .await
        .map(|settings| settings.body.enabled)
        .unwrap_or(false);
    // A joining library refuses a parent whose ancestry names it.
    let ancestry = library_ancestry(&library)
        .await
        .map_err(|_| enroll_error(StatusCode::SERVICE_UNAVAILABLE, "cluster_unavailable"))?;
    let cfg = state.config.read().await;
    let origin = canonical_origin(&crate::origin::effective_origin_from_config(&cfg, None))
        .ok_or_else(|| enroll_error(StatusCode::SERVICE_UNAVAILABLE, "origin_unavailable"))?;
    let mode = cfg.daemon.node_api.mode;
    let spki_fingerprint = (mode == NodeApiMode::SelfSigned)
        .then(|| bookclerk_config::read_node_api_spki_pin(&cfg.paths().files_dir))
        .flatten();
    Ok(OwnerInfo {
        cluster_id,
        library_name: library_name(&origin),
        origin,
        transport: mode.as_str().to_string(),
        enrollment,
        spki_fingerprint,
        software_version: env!("CARGO_PKG_VERSION").to_string(),
        ancestry,
    })
}

/// `GET /api/node/v1/info`.
pub async fn get_info(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    if let Err(refused) = check_transport(&state, &mut parts, "info").await {
        return refused;
    }
    match owner_info(&state).await {
        Ok(info) => Json(info).into_response(),
        Err(response) => response,
    }
}

/// Largest `enroll/oauth` body.
const MAX_OAUTH_ENROLL_BODY_BYTES: usize = 16 * 1024;

/// Default node label when the branch sent no name.
const DEFAULT_BRANCH_LABEL: &str = "branch";

/// `POST /api/node/v1/enroll/oauth`: redeem an approved branch sign-in.
/// Answers `{cluster_id, node_id}`; 403 `transport_insecure` for a refused
/// connection, 403 `enrollment_refused` for a code that does not redeem
/// (unknown, used, expired, another client's, a wrong verifier or
/// redirect URI, enrollment off, or the user no longer active).
pub async fn post_enroll_oauth(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    if let Err(refused) = check_transport(&state, &mut parts, "enroll_oauth").await {
        return refused;
    }
    let body = match super::read_json_body(&parts.headers, body, MAX_OAUTH_ENROLL_BODY_BYTES).await
    {
        Ok(body) => body,
        Err(refused) => return refused,
    };
    let Ok(body) = serde_json::from_slice::<OAuthEnrollRequest>(&body) else {
        return enroll_error(StatusCode::BAD_REQUEST, "invalid enroll request");
    };
    let (Some(sign_public_key), Some(box_public_key)) =
        (decode_key(&body.sign_pk), decode_key(&body.box_pk))
    else {
        return enroll_error(
            StatusCode::BAD_REQUEST,
            "sign_pk and box_pk must be base64url",
        );
    };
    let code = body.code.trim();
    let verifier = body.code_verifier.trim();
    if code.is_empty() || verifier.is_empty() {
        return enroll_error(StatusCode::FORBIDDEN, "enrollment_refused");
    }
    let label = body.branch_name.trim();
    let label = if is_branch_name(label) {
        label
    } else {
        DEFAULT_BRANCH_LABEL
    };
    let (link_id, secret) = authorized_enrollment_link(code, verifier);
    let library = state.library_snapshot().await;
    match library.consume_oidc_auth_code(&hash_token(code)).await {
        Ok(Some((client_id, user_id, redirect_uri, challenge, method, scope))) => {
            let matches = client_id == BRANCH_CLIENT_ID
                && redirect_uri == body.redirect_uri
                && method == "S256"
                && scope == BRANCH_SCOPE
                && constant_time_eq(pkce_challenge(verifier).as_bytes(), challenge.as_bytes());
            if !matches {
                tracing::warn!(
                    user_id,
                    "branch enrollment refused: the code does not match its exchange"
                );
                let _ = library
                    .insert_security_audit_event(
                        &format!("user:{user_id}"),
                        "branch_enroll_refused",
                        Some(r#"{"reason":"code_mismatch"}"#),
                    )
                    .await;
                return enroll_error(StatusCode::FORBIDDEN, "enrollment_refused");
            }
            match record_authorized_enrollment_link(&library, &link_id, &secret, user_id, label)
                .await
            {
                Ok(()) => {}
                Err(LibraryError::Forbidden(_)) => {
                    return enroll_error(StatusCode::FORBIDDEN, "enrollment_refused")
                }
                Err(LibraryError::Invalid(detail)) => {
                    return enroll_error(StatusCode::BAD_REQUEST, &detail)
                }
                Err(err) => {
                    tracing::warn!(error = %err, "branch enrollment could not record its link");
                    return enroll_error(StatusCode::SERVICE_UNAVAILABLE, "enrollment_unavailable");
                }
            }
        }
        // Already consumed (or unknown): only a retry of the same exchange
        // redeems, from the link the first attempt recorded.
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(error = %err, "branch enrollment could not read the code");
            return enroll_error(StatusCode::SERVICE_UNAVAILABLE, "enrollment_unavailable");
        }
    }
    let request = RedeemRequest {
        link_id,
        secret,
        sign_public_key,
        box_public_key,
        software_version: body.software_version,
    };
    redeem_answer(redeem_enrollment_link(&library, &request).await)
}

/// The authorization request a consent is for (the query `/oidc/authorize`
/// forwarded to `/join/approve`).
#[derive(Debug, Clone, Deserialize)]
pub struct BranchAuthorizeParams {
    /// Must be [`BRANCH_CLIENT_ID`].
    pub client_id: String,
    /// Where the code goes: loopback or https.
    pub redirect_uri: String,
    /// Must be `code`.
    pub response_type: String,
    /// Must be [`BRANCH_SCOPE`].
    #[serde(default)]
    pub scope: Option<String>,
    /// Echoed on the redirect.
    #[serde(default)]
    pub state: Option<String>,
    /// PKCE S256 challenge.
    pub code_challenge: String,
    /// Must be `S256`.
    #[serde(default)]
    pub code_challenge_method: Option<String>,
    /// The name the branch asks to join as.
    #[serde(default)]
    pub branch_name: Option<String>,
}

/// Parses an authorize query string; `None` when a parameter repeats or the
/// shape is wrong.
pub(crate) fn parse_branch_query(raw: &str) -> Option<BranchAuthorizeParams> {
    let mut map = serde_json::Map::new();
    for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if map
            .insert(
                name.into_owned(),
                serde_json::Value::String(value.into_owned()),
            )
            .is_some()
        {
            return None;
        }
    }
    serde_json::from_value(serde_json::Value::Object(map)).ok()
}

/// True for a base64url S256 challenge (43 characters).
fn is_challenge(raw: &str) -> bool {
    raw.len() == 43
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Why a branch authorization request is malformed, or `None`.
pub(crate) fn branch_request_problem(params: &BranchAuthorizeParams) -> Option<&'static str> {
    if params.client_id != BRANCH_CLIENT_ID {
        return Some("unknown client");
    }
    if params.response_type != "code" {
        return Some("response_type must be code");
    }
    if params.code_challenge_method.as_deref() != Some("S256")
        || !is_challenge(&params.code_challenge)
    {
        return Some("an S256 code challenge is required");
    }
    if params.scope.as_deref().map(str::trim) != Some(BRANCH_SCOPE) {
        return Some("scope must be node:enroll");
    }
    if !is_branch_redirect_uri(&params.redirect_uri) {
        return Some("the redirect URI must be a loopback or https URL");
    }
    if !params.branch_name.as_deref().is_some_and(is_branch_name) {
        return Some("the branch needs a name of 1 to 64 characters");
    }
    if params
        .state
        .as_deref()
        .is_some_and(|state| state.len() > 512 || state.chars().any(char::is_control))
    {
        return Some("malformed state");
    }
    None
}

/// `redirect_uri` with `pairs` appended to its query.
fn redirect_with(redirect_uri: &str, pairs: &[(&str, &str)]) -> Option<String> {
    let mut url = url::Url::parse(redirect_uri).ok()?;
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in pairs {
            query.append_pair(name, value);
        }
    }
    Some(url.into())
}

/// A JSON error with `detail`.
fn consent_error(status: StatusCode, error: &str, detail: &str) -> Response {
    (status, Json(json!({ "error": error, "detail": detail }))).into_response()
}

/// The signed-in user a consent is for: never the operator, who owns no
/// node.
async fn consenting_user(
    state: &Arc<AppState>,
    caller: UserCaller,
) -> Result<bookclerk_library::UserRecord, Response> {
    let UserCaller::User { user_id } = caller else {
        return Err(consent_error(
            StatusCode::FORBIDDEN,
            "user_required",
            "a branch belongs to a user: sign in with your account on this library",
        ));
    };
    let library = state.library_snapshot().await;
    match library.get_user(user_id).await {
        Ok(Some(user)) if user.status == UserStatus::Active => Ok(user),
        Ok(_) => Err(consent_error(
            StatusCode::FORBIDDEN,
            "user_inactive",
            "this account is not active",
        )),
        Err(err) => {
            tracing::warn!(error = %err, "branch consent could not read the user");
            Err(consent_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "try again",
            ))
        }
    }
}

/// `GET /api/auth/branch-consent?<authorize params>`: what the consent page
/// shows. 400 `invalid_request` for a malformed request (the page then
/// shows the problem and never redirects).
pub async fn get_branch_consent(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Query(params): Query<BranchAuthorizeParams>,
) -> Response {
    if let Some(problem) = branch_request_problem(&params) {
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
    let redirect_origin = canonical_origin(&params.redirect_uri).unwrap_or_default();
    Json(json!({
        "library_name": info.library_name,
        "branch_name": params.branch_name.unwrap_or_default().trim(),
        "redirect_origin": redirect_origin,
        "enrollment": info.enrollment,
        "user": {
            "id": user.id,
            "display_name": user.display_name,
        },
    }))
    .into_response()
}

/// `POST /api/auth/branch-consent` body.
#[derive(Debug, Deserialize)]
pub struct BranchConsentBody {
    /// The authorization request.
    #[serde(flatten)]
    pub params: BranchAuthorizeParams,
    /// True to let the branch join; false to deny.
    pub approve: bool,
}

/// `POST /api/auth/branch-consent`: approve or deny. Answers `{redirect}`,
/// the URL to send the browser to: the branch's callback with a code (or
/// `error=access_denied`). Approving needs an active signed-in user, host
/// enrollment on, and a connection the transport policy allows.
pub async fn post_branch_consent(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let body = match super::read_json_body(&parts.headers, body, 8 * 1024).await {
        Ok(body) => body,
        Err(refused) => return refused,
    };
    let Ok(body) = serde_json::from_slice::<BranchConsentBody>(&body) else {
        return consent_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "malformed consent",
        );
    };
    let params = body.params;
    if let Some(problem) = branch_request_problem(&params) {
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
            Some(redirect) => no_store(Json(json!({ "redirect": redirect })).into_response()),
            None => consent_error(StatusCode::BAD_REQUEST, "invalid_request", "bad redirect"),
        };
    }
    // The transport policy, as for the node API itself: this sign-in leads
    // to an enrollment, so it is TLS except on loopback.
    if let Err(refused) = check_transport(&state, &mut parts, "branch_consent").await {
        return refused;
    }
    let library = state.library_snapshot().await;
    match load_host_enrollment(&library).await {
        Ok(settings) if settings.body.enabled => {}
        Ok(_) => {
            return consent_error(
                StatusCode::FORBIDDEN,
                "enrollment_disabled",
                "this library isn't accepting new branches",
            )
        }
        Err(err) => {
            tracing::warn!(error = %err, "branch consent could not read host enrollment");
            return consent_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "try again");
        }
    }
    let code = {
        use base64::Engine as _;
        use rand::RngCore as _;
        let mut raw = zeroize::Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(raw.as_mut_slice());
        zeroize::Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_slice()),
        )
    };
    let expires = chrono::Utc::now() + chrono::Duration::seconds(BRANCH_CODE_TTL_SECS);
    if let Err(err) = library
        .insert_oidc_auth_code(
            &hash_token(&code),
            BRANCH_CLIENT_ID,
            user.id,
            &params.redirect_uri,
            &params.code_challenge,
            "S256",
            BRANCH_SCOPE,
            expires,
        )
        .await
    {
        tracing::warn!(error = %err, "branch consent could not store its code");
        return consent_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "try again");
    }
    let detail = json!({
        "client_id": BRANCH_CLIENT_ID,
        "branch_name": params.branch_name.as_deref().unwrap_or("").trim(),
        "redirect_origin": canonical_origin(&params.redirect_uri),
    })
    .to_string();
    let _ = library
        .insert_security_audit_event(
            &format!("user:{}", user.id),
            "branch_authorize",
            Some(&detail),
        )
        .await;
    pairs.push(("code", &code));
    if !state_param.is_empty() {
        pairs.push(("state", &state_param));
    }
    match redirect_with(&params.redirect_uri, &pairs) {
        Some(redirect) => no_store(Json(json!({ "redirect": redirect })).into_response()),
        None => consent_error(StatusCode::BAD_REQUEST, "invalid_request", "bad redirect"),
    }
}

/// Marks a response `Cache-Control: no-store`.
pub(crate) fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Settings → Branches (and Settings → Library's sharing settings), for the
/// operator or an Owner: a signed-in user's stored role and status are read
/// ([`verify_cluster_owner`]); anyone else gets 403.
pub(crate) async fn require_branch_admin(
    state: &Arc<AppState>,
    caller: UserCaller,
) -> Result<ConfigActor, Response> {
    match caller {
        UserCaller::Operator => Ok(ConfigActor::Operator {
            id: "operator".into(),
        }),
        UserCaller::User { user_id } => {
            let library = state.library_snapshot().await;
            verify_cluster_owner(&library, user_id).await.map_err(|_| {
                (StatusCode::FORBIDDEN, Json(json!({ "error": "forbidden" }))).into_response()
            })
        }
    }
}

/// `GET /api/config/branches`: this library's join settings: whether new
/// branches may join, the transport mode, and the SPKI fingerprint a
/// joining operator confirms in `self_signed` mode.
pub async fn get_branches(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    let info = match owner_info(&state).await {
        Ok(info) => info,
        Err(response) => return response,
    };
    let library = state.library_snapshot().await;
    let branches = match branches_json(&library).await {
        Ok(branches) => branches,
        Err(err) => return crate::config_authority::events_error(err),
    };
    Json(json!({
        "library_name": info.library_name,
        "origin": info.origin,
        "transport": info.transport,
        "enrollment": info.enrollment,
        "spki_fingerprint": info.spki_fingerprint,
        "branches": branches,
    }))
    .into_response()
}

/// Every branch (host node) of this library, newest first: its owner,
/// whether it is still joined, its two sharing settings as it last
/// reported them (`null` until its first catalog sync), and counts of the
/// titles it shared.
async fn branches_json(
    library: &bookclerk_library::LibraryStore,
) -> bookclerk_library::Result<Vec<serde_json::Value>> {
    let mut nodes: Vec<_> = list_nodes(library)
        .await?
        .into_iter()
        .filter(|node| node.kind == NodeKind::Host)
        .collect();
    nodes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        let owner = match node.owner_user_id {
            Some(id) => library.get_user(id).await?,
            None => None,
        };
        let joined = load_node_credential(library, &node.node_id)
            .await?
            .is_some_and(|credential| credential.revoked_at.is_none());
        let titles = branch_title_counts(library, &node.node_id).await?;
        out.push(json!({
            "node_id": node.node_id,
            "created_at": node.created_at,
            "owner_user_id": node.owner_user_id,
            "owner_name": owner.and_then(|user| user.display_name.or(user.login_name)),
            "joined": joined,
            "share_books": node.share_books,
            "include_branches": node.include_branches,
            "titles": titles,
        }));
    }
    Ok(out)
}

/// `POST /api/config/branches/titles/remove` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveBranchTitles {
    /// Only this branch's entries; every branch's when absent.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// `POST /api/config/branches/titles/remove`: removes the entries of
/// titles from a branch that stopped sharing or left. Delivered files stay.
pub async fn post_remove_branch_titles(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<RemoveBranchTitles>,
) -> Response {
    if let Err(refused) = require_branch_admin(&state, caller).await {
        return refused;
    }
    let library = state.library_snapshot().await;
    let node_id = body
        .node_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    match remove_detached_branch_titles(&library, node_id).await {
        Ok(removed) => {
            tracing::info!(
                node_id = node_id.unwrap_or("*"),
                removed,
                "removed titles from branches that stopped sharing or left"
            );
            Json(json!({ "removed": removed })).into_response()
        }
        Err(err) => crate::config_authority::events_error(err),
    }
}

/// `PUT /api/config/branches` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutBranches {
    /// "Let new branches join" (`core.host_enrollment.enabled`).
    pub enrollment: bool,
}

/// `PUT /api/config/branches`: turn "Let new branches join" on or off.
pub async fn put_branches(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<PutBranches>,
) -> Response {
    let actor = match require_branch_admin(&state, caller).await {
        Ok(actor) => actor,
        Err(refused) => return refused,
    };
    let library = state.library_snapshot().await;
    match set_host_enrollment_enabled(&library, &actor, body.enrollment).await {
        Ok(doc) => Json(json!({ "enrollment": doc.body.enabled })).into_response(),
        Err(err) => crate::config_authority::events_error(err),
    }
}

/// The headers' `Content-Type` is JSON.
pub(crate) fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        })
}

#[cfg(test)]
#[path = "branch_tests.rs"]
pub(crate) mod tests;
