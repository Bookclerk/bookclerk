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
//!   keys. The code is read and checked (client, redirect URI, scope and
//!   the consented name and type it holds, S256 verifier), its link
//!   recorded under the consented name, and only then consumed. The
//!   redeem is the same canonical batch a minted link takes
//!   ([`redeem_enrollment_link_as`]), through a link whose id and secret
//!   derive from the code and verifier ([`authorized_enrollment_link`]):
//!   one node row, `node_credentials`,
//!   a receipt, idempotent retries, and one node under concurrency.
//! - `GET|PUT /api/config/branches` ([`get_branches`], [`put_branches`]):
//!   the owner's Settings → Branches (operator or Owner): the fingerprint
//!   and "Let new branches join".
//!
//! **Clerks.** When the joining install offers it ([`bookclerk_node_proto::CLERK_OFFER_PARAM`]:
//! it is empty) and the signed-in user is an Owner (the stored role, read
//! at consent and again at redeem), the consent page also offers "Join as
//! a clerk of this library": an owner node (`kind = owner`) that shares
//! this library's database and DEK. That needs a database more than one
//! server can open (PostgreSQL or D1, never the SQLite file:
//! [`shared_database`]) and a fresh second factor on the consent page
//! (TOTP or a user-verified passkey, [`post_branch_consent_step_up`]); an
//! Owner with neither is refused. The code is bound to the type chosen (the
//! granted scope, [`ConnectionType::granted_scope`], stored with the
//! consented branch name), so a branch code is never redeemed as a clerk. A clerk's redeem answer carries the
//! `[database]` bootstrap and the DEK sealed to the joining node's box key
//! ([`seal_clerk_bootstrap`]); never the `master.key` file, never through
//! the browser, never logged. Both the consent and the redeem run under the
//! node API transport policy: TLS, or loopback when both ends are on this
//! machine.

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_config::DatabasePluginKind;
use bookclerk_config::NodeApiMode;
use bookclerk_library::control_plane::{
    authorized_enrollment_link, load_cluster_row, load_host_enrollment,
    record_authorized_enrollment_link, redeem_enrollment_link_as, set_host_enrollment_enabled,
    verify_cluster_owner, ConfigActor, EnrolledNode, NodeKind, RedeemRequest,
};
use bookclerk_library::{hash_token, LibraryError, UserStatus};
use bookclerk_node_proto::{
    canonical_origin, is_branch_name, is_branch_redirect_uri, pkce_challenge, seal_clerk_bootstrap,
    ClerkBootstrap, ClerkDatabase, ConnectionType, OAuthEnrollAnswer, OAuthEnrollRequest,
    OwnerInfo, SecretValue, BRANCH_CLIENT_ID, BRANCH_CODE_TTL_SECS, BRANCH_SCOPE,
    CLERK_BOOTSTRAP_VERSION,
};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

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

/// This library's database as a clerk would connect to it, or why it
/// cannot be shared: only PostgreSQL and Cloudflare D1 can be opened by
/// more than one server. The D1 API token is never included (it is
/// environment-only bootstrap on every node).
///
/// # Errors
///
/// Returns the reason shown on the consent page.
pub(crate) fn shared_database(cfg: &bookclerk_config::Config) -> Result<ClerkDatabase, String> {
    match bookclerk_config::DatabasePluginKind::parse(&cfg.database.plugin) {
        Some(DatabasePluginKind::Postgres) => bookclerk_config::resolve_postgres_url(cfg)
            .map(|url| ClerkDatabase::Postgres {
                url: SecretValue::new(url.into_bytes()),
            })
            .map_err(|_| "This library's PostgreSQL connection is not readable here.".into()),
        Some(DatabasePluginKind::D1) => Ok(ClerkDatabase::D1 {
            account_id: cfg.database.d1.account_id.clone(),
            database_id: cfg.database.d1.database_id.clone(),
            api_base: cfg.database.d1.api_base.clone(),
        }),
        Some(DatabasePluginKind::Sqlite) => Err("This library keeps its database in a SQLite \
             file that only this server can open. A clerk needs a PostgreSQL or Cloudflare D1 \
             database."
            .into()),
        None => Err("This library's database plugin cannot be shared with a clerk.".into()),
    }
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
    let connection = body.connection;
    let (link_id, secret) = authorized_enrollment_link(code, verifier);
    let library = state.library_snapshot().await;
    let code_hash = hash_token(code);
    // The code is read, checked and its link recorded before the code is
    // consumed: a failed write leaves the code for a retry of the same
    // exchange, and a retry after the code is spent redeems the link the
    // first attempt recorded.
    match library.peek_oidc_auth_code(&code_hash).await {
        Ok(Some((client_id, user_id, redirect_uri, challenge, method, scope))) => {
            // The scope holds the connection type and the branch name
            // chosen at consent: a code approved for a branch never redeems
            // as a clerk (or the other way round).
            let consented = consented_branch_name(&scope, connection);
            let matches = client_id == BRANCH_CLIENT_ID
                && redirect_uri == body.redirect_uri
                && method == "S256"
                && consented.is_some()
                && constant_time_eq(pkce_challenge(verifier).as_bytes(), challenge.as_bytes());
            let Some(label) = consented.filter(|_| matches) else {
                tracing::warn!(
                    user_id,
                    connection = connection.as_str(),
                    "branch enrollment refused: the code does not match its exchange"
                );
                // A mismatched exchange spends the code, as a failed
                // OAuth exchange does.
                let _ = library.consume_oidc_auth_code(&code_hash).await;
                let _ = library
                    .insert_security_audit_event(
                        &format!("user:{user_id}"),
                        if connection == ConnectionType::Clerk {
                            "clerk_enroll_refused"
                        } else {
                            "branch_enroll_refused"
                        },
                        Some(r#"{"reason":"code_mismatch"}"#),
                    )
                    .await;
                return enroll_error(StatusCode::FORBIDDEN, "enrollment_refused");
            };
            if connection == ConnectionType::Clerk {
                if let Err(refused) = clerk_still_allowed(&state, &library, user_id).await {
                    // A refused clerk spends the code, as a mismatch does.
                    let _ = library.consume_oidc_auth_code(&code_hash).await;
                    return refused;
                }
            }
            // The node is labelled with the name the user consented to,
            // whatever the redeem body says.
            match record_authorized_enrollment_link(&library, &link_id, &secret, user_id, &label)
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
            // Consumed here or by a concurrent attempt of the same exchange
            // (the only one whose verifier matches): either way the link is
            // recorded and the redeem below is single-use.
            if let Err(err) = library.consume_oidc_auth_code(&code_hash).await {
                tracing::warn!(error = %err, "branch enrollment could not spend the code");
                return enroll_error(StatusCode::SERVICE_UNAVAILABLE, "enrollment_unavailable");
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
    match connection {
        ConnectionType::Branch => {
            redeem_answer(redeem_enrollment_link_as(&library, &request, NodeKind::Host).await)
        }
        ConnectionType::Clerk => {
            match redeem_enrollment_link_as(&library, &request, NodeKind::Owner).await {
                Ok(enrolled) => clerk_answer(&state, &library, &enrolled, &request).await,
                Err(err) => redeem_answer(Err(err)),
            }
        }
    }
}

/// Refuses a clerk redeem unless the code's user is still an active Owner
/// and this library's database can still be shared.
async fn clerk_still_allowed(
    state: &Arc<AppState>,
    library: &bookclerk_library::LibraryStore,
    user_id: i64,
) -> Result<ClerkDatabase, Response> {
    if verify_cluster_owner(library, user_id).await.is_err() {
        tracing::warn!(
            user_id,
            "clerk enrollment refused: the user is not an Owner"
        );
        let _ = library
            .insert_security_audit_event(
                &format!("user:{user_id}"),
                "clerk_enroll_refused",
                Some(r#"{"reason":"not_owner"}"#),
            )
            .await;
        return Err(enroll_error(StatusCode::FORBIDDEN, "enrollment_refused"));
    }
    let database = shared_database(&*state.config.read().await);
    database.map_err(|_| {
        tracing::warn!(
            user_id,
            "clerk enrollment refused: this library's database cannot be shared"
        );
        enroll_error(StatusCode::CONFLICT, "database_not_shareable")
    })
}

/// The answer to a clerk redeem: the `[database]` bootstrap and this
/// process's DEK sealed to the node's enrolled box key, bound to the
/// cluster and node ids. Re-checked on every answer (a retry too): the
/// node's owner must still be an Owner and the database shareable.
async fn clerk_answer(
    state: &Arc<AppState>,
    library: &bookclerk_library::LibraryStore,
    enrolled: &EnrolledNode,
    request: &RedeemRequest,
) -> Response {
    let database = match clerk_still_allowed(state, library, enrolled.owner_user_id).await {
        Ok(database) => database,
        Err(refused) => return refused,
    };
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let dek = match bookclerk_library::require_master_key(Some(&files_dir)) {
        Ok(dek) => dek,
        Err(err) => {
            tracing::warn!(error = %err, "clerk enrollment: the data-encryption key is unavailable");
            return enroll_error(StatusCode::SERVICE_UNAVAILABLE, "enrollment_unavailable");
        }
    };
    let bootstrap = ClerkBootstrap {
        v: CLERK_BOOTSTRAP_VERSION,
        cluster_id: enrolled.cluster_id.clone(),
        node_id: enrolled.node_id.clone(),
        database,
        dek: SecretValue::new(dek.as_bytes().to_vec()),
    };
    drop(dek);
    let ephemeral = {
        use rand::RngCore as _;
        let mut raw = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(raw.as_mut_slice());
        raw
    };
    let sealed = match seal_clerk_bootstrap(&request.box_public_key, &ephemeral, &bootstrap) {
        Ok(sealed) => sealed,
        Err(_) => return enroll_error(StatusCode::BAD_REQUEST, "invalid box_pk"),
    };
    drop(bootstrap);
    if !enrolled.replayed {
        let detail = json!({
            "node_id": enrolled.node_id,
            "database": shared_database(&*state.config.read().await)
                .map(|db| db.plugin())
                .unwrap_or("unknown"),
        })
        .to_string();
        let _ = library
            .insert_security_audit_event(
                &format!("user:{}", enrolled.owner_user_id),
                "clerk_enrolled",
                Some(&detail),
            )
            .await;
        tracing::info!(
            node_id = %enrolled.node_id,
            owner_user_id = enrolled.owner_user_id,
            "a clerk joined the library (owner node); its bootstrap was sealed to its node key"
        );
    }
    no_store(
        Json(OAuthEnrollAnswer {
            cluster_id: enrolled.cluster_id.clone(),
            node_id: enrolled.node_id.clone(),
            connection: ConnectionType::Clerk,
            bootstrap: Some(sealed),
        })
        .into_response(),
    )
}

/// Marks the branch name kept in a code's stored scope.
const CONSENTED_NAME_MARK: &str = " name:";

/// The scope stored with a sign-in's authorization code: the connection
/// type's granted scope ([`BRANCH_SCOPE`] for a branch,
/// [`bookclerk_node_proto::CLERK_GRANTED_SCOPE`] for a clerk), then the
/// branch name the user consented to (base64url). The redeem enrolls the
/// node under that name, not one the redeem body picks, and only as that
/// type.
fn consented_scope(connection: ConnectionType, branch_name: &str) -> String {
    use base64::Engine as _;
    format!(
        "{}{CONSENTED_NAME_MARK}{}",
        connection.granted_scope(),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(branch_name.as_bytes())
    )
}

/// The consented branch name in a stored scope written by
/// [`consented_scope`] for `connection`; `None` for any other scope (a
/// code approved for the other connection type among them) or a name that
/// is not a valid branch name.
fn consented_branch_name(scope: &str, connection: ConnectionType) -> Option<String> {
    use base64::Engine as _;
    // The granted scopes contain no `name:` token and base64url has no
    // space, so a branch scope never parses as a clerk one or back.
    let encoded = scope
        .strip_prefix(connection.granted_scope())?
        .strip_prefix(CONSENTED_NAME_MARK)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    let name = String::from_utf8(bytes).ok()?;
    is_branch_name(&name).then_some(name)
}

#[cfg(test)]
mod scope_tests {
    use super::{consented_branch_name, consented_scope, ConnectionType};

    #[test]
    fn the_scope_carries_the_type_and_the_consented_name() {
        for connection in [ConnectionType::Branch, ConnectionType::Clerk] {
            let scope = consented_scope(connection, "Den PC");
            assert_eq!(
                consented_branch_name(&scope, connection).as_deref(),
                Some("Den PC")
            );
        }
        let branch = consented_scope(ConnectionType::Branch, "Den PC");
        let clerk = consented_scope(ConnectionType::Clerk, "Den PC");
        assert_eq!(branch, "node:enroll name:RGVuIFBD");
        assert_eq!(clerk, "node:enroll node:clerk name:RGVuIFBD");
        // A branch-approved code never redeems as a clerk, nor the reverse.
        assert_eq!(consented_branch_name(&branch, ConnectionType::Clerk), None);
        assert_eq!(consented_branch_name(&clerk, ConnectionType::Branch), None);
        // The bare scopes (no consented name) and an invalid name refuse.
        assert_eq!(
            consented_branch_name("node:enroll", ConnectionType::Branch),
            None
        );
        assert_eq!(
            consented_branch_name("node:enroll node:clerk", ConnectionType::Clerk),
            None
        );
        assert_eq!(
            consented_branch_name(
                &consented_scope(ConnectionType::Clerk, ""),
                ConnectionType::Clerk
            ),
            None
        );
    }
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
    /// `1` when the joining install offers to join as a clerk
    /// ([`bookclerk_node_proto::CLERK_OFFER_PARAM`]).
    #[serde(default)]
    pub clerk_offer: Option<String>,
}

impl BranchAuthorizeParams {
    /// True when the joining install offered to join as a clerk.
    #[must_use]
    pub fn clerk_offered(&self) -> bool {
        self.clerk_offer.as_deref() == Some("1")
    }
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
    if params
        .clerk_offer
        .as_deref()
        .is_some_and(|offer| offer != "1")
    {
        return Some("malformed clerk_offer");
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
    let connection = match connection_view(&state, &user, &params).await {
        Ok(view) => view,
        Err(response) => return response,
    };
    Json(json!({
        "library_name": info.library_name,
        "branch_name": params.branch_name.unwrap_or_default().trim(),
        "redirect_origin": redirect_origin,
        "enrollment": info.enrollment,
        "user": {
            "id": user.id,
            "display_name": user.display_name,
        },
        "connection": connection,
    }))
    .into_response()
}

/// The second factors `user` can step up with right now.
async fn step_up_methods(
    library: &bookclerk_library::LibraryStore,
    user: &bookclerk_library::UserRecord,
) -> Result<Vec<&'static str>, Response> {
    let mut methods = Vec::new();
    if user.totp_enabled {
        methods.push("totp");
    }
    let passkeys = library
        .count_webauthn_credentials(user.id)
        .await
        .map_err(|_| consent_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "try again"))?;
    if passkeys > 0 {
        methods.push("passkey");
    }
    Ok(methods)
}

/// What the consent page may offer: a choice between branch and clerk for
/// an Owner (the stored role), branch only for everyone else. Clerk is
/// shown disabled, with the reason, when the joining install did not offer
/// it, the database is not shareable, or the user has no second factor.
async fn connection_view(
    state: &Arc<AppState>,
    user: &bookclerk_library::UserRecord,
    params: &BranchAuthorizeParams,
) -> Result<serde_json::Value, Response> {
    let library = state.library_snapshot().await;
    let owner = verify_cluster_owner(&library, user.id).await.is_ok();
    if !owner {
        return Ok(json!({ "choice": false, "default": "branch" }));
    }
    let methods = step_up_methods(&library, user).await?;
    let reason = if !params.clerk_offered() {
        Some(
            "The joining server already holds library data of its own (or did not offer it), so \
             it can only join as a branch."
                .to_string(),
        )
    } else if let Err(reason) = shared_database(&*state.config.read().await) {
        Some(reason)
    } else if methods.is_empty() {
        Some(
            "Joining as a clerk needs a fresh second factor. Set up an authenticator app or a \
             passkey on your account first."
                .to_string(),
        )
    } else {
        None
    };
    Ok(json!({
        "choice": true,
        "default": "branch",
        "clerk_available": reason.is_none(),
        "clerk_reason": reason,
        "step_up": methods,
    }))
}

/// A fresh second factor for a clerk consent. `Debug` prints no code.
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum StepUp {
    /// A current authenticator code.
    Totp {
        /// Six digits.
        code: String,
    },
    /// A user-verified passkey assertion for a challenge from
    /// `POST /api/auth/branch-consent/step-up`.
    Passkey {
        /// The challenge id.
        challenge_id: String,
        /// The browser's `PublicKeyCredential` JSON.
        credential: serde_json::Value,
    },
}

impl std::fmt::Debug for StepUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Totp { .. } => f.write_str("StepUp::Totp"),
            Self::Passkey { challenge_id, .. } => f
                .debug_struct("StepUp::Passkey")
                .field("challenge_id", challenge_id)
                .finish_non_exhaustive(),
        }
    }
}

/// WebAuthn challenge kind of a clerk step-up.
pub(crate) const CLERK_STEP_UP_KIND: &str = "clerk_step_up";

/// `POST /api/auth/branch-consent` body.
#[derive(Debug, Deserialize)]
pub struct BranchConsentBody {
    /// The authorization request.
    #[serde(flatten)]
    pub params: BranchAuthorizeParams,
    /// True to let the branch join; false to deny.
    pub approve: bool,
    /// `branch` (default) or `clerk`.
    #[serde(default)]
    pub connection: Option<String>,
    /// The fresh second factor a clerk needs.
    #[serde(default)]
    pub step_up: Option<StepUp>,
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
    let connection = match body.connection.as_deref().map(ConnectionType::parse) {
        None => ConnectionType::Branch,
        Some(Some(connection)) => connection,
        Some(None) => {
            return consent_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "connection must be branch or clerk",
            )
        }
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
    if connection == ConnectionType::Clerk {
        if let Err(refused) =
            check_clerk_consent(&state, &parts.headers, &user, &params, body.step_up).await
        {
            return refused;
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
            &consented_scope(
                connection,
                params.branch_name.as_deref().unwrap_or("").trim(),
            ),
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
        "connection": connection.as_str(),
    })
    .to_string();
    let _ = library
        .insert_security_audit_event(
            &format!("user:{}", user.id),
            if connection == ConnectionType::Clerk {
                "clerk_authorize"
            } else {
                "branch_authorize"
            },
            Some(&detail),
        )
        .await;
    pairs.push(("code", &code));
    if !state_param.is_empty() {
        pairs.push(("state", &state_param));
    }
    if connection == ConnectionType::Clerk {
        pairs.push(("connection", ConnectionType::Clerk.as_str()));
    }
    match redirect_with(&params.redirect_uri, &pairs) {
        Some(redirect) => no_store(Json(json!({ "redirect": redirect })).into_response()),
        None => consent_error(StatusCode::BAD_REQUEST, "invalid_request", "bad redirect"),
    }
}

/// The checks a clerk approval adds: the joining install offered it, the
/// signed-in user is an Owner (the stored role), the database can be
/// shared, and a fresh second factor verifies now (TOTP, or a passkey
/// assertion with user verification). An Owner with neither factor is
/// refused with how to set one up.
async fn check_clerk_consent(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    user: &bookclerk_library::UserRecord,
    params: &BranchAuthorizeParams,
    step_up: Option<StepUp>,
) -> Result<(), Response> {
    let library = state.library_snapshot().await;
    let refuse = |status: StatusCode, error: &str, detail: &str| {
        let audit = json!({ "reason": error }).to_string();
        let library = library.clone();
        let user_id = user.id;
        let response = consent_error(status, error, detail);
        async move {
            let _ = library
                .insert_security_audit_event(
                    &format!("user:{user_id}"),
                    "clerk_authorize_refused",
                    Some(&audit),
                )
                .await;
            response
        }
    };
    if !params.clerk_offered() {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "clerk_not_offered",
            "the joining server did not offer to join as a clerk; it can join as a branch",
        )
        .await);
    }
    if verify_cluster_owner(&library, user.id).await.is_err() {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "owner_required",
            "only an Owner of this library can let a server join as a clerk",
        )
        .await);
    }
    if let Err(reason) = shared_database(&*state.config.read().await) {
        return Err(refuse(StatusCode::CONFLICT, "database_not_shareable", &reason).await);
    }
    let methods = step_up_methods(&library, user).await?;
    if methods.is_empty() {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "step_up_unavailable",
            "joining as a clerk needs a fresh second factor: set up an authenticator app or a passkey on your account first",
        )
        .await);
    }
    let throttle_key = format!("clerk-step-up:{}", user.id);
    let auth = state.auth_snapshot().await;
    if auth.login_throttle_check(&throttle_key).await.is_some() {
        return Err(consent_error(
            StatusCode::TOO_MANY_REQUESTS,
            "step_up_locked",
            "too many wrong codes; wait a few minutes",
        ));
    }
    let verified = match step_up {
        None => {
            return Err(consent_error(
                StatusCode::FORBIDDEN,
                "step_up_required",
                "confirm with your authenticator app or a passkey to let a clerk join",
            ))
        }
        Some(StepUp::Totp { code }) => {
            let code = Zeroizing::new(code);
            crate::totp::verify_user_totp(&library, user, &code)
                .await
                .map_err(|_| {
                    consent_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "try again")
                })?
        }
        Some(StepUp::Passkey {
            challenge_id,
            credential,
        }) => crate::passkeys::finish_step_up(
            state,
            headers,
            user.id,
            CLERK_STEP_UP_KIND,
            &challenge_id,
            credential,
        )
        .await
        .is_ok(),
    };
    if !verified {
        let _ = auth.record_login_failure(&throttle_key).await;
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "step_up_failed",
            "that second factor did not verify; try again",
        )
        .await);
    }
    auth.clear_login_failures(&throttle_key).await;
    Ok(())
}

/// `POST /api/auth/branch-consent/step-up`: starts a passkey step-up for a
/// clerk consent (an Owner with a passkey). Answers the WebAuthn request
/// options with a `challenge_id` to send back in the consent's `step_up`.
pub async fn post_branch_consent_step_up(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    headers: HeaderMap,
) -> Response {
    let user = match consenting_user(&state, caller).await {
        Ok(user) => user,
        Err(refused) => return refused,
    };
    let library = state.library_snapshot().await;
    if verify_cluster_owner(&library, user.id).await.is_err() {
        return consent_error(
            StatusCode::FORBIDDEN,
            "owner_required",
            "only an Owner of this library can let a server join as a clerk",
        );
    }
    match crate::passkeys::begin_step_up(&state, &headers, user.id, CLERK_STEP_UP_KIND).await {
        Ok(options) => no_store(options.into_response()),
        Err(StatusCode::NOT_FOUND) => consent_error(
            StatusCode::NOT_FOUND,
            "no_passkey",
            "this account has no passkey",
        ),
        Err(status) => consent_error(status, "unavailable", "could not start the passkey check"),
    }
}

/// Marks a response `Cache-Control: no-store`.
pub(crate) fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Settings → Branches, for the operator or an Owner.
async fn require_branch_admin(
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
    match owner_info(&state).await {
        Ok(info) => Json(json!({
            "library_name": info.library_name,
            "origin": info.origin,
            "transport": info.transport,
            "enrollment": info.enrollment,
            "spki_fingerprint": info.spki_fingerprint,
        }))
        .into_response(),
        Err(response) => response,
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

#[cfg(test)]
#[path = "clerk_tests.rs"]
pub(crate) mod clerk_tests;
