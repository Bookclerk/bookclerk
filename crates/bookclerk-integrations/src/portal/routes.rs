//! Axum routes for portal claim / account linking (`/api/portal`).

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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

/// SPA-facing portal API. Nest under `/api/portal`.
///
/// # Arguments
///
/// * `state` - Shared portal / handler state.
///
/// # Returns
///
/// `Router` result.
pub fn portal_spa_router(state: PortalState) -> Router {
    Router::new()
        .route("/redeem", post(redeem))
        .route("/login/integration", post(login_integration))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/sources", get(sources))
        .route("/sources/{id}/login", post(source_password_login))
        .route("/sources/{id}/oauth/start", post(source_oauth_start))
        // Legacy aliases kept for older SPA / smoke clients.
        .route("/libro/login", post(libro_login_legacy))
        .route("/audible/start", post(audible_start))
        .route("/connections", get(connections))
        .route("/connections/{account_id}/revoke", post(revoke_connection))
        .with_state(state)
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

#[derive(Debug, Deserialize)]
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
    if identity.provider != "local" && !cfg.integrations.is_enabled(&identity.provider) {
        return Err(PortalError::bad(format!(
            "integration `{}` is disabled",
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

    if REDEEM_LOSE_HTTP_RESPONSES
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            (v > 0).then_some(v - 1)
        })
        .is_ok()
    {
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

#[derive(Debug, Deserialize)]
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
async fn login_integration(
    State(state): State<PortalState>,
    Json(body): Json<IntegrationLoginBody>,
) -> Result<Response, PortalError> {
    {
        let cfg = state.config.read().await;
        if !cfg.integrations.is_enabled(&body.provider) {
            return Err(PortalError::bad(format!(
                "integration `{}` is disabled",
                body.provider
            )));
        }
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

#[derive(Debug, Serialize)]
/// JSON body for `GET /me`.
struct MeResponse {
    /// Identity provider id (`local` or an integration plugin id).
    provider: String,
    /// Provider-scoped user id stored on the portal identity row.
    external_user_id: String,
    /// Optional display label from the identity row.
    label: Option<String>,
}

/// Lists enabled content sources with portal auth mode, config options, and brand colors.
async fn sources(State(state): State<PortalState>) -> Json<SourcesResponse> {
    let cfg = state.config.read().await;
    let sources = state.sources.read().await;
    let mut list = Vec::new();
    for s in sources.all() {
        // Appearance follows `[sources.<id>].enabled` even if a stale registry entry remains.
        if !cfg.sources.is_enabled(s.id()) {
            continue;
        }
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

#[derive(Debug, Serialize)]
/// JSON body for `GET /sources`.
struct SourcesResponse {
    /// Enabled storefronts the SPA may offer for connect / login.
    sources: Vec<SourceInfo>,
}

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize)]
/// One storefront config option (key, label, allowed values).
struct SourceConfigOptionInfo {
    /// Config table key (for example `container`).
    key: String,
    /// Operator-facing option name.
    label: String,
    /// Allowed values the SPA may offer for this option.
    values: Vec<ConfigOptionValueInfo>,
}

#[derive(Debug, Serialize)]
/// One allowed value for a storefront config option.
struct ConfigOptionValueInfo {
    /// Stored option value (for example `m4b`).
    id: String,
    /// Operator-facing value label.
    label: String,
}

#[derive(Debug, Serialize)]
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

#[derive(Debug, Deserialize)]
/// JSON body for password-store connect (`email` + `password`).
struct PasswordLoginBody {
    /// Store account email / username.
    email: String,
    /// Store account password; never logged.
    password: String,
}

/// Connects a password storefront, upserts the account, and links it to the signed-in identity.
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
    let library = state.library_snapshot().await;
    let scope = library.scope(source_id);
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
    library.mark_connection_active(&account.account_id).await?;
    library
        .link_account(identity.id, &account.account_id, source_id)
        .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "account_id": account.account_id,
        "source": source_id,
    })))
}

/// Starts interactive OAuth for an OAuth storefront and returns the browser login URL.
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
async fn libro_login_legacy(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Json(body): Json<PasswordLoginBody>,
) -> Result<Json<serde_json::Value>, PortalError> {
    source_password_login(State(state), headers, Path("libro".into()), Json(body)).await
}

/// Legacy `POST /audible/start` alias that forwards to Audible OAuth start.
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
    let (url_tx, mut url_rx) = mpsc::channel::<String>(1);

    tokio::spawn(async move {
        let scope = library.scope(&source_id);
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
                let _ = library.mark_connection_active(&account.account_id).await;
                let _ = library
                    .link_account(identity_id, &account.account_id, &source_id)
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
async fn connections(
    State(state): State<PortalState>,
    headers: HeaderMap,
) -> Result<Json<ConnectionsResponse>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let cfg = state.config.read().await;
    let library = state.library_snapshot().await;
    let links = library.list_account_links(identity.id).await?;
    let mut connections = Vec::new();
    for link in links {
        let acct = library.get_account(&link.account_id).await?;
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
        // plugin is disabled — only new connect/login is gated by enabled.
        let source_enabled = cfg.sources.is_enabled(&link.source);
        connections.push(ConnectionInfo {
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

#[derive(Debug, Serialize)]
/// JSON body for `GET /connections`.
struct ConnectionsResponse {
    /// Linked store accounts the SPA may show or revoke.
    connections: Vec<ConnectionInfo>,
}

#[derive(Debug, Serialize)]
/// One linked store account on the portal Accounts page.
struct ConnectionInfo {
    /// Store account id (unique within its source).
    account_id: String,
    /// Source plugin id that owns this account.
    source: String,
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
async fn revoke_connection(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Path(account_id): Path<String>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let links = library.list_account_links(identity.id).await?;
    if !links.iter().any(|l| l.account_id == account_id) {
        return Err(PortalError::bad("account not linked to this identity"));
    }
    library.unlink_account(identity.id, &account_id).await?;
    // Exclusive-link invariant: only delete secrets when no other identity
    // still references this account_id. Propagate count failures — do not
    // treat them as "still linked" (that would skip secret cleanup).
    // Revoking credentials keeps already-acquired library rows (product policy).
    let remaining = library.count_account_links_for_account(&account_id).await?;
    if remaining == 0 {
        if let Err(err) = library.delete_account_secrets(&account_id).await {
            warn!(%account_id, %err, "failed to delete encrypted_secrets on revoke");
        }
        library.revoke_credentials(&account_id).await?;
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

/// Rejects connect/login when the source is disabled or not registered.
async fn require_source_enabled(state: &PortalState, id: &str) -> Result<(), PortalError> {
    let cfg = state.config.read().await;
    if !cfg.sources.is_enabled(id) {
        return Err(PortalError::bad(format!("source `{id}` is disabled")));
    }
    // Also require registration (registry builders skip disabled sources).
    if find_source(state, id).await.is_none() {
        return Err(PortalError::bad(format!("source `{id}` is not registered")));
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
    if !config.integrations.is_enabled(&user.provider) {
        return Err(crate::error::IntegrationError::message(format!(
            "integration `{}` is disabled",
            user.provider
        )));
    }
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

impl IntoResponse for PortalError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}
