//! Axum routes for portal claim / account linking (`/api/portal`).

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use utoipa_axum::{router::OpenApiRouter, routes};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use bookclerk_config::{session_cookie_flags, Config};
use bookclerk_library::{
    classify_session_client, hash_password, hash_token, parse_claim_redeem_nonce, LibraryStore,
};
use bookclerk_source::{ContentSource, LoginOptions, PortalAuthMode, SourceRegistry};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::brands::{integration_brand, Brand};

use crate::registry::IntegrationRegistry;
use crate::tickets::{
    identity_from_session, inspect_claim_ticket, mint_claim_ticket,
    redeem_ticket_to_session_with_client, session_for_identity,
};
use crate::types::ExternalUser;

/// Cookie name for the portal session token shared by `/api/portal` and the SPA.
const SESSION_COOKIE: &str = "bookclerk_portal_session";

/// Drop the next N successful redeem HTTP responses after the DB commit.
///
/// Production stays at 0. Tests inject a committed-but-lost first reply so the
/// retry can recover the receipt through the real handler.
pub fn redeem_lose_next_responses(n: i32) {
    REDEEM_LOSE_HTTP_RESPONSES.store(n, Ordering::SeqCst);
}

/// Remaining successful redeem replies to drop after the ticket is committed (tests only).
static REDEEM_LOSE_HTTP_RESPONSES: AtomicI32 = AtomicI32::new(0);

/// Shared state for portal handlers.
#[derive(Clone)]
pub struct PortalState {
    /// Loaded Bookclerk configuration shared with portal handlers.
    pub config: Arc<RwLock<Config>>,
    /// Open library store used for identity / ticket / session rows.
    pub library: Arc<RwLock<LibraryStore>>,
    /// Configured outbound integration registry.
    pub integrations: Arc<RwLock<IntegrationRegistry>>,
    /// Content-source registry used for portal store connect / OAuth.
    pub sources: Arc<RwLock<SourceRegistry>>,
}

impl PortalState {
    /// Clones the current library store so a handler can drop the state lock.
    async fn library_snapshot(&self) -> LibraryStore {
        self.library.read().await.clone()
    }
}

/// The SPA Accounts / claim routes, before state: the daemon nests them at
/// `/api/portal` and lists them in its OpenAPI document.
///
/// # Returns
///
/// The routes, relative to their mount point.
// The legacy aliases are deprecated in the document and still served.
#[allow(deprecated)]
pub fn portal_spa_routes() -> OpenApiRouter<PortalState> {
    OpenApiRouter::new()
        .routes(routes!(redeem))
        .routes(routes!(login_integration))
        .routes(routes!(logout))
        .routes(routes!(me))
        .routes(routes!(sources))
        .routes(routes!(source_password_login))
        .routes(routes!(source_oauth_start))
        // Legacy aliases kept for older SPA / smoke clients.
        .routes(routes!(libro_login_legacy))
        .routes(routes!(audible_start))
        .routes(routes!(connections))
        .routes(routes!(revoke_connection))
}

/// The SPA Accounts / claim routes with their state, ready to nest.
///
/// # Arguments
///
/// * `state` - Shared portal state.
pub fn portal_spa_router(state: PortalState) -> Router {
    portal_spa_routes().split_for_parts().0.with_state(state)
}

/// Resolve a portal identity from the request cookie, if present and valid.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `headers` - Incoming HTTP headers (cookie lookup).
///
/// # Returns
///
/// `Some(...)` when found / applicable; otherwise `None`.
pub async fn portal_identity_from_headers(
    library: &LibraryStore,
    headers: &HeaderMap,
) -> Option<bookclerk_library::PortalIdentity> {
    let raw = cookie_value(headers, SESSION_COOKIE)?;
    identity_from_session(library, &raw).await.ok().flatten()
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// JSON body for `POST /redeem` (claim ticket plus retry nonce).
struct RedeemBody {
    /// Raw claim-ticket token to redeem into a portal session.
    ticket: String,
    /// Browser-generated one-time nonce persisted across HTTP retries.
    nonce: String,
    /// Required when the linked local user has no password (invite / reset).
    #[serde(default)]
    password: Option<String>,
}

/// Redeems a claim ticket, optionally setting a first password, and sets the session cookie.
#[utoipa::path(
    operation_id = "portal_redeem",
    post,
    path = "/redeem",
    request_body = RedeemBody,
    responses(
        (status = 200, description = "Signed in; the response sets the portal session cookie.", body = serde_json::Value),
        (status = 400, description = "The ticket is invalid, used or expired.", body = PortalErrorBody),
    ),
)]
async fn redeem(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Json(body): Json<RedeemBody>,
) -> Result<Response, PortalError> {
    let nonce = parse_claim_redeem_nonce(&body.nonce).map_err(PortalError::from)?;
    let cfg = state.config.read().await;
    let library = state.library_snapshot().await;
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let client = classify_session_client(ua, ua.is_none());
    let raw_ticket = body.ticket.trim();
    // Peek first so a missing/too-short invite password does not burn the ticket.
    // Already-redeemed tickets are allowed through so a lost HTTP reply can
    // recover the committed receipt; unredeemed expired tickets still fail.
    let inspected = inspect_claim_ticket(&library, raw_ticket).await?;
    let identity = &inspected.identity;
    if identity.provider != "local"
        && state
            .integrations
            .read()
            .await
            .get(&identity.provider)
            .is_none()
    {
        return Err(PortalError::bad(format!(
            "integration `{}` is not deployed on this host",
            identity.provider
        )));
    }

    let password_plain = body
        .password
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut password_hash = None;
    let mut password_fingerprint = None;
    if identity.provider == "local" {
        if let Some(user_id) = identity.user_id {
            let existing = library
                .get_user_password_hash(user_id)
                .await
                .map_err(|e| PortalError::bad(e.to_string()))?;
            if existing.is_none() && !inspected.redeemed {
                let password = password_plain.ok_or_else(|| {
                    PortalError::bad("password required — set a password to finish claim login")
                })?;
                if password.len() < 8 {
                    return Err(PortalError::bad("password must be at least 8 characters"));
                }
                password_hash =
                    Some(hash_password(password).map_err(|e| PortalError::bad(e.to_string()))?);
            }
        }
    }
    if let Some(password) = password_plain {
        let dek = bookclerk_library::require_master_key(None)?;
        password_fingerprint = Some(bookclerk_library::derive_claim_password_fingerprint(
            &dek, nonce, password,
        ));
    }
    let integrations = cfg.integrations.clone();
    drop(cfg);

    let (session, identity) = redeem_ticket_to_session_with_client(
        &library,
        &integrations,
        raw_ticket,
        nonce,
        Some(&client),
        password_hash.as_deref(),
        password_fingerprint.as_deref(),
    )
    .await?;

    // `try_update` is 1.95+ and does not compile on the 1.94 MSRV. `fetch_update`
    // is slated for deprecation (1.99); the allow keeps `-D warnings` green once
    // it lands. Switch to `try_update` when MSRV reaches 1.95.
    #[allow(deprecated)]
    let lose_response = REDEEM_LOSE_HTTP_RESPONSES
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            (v > 0).then_some(v - 1)
        })
        .is_ok();
    if lose_response {
        return Err(PortalError::unavailable(
            "database temporarily unavailable — retry the same redeem",
        ));
    }

    info!(
        identity_id = identity.id,
        provider = %identity.provider,
        "claim ticket redeemed"
    );
    Ok(session_response(session, &state).await)
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// JSON body for `POST /login/integration` (provider credential login).
struct IntegrationLoginBody {
    /// Integration plugin id (must be enabled in config).
    provider: String,
    /// Provider login name forwarded to `authenticate_user`.
    username: String,
    /// Provider password; never logged.
    password: String,
}

/// Authenticates against an enabled integration and mints a portal session.
#[utoipa::path(
    operation_id = "portal_login_integration",
    post,
    path = "/login/integration",
    request_body = IntegrationLoginBody,
    responses(
        (status = 200, description = "Signed in through the integration.", body = serde_json::Value),
        (status = 400, description = "The request is invalid, or the integration refused the login.", body = PortalErrorBody),
    ),
)]
async fn login_integration(
    State(state): State<PortalState>,
    Json(body): Json<IntegrationLoginBody>,
) -> Result<Response, PortalError> {
    if state
        .integrations
        .read()
        .await
        .get(&body.provider)
        .is_none()
    {
        return Err(PortalError::bad(format!(
            "integration `{}` is not deployed on this host",
            body.provider
        )));
    }
    let integration = state
        .integrations
        .read()
        .await
        .get(&body.provider)
        .ok_or_else(|| PortalError::bad("unknown integration provider"))?;
    if !integration.supports_credential_login() {
        return Err(PortalError::bad(format!(
            "integration `{}` does not support credential login",
            body.provider
        )));
    }
    let user = integration
        .authenticate_user(body.username.trim(), &body.password)
        .await
        .map_err(|e| PortalError::bad(e.to_string()))?;
    let identity = {
        let library = state.library_snapshot().await;
        library
            .upsert_portal_identity(
                &user.provider,
                &user.external_user_id,
                user.display_name.as_deref(),
            )
            .await?
    };
    let cfg = state.config.read().await;
    let library = state.library_snapshot().await;
    let session = session_for_identity(&library, &cfg.integrations, &identity).await?;
    drop(cfg);
    Ok(session_response(session, &state).await)
}

/// Revokes the portal session row and clears the session cookie.
#[utoipa::path(
    operation_id = "portal_logout",
    post,
    path = "/logout",
    responses(
        (status = 200, description = "Signed out (`{\"ok\": true}`); the cookie is cleared.", body = serde_json::Value),
    ),
)]
async fn logout(State(state): State<PortalState>, headers: HeaderMap) -> Response {
    if let Some(raw) = cookie_value(&headers, SESSION_COOKIE) {
        let library = state.library_snapshot().await;
        let hash = hash_token(&raw);
        if let Err(err) = library.delete_portal_session(&hash).await {
            warn!(error = %err, "failed to revoke portal session");
        }
    }
    let flags = {
        let cfg = state.config.read().await;
        session_cookie_flags(cfg.integrations.public_origin.as_deref())
    };
    let cookie = format!("{SESSION_COOKIE}=; {flags}; Max-Age=0");
    let mut out = HeaderMap::new();
    if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
        out.append(header::SET_COOKIE, v);
    }
    (StatusCode::OK, out, Json(serde_json::json!({ "ok": true }))).into_response()
}

/// Returns the signed-in portal identity, or 401 when the cookie is missing or expired.
#[utoipa::path(
    operation_id = "portal_me",
    get,
    path = "/me",
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The signed-in identity.", body = MeResponse),
        (status = 401, description = "The caller is not signed in.", body = PortalErrorBody),
    ),
)]
async fn me(
    State(state): State<PortalState>,
    headers: HeaderMap,
) -> Result<Json<MeResponse>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    Ok(Json(MeResponse {
        provider: identity.provider,
        external_user_id: identity.external_user_id,
        label: identity.label,
    }))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /me`.
struct MeResponse {
    /// Identity provider id (`local` or an integration plugin id).
    provider: String,
    /// Provider-scoped user id stored on the portal identity row.
    external_user_id: String,
    /// Optional display label from the identity row.
    label: Option<String>,
}

/// Lists the deployed content sources (one per plugin, through its default
/// instance) with portal auth mode, config options, and brand colors.
#[utoipa::path(
    operation_id = "portal_sources",
    get,
    path = "/sources",
    responses(
        (status = 200, description = "Deployed content sources.", body = SourcesResponse),
    ),
)]
async fn sources(State(state): State<PortalState>) -> Json<SourcesResponse> {
    let sources = state.sources.read().await;
    let mut list = Vec::new();
    for s in sources.routed() {
        let brand = Brand::from(s.portal_brand());
        let config_options: Vec<SourceConfigOptionInfo> = s
            .config_options()
            .iter()
            .map(|opt| SourceConfigOptionInfo {
                key: opt.key.into(),
                label: opt.label.into(),
                values: opt
                    .values
                    .iter()
                    .map(|v| ConfigOptionValueInfo {
                        id: v.id.into(),
                        label: v.label.into(),
                    })
                    .collect(),
            })
            .collect();
        list.push(SourceInfo {
            id: s.id().into(),
            name: s.display_name().into(),
            auth: s.portal_auth_mode().as_str().into(),
            config_options,
            brand: BrandInfo {
                bg: brand.bg.into(),
                fg: brand.fg.into(),
                accent: brand.accent.into(),
                logo: brand.logo_href().into(),
            },
        });
    }
    Json(SourcesResponse { sources: list })
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /sources`.
struct SourcesResponse {
    /// Enabled storefronts the SPA may offer for connect / login.
    sources: Vec<SourceInfo>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One enabled storefront as shown on the portal Accounts page.
struct SourceInfo {
    /// Source plugin id (`audible`, `libro`, …).
    id: String,
    /// Operator-facing storefront name.
    name: String,
    /// Portal auth mode (`password` or `oauth`).
    auth: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Store-specific knobs the SPA can present (omitted when empty).
    config_options: Vec<SourceConfigOptionInfo>,
    /// Storefront colors and logo href for the Accounts UI.
    brand: BrandInfo,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One storefront config option (key, label, allowed values).
struct SourceConfigOptionInfo {
    /// Config table key (for example `container`).
    key: String,
    /// Operator-facing option name.
    label: String,
    /// Allowed values the SPA may offer for this option.
    values: Vec<ConfigOptionValueInfo>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One allowed value for a storefront config option.
struct ConfigOptionValueInfo {
    /// Stored option value (for example `m4b`).
    id: String,
    /// Operator-facing value label.
    label: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// CSS colors and logo href for a storefront or integration brand.
struct BrandInfo {
    /// Background color (`#RRGGBB`).
    bg: String,
    /// Foreground / text color (`#RRGGBB`).
    fg: String,
    /// Accent color (`#RRGGBB`).
    accent: String,
    /// Logo image href (often a favicon URL).
    logo: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// JSON body for password-store connect (`email` + `password`).
struct PasswordLoginBody {
    /// Store account email / username.
    email: String,
    /// Store account password; never logged.
    password: String,
}

/// Connects a password storefront, upserts the account, and links it to the signed-in identity.
#[utoipa::path(
    operation_id = "portal_source_password_login",
    post,
    path = "/sources/{id}/login",
    params(("id" = String, Path, description = "Source plugin id.")),
    request_body = PasswordLoginBody,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The account was connected.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in.", body = PortalErrorBody),
        (status = 400, description = "The login failed.", body = PortalErrorBody),
    ),
)]
async fn source_password_login(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PasswordLoginBody>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let source = find_source(&state, &id)
        .await
        .ok_or_else(|| PortalError::bad("unknown source"))?;
    require_source_enabled(&state, source.id()).await?;
    if source.portal_auth_mode() != PortalAuthMode::Password {
        return Err(PortalError::bad(
            "this source uses OAuth; call /oauth/start instead",
        ));
    }

    let source_id = source.id();
    let plugin_key = source.plugin_key().to_string();
    let library = state.library_snapshot().await;
    let scope = bookclerk_source::source_scope(&library, source.as_ref());
    let account = source
        .login(
            &scope,
            LoginOptions {
                marketplace: "us".into(),
                label: None,
                email: Some(body.email.trim().to_string()),
                password: Some(body.password),
                force: true,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| PortalError::bad(e.to_string()))?;

    scope
        .upsert_account(
            &account.account_id,
            &account.marketplace,
            account.label.as_deref(),
            true,
        )
        .await?;
    library
        .mark_connection_active(&plugin_key, &account.account_id)
        .await?;
    library
        .link_account(identity.id, &plugin_key, &account.account_id, source_id)
        .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "account_id": account.account_id,
        "source": source_id,
        "plugin_key": plugin_key,
    })))
}

/// Starts interactive OAuth for an OAuth storefront and returns the browser login URL.
#[utoipa::path(
    operation_id = "portal_source_oauth_start",
    post,
    path = "/sources/{id}/oauth/start",
    params(("id" = String, Path, description = "Source plugin id.")),
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The browser login URL.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in.", body = PortalErrorBody),
        (status = 400, description = "The source does not use OAuth.", body = PortalErrorBody),
    ),
)]
async fn source_oauth_start(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let source = find_source(&state, &id)
        .await
        .ok_or_else(|| PortalError::bad("unknown source"))?;
    require_source_enabled(&state, source.id()).await?;
    if source.portal_auth_mode() != PortalAuthMode::Oauth {
        return Err(PortalError::bad(
            "this source uses password login; call /login instead",
        ));
    }
    let url = start_source_oauth_session(&state, source, identity.id).await?;
    Ok(Json(serde_json::json!({ "url": url })))
}

/// Legacy `POST /libro/login` alias that forwards to the Libro password-login handler.
#[utoipa::path(
    operation_id = "portal_libro_login_legacy",
    post,
    path = "/libro/login",
    request_body = PasswordLoginBody,
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The account was connected.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in.", body = PortalErrorBody),
    ),
)]
#[deprecated(note = "legacy alias; call the `/sources/{id}/...` route instead")]
async fn libro_login_legacy(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Json(body): Json<PasswordLoginBody>,
) -> Result<Json<serde_json::Value>, PortalError> {
    source_password_login(State(state), headers, Path("libro".into()), Json(body)).await
}

/// Legacy `POST /audible/start` alias that forwards to Audible OAuth start.
#[utoipa::path(
    operation_id = "portal_audible_start",
    post,
    path = "/audible/start",
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The browser login URL.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in.", body = PortalErrorBody),
    ),
)]
#[deprecated(note = "legacy alias; call the `/sources/{id}/...` route instead")]
async fn audible_start(
    State(state): State<PortalState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, PortalError> {
    source_oauth_start(State(state), headers, Path("audible".into())).await
}

/// Start interactive OAuth via [`ContentSource::login_with_oauth_progress`].
///
/// Uses `0.0.0.0:0` so the daemon accepts remote callbacks when published behind
/// a reverse proxy. The printed URL may still show a container-local host; set
/// `integrations.public_origin` and rewrite as needed at the proxy layer.
async fn start_source_oauth_session(
    state: &PortalState,
    source: Arc<dyn ContentSource>,
    identity_id: i64,
) -> Result<String, PortalError> {
    use bookclerk_source::{LoginOptions, OAuthProgress};
    use tokio::sync::mpsc;

    let library = state.library_snapshot().await;
    let source_id = source.id().to_string();
    let plugin_key = source.plugin_key().to_string();
    let (url_tx, mut url_rx) = mpsc::channel::<String>(1);

    tokio::spawn(async move {
        let scope = bookclerk_source::source_scope(&library, source.as_ref());
        let opts = LoginOptions {
            force: true,
            callback_bind: Some("0.0.0.0:0".into()),
            ..Default::default()
        };
        let url_tx2 = url_tx.clone();
        let result = source
            .login_with_oauth_progress(&scope, opts, &move |progress| {
                if let OAuthProgress::LoginUrl { url, .. } = &progress {
                    let _ = url_tx2.try_send(url.clone());
                }
            })
            .await;
        match result {
            Ok(account) => {
                let _ = library
                    .mark_connection_active(&plugin_key, &account.account_id)
                    .await;
                let _ = library
                    .link_account(identity_id, &plugin_key, &account.account_id, &source_id)
                    .await;
                info!(
                    account = %account.account_id,
                    source = %source_id,
                    "portal OAuth login completed"
                );
            }
            Err(err) => warn!(%err, source = %source_id, "portal OAuth login failed"),
        }
    });

    let url = tokio::time::timeout(std::time::Duration::from_secs(5), url_rx.recv())
        .await
        .map_err(|_| PortalError::bad("timed out waiting for OAuth login URL"))?
        .ok_or_else(|| PortalError::bad("OAuth login URL channel closed"))?;
    Ok(url)
}

/// Lists store accounts linked to the signed-in identity, including disabled sources.
#[utoipa::path(
    operation_id = "portal_connections",
    get,
    path = "/connections",
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "Store accounts linked to the caller.", body = ConnectionsResponse),
        (status = 401, description = "The caller is not signed in.", body = PortalErrorBody),
    ),
)]
async fn connections(
    State(state): State<PortalState>,
    headers: HeaderMap,
) -> Result<Json<ConnectionsResponse>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let links = library.list_account_links(identity.id).await?;
    let mut connections = Vec::new();
    for link in links {
        let acct = library
            .get_account(&link.plugin_key, &link.account_id)
            .await?;
        let brand = if let Some(s) = find_source(&state, &link.source).await {
            Some(Brand::from(s.portal_brand()))
        } else {
            state
                .integrations
                .read()
                .await
                .get(&link.source)
                .and_then(|i| i.portal_brand())
                .or_else(|| integration_brand(&link.source))
        };
        // Still list (and allow revoke of) connections even when the source
        // plugin is not deployed here — only new connect/login needs it.
        let source_enabled = find_source(&state, &link.plugin_key).await.is_some();
        connections.push(ConnectionInfo {
            plugin_key: link.plugin_key,
            account_id: link.account_id,
            source: link.source,
            label: acct.as_ref().and_then(|a| a.label.clone()),
            connection_status: acct
                .map(|a| a.connection_status)
                .unwrap_or_else(|| "active".into()),
            source_enabled,
            brand: brand.map(|b| BrandInfo {
                bg: b.bg.into(),
                fg: b.fg.into(),
                accent: b.accent.into(),
                logo: b.logo_href().into(),
            }),
        });
    }
    Ok(Json(ConnectionsResponse { connections }))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /connections`.
struct ConnectionsResponse {
    /// Linked store accounts the SPA may show or revoke.
    connections: Vec<ConnectionInfo>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One linked store account on the portal Accounts page.
struct ConnectionInfo {
    /// Store account id (unique within its source).
    account_id: String,
    /// Source plugin id that owns this account.
    source: String,
    /// Provenance-qualified PluginKey that owns the account.
    plugin_key: String,
    /// Operator-facing account label when the store provided one.
    label: Option<String>,
    /// Connection state (`active`, revoked, …); defaults to `active` if the account row is missing.
    connection_status: String,
    /// Whether `[sources.<id>] enabled` is currently true (new connects blocked when false).
    source_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Storefront or integration brand when a plugin or fallback palette is available.
    brand: Option<BrandInfo>,
}

/// Unlinks a store account from this identity and deletes secrets when no other identity remains.
///
/// The account is addressed by `(source, account_id)`: the same store id may
/// exist under two plugins. `source` is the alias or plugin key of the link.
///
/// When the last link goes, the plugin first signs the account out at the
/// store (for example, Audible deregisters its device), best effort.
#[utoipa::path(
    operation_id = "portal_revoke_connection",
    post,
    path = "/sources/{source}/connections/{account_id}/revoke",
    params(("source" = String, Path, description = "Source plugin id."), ("account_id" = String, Path, description = "Store account id.")),
    security(("portalSession" = [])),
    responses(
        (status = 200, description = "The account was unlinked.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in.", body = PortalErrorBody),
        (status = 400, description = "The account is not linked to this identity.", body = PortalErrorBody),
    ),
)]
async fn revoke_connection(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Path((source, account_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let links = library.list_account_links(identity.id).await?;
    let Some(link) = links.iter().find(|l| {
        l.account_id == account_id
            && (l.plugin_key == source || l.source.eq_ignore_ascii_case(&source))
    }) else {
        return Err(PortalError::bad("account not linked to this identity"));
    };
    let plugin_key = link.plugin_key.clone();
    library
        .unlink_account(identity.id, &plugin_key, &account_id)
        .await?;
    // Exclusive-link invariant: only delete secrets when no other identity
    // still references this account. Propagate count failures — do not
    // treat them as "still linked" (that would skip secret cleanup).
    // Revoking credentials keeps already-acquired library rows (product policy).
    let remaining = library
        .count_account_links_for_account(&plugin_key, &account_id)
        .await?;
    if remaining == 0 {
        // Let the plugin sign the account out at the store (for example,
        // deregister the login device) while its credentials still exist.
        // Best effort: a missing plugin or store error never blocks revoke.
        if let Some(content) = find_source(&state, &plugin_key).await {
            let scope = bookclerk_source::source_scope(&library, content.as_ref());
            if let Err(err) = content.revoke_credentials(&scope, &account_id).await {
                warn!(%account_id, %err, "store sign-out failed on revoke");
            }
        }
        if let Err(err) = library
            .delete_account_secrets(&plugin_key, &account_id)
            .await
        {
            warn!(%account_id, %err, "failed to delete encrypted_secrets on revoke");
        }
        library.revoke_credentials(&plugin_key, &account_id).await?;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Resolves the portal identity from the session cookie, or returns 401.
async fn require_identity(
    state: &PortalState,
    headers: &HeaderMap,
) -> Result<bookclerk_library::PortalIdentity, PortalError> {
    let raw = cookie_value(headers, SESSION_COOKIE)
        .ok_or_else(|| PortalError::unauthorized("not signed in"))?;
    let library = state.library_snapshot().await;
    identity_from_session(&library, &raw)
        .await?
        .ok_or_else(|| PortalError::unauthorized("session expired"))
}

/// Rejects connect/login when no deployment of the source runs on this host.
async fn require_source_enabled(state: &PortalState, id: &str) -> Result<(), PortalError> {
    if find_source(state, id).await.is_none() {
        return Err(PortalError::bad(format!(
            "source `{id}` is not deployed on this host"
        )));
    }
    Ok(())
}

/// Looks up a content source by plugin id or alias.
async fn find_source(state: &PortalState, id_or_alias: &str) -> Option<Arc<dyn ContentSource>> {
    state.sources.read().await.get(id_or_alias)
}

/// Sets the portal session cookie (`Path=/`, TTL from `portal_session_ttl_hours`) and returns `{ ok: true }`.
async fn session_response(session: String, state: &PortalState) -> Response {
    let cfg = state.config.read().await;
    let max_age = cfg.integrations.portal_session_ttl_hours * 3600;
    let flags = session_cookie_flags(cfg.integrations.public_origin.as_deref());
    // Path=/ so the SPA (and /api/portal) can share the session cookie.
    let cookie = format!("{SESSION_COOKIE}={session}; {flags}; Max-Age={max_age}");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// Extracts a named cookie value from the `Cookie` header, if present.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Mint a claim ticket for an external user (daemon watcher / CLI).
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `config` - Loaded Bookclerk configuration.
/// * `user` - External user identity from an integration login/watcher.
/// * `created_by` - Actor string recorded on the claim ticket (`daemon`, CLI user, …).
///
/// # Returns
///
/// On success, `crate::Result<crate::tickets::MintedClaimTicket>`.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn mint_for_external_user(
    library: &LibraryStore,
    config: &Config,
    user: &ExternalUser,
    created_by: &str,
) -> crate::Result<crate::tickets::MintedClaimTicket> {
    let minted = mint_claim_ticket(library, &config.integrations, user, created_by).await?;
    if let Some(url) = crate::tickets::ticket_portal_url(&config.integrations, &minted.token) {
        info!(%url, identity = minted.identity.id, "minted claim ticket");
    } else {
        info!(
            token = %minted.token,
            identity = minted.identity.id,
            "minted claim ticket (set integrations.public_origin to log a URL)"
        );
    }
    Ok(minted)
}

#[derive(Debug)]
/// Handler error mapped to an HTTP status and `{ "error": ... }` JSON body.
struct PortalError {
    /// HTTP status returned to the SPA.
    status: StatusCode,
    /// Operator-facing error text (no structured code).
    message: String,
}

impl PortalError {
    /// Builds a 400 response for invalid input or a disabled/unknown source.
    fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    /// Builds a 401 response for a missing or expired portal session.
    fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg.into(),
        }
    }

    /// Builds a 503 response for a transient store outage (redeem retry path).
    fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
        }
    }
}

impl From<bookclerk_library::LibraryError> for PortalError {
    fn from(value: bookclerk_library::LibraryError) -> Self {
        match value {
            bookclerk_library::LibraryError::Unavailable(msg) => Self::unavailable(msg),
            other => Self::bad(other.to_string()),
        }
    }
}

impl From<crate::error::IntegrationError> for PortalError {
    fn from(value: crate::error::IntegrationError) -> Self {
        match value {
            crate::error::IntegrationError::Library(
                bookclerk_library::LibraryError::Unavailable(msg),
            ) => Self::unavailable(msg),
            other => Self::bad(other.to_string()),
        }
    }
}

/// JSON body of every portal error response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PortalErrorBody {
    /// Operator-facing error text (no structured code).
    pub error: String,
}

impl IntoResponse for PortalError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(PortalErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}
