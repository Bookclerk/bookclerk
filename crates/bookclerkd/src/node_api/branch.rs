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
//!   ([`redeem_enrollment_link_as`]), through a link whose id derives from
//!   the code and whose secret from the code, verifier and node kind
//!   ([`authorized_enrollment_link`]):
//!   one node row, `node_credentials`,
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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_config::DatabasePluginKind;
use bookclerk_config::NodeApiMode;
use bookclerk_library::control_plane::{
    accounts_to_carry, authorized_enrollment_link, branch_title_counts, check_redeem_request,
    enrollment_link_brings_accounts, library_ancestry, list_nodes, load_cluster_row,
    load_host_enrollment, load_node_credential, member_accounts, record_accounts_sent,
    record_authorized_enrollment_link, redeem_enrollment_link_as, remove_detached_branch_titles,
    set_host_enrollment_enabled, verify_cluster_owner, ConfigActor, EnrolledNode, NodeKind,
    RedeemRequest,
};
use bookclerk_library::{hash_token, LibraryError, UserStatus};
use bookclerk_node_proto::{
    canonical_origin, is_branch_name, is_branch_redirect_uri, pkce_challenge, seal_account_bundle,
    seal_clerk_bootstrap, AccountBundle, ClerkBootstrap, ClerkDatabase, ConnectionType,
    OAuthEnrollAnswer, OAuthEnrollRequest, OwnerInfo, SecretValue, ACCOUNT_BUNDLE_VERSION,
    BRANCH_CLIENT_ID, BRANCH_CODE_TTL_SECS, BRANCH_SCOPE, CLERK_BOOTSTRAP_VERSION,
};
use serde::{Deserialize, Serialize};
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
        // A clerk sends its own D1 token only to Cloudflare's API, so a
        // library on another D1 endpoint is not offered as a clerk's.
        Some(DatabasePluginKind::D1) => {
            bookclerk_node_client::checked_d1_api_base(&cfg.database.d1.api_base)
                .map(|api_base| ClerkDatabase::D1 {
                    account_id: cfg.database.d1.account_id.clone(),
                    database_id: cfg.database.d1.database_id.clone(),
                    api_base,
                })
                .map_err(|_| {
                    "This library's Cloudflare D1 API origin is not https://api.cloudflare.com, \
             and a clerk only sends its D1 token there."
                        .into()
                })
        }
        Some(DatabasePluginKind::Sqlite) => Err("This library keeps its database in a SQLite \
             file that only this server can open. A clerk needs a PostgreSQL or Cloudflare D1 \
             database."
            .into()),
        None => Err("This library's database plugin cannot be shared with a clerk.".into()),
    }
}

/// What `info` and Settings → Branches report.
pub(super) async fn owner_info(state: &Arc<AppState>) -> Result<OwnerInfo, Response> {
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
#[utoipa::path(
    get,
    path = "/api/node/v1/info",
    responses(
        (status = 200, description = "What this library says about itself to a joining install.", body = bookclerk_node_proto::OwnerInfo),
        (status = 403, description = "The connection failed the transport policy (`transport_insecure`, with the check in `reason`).", body = crate::http_error::TransportRefused, example = json!({"error": "transport_insecure", "reason": "plain_http"})),
        (status = 404, description = "The node API is not mounted on this listener (`node_api_unmounted`).", body = crate::http_error::ErrorMessage, example = json!({"error": "node_api_unmounted"})),
        (status = 503, description = "This library's cluster row (`cluster_unavailable`) or origin (`origin_unavailable`) is not available yet.", body = crate::http_error::ErrorMessage, examples(
            ("cluster_unavailable" = (summary = "The cluster row could not be read.", value = json!({"error": "cluster_unavailable"}))),
            ("origin_unavailable" = (summary = "The library's origin is not known yet.", value = json!({"error": "origin_unavailable"}))),
        )),
    ),
)]
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
/// Answers `{cluster_id, node_id}` for a branch, plus `connection` and the
/// sealed `bootstrap` for a clerk; 403 `transport_insecure` for a refused
/// connection, 403 `enrollment_refused` for a code that does not redeem
/// (unknown, used, expired, another client's, a wrong verifier or
/// redirect URI, enrollment off, or the user no longer active).
#[utoipa::path(
    post,
    path = "/api/node/v1/enroll/oauth",
    request_body = bookclerk_node_proto::OAuthEnrollRequest,
    responses(
        (status = 200, description = "The install is enrolled: its cluster and node ids for a branch, and for a clerk also `connection` and the clerk bootstrap sealed to its box key.", body = bookclerk_node_proto::OAuthEnrollAnswer, examples(
            ("branch" = (summary = "A branch joined.", value = json!({"cluster_id": "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f", "node_id": "7d2b9e4c-1f3a-4b8d-9c6e-2a5f8b1d4e7c"}))),
            ("clerk" = (summary = "A clerk joined; its bootstrap is sealed to its box key.", value = json!({"cluster_id": "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f", "node_id": "7d2b9e4c-1f3a-4b8d-9c6e-2a5f8b1d4e7c", "connection": "clerk", "bootstrap": "Qm9va2NsZXJrIHNlYWxlZCBjbGVyayBib290c3RyYXA6IGVwaGVtZXJhbCBrZXkgKyBjaXBoZXJ0ZXh0"}))),
        )),
        (status = 400, description = "The body is not an enroll request, or its keys are not base64url.", body = crate::http_error::ErrorMessage, examples(
            ("malformed" = (summary = "The body does not parse as an enroll request.", value = json!({"error": "invalid enroll request"}))),
            ("bad_keys" = (summary = "A public key is not base64url.", value = json!({"error": "sign_pk and box_pk must be base64url"}))),
        )),
        (status = 403, description = "The connection failed the transport policy (`transport_insecure`, with the check in `reason`), or the code does not redeem: blank, unknown, used, expired, wrong verifier, or the user may no longer let a clerk join (`enrollment_refused`).", body = crate::http_error::NodeRefusal, examples(
            ("transport_insecure" = (summary = "Plain HTTP from another machine.", value = json!({"error": "transport_insecure", "reason": "plain_http"}))),
            ("enrollment_refused" = (summary = "The code does not redeem.", value = json!({"error": "enrollment_refused"}))),
        )),
        (status = 404, description = "The node API is not mounted on this listener (`node_api_unmounted`).", body = crate::http_error::ErrorMessage, example = json!({"error": "node_api_unmounted"})),
        (status = 409, description = "This library's database cannot be shared with a clerk (`database_not_shareable`).", body = crate::http_error::ErrorMessage, example = json!({"error": "database_not_shareable"})),
        (status = 413, description = "The body is larger than 16 KiB.", body = crate::http_error::ErrorMessage, example = json!({"error": "request too large"})),
        (status = 415, description = "The request is not `application/json`.", body = crate::http_error::ErrorMessage, example = json!({"error": "expected application/json"})),
        (status = 503, description = "The enrollment could not be read or recorded; try again (`enrollment_unavailable`).", body = crate::http_error::ErrorMessage, example = json!({"error": "enrollment_unavailable"})),
    ),
)]
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
    let kind = match connection {
        ConnectionType::Branch => NodeKind::Host,
        ConnectionType::Clerk => NodeKind::Owner,
    };
    // The link's secret binds the kind, so a link recorded for a branch
    // never redeems as a clerk (or the other way round), even after a
    // first redeem that failed.
    let (link_id, secret) = authorized_enrollment_link(code, verifier, kind);
    let request = RedeemRequest {
        link_id,
        secret,
        sign_public_key,
        box_public_key,
        software_version: body.software_version,
    };
    // Refused before the code is spent, so a malformed request leaves it
    // for a corrected retry.
    if let Err(err) = check_redeem_request(&request) {
        return redeem_answer(Err(err));
    }
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
            let consented = consented_branch(&scope, connection);
            let matches = client_id == BRANCH_CLIENT_ID
                && redirect_uri == body.redirect_uri
                && method == "S256"
                && consented.is_some()
                && constant_time_eq(pkce_challenge(verifier).as_bytes(), challenge.as_bytes());
            let Some(Consented {
                name: label,
                bring_connections,
            }) = consented.filter(|_| matches)
            else {
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
            match record_authorized_enrollment_link(
                &library,
                &request.link_id,
                &request.secret,
                user_id,
                &label,
                bring_connections,
            )
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
    match connection {
        ConnectionType::Branch => match redeem_enrollment_link_as(&library, &request, kind).await {
            Ok(enrolled) => branch_answer(&state, &library, enrolled, &request).await,
            Err(err) => redeem_answer(Err(err)),
        },
        ConnectionType::Clerk => match redeem_enrollment_link_as(&library, &request, kind).await {
            Ok(enrolled) => clerk_answer(&state, &library, &enrolled, &request).await,
            Err(err) => redeem_answer(Err(err)),
        },
    }
}

/// Plugin keys whose storefront declares `credentials = "shareable"` in
/// every `plugin.toml` this node knows for it: the plugins discovered on
/// disk and the active installed versions. Any other plugin counts as a
/// single keeper, whose accounts do not travel.
pub(crate) async fn shareable_plugin_keys(
    state: &Arc<AppState>,
    library: &bookclerk_library::LibraryStore,
) -> BTreeSet<String> {
    use bookclerk_plugin_host::{CredentialSharing, PluginManifest};
    /// Records one manifest's declaration for `key`; `None` is a manifest
    /// that could not be read, which is not a declaration.
    fn note(seen: &mut BTreeMap<String, bool>, key: String, manifest: Option<&PluginManifest>) {
        let shareable = manifest
            .is_some_and(|manifest| manifest.credential_sharing() == CredentialSharing::Shareable);
        let entry = seen.entry(key).or_insert(true);
        *entry = *entry && shareable;
    }
    let mut seen: BTreeMap<String, bool> = BTreeMap::new();
    let cfg = state.config.read().await.clone();
    match tokio::task::spawn_blocking(move || bookclerk_plugin_host::discover_plugins(&cfg)).await {
        Ok(Ok(plugins)) => {
            for plugin in plugins {
                note(
                    &mut seen,
                    plugin.identity.plugin_key.to_string(),
                    Some(&plugin.manifest),
                );
            }
        }
        // Without every declaration, nothing travels.
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "plugin discovery failed; no connections travel");
            return BTreeSet::new();
        }
        Err(err) => {
            tracing::warn!(error = %err, "plugin discovery stopped; no connections travel");
            return BTreeSet::new();
        }
    }
    match bookclerk_library::control_plane::list_installs(library).await {
        Ok(installs) => {
            for install in installs {
                let version = match bookclerk_library::control_plane::load_active_version(
                    library,
                    &install.plugin_key,
                )
                .await
                {
                    Ok(Some(version)) => version,
                    Ok(None) => continue,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "an installed plugin could not be read; no connections travel"
                        );
                        return BTreeSet::new();
                    }
                };
                let manifest = PluginManifest::parse(&version.manifest_toml).ok();
                note(&mut seen, install.plugin_key, manifest.as_ref());
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "installed plugins could not be read; no connections travel");
            return BTreeSet::new();
        }
    }
    seen.into_iter()
        .filter_map(|(key, shareable)| shareable.then_some(key))
        .collect()
}

/// The answer to a branch redeem: its cluster and node ids, and when the
/// user who approved it chose "Bring my store connections to this
/// branch", the user's own shareable store accounts sealed to the node's
/// box key ([`seal_account_bundle`]). A retry of the same exchange answers
/// with them again. A bundle that cannot be made is logged and left out:
/// the branch still joins, and the person connects the accounts there.
async fn branch_answer(
    state: &Arc<AppState>,
    library: &bookclerk_library::LibraryStore,
    enrolled: EnrolledNode,
    request: &RedeemRequest,
) -> Response {
    let bring = enrollment_link_brings_accounts(library, &request.link_id)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "branch enrollment could not read the connections choice");
            false
        });
    let accounts = if bring {
        sealed_connections(state, library, &enrolled, &request.box_public_key).await
    } else {
        None
    };
    Json(OAuthEnrollAnswer {
        cluster_id: enrolled.cluster_id,
        node_id: enrolled.node_id,
        connection: ConnectionType::Branch,
        bootstrap: None,
        accounts,
    })
    .into_response()
}

/// The enrolled node owner's shareable store accounts sealed to
/// `box_public_key`, recorded as sent to the node; `None` when there are
/// none or they cannot be sealed (logged).
async fn sealed_connections(
    state: &Arc<AppState>,
    library: &bookclerk_library::LibraryStore,
    enrolled: &EnrolledNode,
    box_public_key: &[u8],
) -> Option<String> {
    // Checked on every redeem, a replay included: a user disabled since
    // the join sends nothing more.
    match library.get_user(enrolled.owner_user_id).await {
        Ok(Some(user)) if user.status == UserStatus::Active => {}
        Ok(_) => return None,
        Err(err) => {
            tracing::warn!(error = %err, "branch enrollment could not read the joining user");
            return None;
        }
    }
    // Also checked on every redeem: a branch revoked since the join (or
    // whose credential is gone) is no longer trusted with secrets.
    match load_node_credential(library, &enrolled.node_id).await {
        Ok(Some(credential)) if credential.revoked_at.is_none() => {}
        Ok(_) => return None,
        Err(err) => {
            tracing::warn!(error = %err, "branch enrollment could not read the branch's credential");
            return None;
        }
    }
    let shareable = shareable_plugin_keys(state, library).await;
    let is_shareable = move |key: &str| shareable.contains(key);
    let accounts = match accounts_to_carry(library, enrolled.owner_user_id, &is_shareable).await {
        Ok(accounts) if accounts.is_empty() => return None,
        Ok(accounts) => accounts,
        Err(err) => {
            tracing::warn!(error = %err, "branch enrollment could not gather the store connections");
            return None;
        }
    };
    let bundle = AccountBundle {
        v: ACCOUNT_BUNDLE_VERSION,
        cluster_id: enrolled.cluster_id.clone(),
        node_id: enrolled.node_id.clone(),
        accounts,
    };
    let ephemeral = {
        use rand::RngCore as _;
        let mut raw = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(raw.as_mut_slice());
        raw
    };
    let sealed = match seal_account_bundle(box_public_key, &ephemeral, &bundle) {
        Ok(sealed) => sealed,
        Err(err) => {
            tracing::warn!(error = %err, "branch enrollment could not seal the store connections");
            return None;
        }
    };
    // Audited on every send, a retried redeem included, since a retry
    // gathers the accounts again. Nothing is sent without its record.
    let detail = json!({
        "node_id": enrolled.node_id,
        "accounts": bundle.accounts.len(),
        "replayed": enrolled.replayed,
    })
    .to_string();
    if let Err(err) = library
        .insert_security_audit_event(
            &format!("user:{}", enrolled.owner_user_id),
            "branch_connections_sent",
            Some(&detail),
        )
        .await
    {
        tracing::warn!(error = %err, "branch enrollment could not audit the connections; none sent");
        return None;
    }
    // Nor without the record of where each account went.
    if let Err(err) = record_accounts_sent(library, &enrolled.node_id, &bundle.accounts).await {
        tracing::warn!(error = %err, "branch enrollment could not record the connections; none sent");
        return None;
    }
    tracing::info!(
        node_id = %enrolled.node_id,
        accounts = bundle.accounts.len(),
        "store connections sealed to a joining branch for its owner"
    );
    Some(sealed)
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
            accounts: None,
        })
        .into_response(),
    )
}

/// Marks the branch name kept in a code's stored scope.
const CONSENTED_NAME_MARK: &str = " name:";

/// Ends a stored scope whose user chose "Bring my store connections to
/// this branch".
const BRING_CONNECTIONS_MARK: &str = " bring:connections";

/// The scope stored with a sign-in's authorization code: the connection
/// type's granted scope ([`BRANCH_SCOPE`] for a branch,
/// [`bookclerk_node_proto::CLERK_GRANTED_SCOPE`] for a clerk), then the
/// branch name the user consented to (base64url), then
/// [`BRING_CONNECTIONS_MARK`] when the user chose to bring their store
/// connections (a branch only). The redeem enrolls the node under that
/// name, not one the redeem body picks, and only as that type.
fn consented_scope(connection: ConnectionType, branch_name: &str, bring: bool) -> String {
    use base64::Engine as _;
    format!(
        "{}{CONSENTED_NAME_MARK}{}{}",
        connection.granted_scope(),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(branch_name.as_bytes()),
        if bring && connection == ConnectionType::Branch {
            BRING_CONNECTIONS_MARK
        } else {
            ""
        }
    )
}

/// What a user consented to, read back from a stored scope.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Consented {
    /// The branch name.
    name: String,
    /// The user chose to bring their store connections.
    bring_connections: bool,
}

/// What [`consented_scope`] wrote for `connection`; `None` for any other
/// scope (a code approved for the other connection type among them) or a
/// name that is not a valid branch name.
fn consented_branch(scope: &str, connection: ConnectionType) -> Option<Consented> {
    use base64::Engine as _;
    // The granted scopes contain no `name:` token and base64url has no
    // space, so a branch scope never parses as a clerk one or back.
    let rest = scope
        .strip_prefix(connection.granted_scope())?
        .strip_prefix(CONSENTED_NAME_MARK)?;
    let (encoded, bring_connections) = match rest.strip_suffix(BRING_CONNECTIONS_MARK) {
        Some(encoded) if connection == ConnectionType::Branch => (encoded, true),
        Some(_) => return None,
        None => (rest, false),
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    let name = String::from_utf8(bytes).ok()?;
    is_branch_name(&name).then_some(Consented {
        name,
        bring_connections,
    })
}

#[cfg(test)]
mod scope_tests {
    use super::{consented_branch, consented_scope, ConnectionType, Consented};

    /// The consented name, when the scope parses for `connection`.
    fn name(scope: &str, connection: ConnectionType) -> Option<String> {
        consented_branch(scope, connection).map(|consented| consented.name)
    }

    #[test]
    fn the_scope_carries_the_type_and_the_consented_name() {
        for connection in [ConnectionType::Branch, ConnectionType::Clerk] {
            let scope = consented_scope(connection, "Den PC", false);
            assert_eq!(name(&scope, connection).as_deref(), Some("Den PC"));
        }
        let branch = consented_scope(ConnectionType::Branch, "Den PC", false);
        let clerk = consented_scope(ConnectionType::Clerk, "Den PC", false);
        assert_eq!(branch, "node:enroll name:RGVuIFBD");
        assert_eq!(clerk, "node:enroll node:clerk name:RGVuIFBD");
        // A branch-approved code never redeems as a clerk, nor the reverse.
        assert_eq!(name(&branch, ConnectionType::Clerk), None);
        assert_eq!(name(&clerk, ConnectionType::Branch), None);
        // The bare scopes (no consented name) and an invalid name refuse.
        assert_eq!(name("node:enroll", ConnectionType::Branch), None);
        assert_eq!(name("node:enroll node:clerk", ConnectionType::Clerk), None);
        assert_eq!(
            name(
                &consented_scope(ConnectionType::Clerk, "", false),
                ConnectionType::Clerk
            ),
            None
        );
    }

    #[test]
    fn only_a_branch_scope_carries_the_choice_to_bring_connections() {
        let branch = consented_scope(ConnectionType::Branch, "Den PC", true);
        assert_eq!(branch, "node:enroll name:RGVuIFBD bring:connections");
        assert_eq!(
            consented_branch(&branch, ConnectionType::Branch),
            Some(Consented {
                name: "Den PC".into(),
                bring_connections: true,
            })
        );
        // A clerk shares the library's database: nothing to bring.
        let clerk = consented_scope(ConnectionType::Clerk, "Den PC", true);
        assert_eq!(clerk, "node:enroll node:clerk name:RGVuIFBD");
        assert_eq!(
            consented_branch(
                "node:enroll node:clerk name:RGVuIFBD bring:connections",
                ConnectionType::Clerk
            ),
            None
        );
        assert_eq!(
            consented_branch(
                "node:enroll name:RGVuIFBD bring:other",
                ConnectionType::Branch
            ),
            None
        );
    }
}

/// The authorization request a consent is for (the query `/oidc/authorize`
/// forwarded to `/join/approve`).
#[derive(Debug, Clone, Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
pub struct BranchAuthorizeParams {
    /// Must be [`BRANCH_CLIENT_ID`].
    #[schema(example = "bookclerk-branch")]
    pub client_id: String,
    /// Where the code goes: loopback or https.
    #[schema(example = "http://127.0.0.1:41923/enroll/callback")]
    pub redirect_uri: String,
    /// Must be `code`.
    #[schema(example = "code")]
    pub response_type: String,
    /// Must be [`BRANCH_SCOPE`].
    #[serde(default)]
    #[schema(example = "node:enroll")]
    pub scope: Option<String>,
    /// Echoed on the redirect.
    #[serde(default)]
    #[schema(example = "c3RhdGUtOGY2ZTJkMWE")]
    pub state: Option<String>,
    /// PKCE S256 challenge.
    #[schema(example = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")]
    pub code_challenge: String,
    /// Must be `S256`.
    #[serde(default)]
    #[schema(example = "S256")]
    pub code_challenge_method: Option<String>,
    /// The name the branch asks to join as.
    #[serde(default)]
    #[schema(example = "study-mini")]
    pub branch_name: Option<String>,
    /// `1` when the joining install offers to join as a clerk
    /// ([`bookclerk_node_proto::CLERK_OFFER_PARAM`]).
    #[serde(default)]
    #[schema(example = "1")]
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
pub(super) fn is_challenge(raw: &str) -> bool {
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
pub(super) fn redirect_with(redirect_uri: &str, pairs: &[(&str, &str)]) -> Option<String> {
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
pub(super) fn consent_error(status: StatusCode, error: &str, detail: &str) -> Response {
    (
        status,
        Json(crate::http_error::ErrorDetail::new(error, detail)),
    )
        .into_response()
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /api/auth/branch-consent`: what the consent page shows.
pub struct BranchConsentPage {
    /// This library's name.
    #[schema(example = "books.example.com")]
    pub library_name: String,
    /// The name the branch asks to join as.
    #[schema(example = "Living-room NAS")]
    pub branch_name: String,
    /// Origin of the branch's redirect URI, for the page to name.
    #[schema(example = "http://127.0.0.1:8787")]
    pub redirect_origin: String,
    /// Whether this library lets new branches join right now.
    #[schema(example = true)]
    pub enrollment: bool,
    /// The signed-in user the branch would belong to.
    pub user: ConsentingUser,
    /// The connection types the page may offer.
    pub connection: ConnectionChoice,
    /// The user's store connections here, and whether each would travel
    /// to the branch if the user chooses to bring them.
    pub connections: Vec<ConsentConnection>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One of the consenting user's store connections, as the consent page
/// lists it.
pub struct ConsentConnection {
    /// Display alias of the storefront.
    #[schema(example = "libro")]
    pub source: String,
    /// Store account id.
    #[schema(example = "sam@example.com")]
    pub account_id: String,
    /// The account's display label, when it has one.
    #[schema(example = "Sam")]
    pub label: Option<String>,
    /// True when the storefront lets its login be used in more than one
    /// place, so the connection travels; false when it keeps one login
    /// that only this library refreshes.
    #[schema(example = true)]
    pub travels: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// The user a branch consent is for.
pub struct ConsentingUser {
    /// `users.id`.
    #[schema(example = 7)]
    pub id: i64,
    /// Display name, when set.
    #[schema(example = "Sam")]
    pub display_name: Option<String>,
}

/// What the consent page may offer: branch only, or a choice between
/// branch and clerk for an Owner.
///
/// The schema is written by hand as an `anyOf`: an Owner's answer also
/// satisfies [`BranchOnlyChoice`] (that schema admits extra properties),
/// so the derived `oneOf` would reject it.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ConnectionChoice {
    /// Only a branch (the user is not an Owner).
    BranchOnly(BranchOnlyChoice),
    /// An Owner may pick clerk, when it is available.
    Owner(OwnerChoice),
}

impl utoipa::PartialSchema for ConnectionChoice {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{AnyOfBuilder, RefBuilder};
        AnyOfBuilder::new()
            .item(
                RefBuilder::new()
                    .ref_location_from_schema_name("BranchOnlyChoice")
                    .description(Some("Only a branch (the user is not an Owner).")),
            )
            .item(
                RefBuilder::new()
                    .ref_location_from_schema_name("OwnerChoice")
                    .description(Some("An Owner may pick clerk, when it is available.")),
            )
            .description(Some(
                "What the consent page may offer: branch only, or a choice between branch and clerk for an Owner.",
            ))
            .into()
    }
}

impl utoipa::ToSchema for ConnectionChoice {
    fn schemas(
        schemas: &mut Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) {
        use utoipa::PartialSchema;
        schemas.push(("BranchOnlyChoice".to_string(), BranchOnlyChoice::schema()));
        schemas.push(("OwnerChoice".to_string(), OwnerChoice::schema()));
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// The consent page's connection block when only a branch is offered.
pub struct BranchOnlyChoice {
    /// Always `false`.
    #[schema(example = false)]
    pub choice: bool,
    /// Always `branch`.
    #[schema(example = "branch")]
    pub default: &'static str,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// The consent page's connection block for an Owner, who may pick clerk.
pub struct OwnerChoice {
    /// Always `true`.
    #[schema(example = true)]
    pub choice: bool,
    /// Always `branch`.
    #[schema(example = "branch")]
    pub default: &'static str,
    /// Whether clerk may be chosen right now.
    #[schema(example = false)]
    pub clerk_available: bool,
    /// Why clerk is shown disabled, when it is.
    #[schema(
        example = "The joining server already holds library data of its own (or did not offer it), so it can only join as a branch."
    )]
    pub clerk_reason: Option<String>,
    /// The second factors the Owner can step up with (`totp`, `passkey`).
    #[schema(example = json!(["passkey"]))]
    pub step_up: Vec<&'static str>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body after a branch consent was answered.
pub struct ConsentRedirect {
    /// Where to send the browser: the branch's callback with a code, or with `error=access_denied`.
    #[schema(example = "http://127.0.0.1:8787/api/config/join/callback?code=8f2c…&state=xyz")]
    pub redirect: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /api/config/branches`.
pub struct BranchesResponse {
    /// This library's name.
    #[schema(example = "books.example.com")]
    pub library_name: String,
    /// The canonical origin branches connect to.
    #[schema(example = "https://books.example.com")]
    pub origin: String,
    /// The node API transport mode (`acme`, `proxy`, `self_signed`, `loopback`).
    #[schema(example = "self_signed")]
    pub transport: String,
    /// Whether new branches may join.
    #[schema(example = true)]
    pub enrollment: bool,
    /// In `self_signed` mode, the fingerprint a joining operator confirms.
    #[schema(example = "kQ3nUj9Z2vY4xW8pT6rS1aB5cD7eF0gH2iJ4kL6mN8o")]
    pub spki_fingerprint: Option<String>,
    /// Every branch, newest first.
    pub branches: Vec<BranchView>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One branch (host node) of this library.
pub struct BranchView {
    /// The branch's node id.
    #[schema(example = "7d2b9e4c-1f3a-4b8d-9c6e-2a5f8b1d4e7c")]
    pub node_id: String,
    /// RFC 3339 time the node was created.
    #[schema(example = "2026-10-29T09:02:41Z")]
    pub created_at: String,
    /// The user who owns the branch.
    #[schema(example = 7)]
    pub owner_user_id: Option<i64>,
    /// That user's display or login name.
    #[schema(example = "Sam")]
    pub owner_name: Option<String>,
    /// Whether its credential is still valid (it has not left or been revoked).
    #[schema(example = true)]
    pub joined: bool,
    /// "Share this branch's books" as it last reported it; `null` until its first catalog sync.
    #[schema(example = true)]
    pub share_books: Option<bool>,
    /// "Include books from my own branches" as it last reported it; `null` until its first catalog sync.
    #[schema(example = true)]
    pub include_branches: Option<bool>,
    /// Counts of the titles it shared.
    pub titles: bookclerk_library::control_plane::BranchTitleCounts,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body after detached branch titles were removed.
pub struct BranchTitlesRemoved {
    /// How many title entries were removed.
    #[schema(example = 3)]
    pub removed: u64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body after `PUT /api/config/branches`.
pub struct BranchJoinSetting {
    /// "Let new branches join", as stored.
    #[schema(example = false)]
    pub enrollment: bool,
}

/// The signed-in user a consent is for: never the operator, who owns no
/// node.
pub(super) async fn consenting_user(
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
#[utoipa::path(
    get,
    path = "/api/auth/branch-consent",
    params(BranchAuthorizeParams),
    security(("portalSession" = []), ("operatorSession" = [])),
    responses(
        (status = 200, description = "What the consent page shows.", body = BranchConsentPage),
        (status = 400, description = "The authorize parameters are invalid (`invalid_request`).", body = crate::http_error::ErrorDetail, examples(
            ("unknown_client" = (summary = "The client id is not the branch client.", value = json!({"error": "invalid_request", "detail": "unknown client"}))),
            ("code_challenge" = (summary = "No S256 code challenge.", value = json!({"error": "invalid_request", "detail": "an S256 code challenge is required"}))),
            ("redirect_uri" = (summary = "The redirect URI is not loopback or https.", value = json!({"error": "invalid_request", "detail": "the redirect URI must be a loopback or https URL"}))),
            ("branch_name" = (summary = "The branch name is empty or too long.", value = json!({"error": "invalid_request", "detail": "the branch needs a name of 1 to 64 characters"}))),
        )),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is not a library user (`user_required`; an operator session counts only while impersonating), the account is inactive (`user_inactive`), the session has no user account (the standard error body), or the user must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorAnswer, examples(
            ("user_required" = (summary = "The session is not a library user.", value = json!({"error": "user_required", "detail": "a branch belongs to a user: sign in with your account on this library"}))),
            ("user_inactive" = (summary = "The account is not active.", value = json!({"error": "user_inactive", "detail": "this account is not active"}))),
            ("no_user" = (summary = "The session has no user account.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The user must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
        (status = 503, description = "The library could not be read; try again (`unavailable`), or this library's cluster row or origin is not available yet (`cluster_unavailable`, `origin_unavailable`).", body = crate::http_error::ErrorAnswer, examples(
            ("unavailable" = (summary = "A read failed; try again.", value = json!({"error": "unavailable", "detail": "try again"}))),
            ("cluster_unavailable" = (summary = "This library's cluster row is not available yet.", value = json!({"error": "cluster_unavailable"}))),
        )),
    ),
)]
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
    let connections = consent_connections(&state, user.id).await;
    Json(BranchConsentPage {
        library_name: info.library_name,
        branch_name: params.branch_name.unwrap_or_default().trim().to_owned(),
        redirect_origin,
        enrollment: info.enrollment,
        user: ConsentingUser {
            id: user.id,
            display_name: user.display_name,
        },
        connection,
        connections,
    })
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
) -> Result<ConnectionChoice, Response> {
    let library = state.library_snapshot().await;
    let owner = verify_cluster_owner(&library, user.id).await.is_ok();
    if !owner {
        return Ok(ConnectionChoice::BranchOnly(BranchOnlyChoice {
            choice: false,
            default: "branch",
        }));
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
    Ok(ConnectionChoice::Owner(OwnerChoice {
        choice: true,
        default: "branch",
        clerk_available: reason.is_none(),
        clerk_reason: reason,
        step_up: methods,
    }))
}

/// A fresh second factor for a clerk consent. `Debug` prints no code.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum StepUp {
    /// A current authenticator code.
    Totp {
        /// Six digits.
        #[schema(example = "492817")]
        code: String,
    },
    /// A user-verified passkey assertion for a challenge from
    /// `POST /api/auth/branch-consent/step-up`.
    Passkey {
        /// The challenge id.
        #[schema(example = "6d2f0c1e-8a4b-4f3d-9e7c-5b1a2c3d4e5f")]
        challenge_id: String,
        /// The browser's `PublicKeyCredential` JSON.
        #[schema(schema_with = crate::passkeys::credential_schema)]
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

/// The store connections `user_id` has here, each marked with whether it
/// would travel to a branch. A read failure lists none, so the page
/// offers nothing to bring rather than refusing the consent.
async fn consent_connections(state: &Arc<AppState>, user_id: i64) -> Vec<ConsentConnection> {
    let library = state.library_snapshot().await;
    let accounts = match member_accounts(&library, user_id).await {
        Ok(accounts) => accounts,
        Err(err) => {
            tracing::warn!(error = %err, "branch consent could not list the user's connections");
            return Vec::new();
        }
    };
    if accounts.is_empty() {
        return Vec::new();
    }
    // The same selection a redeem makes, so a listed connection travels
    // exactly when it would be sent.
    let shareable = shareable_plugin_keys(state, &library).await;
    let is_shareable = move |key: &str| shareable.contains(key);
    let carried: BTreeSet<(String, String)> = accounts_to_carry(&library, user_id, &is_shareable)
        .await
        .map(|carried| {
            carried
                .into_iter()
                .map(|account| (account.plugin_key, account.account_id))
                .collect()
        })
        .unwrap_or_default();
    accounts
        .into_iter()
        .map(|account| ConsentConnection {
            travels: carried.contains(&(account.plugin_key.clone(), account.account_id.clone())),
            source: account.source,
            account_id: account.account_id,
            label: account.label,
        })
        .collect()
}

/// WebAuthn challenge kind of a clerk step-up.
pub(crate) const CLERK_STEP_UP_KIND: &str = "clerk_step_up";

/// `POST /api/auth/branch-consent` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BranchConsentBody {
    /// The authorization request.
    #[serde(flatten)]
    pub params: BranchAuthorizeParams,
    /// True to let the branch join; false to deny.
    #[schema(example = true)]
    pub approve: bool,
    /// `branch` (default) or `clerk`.
    #[serde(default)]
    #[schema(example = "branch")]
    pub connection: Option<String>,
    /// The fresh second factor a clerk needs.
    #[serde(default)]
    pub step_up: Option<StepUp>,
    /// True to send the user's store connections that may travel to the
    /// branch when it joins. Ignored for a clerk. Defaults to false.
    #[serde(default)]
    #[schema(example = false)]
    pub bring_connections: bool,
}

/// `POST /api/auth/branch-consent`: approve or deny. Answers `{redirect}`,
/// the URL to send the browser to: the branch's callback with a code (or
/// `error=access_denied`). Approving needs an active signed-in user, host
/// enrollment on, and a connection the transport policy allows.
#[utoipa::path(
    post,
    path = "/api/auth/branch-consent",
    request_body = BranchConsentBody,
    security(("portalSession" = []), ("operatorSession" = [])),
    responses(
        (status = 200, description = "`{redirect}` back to the branch.", body = ConsentRedirect),
        (status = 400, description = "The body or its authorize parameters are invalid (`invalid_request`), or the joining server did not offer a clerk join (`clerk_not_offered`).", body = crate::http_error::ErrorDetail, examples(
            ("malformed" = (summary = "The body does not parse.", value = json!({"error": "invalid_request", "detail": "malformed consent"}))),
            ("bad_connection" = (summary = "The connection type is not branch or clerk.", value = json!({"error": "invalid_request", "detail": "connection must be branch or clerk"}))),
            ("bad_redirect" = (summary = "The redirect URI cannot carry the answer.", value = json!({"error": "invalid_request", "detail": "bad redirect"}))),
            ("clerk_not_offered" = (summary = "The joining server did not offer to join as a clerk.", value = json!({"error": "clerk_not_offered", "detail": "the joining server did not offer to join as a clerk; it can join as a branch"}))),
            ("unknown_client" = (summary = "The client id is not the branch client.", value = json!({"error": "invalid_request", "detail": "unknown client"}))),
        )),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is not an active library user (`user_required`, `user_inactive`), this library is not accepting branches (`enrollment_disabled`), a clerk join needs an Owner (`owner_required`) with a second factor (`step_up_unavailable`) that was confirmed (`step_up_required`) and passed (`step_up_failed`), the connection failed the transport policy (`transport_insecure`), the session has no user account (the standard error body), or the user must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::NodeRefusal, examples(
            ("user_required" = (summary = "The session is not a library user.", value = json!({"error": "user_required", "detail": "a branch belongs to a user: sign in with your account on this library"}))),
            ("user_inactive" = (summary = "The account is not active.", value = json!({"error": "user_inactive", "detail": "this account is not active"}))),
            ("enrollment_disabled" = (summary = "This library has host enrollment switched off.", value = json!({"error": "enrollment_disabled", "detail": "this library isn't accepting new branches"}))),
            ("owner_required" = (summary = "Only an Owner can let a clerk join.", value = json!({"error": "owner_required", "detail": "only an Owner of this library can let a server join as a clerk"}))),
            ("step_up_unavailable" = (summary = "The Owner has no second factor to confirm with.", value = json!({"error": "step_up_unavailable", "detail": "joining as a clerk needs a fresh second factor: set up an authenticator app or a passkey on your account first"}))),
            ("step_up_required" = (summary = "The clerk join was not confirmed with a second factor.", value = json!({"error": "step_up_required", "detail": "confirm with your authenticator app or a passkey to let a clerk join"}))),
            ("transport_insecure" = (summary = "Plain HTTP from another machine.", value = json!({"error": "transport_insecure", "reason": "plain_http"}))),
            ("no_user" = (summary = "The session has no user account.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The user must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
        (status = 404, description = "The node API is not mounted on this listener (`node_api_unmounted`).", body = crate::http_error::ErrorMessage, example = json!({"error": "node_api_unmounted"})),
        (status = 409, description = "This library's database cannot be shared with a clerk (`database_not_shareable`).", body = crate::http_error::ErrorDetail, example = json!({"error": "database_not_shareable", "detail": "This library keeps its database in a SQLite file that only this server can open. A clerk needs a PostgreSQL or Cloudflare D1 database."})),
        (status = 413, description = "The body is larger than 8 KiB.", body = crate::http_error::ErrorMessage, example = json!({"error": "request too large"})),
        (status = 415, description = "The request is not `application/json`.", body = crate::http_error::ErrorMessage, example = json!({"error": "expected application/json"})),
        (status = 429, description = "Too many wrong step-up codes; wait a few minutes (`step_up_locked`).", body = crate::http_error::ErrorDetail, example = json!({"error": "step_up_locked", "detail": "too many wrong codes; wait a few minutes"})),
        (status = 503, description = "The library could not be read; try again (`unavailable`), or this library's cluster row or origin is not available yet (`cluster_unavailable`, `origin_unavailable`).", body = crate::http_error::ErrorAnswer, examples(
            ("unavailable" = (summary = "A read failed; try again.", value = json!({"error": "unavailable", "detail": "try again"}))),
            ("cluster_unavailable" = (summary = "This library's cluster row is not available yet.", value = json!({"error": "cluster_unavailable"}))),
        )),
    ),
)]
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
            Some(redirect) => no_store(Json(ConsentRedirect { redirect }).into_response()),
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
                body.bring_connections,
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
        Some(redirect) => no_store(Json(ConsentRedirect { redirect }).into_response()),
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
#[utoipa::path(
    post,
    path = "/api/auth/branch-consent/step-up",
    security(("portalSession" = []), ("operatorSession" = [])),
    responses(
        (status = 200, description = "Passkey request options.", body = crate::passkeys::PasskeyRequestOptions),
        (status = 400, description = "This server's public origin is not a URL WebAuthn can use, so the passkey check could not start (`unavailable`).", body = crate::http_error::ErrorDetail, example = json!({"error": "unavailable", "detail": "could not start the passkey check"})),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is not an active library user (`user_required`, `user_inactive`), not an Owner (`owner_required`), the session has no user account (the standard error body), or the user must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorAnswer, examples(
            ("user_required" = (summary = "The session is not a library user.", value = json!({"error": "user_required", "detail": "a branch belongs to a user: sign in with your account on this library"}))),
            ("user_inactive" = (summary = "The account is not active.", value = json!({"error": "user_inactive", "detail": "this account is not active"}))),
            ("owner_required" = (summary = "Only an Owner can let a clerk join.", value = json!({"error": "owner_required", "detail": "only an Owner of this library can let a server join as a clerk"}))),
            ("no_user" = (summary = "The session has no user account.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The user must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
        (status = 404, description = "The account has no passkey (`no_passkey`).", body = crate::http_error::ErrorDetail, example = json!({"error": "no_passkey", "detail": "this account has no passkey"})),
        (status = 500, description = "The passkeys could not be read, or the challenge could not be created or stored (`unavailable`).", body = crate::http_error::ErrorDetail, example = json!({"error": "unavailable", "detail": "could not start the passkey check"})),
        (status = 503, description = "The user record could not be read (`unavailable`); try again.", body = crate::http_error::ErrorDetail, example = json!({"error": "unavailable", "detail": "try again"})),
    ),
)]
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
                (
                    StatusCode::FORBIDDEN,
                    Json(crate::http_error::ErrorMessage::new("forbidden")),
                )
                    .into_response()
            })
        }
    }
}

/// `GET /api/config/branches`: this library's join settings: whether new
/// branches may join, the transport mode, and the SPKI fingerprint a
/// joining operator confirms in `self_signed` mode.
#[utoipa::path(
    get,
    path = "/api/config/branches",
    responses(
        (status = 200, description = "The library's join settings and branches.", body = BranchesResponse),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The user is not an active Owner (`forbidden`), the session has no user account (the standard error body), or the user must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorAnswer, examples(
            ("forbidden" = (summary = "The user is not an active Owner.", value = json!({"error": "forbidden"}))),
            ("no_user" = (summary = "The session has no user account.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The user must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
        (status = 500, description = "The library database could not be read.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
        (status = 503, description = "This library's cluster row (`cluster_unavailable`) or origin (`origin_unavailable`) is not available yet.", body = crate::http_error::ErrorMessage, examples(
            ("cluster_unavailable" = (summary = "The cluster row could not be read.", value = json!({"error": "cluster_unavailable"}))),
            ("origin_unavailable" = (summary = "The library's origin is not known yet.", value = json!({"error": "origin_unavailable"}))),
        )),
    ),
)]
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
    Json(BranchesResponse {
        library_name: info.library_name,
        origin: info.origin,
        transport: info.transport,
        enrollment: info.enrollment,
        spki_fingerprint: info.spki_fingerprint,
        branches,
    })
    .into_response()
}

/// Every branch (host node) of this library, newest first: its owner,
/// whether it is still joined, its two sharing settings as it last
/// reported them (`null` until its first catalog sync), and counts of the
/// titles it shared.
async fn branches_json(
    library: &bookclerk_library::LibraryStore,
) -> bookclerk_library::Result<Vec<BranchView>> {
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
        out.push(BranchView {
            node_id: node.node_id,
            created_at: node.created_at,
            owner_user_id: node.owner_user_id,
            owner_name: owner.and_then(|user| user.display_name.or(user.login_name)),
            joined,
            share_books: node.share_books,
            include_branches: node.include_branches,
            titles,
        });
    }
    Ok(out)
}

/// `POST /api/config/branches/titles/remove` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveBranchTitles {
    /// Only this branch's entries; every branch's when absent.
    #[serde(default)]
    #[schema(example = "2b7e1c4d-9f3a-4e8b-a6c5-0d1e2f3a4b5c")]
    pub node_id: Option<String>,
}

/// `POST /api/config/branches/titles/remove`: removes the entries of
/// titles from a branch that stopped sharing or left. Delivered files stay.
#[utoipa::path(
    post,
    path = "/api/config/branches/titles/remove",
    request_body = RemoveBranchTitles,
    responses(
        (status = 200, description = "The branch's titles were removed.", body = BranchTitlesRemoved),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The user is not an active Owner (`forbidden`), the session has no user account (the standard error body), or the user must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorAnswer, examples(
            ("forbidden" = (summary = "The user is not an active Owner.", value = json!({"error": "forbidden"}))),
            ("no_user" = (summary = "The session has no user account.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The user must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
        (status = 500, description = "The library database could not be read or written.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
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
            Json(BranchTitlesRemoved { removed }).into_response()
        }
        Err(err) => crate::config_authority::events_error(err),
    }
}

/// `PUT /api/config/branches` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutBranches {
    /// "Let new branches join" (`core.host_enrollment.enabled`).
    #[schema(example = true)]
    pub enrollment: bool,
}

/// `PUT /api/config/branches`: turn "Let new branches join" on or off.
#[utoipa::path(
    put,
    path = "/api/config/branches",
    request_body = PutBranches,
    responses(
        (status = 200, description = "The settings after the change.", body = BranchJoinSetting),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The user is not an active Owner (`forbidden`), the session has no user account (the standard error body), or the user must enroll a second factor first (`mfa_enrollment_required`).", body = crate::http_error::ErrorAnswer, examples(
            ("forbidden" = (summary = "The user is not an active Owner.", value = json!({"error": "forbidden"}))),
            ("no_user" = (summary = "The session has no user account.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
            ("mfa_enrollment_required" = (summary = "The user must enroll a second factor first.", value = json!({"error": "mfa_enrollment_required", "message": "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.", "status": 403}))),
        )),
        (status = 409, description = "The setting kept changing under the write; retry.", body = crate::http_error::ErrorMessage, example = json!({"error": "core.host_enrollment kept changing; retry"})),
        (status = 500, description = "The library database could not be read or written.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
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
        Ok(doc) => Json(BranchJoinSetting {
            enrollment: doc.body.enabled,
        })
        .into_response(),
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
