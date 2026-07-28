//! Axum routes for portal claim / Accounts linking (`/api/portal`).

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bookclerk_config::Config;
use bookclerk_library::LibraryStore;
use bookclerk_source::{ContentSource, LoginOptions, PortalAuthMode};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::brands::{integration_brand, Brand};
use crate::registry::IntegrationRegistry;
use crate::tickets::{
    identity_from_session, mint_claim_ticket, redeem_ticket_to_session, session_for_identity,
};
use crate::types::ExternalUser;

const SESSION_COOKIE: &str = "bookclerk_portal_session";

/// Shared state for portal handlers.
#[derive(Clone)]
pub struct PortalState {
    pub config: Arc<RwLock<Config>>,
    pub library: LibraryStore,
    pub integrations: IntegrationRegistry,
    pub files_dir: PathBuf,
    pub sources: Vec<Arc<dyn ContentSource>>,
}

/// SPA-facing portal API. Nest under `/api/portal`.
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
pub fn portal_identity_from_headers(
    library: &LibraryStore,
    headers: &HeaderMap,
) -> Option<bookclerk_library::PortalIdentity> {
    let raw = cookie_value(headers, SESSION_COOKIE)?;
    identity_from_session(library, &raw).ok().flatten()
}

#[derive(Debug, Deserialize)]
struct RedeemBody {
    ticket: String,
}

async fn redeem(
    State(state): State<PortalState>,
    Json(body): Json<RedeemBody>,
) -> Result<Response, PortalError> {
    let cfg = state.config.read().await;
    let (session, identity) =
        redeem_ticket_to_session(&state.library, &cfg.integrations, body.ticket.trim())?;
    // Defense-in-depth: refuse tickets whose provider integration is disabled.
    if !cfg.integrations.is_enabled(&identity.provider) {
        return Err(PortalError::bad(format!(
            "integration `{}` is disabled",
            identity.provider
        )));
    }
    drop(cfg);
    info!(
        identity_id = identity.id,
        provider = %identity.provider,
        "claim ticket redeemed"
    );
    Ok(session_response(session, &state).await)
}

#[derive(Debug, Deserialize)]
struct IntegrationLoginBody {
    provider: String,
    username: String,
    password: String,
}

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
    let identity = state.library.upsert_portal_identity(
        &user.provider,
        &user.external_user_id,
        user.display_name.as_deref(),
    )?;
    let cfg = state.config.read().await;
    let session = session_for_identity(&state.library, &cfg.integrations, &identity)?;
    drop(cfg);
    Ok(session_response(session, &state).await)
}

async fn logout(State(_state): State<PortalState>) -> Response {
    let cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    let mut headers = HeaderMap::new();
    if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, v);
    }
    (
        StatusCode::OK,
        headers,
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

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
struct MeResponse {
    provider: String,
    external_user_id: String,
    label: Option<String>,
}

async fn sources(State(state): State<PortalState>) -> Json<SourcesResponse> {
    let cfg = state.config.read().await;
    let mut list = Vec::new();
    for s in &state.sources {
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
struct SourcesResponse {
    sources: Vec<SourceInfo>,
}

#[derive(Debug, Serialize)]
struct SourceInfo {
    id: String,
    name: String,
    auth: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    config_options: Vec<SourceConfigOptionInfo>,
    brand: BrandInfo,
}

#[derive(Debug, Serialize)]
struct SourceConfigOptionInfo {
    key: String,
    label: String,
    values: Vec<ConfigOptionValueInfo>,
}

#[derive(Debug, Serialize)]
struct ConfigOptionValueInfo {
    id: String,
    label: String,
}

#[derive(Debug, Serialize)]
struct BrandInfo {
    bg: String,
    fg: String,
    accent: String,
    logo: String,
}

#[derive(Debug, Deserialize)]
struct PasswordLoginBody {
    email: String,
    password: String,
}

async fn source_password_login(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PasswordLoginBody>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let source = find_source(&state, &id).ok_or_else(|| PortalError::bad("unknown source"))?;
    require_source_enabled(&state, source.id()).await?;
    if source.portal_auth_mode() != PortalAuthMode::Password {
        return Err(PortalError::bad(
            "this source uses OAuth; call /oauth/start instead",
        ));
    }

    let account = source
        .login(
            &state.files_dir,
            LoginOptions {
                marketplace: "us".into(),
                label: None,
                email: Some(body.email.trim().to_string()),
                password: Some(body.password),
                force: true,
            },
        )
        .await
        .map_err(|e| PortalError::bad(e.to_string()))?;

    let source_id = source.id();
    state.library.upsert_account_with_source(
        &account.account_id,
        &account.marketplace,
        account.label.as_deref(),
        true,
        source_id,
    )?;
    state.library.mark_connection_active(&account.account_id)?;
    state
        .library
        .link_account(identity.id, &account.account_id, source_id)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "account_id": account.account_id,
        "source": source_id,
    })))
}

async fn source_oauth_start(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let source = find_source(&state, &id).ok_or_else(|| PortalError::bad("unknown source"))?;
    require_source_enabled(&state, source.id()).await?;
    if source.portal_auth_mode() != PortalAuthMode::Oauth {
        return Err(PortalError::bad(
            "this source uses password login; call /login instead",
        ));
    }
    // Interactive OAuth callback flow is currently implemented for Audible only.
    if source.id() != "audible" {
        return Err(PortalError::bad(
            "OAuth start is only implemented for Audible",
        ));
    }
    let url = start_audible_login_session(&state, identity.id).await?;
    Ok(Json(serde_json::json!({ "url": url })))
}

async fn libro_login_legacy(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Json(body): Json<PasswordLoginBody>,
) -> Result<Json<serde_json::Value>, PortalError> {
    source_password_login(State(state), headers, Path("libro".into()), Json(body)).await
}

async fn audible_start(
    State(state): State<PortalState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, PortalError> {
    source_oauth_start(State(state), headers, Path("audible".into())).await
}

/// Start Audible LoginServer bound for reverse-proxy use; return the browser URL.
///
/// Uses `0.0.0.0:0` so the daemon accepts remote callbacks when published behind
/// a reverse proxy. The printed URL may still show a container-local host; set
/// `integrations.public_origin` and rewrite as needed at the proxy layer.
async fn start_audible_login_session(
    state: &PortalState,
    identity_id: i64,
) -> Result<String, PortalError> {
    use bookclerk_audible::{begin_login, AuthLoginOptions, LoginProgress};
    use tokio::sync::mpsc;

    let files_dir = state.files_dir.clone();
    let password_file = state.config.read().await.auth.password_file.clone();
    let allow_plaintext = state.config.read().await.auth.allow_plaintext;
    let library = state.library.clone();
    let (url_tx, mut url_rx) = mpsc::channel::<String>(1);

    tokio::spawn(async move {
        let opts = AuthLoginOptions {
            files_dir,
            password_file,
            allow_plaintext,
            show_qr: false,
            callback_bind: "0.0.0.0:0".parse().expect("bind"),
            ..Default::default()
        };
        let url_tx2 = url_tx.clone();
        let result = begin_login(opts, move |progress| {
            if let LoginProgress::LoginUrl { url, .. } = &progress {
                let _ = url_tx2.try_send(url.clone());
            }
        })
        .await;
        match result {
            Ok(session) => {
                let _ = library.upsert_account_with_source(
                    &session.account_id,
                    &session.marketplace,
                    session.label.as_deref(),
                    true,
                    "audible",
                );
                let _ = library.mark_connection_active(&session.account_id);
                let _ = library.link_account(identity_id, &session.account_id, "audible");
                info!(account = %session.account_id, "portal Audible login completed");
            }
            Err(err) => warn!(%err, "portal Audible login failed"),
        }
    });

    let url = tokio::time::timeout(std::time::Duration::from_secs(5), url_rx.recv())
        .await
        .map_err(|_| PortalError::bad("timed out waiting for Audible login URL"))?
        .ok_or_else(|| PortalError::bad("Audible login URL channel closed"))?;
    Ok(url)
}

async fn connections(
    State(state): State<PortalState>,
    headers: HeaderMap,
) -> Result<Json<ConnectionsResponse>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let cfg = state.config.read().await;
    let links = state.library.list_account_links(identity.id)?;
    let mut connections = Vec::new();
    for link in links {
        let acct = state.library.get_account(&link.account_id)?;
        let brand = find_source(&state, &link.source)
            .map(|s| Brand::from(s.portal_brand()))
            .or_else(|| {
                state
                    .integrations
                    .get(&link.source)
                    .and_then(|i| i.portal_brand())
                    .or_else(|| integration_brand(&link.source))
            });
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
struct ConnectionsResponse {
    connections: Vec<ConnectionInfo>,
}

#[derive(Debug, Serialize)]
struct ConnectionInfo {
    account_id: String,
    source: String,
    label: Option<String>,
    connection_status: String,
    /// Whether `[sources.<id>] enabled` is currently true (new connects blocked when false).
    source_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    brand: Option<BrandInfo>,
}

async fn revoke_connection(
    State(state): State<PortalState>,
    headers: HeaderMap,
    Path(account_id): Path<String>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let identity = require_identity(&state, &headers).await?;
    let links = state.library.list_account_links(identity.id)?;
    if !links.iter().any(|l| l.account_id == account_id) {
        return Err(PortalError::bad("account not linked to this identity"));
    }
    let suffixes: Vec<&str> = state
        .sources
        .iter()
        .flat_map(|s| s.auth_credential_suffixes().iter().copied())
        .fold(Vec::new(), |mut acc, suffix| {
            if !acc.contains(&suffix) {
                acc.push(suffix);
            }
            acc
        });
    match bookclerk_source::remove_account_credentials(&state.files_dir, &account_id, &suffixes) {
        Ok(paths) => {
            for path in paths {
                info!(path = %path.display(), "removed auth file on revoke");
            }
        }
        Err(err) => warn!(%account_id, %err, "failed to remove auth files on revoke"),
    }
    state.library.revoke_credentials(&account_id)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn require_identity(
    state: &PortalState,
    headers: &HeaderMap,
) -> Result<bookclerk_library::PortalIdentity, PortalError> {
    let raw = cookie_value(headers, SESSION_COOKIE)
        .ok_or_else(|| PortalError::unauthorized("not signed in"))?;
    identity_from_session(&state.library, &raw)?
        .ok_or_else(|| PortalError::unauthorized("session expired"))
}

async fn require_source_enabled(state: &PortalState, id: &str) -> Result<(), PortalError> {
    let cfg = state.config.read().await;
    if !cfg.sources.is_enabled(id) {
        return Err(PortalError::bad(format!("source `{id}` is disabled")));
    }
    // Also require registration (registry builders skip disabled sources).
    if find_source(state, id).is_none() {
        return Err(PortalError::bad(format!("source `{id}` is not registered")));
    }
    Ok(())
}

fn find_source(state: &PortalState, id_or_alias: &str) -> Option<Arc<dyn ContentSource>> {
    let needle = id_or_alias.trim().to_ascii_lowercase();
    state
        .sources
        .iter()
        .find(|s| {
            s.id().eq_ignore_ascii_case(&needle)
                || s.aliases().iter().any(|a| a.eq_ignore_ascii_case(&needle))
        })
        .cloned()
}

async fn session_response(session: String, state: &PortalState) -> Response {
    let cfg = state.config.read().await;
    let max_age = cfg.integrations.portal_session_ttl_hours * 3600;
    // Path=/ so the SPA (and /api/portal) can share the session cookie.
    let cookie =
        format!("{SESSION_COOKIE}={session}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

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
pub fn mint_for_external_user(
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
    let minted = mint_claim_ticket(library, &config.integrations, user, created_by)?;
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
struct PortalError {
    status: StatusCode,
    message: String,
}

impl PortalError {
    fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg.into(),
        }
    }
}

impl From<bookclerk_library::LibraryError> for PortalError {
    fn from(value: bookclerk_library::LibraryError) -> Self {
        Self::bad(value.to_string())
    }
}

impl From<crate::error::IntegrationError> for PortalError {
    fn from(value: crate::error::IntegrationError) -> Self {
        Self::bad(value.to_string())
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
