//! OIDC/OAuth relying party (identity broker) for first-party User login.
//!
//! Bookclerk remains the authorization server for Audiobookshelf. These routes
//! consume upstream IdPs, JIT/link Users, and mint portal sessions. The Operator
//! account is never an OAuth subject.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bookclerk_config::{
    allowlist_permits, email_domain_allowed, oidc_apple_private_key_env_key,
    oidc_client_secret_env_key, oidc_secret_store_name, oidc_transaction_cookie_flags,
    resolve_mapped_role, OidcBrokerConfig, OidcProviderConfig, OidcProvisionMode,
};
use bookclerk_library::{
    build_sealed_record, hash_token, secret_account_type, secret_kind, EncryptedSecretRecord,
    LibraryError, SecretStore, UserRecord, UserRole, UserStatus,
};
use chrono::{Duration as ChronoDuration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::api::AppState;
use crate::auth::{
    cookie_value, issue_elevation, issue_portal_session, require_operator_or_recent_owner,
    resolve_portal_caller_identity, timed_portal_identity_from_headers, too_many_requests,
    ClientIp,
};
use crate::oidc_verify::{
    apple_client_secret_jwt, github_verified_email, json_subject, verify_id_token, UpstreamProfile,
};
use openidconnect::core::{CoreJwsSigningAlgorithm, CoreProviderMetadata};

/// Browser-bound OIDC login transaction (must match `state` on callback).
const OIDC_TX_COOKIE: &str = "bookclerk_oidc_tx";

/// 303 See Other so Apple `form_post` (POST) failures become GET at the SPA.
fn sso_error_redirect(location: &'static str) -> Response {
    Redirect::to(location).into_response()
}

/// RP routes (`/api/auth/oidc/*`).
pub fn router(state: Arc<AppState>) -> Router {
    let public = Router::new()
        .route("/api/auth/oidc/providers", get(list_providers))
        .route("/api/auth/oidc/identities", get(list_identities))
        .route("/api/auth/oidc/login", get(login_start))
        .route("/api/auth/oidc/elevate", get(elevate_start))
        .route(
            "/api/auth/oidc/callback",
            get(callback_get).post(callback_post),
        )
        .with_state(state.clone());
    let config = Router::new()
        .route(
            "/api/auth/oidc/config",
            get(get_oidc_config).put(put_oidc_config),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_operator_or_owner_auth,
        ))
        .with_state(state);
    public.merge(config)
}

#[derive(Debug, Deserialize)]
/// Optional `?provider=` query on login and elevate starts.
struct ProviderQuery {
    /// IdP id to start; omitted when the caller has only one enabled provider.
    provider: Option<String>,
}

/// Lists enabled IdPs for the login picker (id and display name only).
async fn list_providers(State(state): State<Arc<AppState>>) -> Json<Value> {
    let cfg = state.config.read().await;
    let providers: Vec<Value> = cfg
        .auth
        .oidc
        .enabled_providers()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "name": p.display_name(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "enabled": cfg.auth.oidc.enabled,
        "providers": providers,
    }))
}

#[derive(Debug, Serialize)]
/// Operator-facing OIDC broker snapshot; secret material is never included.
struct OidcConfigResponse {
    /// Whether the broker accepts new authorize starts.
    enabled: bool,
    /// Broker-wide email domain allowlist applied before per-provider rules.
    allowed_email_domains: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Absolute `/api/auth/oidc/callback` derived from `public_origin`, if set.
    callback_url: Option<String>,
    /// Configured IdPs with presence flags instead of secret material.
    providers: Vec<OidcProviderView>,
}

#[derive(Debug, Serialize)]
/// One IdP as shown to an operator (no client secret or Apple PEM).
struct OidcProviderView {
    /// Stable provider id used in authorize URLs and sealed-row names.
    id: String,
    /// Operator-facing display name.
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Built-in social preset (`google`, `github`, `apple`, `discord`) when not a generic issuer.
    preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// OpenID issuer URL for discovery when no social preset applies.
    issuer: Option<String>,
    /// OAuth client id registered at the IdP.
    client_id: String,
    /// OAuth scopes requested at authorize.
    scopes: Vec<String>,
    /// How a first login becomes a portal user (JIT, mapped role, allowlist, invite-only).
    provision: OidcProvisionMode,
    /// Portal role used when no mapped claim matches (`member` by default).
    default_role: String,
    /// Claim name read from id_token/userinfo for role mapping (`groups` by default).
    role_claim: String,
    /// IdP group/role claim values mapped onto Bookclerk portal roles.
    role_map: BTreeMap<String, String>,
    /// When true, a verified email may attach this IdP to an existing user.
    link_by_email: bool,
    /// Per-provider email domain allowlist (in addition to the broker-wide list).
    allowed_email_domains: Vec<String>,
    /// Exact emails permitted under allowlist provision.
    allowed_emails: Vec<String>,
    /// Exact IdP subject ids permitted under allowlist provision.
    allowed_subjects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Apple Developer team id used to mint the client-secret JWT.
    apple_team_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Apple Sign In key id paired with the sealed private key.
    apple_key_id: Option<String>,
    /// Whether a client secret exists in env, TOML, or the sealed store (value never returned).
    has_client_secret: bool,
    /// Whether an Apple PEM exists in env, TOML, or the sealed store (PEM never returned).
    has_apple_private_key: bool,
    /// Where the client secret was found: `env`, `config`, `store`, or `none`.
    secret_source: &'static str,
}

#[derive(Debug, Deserialize)]
/// Operator PUT body that replaces the broker table (secrets omitted or replaced).
struct OidcConfigPut {
    /// Whether the broker should accept authorize starts after this write.
    enabled: bool,
    #[serde(default)]
    /// Replacement broker-wide email domain allowlist.
    allowed_email_domains: Vec<String>,
    #[serde(default)]
    /// Full replacement list of IdPs (omitted secrets keep the previous generation).
    providers: Vec<OidcProviderPut>,
    /// Required for a non-elevated Owner whose portal session is older than 15 minutes.
    #[serde(default)]
    current_password: Option<String>,
}

#[derive(Debug, Deserialize)]
/// One IdP in a PUT: omit secret fields to keep, empty string to clear.
struct OidcProviderPut {
    /// Stable provider id; also the logical name of the sealed client-secret row.
    id: String,
    #[serde(default)]
    /// Operator-facing display name stored in config.toml.
    name: String,
    #[serde(default)]
    /// Built-in social preset; empty or omitted means a generic OpenID issuer.
    preset: Option<String>,
    #[serde(default)]
    /// OpenID issuer URL required when `preset` is unset.
    issuer: Option<String>,
    /// OAuth client id registered at the IdP.
    client_id: String,
    /// Omit to keep; empty string to clear; non-empty to store.
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    /// OAuth scopes; empty is replaced with the provider default set.
    scopes: Vec<String>,
    #[serde(default)]
    /// JIT, mapped-role, allowlist, or invite-only policy for first login.
    provision: OidcProvisionMode,
    #[serde(default = "default_member_role")]
    /// Fallback portal role when no mapped claim matches.
    default_role: String,
    #[serde(default = "default_groups_claim")]
    /// Claim name used for role mapping (defaults to `groups`).
    role_claim: String,
    #[serde(default)]
    /// Claim value to portal role mapping applied at provision time.
    role_map: BTreeMap<String, String>,
    #[serde(default)]
    /// When true, verified email may attach this IdP to an existing user.
    link_by_email: bool,
    #[serde(default)]
    /// Per-provider email domain allowlist.
    allowed_email_domains: Vec<String>,
    #[serde(default)]
    /// Exact emails permitted under allowlist provision.
    allowed_emails: Vec<String>,
    #[serde(default)]
    /// Exact IdP subject ids permitted under allowlist provision.
    allowed_subjects: Vec<String>,
    #[serde(default)]
    /// Apple Developer team id for client-secret JWT minting.
    apple_team_id: Option<String>,
    #[serde(default)]
    /// Apple Sign In key id paired with the sealed private key.
    apple_key_id: Option<String>,
    /// Omit to keep; empty string to clear; non-empty to store.
    #[serde(default)]
    apple_private_key: Option<String>,
}

/// Default portal role when a PUT omits `default_role`.
fn default_member_role() -> String {
    "member".into()
}

/// Default role-claim name (`groups`) when a PUT omits `role_claim`.
fn default_groups_claim() -> String {
    "groups".into()
}

/// JSON error body for operator config mutations (`error: oidc_config`).
fn oidc_config_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": "oidc_config",
            "message": message.into(),
        })),
    )
        .into_response()
}

/// Builds `{public_origin}/api/auth/oidc/callback`, or `None` when origin is unset.
fn oidc_callback_url(public_origin: Option<&str>) -> Option<String> {
    let origin = public_origin.map(str::trim).filter(|s| !s.is_empty())?;
    Some(format!(
        "{}/api/auth/oidc/callback",
        origin.trim_end_matches('/')
    ))
}

/// True when the provider's `BOOKCLERK_OIDC_*` env var is a non-empty secret.
fn env_has_client_secret(provider_id: &str) -> bool {
    std::env::var(oidc_client_secret_env_key(provider_id))
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// True when config.toml still embeds a non-empty client secret (legacy).
fn toml_has_client_secret(provider: &OidcProviderConfig) -> bool {
    provider
        .client_secret
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
}

/// Current secret-generation counter used to name sealed OIDC rows.
async fn live_oidc_generation(state: &AppState) -> u64 {
    state.config.read().await.auth.oidc.secret_generation
}

/// True when a sealed `oidc_client` row exists for this provider generation.
async fn store_has_client_secret(state: &AppState, provider_id: &str) -> bool {
    let generation = live_oidc_generation(state).await;
    let library = state.library_snapshot().await;
    let store = SecretStore::new(library.db());
    store
        .get(
            secret_kind::OIDC_CLIENT,
            Some("oidc"),
            secret_account_type::OPERATOR,
            Some("operator"),
            &oidc_secret_store_name(provider_id, generation),
        )
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Reports where a client secret can be read: env, TOML, sealed store, or none.
async fn secret_source_for(state: &AppState, provider: &OidcProviderConfig) -> &'static str {
    if env_has_client_secret(&provider.id) {
        return "env";
    }
    if toml_has_client_secret(provider) {
        return "config";
    }
    if store_has_client_secret(state, &provider.id).await {
        return "store";
    }
    "none"
}

/// Trims and drops empty strings from allowlist and scope vectors.
fn trim_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Maps a PUT provider into config, leaving secrets unset so they stay sealed.
fn provider_from_put(p: OidcProviderPut) -> OidcProviderConfig {
    let mut cfg = OidcProviderConfig {
        id: p.id.trim().to_string(),
        name: p.name.trim().to_string(),
        preset: p
            .preset
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        issuer: p
            .issuer
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        client_id: p.client_id.trim().to_string(),
        client_secret: None,
        scopes: trim_list(p.scopes),
        provision: p.provision,
        default_role: p.default_role.trim().to_string(),
        role_claim: p.role_claim.trim().to_string(),
        role_map: p.role_map,
        link_by_email: p.link_by_email,
        apple_team_id: p
            .apple_team_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        apple_key_id: p
            .apple_key_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        apple_private_key: None,
        allowed_email_domains: trim_list(p.allowed_email_domains),
        allowed_emails: trim_list(p.allowed_emails),
        allowed_subjects: trim_list(p.allowed_subjects),
    };
    if cfg.scopes.is_empty() {
        cfg.scopes = OidcProviderConfig::default().scopes;
    }
    if cfg.default_role.is_empty() {
        cfg.default_role = default_member_role();
    }
    if cfg.role_claim.is_empty() {
        cfg.role_claim = default_groups_claim();
    }
    cfg
}

/// Logical secret name for the Apple PEM (`{provider_id}__apple_key`).
fn apple_key_secret_name(provider_id: &str) -> String {
    format!("{}__apple_key", provider_id.trim())
}

/// True when the provider's Apple PEM env var is non-empty.
fn env_has_apple_private_key(provider_id: &str) -> bool {
    std::env::var(oidc_apple_private_key_env_key(provider_id))
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// True when config.toml still embeds a non-empty Apple PEM (legacy).
fn toml_has_apple_private_key(provider: &OidcProviderConfig) -> bool {
    provider
        .apple_private_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
}

/// True when a sealed Apple PEM row exists for this provider generation.
async fn store_has_apple_private_key(state: &AppState, provider_id: &str) -> bool {
    let generation = live_oidc_generation(state).await;
    let library = state.library_snapshot().await;
    let store = SecretStore::new(library.db());
    store
        .get(
            secret_kind::OIDC_CLIENT,
            Some("oidc"),
            secret_account_type::OPERATOR,
            Some("operator"),
            &oidc_secret_store_name(&apple_key_secret_name(provider_id), generation),
        )
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Test failpoint: `-1` unlimited; `0` fail next mutation; `n>0` succeed `n` times then fail.
#[cfg(test)]
static SECRET_MUTATION_SUCCESSES_REMAINING: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(-1);

/// Test failpoint for the config.toml publish step (after next-generation secrets exist).
#[cfg(test)]
static CONFIG_WRITE_SUCCESSES_REMAINING: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(-1);

/// Test-only: fail the next sealed-secret write after N successes (`-1` = off).
fn oidc_secret_mutation_failpoint() -> Result<(), String> {
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        let remaining = SECRET_MUTATION_SUCCESSES_REMAINING.load(Ordering::SeqCst);
        if remaining == 0 {
            return Err("injected oidc secret mutation failure".into());
        }
        if remaining > 0 {
            SECRET_MUTATION_SUCCESSES_REMAINING.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// Test-only: fail the config.toml publish after N successes (`-1` = off).
fn oidc_config_write_failpoint() -> Result<(), String> {
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        let remaining = CONFIG_WRITE_SUCCESSES_REMAINING.load(Ordering::SeqCst);
        if remaining == 0 {
            return Err("injected oidc config write failure".into());
        }
        if remaining > 0 {
            CONFIG_WRITE_SUCCESSES_REMAINING.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// Loads one operator `oidc_client` row by sealed-store name; fail-closed on DB error.
async fn load_oidc_secret_row(
    state: &AppState,
    name: &str,
) -> Result<Option<EncryptedSecretRecord>, String> {
    let library = state.library_snapshot().await;
    SecretStore::new(library.db())
        .get(
            secret_kind::OIDC_CLIENT,
            Some("oidc"),
            secret_account_type::OPERATOR,
            Some("operator"),
            name.trim(),
        )
        .await
        .map_err(|e| format!("could not load secret snapshot: {e}"))
}

/// Copies a sealed OIDC row to the next generation name without unsealing plaintext.
async fn copy_oidc_secret_row(
    state: &AppState,
    from_name: &str,
    to_name: &str,
) -> Result<(), String> {
    if from_name == to_name {
        return Ok(());
    }
    let Some(mut row) = load_oidc_secret_row(state, from_name).await? else {
        return Ok(());
    };
    row.id = None;
    row.name = to_name.to_string();
    let library = state.library_snapshot().await;
    SecretStore::new(library.db())
        .upsert(&row)
        .await
        .map_err(|e| format!("could not copy secret: {e}"))
}

/// Seals plaintext with the process DEK and upserts an operator `oidc_client` row.
async fn persist_oidc_secret_named(
    state: &AppState,
    name: &str,
    plaintext: &str,
) -> Result<(), String> {
    oidc_secret_mutation_failpoint()?;
    let record = build_sealed_record(
        plaintext.as_bytes(),
        secret_kind::OIDC_CLIENT,
        "oidc",
        secret_account_type::OPERATOR,
        "operator",
        name.trim(),
    )
    .map_err(|e| format!("could not seal secret: {e}"))?;
    let library = state.library_snapshot().await;
    SecretStore::new(library.db())
        .upsert(&record)
        .await
        .map_err(|e| format!("could not store secret: {e}"))?;
    bookclerk_config::register_secret(plaintext);
    Ok(())
}

/// Seals a client secret under the generation-qualified provider name.
async fn persist_oidc_secret(
    state: &AppState,
    provider_id: &str,
    plaintext: &str,
    generation: u64,
) -> Result<(), String> {
    persist_oidc_secret_named(
        state,
        &oidc_secret_store_name(provider_id, generation),
        plaintext,
    )
    .await
}

/// Seals an Apple PEM under the generation-qualified `{id}__apple_key` name.
async fn persist_apple_private_key(
    state: &AppState,
    provider_id: &str,
    plaintext: &str,
    generation: u64,
) -> Result<(), String> {
    persist_oidc_secret_named(
        state,
        &oidc_secret_store_name(&apple_key_secret_name(provider_id), generation),
        plaintext,
    )
    .await
}

/// Deletes one operator `oidc_client` row by name (used on generation rollback).
async fn delete_named_oidc_secret(state: &AppState, name: &str) -> Result<(), String> {
    let library = state.library_snapshot().await;
    SecretStore::new(library.db())
        .delete(
            secret_kind::OIDC_CLIENT,
            Some("oidc"),
            secret_account_type::OPERATOR,
            Some("operator"),
            name.trim(),
        )
        .await
        .map_err(|e| format!("could not delete secret: {e}"))
}

/// Operator GET: broker settings plus secret-presence flags (never plaintext).
async fn get_oidc_config(State(state): State<Arc<AppState>>) -> Json<OidcConfigResponse> {
    let (enabled, allowed_email_domains, callback_url, configured) = {
        let cfg = state.config.read().await;
        (
            cfg.auth.oidc.enabled,
            cfg.auth.oidc.allowed_email_domains.clone(),
            oidc_callback_url(cfg.integrations.public_origin.as_deref()),
            cfg.auth.oidc.providers.clone(),
        )
    };
    let mut providers = Vec::with_capacity(configured.len());
    for provider in &configured {
        let source = secret_source_for(&state, provider).await;
        providers.push(OidcProviderView {
            id: provider.id.clone(),
            name: provider.name.clone(),
            preset: provider.preset.clone(),
            issuer: provider.issuer.clone(),
            client_id: provider.client_id.clone(),
            scopes: provider.scopes.clone(),
            provision: provider.provision,
            default_role: provider.default_role.clone(),
            role_claim: provider.role_claim.clone(),
            role_map: provider.role_map.clone(),
            link_by_email: provider.link_by_email,
            allowed_email_domains: provider.allowed_email_domains.clone(),
            allowed_emails: provider.allowed_emails.clone(),
            allowed_subjects: provider.allowed_subjects.clone(),
            apple_team_id: provider.apple_team_id.clone(),
            apple_key_id: provider.apple_key_id.clone(),
            has_client_secret: source != "none",
            has_apple_private_key: env_has_apple_private_key(&provider.id)
                || toml_has_apple_private_key(provider)
                || store_has_apple_private_key(&state, &provider.id).await,
            secret_source: source,
        });
    }
    Json(OidcConfigResponse {
        enabled,
        allowed_email_domains,
        callback_url,
        providers,
    })
}

/// Operator PUT: writes next-generation sealed secrets, then publishes config.toml.
async fn put_oidc_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<OidcConfigPut>,
) -> Result<Json<OidcConfigResponse>, Response> {
    require_operator_or_recent_owner(&state, &headers, body.current_password.as_deref())
        .await
        .map_err(|status| {
            oidc_config_error(
                status,
                if status == StatusCode::UNAUTHORIZED {
                    "recent authentication required to change sign-in providers"
                } else {
                    "forbidden"
                },
            )
        })?;

    let _reload_guard = state.reload_lock.lock().await;

    let config_path = {
        let cfg = state.config.read().await;
        cfg.paths.as_ref().map(|p| p.config_file.clone())
    };
    let Some(config_path) = config_path else {
        return Err(oidc_config_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config path is not available",
        ));
    };

    let old_providers = {
        let cfg = state.config.read().await;
        cfg.auth.oidc.providers.clone()
    };
    let old_gen = {
        let cfg = state.config.read().await;
        cfg.auth.oidc.secret_generation
    };
    let old_by_id: BTreeMap<String, OidcProviderConfig> = old_providers
        .iter()
        .map(|p| (p.id.trim().to_string(), p.clone()))
        .collect();
    let next_gen = old_gen.saturating_add(1);

    let mut next = OidcBrokerConfig {
        enabled: body.enabled,
        allowed_email_domains: trim_list(body.allowed_email_domains),
        providers: Vec::with_capacity(body.providers.len()),
        secret_generation: next_gen,
    };
    let mut secret_actions: Vec<(String, Option<String>, bool)> = Vec::new();
    let mut apple_key_actions: Vec<(String, Option<String>, bool)> = Vec::new();

    for put in body.providers {
        let secret_action = put.client_secret.clone();
        let apple_key_action = put.apple_private_key.clone();
        let mut provider = provider_from_put(put);
        let id = provider.id.clone();
        match secret_action.as_deref().map(str::trim) {
            Some(secret) if !secret.is_empty() => {
                secret_actions.push((id.clone(), Some(secret.to_string()), false));
                provider.client_secret = None;
            }
            Some(_) => {
                secret_actions.push((id.clone(), None, true));
                provider.client_secret = None;
            }
            None => {
                if let Some(old) = old_by_id.get(&provider.id) {
                    if toml_has_client_secret(old) {
                        provider.client_secret = old.client_secret.clone();
                    }
                }
            }
        }
        match apple_key_action.as_deref().map(str::trim) {
            Some(key) if !key.is_empty() => {
                apple_key_actions.push((id, Some(key.to_string()), false));
                provider.apple_private_key = None;
            }
            Some(_) => {
                apple_key_actions.push((id, None, true));
                provider.apple_private_key = None;
            }
            None => {
                if let Some(old) = old_by_id.get(&provider.id) {
                    if toml_has_apple_private_key(old) {
                        provider.apple_private_key = old.apple_private_key.clone();
                    }
                }
            }
        }
        next.providers.push(provider);
    }

    if let Err(err) = next.validate_providers() {
        return Err(oidc_config_error(StatusCode::BAD_REQUEST, err.to_string()));
    }

    let mut next_logical = BTreeSet::new();
    for provider in &next.providers {
        next_logical.insert(provider.id.trim().to_string());
        next_logical.insert(apple_key_secret_name(&provider.id));
    }
    let mut old_logical = BTreeSet::new();
    for old in &old_providers {
        old_logical.insert(old.id.trim().to_string());
        old_logical.insert(apple_key_secret_name(&old.id));
    }
    let next_names: Vec<String> = next_logical
        .iter()
        .map(|logical| oidc_secret_store_name(logical, next_gen))
        .collect();
    let mut written = BTreeSet::new();
    let mut cleared = BTreeSet::new();

    let apply = async {
        for provider in &mut next.providers {
            if let Some(secret) = provider
                .client_secret
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            {
                persist_oidc_secret(&state, &provider.id, &secret, next_gen).await?;
                written.insert(provider.id.trim().to_string());
                provider.client_secret = None;
            }
            if let Some(key) = provider
                .apple_private_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            {
                persist_apple_private_key(&state, &provider.id, &key, next_gen).await?;
                written.insert(apple_key_secret_name(&provider.id));
                provider.apple_private_key = None;
            }
        }
        for (id, secret, clear) in &secret_actions {
            if let Some(secret) = secret {
                persist_oidc_secret(&state, id, secret, next_gen).await?;
                written.insert(id.clone());
            } else if *clear {
                cleared.insert(id.clone());
            }
        }
        for (id, key, clear) in &apple_key_actions {
            let logical = apple_key_secret_name(id);
            if let Some(key) = key {
                persist_apple_private_key(&state, id, key, next_gen).await?;
                written.insert(logical);
            } else if *clear {
                cleared.insert(logical);
            }
        }
        for logical in &next_logical {
            if written.contains(logical) || cleared.contains(logical) {
                continue;
            }
            copy_oidc_secret_row(
                &state,
                &oidc_secret_store_name(logical, old_gen),
                &oidc_secret_store_name(logical, next_gen),
            )
            .await?;
        }
        oidc_config_write_failpoint()?;
        {
            let mut staged = state.config.read().await.clone();
            staged.auth.oidc = next.clone();
            staged.register_known_secrets();
            staged
                .write_toml_file(&config_path)
                .map_err(|err| format!("failed to write config.toml: {err}"))?;
        }
        {
            let mut cfg = state.config.write().await;
            cfg.auth.oidc = next.clone();
            cfg.register_known_secrets();
        }
        for logical in old_logical.union(&next_logical) {
            let old_name = oidc_secret_store_name(logical, old_gen);
            let new_name = oidc_secret_store_name(logical, next_gen);
            if old_name != new_name {
                if let Err(err) = delete_named_oidc_secret(&state, &old_name).await {
                    tracing::error!(
                        secret = %old_name,
                        error = %err,
                        "failed to drop previous OIDC secret generation"
                    );
                }
            }
        }
        Ok::<(), String>(())
    }
    .await;

    if let Err(err) = apply {
        let mut rollback_errs = Vec::new();
        for name in &next_names {
            if let Err(restore_err) = delete_named_oidc_secret(&state, name).await {
                rollback_errs.push(restore_err);
            }
        }
        let message = if rollback_errs.is_empty() {
            err
        } else {
            format!(
                "{err}; also failed to drop unpublished secret generation: {}",
                rollback_errs.join("; ")
            )
        };
        return Err(oidc_config_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            message,
        ));
    }

    Ok(get_oidc_config(State(state.clone())).await)
}

/// Lists OIDC portal identities for the cookie-authenticated caller.
async fn list_identities(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let library = state.library_snapshot().await;
    let identity = timed_portal_identity_from_headers(&library, &headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let (_, _, user, _) = resolve_portal_caller_identity(&library, &identity).await;
    let user = user.ok_or(StatusCode::UNAUTHORIZED)?;
    let rows = library
        .list_portal_identities_for_user(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let identities: Vec<Value> = rows
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "provider": p.provider,
                "external_user_id": p.external_user_id,
                "label": p.label,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "identities": identities })))
}

/// Starts an OIDC login authorize redirect, honoring login throttle.
async fn login_start(
    State(state): State<Arc<AppState>>,
    ClientIp(client_key): ClientIp,
    Query(q): Query<ProviderQuery>,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    if let Some(retry_after) = auth.login_throttle_check(&client_key).await {
        return Ok(too_many_requests(retry_after));
    }
    match start_authorize(&state, q.provider.as_deref(), "login", None).await {
        Ok(res) => {
            auth.clear_login_failures(&client_key).await;
            Ok(res)
        }
        Err(StatusCode::INTERNAL_SERVER_ERROR) => {
            if let Some(retry_after) = auth.record_login_failure(&client_key).await {
                return Ok(too_many_requests(retry_after));
            }
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(other) => {
            let _ = auth.record_login_failure(&client_key).await;
            Err(other)
        }
    }
}

/// Starts an Owner re-auth elevate flow against a linked IdP (`prompt=login`).
async fn elevate_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ProviderQuery>,
) -> Result<Response, StatusCode> {
    let library = state.library_snapshot().await;
    let identity = timed_portal_identity_from_headers(&library, &headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let (role, _, user, _) = resolve_portal_caller_identity(&library, &identity).await;
    if role != "owner" {
        return Err(StatusCode::FORBIDDEN);
    }
    let user = user.ok_or(StatusCode::FORBIDDEN)?;
    let provider_id = match q
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        None => {
            let linked = library
                .list_portal_identities_for_user(user.id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            linked
                .into_iter()
                .find_map(|p| p.provider.strip_prefix("oidc:").map(str::to_string))
                .ok_or(StatusCode::BAD_REQUEST)?
        }
    };
    start_authorize(&state, Some(&provider_id), "elevate", Some(user.id)).await
}

/// Persists hashed RP state and redirects to the IdP authorize URL with PKCE.
async fn start_authorize(
    state: &AppState,
    provider_id: Option<&str>,
    purpose: &str,
    user_id: Option<i64>,
) -> Result<Response, StatusCode> {
    let cfg = state.config.read().await;
    if !cfg.auth.oidc.enabled {
        return Err(StatusCode::NOT_FOUND);
    }
    let id = provider_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let provider = cfg
        .auth
        .oidc
        .provider(id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    let origin = public_origin(&cfg);
    let tx_flags = oidc_transaction_cookie_flags(cfg.integrations.public_origin.as_deref());
    drop(cfg);
    let endpoints = resolve_endpoints(&provider)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let state_raw = Uuid::new_v4().to_string();
    let nonce = random_b64url(32);
    let verifier = random_b64url(32);
    let challenge = pkce_challenge(&verifier);
    let library = state.library_snapshot().await;
    library
        .insert_oidc_rp_state(
            &hash_token(&state_raw),
            provider.id.trim(),
            &verifier,
            &nonce,
            purpose,
            user_id,
            Utc::now() + ChronoDuration::minutes(10),
        )
        .await
        .map_err(|err| {
            if err.to_string().contains("too many") {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;
    let redirect_uri = format!("{origin}/api/auth/oidc/callback");
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&scope={}&code_challenge={}&code_challenge_method=S256",
        endpoints.authorize,
        urlencoding(&provider.client_id),
        urlencoding(&redirect_uri),
        urlencoding(&state_raw),
        urlencoding(&provider.effective_scopes().join(" ")),
        urlencoding(&challenge),
    );
    if provider.effective_scopes().iter().any(|s| s == "openid") {
        url.push_str("&nonce=");
        url.push_str(&urlencoding(&nonce));
    }
    if purpose == "elevate" {
        url.push_str("&prompt=login");
    }
    if provider.social_preset() == Some("apple") {
        url.push_str("&response_mode=form_post");
    }
    let mut response = Redirect::temporary(&url).into_response();
    let cookie = format!("{OIDC_TX_COOKIE}={state_raw}; {tx_flags}; Max-Age=600");
    if let Ok(value) = header::HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    Ok(response)
}

#[derive(Debug, Deserialize)]
/// Query or form_post fields from the IdP callback.
struct CallbackParams {
    /// Authorization code to exchange; required unless `error` is set.
    code: Option<String>,
    /// CSRF `state` that must match the `bookclerk_oidc_tx` cookie.
    state: Option<String>,
    /// IdP-reported deny/error; redirects the browser to `/?sso_error=denied`.
    error: Option<String>,
    /// Apple first-login name/email JSON (form_post only).
    user: Option<String>,
}

/// GET callback (most IdPs); finishes the code exchange.
async fn callback_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<CallbackParams>,
) -> Result<Response, StatusCode> {
    finish_callback(&state, &headers, q).await
}

/// POST callback (Apple `form_post`); finishes the code exchange.
async fn callback_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CallbackParams>,
) -> Result<Response, StatusCode> {
    finish_callback(&state, &headers, form).await
}

/// Consumes one-time RP state, verifies tokens, then provisions or elevates.
async fn finish_callback(
    state: &AppState,
    headers: &HeaderMap,
    q: CallbackParams,
) -> Result<Response, StatusCode> {
    if q.error.is_some() {
        return Ok(sso_error_redirect("/?sso_error=denied"));
    }
    let code = q
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let state_raw = q
        .state
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let tx_cookie = cookie_value(headers, OIDC_TX_COOKIE);
    if tx_cookie.as_deref() != Some(state_raw) {
        return Ok(sso_error_redirect("/?sso_error=csrf"));
    }
    let library = state.library_snapshot().await;
    let Some((provider_id, verifier, nonce, purpose, elevate_user_id)) = library
        .take_oidc_rp_state(&hash_token(state_raw))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Ok(sso_error_redirect("/?sso_error=expired"));
    };
    let cfg = state.config.read().await;
    let Some(provider) = cfg.auth.oidc.provider(&provider_id).cloned() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let origin = public_origin(&cfg);
    let global_domains = cfg.auth.oidc.allowed_email_domains.clone();
    drop(cfg);
    let endpoints = resolve_endpoints(&provider)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let secret = client_secret(state, &provider).await;
    let redirect_uri = format!("{origin}/api/auth/oidc/callback");
    let token_json = exchange_code(
        &endpoints.token,
        &provider.client_id,
        secret.as_deref(),
        code,
        &redirect_uri,
        &verifier,
    )
    .await
    .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let access_token = token_json.get("access_token").and_then(Value::as_str);
    let id_token = token_json.get("id_token").and_then(Value::as_str);
    let preset = provider.social_preset().unwrap_or("");
    let requires_id_token = matches!(preset, "google" | "apple" | "")
        || provider.effective_scopes().iter().any(|s| s == "openid");

    let mut verified_claims = None;
    if requires_id_token {
        let id_token = id_token.ok_or(StatusCode::BAD_GATEWAY)?;
        let jwks_uri = endpoints
            .jwks_uri
            .as_deref()
            .ok_or(StatusCode::BAD_GATEWAY)?;
        match verify_id_token(
            id_token,
            jwks_uri,
            &endpoints.issuer,
            provider.client_id.trim(),
            &nonce,
            &endpoints.id_token_signing_algs,
        )
        .await
        {
            Ok(claims) => verified_claims = Some(claims),
            Err(()) => {
                return Ok(sso_error_redirect("/?sso_error=nonce"));
            }
        }
    }

    let userinfo = if let Some(token) = access_token {
        fetch_userinfo(&endpoints.userinfo, token)
            .await
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    let mut profile = match preset {
        "github" => {
            let email = if let Some(token) = access_token {
                fetch_github_verified_email(token).await.ok().flatten()
            } else {
                None
            };
            UpstreamProfile::from_github_user(&userinfo, email)
        }
        "discord" => UpstreamProfile::from_discord(&userinfo),
        _ => {
            let claims = verified_claims.as_ref().ok_or(StatusCode::BAD_GATEWAY)?;
            UpstreamProfile::from_oidc(claims, &userinfo)
        }
    }
    .ok_or(StatusCode::BAD_GATEWAY)?;
    if let Some(claims) = verified_claims.as_ref() {
        if let Some(info_sub) = json_subject(userinfo.get("sub")) {
            let claim_sub = json_subject(claims.get("sub")).unwrap_or_default();
            if info_sub != claim_sub {
                return Ok(sso_error_redirect("/?sso_error=sub"));
            }
        }
    }
    if preset == "apple" {
        if let Some(user_json) = q.user.as_deref() {
            profile.merge_apple_user_json(user_json);
        }
    }
    if purpose == "elevate" {
        let user_id = elevate_user_id.ok_or(StatusCode::BAD_REQUEST)?;
        let expected = format!("{}|{}", provider.portal_provider(), profile.sub);
        let linked = library
            .list_portal_identities_for_user(user_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let ok = linked.iter().any(|p| {
            p.provider == provider.portal_provider() && p.external_user_id == profile.sub
                || format!("{}|{}", p.provider, p.external_user_id) == expected
        });
        if !ok {
            return Ok(sso_error_redirect("/settings?sso_error=mismatch"));
        }
        let user = library
            .get_user(user_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::NOT_FOUND)?;
        if user.role != UserRole::Owner {
            return Err(StatusCode::FORBIDDEN);
        }
        let res = issue_elevation(state, &library, user.id, headers).await?;
        if res.status() == StatusCode::OK {
            if let Some(cookie) = res.headers().get(header::SET_COOKIE).cloned() {
                return Ok((
                    StatusCode::SEE_OTHER,
                    [
                        (header::SET_COOKIE, cookie),
                        (
                            header::LOCATION,
                            axum::http::HeaderValue::from_static("/settings"),
                        ),
                    ],
                )
                    .into_response());
            }
        }
        return Ok(res);
    }

    match provision_user(
        state,
        &library,
        &provider,
        &global_domains,
        &profile,
        verified_claims.as_ref(),
        &userinfo,
    )
    .await
    {
        Ok(user) => {
            if matches!(user.status, UserStatus::Disabled) {
                return Ok(sso_error_redirect("/?sso_error=disabled"));
            }
            let issued =
                issue_portal_session(state, &library, &user, headers, "oidc_login").await?;
            if let Some(cookie) = issued.headers().get(header::SET_COOKIE).cloned() {
                return Ok((
                    StatusCode::SEE_OTHER,
                    [
                        (header::SET_COOKIE, cookie),
                        (header::LOCATION, axum::http::HeaderValue::from_static("/")),
                    ],
                )
                    .into_response());
            }
            Ok(issued)
        }
        Err(ProvisionError::Denied) => Ok(sso_error_redirect("/?sso_error=no_role")),
        Err(ProvisionError::Conflict) => Ok(sso_error_redirect("/?sso_error=conflict")),
        Err(ProvisionError::Internal) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Application outcomes from JIT/link that become SSO error redirects.
enum ProvisionError {
    /// Caller is not allowed to sign in (allowlist, domain, invite-only, disabled).
    Denied,
    /// Identity or email is already linked to a different user.
    Conflict,
    /// Library/DB failure while creating or linking the user.
    Internal,
}

/// JIT-creates, email-links, or role-syncs a portal user from a verified IdP profile.
async fn provision_user(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    provider: &OidcProviderConfig,
    global_domains: &[String],
    profile: &UpstreamProfile,
    verified_claims: Option<&Value>,
    userinfo: &Value,
) -> Result<UserRecord, ProvisionError> {
    let portal_provider = provider.portal_provider();
    let verified_email = profile.verified_email();
    if !email_domain_allowed(verified_email, global_domains) {
        return Err(ProvisionError::Denied);
    }
    if !matches!(provider.provision, OidcProvisionMode::Allowlist)
        && !email_domain_allowed(verified_email, &provider.allowed_email_domains)
    {
        return Err(ProvisionError::Denied);
    }
    let claim_values = collect_role_claims(verified_claims, userinfo, &provider.role_claim);
    let mapped = resolve_mapped_role(&provider.role_map, &claim_values);
    let desired_role = match provider.provision {
        OidcProvisionMode::MappedRole => {
            let raw = mapped.as_deref().ok_or(ProvisionError::Denied)?;
            UserRole::parse(raw).ok_or(ProvisionError::Denied)?
        }
        OidcProvisionMode::Any => mapped
            .as_deref()
            .and_then(UserRole::parse)
            .unwrap_or_else(|| UserRole::parse(&provider.default_role).unwrap_or(UserRole::Member)),
        OidcProvisionMode::Allowlist => {
            if !allowlist_permits(
                verified_email,
                &profile.sub,
                &provider.allowed_emails,
                &provider.allowed_email_domains,
                &provider.allowed_subjects,
                global_domains,
            ) {
                return Err(ProvisionError::Denied);
            }
            mapped
                .as_deref()
                .and_then(UserRole::parse)
                .unwrap_or_else(|| {
                    UserRole::parse(&provider.default_role).unwrap_or(UserRole::Member)
                })
        }
        OidcProvisionMode::InviteOnly => mapped
            .as_deref()
            .and_then(UserRole::parse)
            .unwrap_or(UserRole::Member),
    };

    if let Some(existing) = library
        .get_portal_identity(&portal_provider, &profile.sub)
        .await
        .map_err(|_| ProvisionError::Internal)?
    {
        let uid = existing.user_id.ok_or(ProvisionError::Internal)?;
        let mut user = library
            .get_user(uid)
            .await
            .map_err(|_| ProvisionError::Internal)?
            .ok_or(ProvisionError::Internal)?;
        if (matches!(provider.provision, OidcProvisionMode::MappedRole) || mapped.is_some())
            && user.role != desired_role
        {
            match library.set_user_role(user.id, desired_role).await {
                Ok(updated) => user = updated,
                Err(LibraryError::LastOwner) => {
                    let _ = library
                        .insert_security_audit_event(
                            &format!("user:{}", user.id),
                            "oidc_role_sync_blocked",
                            Some(r#"{"reason":"last_owner"}"#),
                        )
                        .await;
                }
                Err(_) => return Err(ProvisionError::Internal),
            }
        }
        refresh_profile(library, &user, profile).await?;
        return library
            .get_user(user.id)
            .await
            .map_err(|_| ProvisionError::Internal)?
            .ok_or(ProvisionError::Internal);
    }

    if provider.link_by_email {
        if let Some(email) = verified_email {
            if let Some(existing) = library
                .get_user_by_email(email)
                .await
                .map_err(|_| ProvisionError::Internal)?
            {
                if matches!(existing.status, UserStatus::Disabled) {
                    return Err(ProvisionError::Denied);
                }
                library
                    .link_portal_identity_to_user(
                        &portal_provider,
                        &profile.sub,
                        existing.id,
                        profile.name.as_deref(),
                    )
                    .await
                    .map_err(|_| ProvisionError::Conflict)?;
                let mut user = existing;
                if (matches!(provider.provision, OidcProvisionMode::MappedRole) || mapped.is_some())
                    && user.role != desired_role
                {
                    match library.set_user_role(user.id, desired_role).await {
                        Ok(updated) => user = updated,
                        Err(LibraryError::LastOwner) => {}
                        Err(_) => return Err(ProvisionError::Internal),
                    }
                }
                refresh_profile(library, &user, profile).await?;
                return library
                    .get_user(user.id)
                    .await
                    .map_err(|_| ProvisionError::Internal)?
                    .ok_or(ProvisionError::Internal);
            }
        }
    }

    if matches!(provider.provision, OidcProvisionMode::InviteOnly) {
        return Err(ProvisionError::Denied);
    }

    let user = library
        .create_user_with_profile(
            desired_role,
            profile.name.as_deref(),
            None,
            verified_email,
            None,
        )
        .await
        .map_err(|_| ProvisionError::Conflict)?;
    library
        .link_portal_identity_to_user(
            &portal_provider,
            &profile.sub,
            user.id,
            profile.name.as_deref(),
        )
        .await
        .map_err(|_| ProvisionError::Internal)?;
    let _ = library
        .insert_security_audit_event(
            &format!("user:{}", user.id),
            "oidc_jit",
            Some(&format!(
                r#"{{"provider":"{}","role":"{}"}}"#,
                provider.id,
                desired_role.as_str()
            )),
        )
        .await;
    let _ = state;
    Ok(user)
}

/// Updates display name and verified email when the IdP profile changed.
async fn refresh_profile(
    library: &bookclerk_library::LibraryStore,
    user: &UserRecord,
    profile: &UpstreamProfile,
) -> Result<(), ProvisionError> {
    if let Some(name) = profile
        .name
        .as_deref()
        .filter(|&n| user.display_name.as_deref() != Some(n))
    {
        let _ = library.set_user_display_name(user.id, Some(name)).await;
    }
    if let Some(email) = profile
        .verified_email()
        .filter(|&e| user.email.as_deref() != Some(e))
    {
        let _ = library.set_user_email(user.id, Some(email)).await;
    }
    Ok(())
}

/// Resolved authorize, token, userinfo, and JWKS URLs for one IdP.
struct Endpoints {
    /// Authorization endpoint used for the browser redirect.
    authorize: String,
    /// Token endpoint used for the authorization-code exchange.
    token: String,
    /// UserInfo URL; empty for Apple (profile comes from the id_token).
    userinfo: String,
    /// JWKS URL for id_token signature verification; absent for GitHub/Discord.
    jwks_uri: Option<String>,
    /// Expected `iss` claim; discovery fails closed if it disagrees with the well-known issuer.
    issuer: String,
    /// Allowed id_token signing algorithms from discovery or the Apple preset.
    id_token_signing_algs: Vec<CoreJwsSigningAlgorithm>,
}

/// Uses a social preset or OpenID discovery; fails closed on issuer mismatch.
async fn resolve_endpoints(provider: &OidcProviderConfig) -> Result<Endpoints, ()> {
    match provider.social_preset() {
        Some("google") => {
            return discovery("https://accounts.google.com").await;
        }
        Some("github") => {
            return Ok(Endpoints {
                authorize: "https://github.com/login/oauth/authorize".into(),
                token: "https://github.com/login/oauth/access_token".into(),
                userinfo: "https://api.github.com/user".into(),
                jwks_uri: None,
                issuer: String::from("https://github.com"),
                id_token_signing_algs: Vec::new(),
            });
        }
        Some("apple") => {
            return Ok(Endpoints {
                authorize: "https://appleid.apple.com/auth/authorize".into(),
                token: "https://appleid.apple.com/auth/token".into(),
                userinfo: String::new(),
                jwks_uri: Some("https://appleid.apple.com/auth/keys".into()),
                issuer: String::from("https://appleid.apple.com"),
                id_token_signing_algs: vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
            });
        }
        Some("discord") => {
            return Ok(Endpoints {
                authorize: "https://discord.com/api/oauth2/authorize".into(),
                token: "https://discord.com/api/oauth2/token".into(),
                userinfo: "https://discord.com/api/users/@me".into(),
                jwks_uri: None,
                issuer: String::from("https://discord.com"),
                id_token_signing_algs: Vec::new(),
            });
        }
        _ => {}
    }
    let issuer = provider
        .issuer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(())?;
    discovery(issuer).await
}

/// Reqwest client that refuses redirects (token/userinfo must not hop).
fn http_client() -> Result<reqwest::Client, ()> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ())
}

/// Fetches `/.well-known/openid-configuration` and checks the returned issuer.
async fn discovery(issuer: &str) -> Result<Endpoints, ()> {
    let issuer = issuer.trim().trim_end_matches('/');
    let url = format!("{issuer}/.well-known/openid-configuration");
    let resp: Value = http_client()?
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?
        .json()
        .await
        .map_err(|_| ())?;
    let metadata: CoreProviderMetadata = serde_json::from_value(resp).map_err(|_| ())?;
    let discovered = metadata.issuer().as_str().trim_end_matches('/');
    if discovered != issuer {
        return Err(());
    }
    Ok(Endpoints {
        authorize: metadata.authorization_endpoint().as_str().to_string(),
        token: metadata.token_endpoint().ok_or(())?.as_str().to_string(),
        userinfo: metadata
            .userinfo_endpoint()
            .map(|u| u.as_str().to_string())
            .unwrap_or_default(),
        jwks_uri: Some(metadata.jwks_uri().as_str().to_string()),
        issuer: metadata.issuer().as_str().to_string(),
        id_token_signing_algs: metadata.id_token_signing_alg_values_supported().clone(),
    })
}

/// POSTs the authorization code and PKCE verifier to the token endpoint.
async fn exchange_code(
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<Value, ()> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    let resp = http_client()?
        .post(token_url)
        .header(header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?;
    resp.json().await.map_err(|_| ())
}

/// GETs UserInfo with the access token; fails when the URL is empty.
async fn fetch_userinfo(url: &str, access_token: &str) -> Result<Value, ()> {
    if url.trim().is_empty() {
        return Err(());
    }
    http_client()?
        .get(url)
        .bearer_auth(access_token)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "bookclerk")
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?
        .json()
        .await
        .map_err(|_| ())
}

/// Resolves the client secret from env, TOML, or a sealed store row (Apple mints a JWT).
async fn client_secret(state: &AppState, provider: &OidcProviderConfig) -> Option<String> {
    if provider.social_preset() == Some("apple") {
        return apple_client_secret(state, provider).await;
    }
    if let Ok(v) = std::env::var(oidc_client_secret_env_key(&provider.id)) {
        let trimmed = v.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if let Some(s) = provider
        .client_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(s.to_string());
    }
    let library = state.library_snapshot().await;
    let generation = live_oidc_generation(state).await;
    let store = bookclerk_library::SecretStore::new(library.db());
    if let Ok(Some(row)) = store
        .get(
            bookclerk_library::secret_kind::OIDC_CLIENT,
            Some("oidc"),
            bookclerk_library::secret_account_type::OPERATOR,
            Some("operator"),
            &oidc_secret_store_name(provider.id.trim(), generation),
        )
        .await
    {
        if let Ok(plain) = bookclerk_library::unseal_secret(&row) {
            let s = String::from_utf8_lossy(&plain).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Unseals one operator `oidc_client` row; returns `None` if missing or unseal fails.
async fn load_named_secret(state: &AppState, name: &str) -> Option<String> {
    let library = state.library_snapshot().await;
    let store = SecretStore::new(library.db());
    let row = store
        .get(
            secret_kind::OIDC_CLIENT,
            Some("oidc"),
            secret_account_type::OPERATOR,
            Some("operator"),
            name.trim(),
        )
        .await
        .ok()
        .flatten()?;
    let plain = bookclerk_library::unseal_secret(&row).ok()?;
    let s = String::from_utf8_lossy(&plain).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Resolves the Apple PEM from env, TOML, or the sealed store (operator-only).
async fn apple_private_key_pem(state: &AppState, provider: &OidcProviderConfig) -> Option<String> {
    if let Ok(v) = std::env::var(oidc_apple_private_key_env_key(&provider.id)) {
        let trimmed = v.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if let Some(s) = provider
        .apple_private_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(s.to_string());
    }
    load_named_secret(
        state,
        &oidc_secret_store_name(
            &apple_key_secret_name(&provider.id),
            live_oidc_generation(state).await,
        ),
    )
    .await
}

/// Mints a 30-day Apple client-secret JWT from team id, key id, and the PEM.
async fn apple_client_secret(state: &AppState, provider: &OidcProviderConfig) -> Option<String> {
    let team_id = provider
        .apple_team_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let key_id = provider
        .apple_key_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let pem = apple_private_key_pem(state, provider).await?;
    let now = Utc::now().timestamp();
    apple_client_secret_jwt(
        team_id,
        key_id,
        provider.client_id.trim(),
        &pem,
        now,
        now + 86400 * 30,
    )
    .ok()
}

/// Loads GitHub `/user/emails` and returns the primary verified address.
async fn fetch_github_verified_email(access_token: &str) -> Result<Option<String>, ()> {
    let emails: Value = http_client()?
        .get("https://api.github.com/user/emails")
        .bearer_auth(access_token)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "bookclerk")
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?
        .json()
        .await
        .map_err(|_| ())?;
    Ok(github_verified_email(&emails))
}

/// Trimmed `integrations.public_origin`, or loopback `http://127.0.0.1:8787`.
fn public_origin(cfg: &bookclerk_config::Config) -> String {
    cfg.integrations
        .public_origin
        .as_deref()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("http://127.0.0.1:8787"))
}

/// Gathers role/group strings from userinfo, id_token, and Keycloak `realm_access.roles`.
fn collect_role_claims(
    verified_claims: Option<&Value>,
    userinfo: &Value,
    claim: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    push_claim_values(userinfo, claim, &mut out);
    if let Some(claims) = verified_claims {
        push_claim_values(claims, claim, &mut out);
        if let Some(roles) = claims
            .pointer("/realm_access/roles")
            .and_then(Value::as_array)
        {
            for v in roles {
                if let Some(s) = v.as_str() {
                    out.push(s.to_string());
                }
            }
        }
    }
    out
}

/// Appends a string or string-array claim onto the role-mapping input.
fn push_claim_values(obj: &Value, claim: &str, out: &mut Vec<String>) {
    match obj.get(claim) {
        Some(Value::String(s)) => out.push(s.clone()),
        Some(Value::Array(arr)) => {
            for v in arr {
                if let Some(s) = v.as_str() {
                    out.push(s.to_string());
                }
            }
        }
        _ => {}
    }
}

/// S256 PKCE challenge (`BASE64URL(SHA256(verifier))`).
fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Cryptographic random bytes encoded as unpadded URL-safe Base64.
fn random_b64url(nbytes: usize) -> String {
    let mut buf = vec![0u8; nbytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// Percent-encodes a query value, leaving RFC 3986 unreserved characters.
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn pkce_is_s256() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(v),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn mapped_role_from_userinfo_groups() {
        let mut map = BTreeMap::new();
        map.insert("bookclerk-admins".into(), "administrator".into());
        map.insert("bookclerk-users".into(), "member".into());
        let info = serde_json::json!({"groups": ["bookclerk-users", "bookclerk-admins"]});
        let claims = collect_role_claims(None, &info, "groups");
        assert_eq!(
            resolve_mapped_role(&map, &claims).as_deref(),
            Some("administrator")
        );
    }

    #[test]
    fn urlencoding_percent_encodes_space() {
        assert_eq!(urlencoding("openid profile"), "openid%20profile");
    }
}

#[cfg(test)]
mod http_tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use bookclerk_config::{OidcProviderConfig, OidcProvisionMode};
    use tower::ServiceExt;

    use super::*;
    use crate::auth::OperatorAuthState;

    fn oidc_tx_cookie_header(res: &axum::http::Response<axum::body::Body>) -> String {
        res.headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|s| {
                s.split(';')
                    .next()
                    .filter(|part| part.starts_with("bookclerk_oidc_tx="))
                    .map(str::to_string)
            })
            .expect("oidc transaction cookie")
    }

    fn discovery_doc(issuer: &str) -> Value {
        serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "userinfo_endpoint": format!("{issuer}/userinfo"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"]
        })
    }

    async fn harness(
        oidc_enabled: bool,
        providers: Vec<OidcProviderConfig>,
    ) -> (Arc<AppState>, axum::Router, bookclerk_library::LibraryStore) {
        use bookclerk_config::{Config, ListenAddrs};
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::LibraryStore;
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_source::SourceRegistry;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        let _ = library.ensure_users_bridged().await;
        let mut cfg = Config::default();
        cfg.daemon.listen = ListenAddrs::parse_list("127.0.0.1:8787").unwrap();
        cfg.daemon.auth.enabled = true;
        cfg.auth.oidc.enabled = oidc_enabled;
        cfg.auth.oidc.providers = providers;

        let state = Arc::new(AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(library.clone())),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                "op-token-oidc".into(),
                12,
                true,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
            tray_handoff: Mutex::new(None),
        });
        let app = crate::api::router(state.clone(), None);
        (state, app, library)
    }

    fn github_provider() -> OidcProviderConfig {
        OidcProviderConfig {
            id: "github".into(),
            name: "GitHub".into(),
            preset: Some("github".into()),
            client_id: "test-client".into(),
            provision: OidcProvisionMode::Any,
            ..OidcProviderConfig::default()
        }
    }

    #[tokio::test]
    async fn providers_hidden_when_disabled() {
        let (_state, app, _library) = harness(false, vec![github_provider()]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["enabled"], false);
        assert_eq!(json["providers"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn github_login_redirects_to_authorize() {
        let (_state, app, _library) = harness(true, vec![github_provider()]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login?provider=github")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            loc.starts_with("https://github.com/login/oauth/authorize?"),
            "{loc}"
        );
        assert!(loc.contains("client_id=test-client"), "{loc}");
        assert!(loc.contains("code_challenge_method=S256"), "{loc}");
        assert!(!loc.contains("prompt=login"), "{loc}");
        let _ = oidc_tx_cookie_header(&res);
    }

    #[tokio::test]
    async fn mixed_case_github_preset_uses_github_endpoints_and_scopes() {
        let mut provider = github_provider();
        provider.preset = Some("GitHub".into());
        let (_state, app, _library) = harness(true, vec![provider]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login?provider=github")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            loc.starts_with("https://github.com/login/oauth/authorize?"),
            "{loc}"
        );
        assert!(loc.contains("scope=read%3Auser%20user%3Aemail"), "{loc}");
        assert!(!loc.contains("nonce="), "{loc}");
        let endpoints = resolve_endpoints(&{
            let mut p = github_provider();
            p.preset = Some("GITHUB".into());
            p
        })
        .await
        .expect("github endpoints");
        assert_eq!(
            endpoints.authorize,
            "https://github.com/login/oauth/authorize"
        );
    }

    #[tokio::test]
    async fn mixed_case_apple_preset_uses_apple_authorize() {
        let mut provider = apple_provider();
        provider.preset = Some("Apple".into());
        let endpoints = resolve_endpoints(&provider).await.expect("apple endpoints");
        assert_eq!(
            endpoints.authorize,
            "https://appleid.apple.com/auth/authorize"
        );
        let (_state, app, _library) = harness(true, vec![provider]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login?provider=apple")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            loc.starts_with("https://appleid.apple.com/auth/authorize?"),
            "{loc}"
        );
        assert!(loc.contains("response_mode=form_post"), "{loc}");
        assert!(loc.contains("nonce="), "{loc}");
    }

    #[tokio::test]
    async fn elevate_requires_owner_session() {
        let (_state, app, _library) = harness(true, vec![github_provider()]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/elevate?provider=github")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn elevate_owner_redirects_with_prompt_login() {
        use bookclerk_library::{hash_token, UserRole};
        use chrono::{Duration as ChronoDuration, Utc};
        use uuid::Uuid;

        let (_state, app, library) = harness(true, vec![github_provider()]).await;
        let owner = library
            .create_user(UserRole::Owner, Some("Owner"), None)
            .await
            .unwrap();
        library
            .link_portal_identity_to_user("oidc:github", "gh-sub", owner.id, Some("Owner"))
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(owner.id, Some("Owner"))
            .await
            .unwrap();
        let raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(12),
            )
            .await
            .unwrap();
        let cookie = format!("{}={raw}", crate::auth::PORTAL_SESSION_COOKIE);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/elevate?provider=github")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(loc.contains("prompt=login"), "{loc}");
        assert!(
            loc.starts_with("https://github.com/login/oauth/authorize?"),
            "{loc}"
        );
    }

    #[tokio::test]
    async fn login_unknown_provider_is_not_found() {
        let (_state, app, _library) = harness(true, vec![github_provider()]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login?provider=nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    async fn portal_cookie_for_user(
        library: &bookclerk_library::LibraryStore,
        role: bookclerk_library::UserRole,
        label: &str,
    ) -> String {
        use bookclerk_library::hash_token;
        use chrono::{Duration as ChronoDuration, Utc};
        use uuid::Uuid;

        let user = library.create_user(role, Some(label), None).await.unwrap();
        let identity = library
            .ensure_local_portal_identity(user.id, Some(label))
            .await
            .unwrap();
        let raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(12),
            )
            .await
            .unwrap();
        format!("{}={raw}", crate::auth::PORTAL_SESSION_COOKIE)
    }

    async fn persist_harness() -> (
        Arc<AppState>,
        axum::Router,
        bookclerk_library::LibraryStore,
        tempfile::TempDir,
        tokio::sync::MutexGuard<'static, ()>,
    ) {
        let dek = crate::auth::tests::process_dek_lock().await;
        let dir = tempfile::tempdir().unwrap();
        bookclerk_library::configure_master_key(dir.path()).unwrap();
        let (state, app, library) = harness(false, vec![]).await;
        {
            let mut cfg = state.config.write().await;
            cfg.paths = Some(bookclerk_config::Paths::from_files_dir(
                dir.path().to_path_buf(),
            ));
            cfg.write_toml_file(&cfg.paths().config_file).unwrap();
        }
        (state, app, library, dir, dek)
    }

    struct SecretMutationFailpointGuard;

    impl Drop for SecretMutationFailpointGuard {
        fn drop(&mut self) {
            SECRET_MUTATION_SUCCESSES_REMAINING.store(-1, std::sync::atomic::Ordering::SeqCst);
            CONFIG_WRITE_SUCCESSES_REMAINING.store(-1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn acquire_secret_failpoint() -> (
        tokio::sync::MutexGuard<'static, ()>,
        SecretMutationFailpointGuard,
    ) {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        let lock = LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        SECRET_MUTATION_SUCCESSES_REMAINING.store(-1, std::sync::atomic::Ordering::SeqCst);
        CONFIG_WRITE_SUCCESSES_REMAINING.store(-1, std::sync::atomic::Ordering::SeqCst);
        (lock, SecretMutationFailpointGuard)
    }

    async fn oidc_store_secret_plaintext(
        library: &bookclerk_library::LibraryStore,
        name: &str,
    ) -> Option<String> {
        let rec = bookclerk_library::SecretStore::new(library.db())
            .get(
                bookclerk_library::secret_kind::OIDC_CLIENT,
                Some("oidc"),
                bookclerk_library::secret_account_type::OPERATOR,
                Some("operator"),
                name,
            )
            .await
            .unwrap()?;
        Some(String::from_utf8(bookclerk_library::unseal_secret(&rec).unwrap()).unwrap())
    }

    async fn oidc_live_secret_plaintext(
        state: &AppState,
        library: &bookclerk_library::LibraryStore,
        logical: &str,
    ) -> Option<String> {
        let generation = state.config.read().await.auth.oidc.secret_generation;
        oidc_store_secret_plaintext(library, &oidc_secret_store_name(logical, generation)).await
    }

    async fn put_oidc_json(app: axum::Router, cookie: &str, body: &Value) -> (StatusCode, Value) {
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/oidc/config")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        (status, json_body(res).await)
    }

    async fn json_body(res: axum::response::Response) -> Value {
        let bytes = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes) }))
    }

    #[tokio::test]
    async fn oidc_config_get_requires_auth() {
        let (_state, app, _library) = harness(false, vec![]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn oidc_config_get_as_owner() {
        let (_state, app, library) = harness(true, vec![github_provider()]).await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/config")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = json_body(res).await;
        assert_eq!(json["enabled"], true);
        assert_eq!(json["providers"][0]["id"], "github");
        assert_eq!(json["providers"][0]["has_client_secret"], false);
        assert_eq!(json["providers"][0]["secret_source"], "none");
        assert!(json["providers"][0].get("client_secret").is_none());
    }

    #[tokio::test]
    async fn oidc_config_put_member_forbidden() {
        let (_state, app, library, _dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Member, "Member").await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/oidc/config")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::from(r#"{"enabled":true,"providers":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn oidc_config_put_administrator_forbidden() {
        let (_state, app, library, _dir, _dek) = persist_harness().await;
        let cookie = portal_cookie_for_user(
            &library,
            bookclerk_library::UserRole::Administrator,
            "Admin",
        )
        .await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/oidc/config")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::from(r#"{"enabled":true,"providers":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn oidc_config_put_owner_persists() {
        let (state, app, library, dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let body = serde_json::json!({
            "enabled": true,
            "allowed_email_domains": ["family.example"],
            "providers": [{
                "id": "github",
                "name": "GitHub",
                "preset": "github",
                "client_id": "ui-client",
                "provision": "any",
                "default_role": "member",
                "link_by_email": true
            }]
        });
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/oidc/config")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::COOKIE, &cookie)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let json = json_body(res).await;
        assert_eq!(status, StatusCode::OK, "{json:?}");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["providers"][0]["id"], "github");
        assert_eq!(json["providers"][0]["client_id"], "ui-client");

        let live = state.config.read().await;
        assert!(live.auth.oidc.enabled);
        assert_eq!(live.auth.oidc.providers[0].client_id, "ui-client");
        drop(live);

        let on_disk = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(on_disk.contains("id = \"github\""), "{on_disk}");
        assert!(on_disk.contains("client_id = \"ui-client\""), "{on_disk}");
        assert!(!on_disk.contains("client_secret"), "{on_disk}");
    }

    #[tokio::test]
    async fn oidc_config_put_rolls_back_after_first_secret_mutation() {
        let (_lock, _failpoint) = acquire_secret_failpoint().await;
        let (state, app, library, dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let seed = serde_json::json!({
            "enabled": true,
            "providers": [{
                "id": "github",
                "name": "GitHub",
                "preset": "github",
                "client_id": "seed-client",
                "client_secret": "original-github-secret",
                "provision": "any",
                "default_role": "member"
            }]
        });
        let (status, json) = put_oidc_json(app.clone(), &cookie, &seed).await;
        assert_eq!(status, StatusCode::OK, "{json:?}");
        assert_eq!(
            oidc_live_secret_plaintext(&state, &library, "github")
                .await
                .as_deref(),
            Some("original-github-secret")
        );

        SECRET_MUTATION_SUCCESSES_REMAINING.store(1, std::sync::atomic::Ordering::SeqCst);
        let update = serde_json::json!({
            "enabled": false,
            "providers": [
                {
                    "id": "github",
                    "name": "GitHub",
                    "preset": "github",
                    "client_id": "new-github-client",
                    "client_secret": "new-github-secret",
                    "provision": "any",
                    "default_role": "member"
                },
                {
                    "id": "discord",
                    "name": "Discord",
                    "preset": "discord",
                    "client_id": "discord-client",
                    "client_secret": "new-discord-secret",
                    "provision": "any",
                    "default_role": "member"
                }
            ]
        });
        let (status, json) = put_oidc_json(app, &cookie, &update).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json:?}");
        assert!(
            json["message"]
                .as_str()
                .unwrap_or("")
                .contains("injected oidc secret mutation failure"),
            "{json:?}"
        );

        let live = state.config.read().await;
        assert!(live.auth.oidc.enabled, "runtime config must roll back");
        assert_eq!(live.auth.oidc.providers.len(), 1);
        assert_eq!(live.auth.oidc.providers[0].id, "github");
        assert_eq!(live.auth.oidc.providers[0].client_id, "seed-client");
        drop(live);

        let on_disk = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(on_disk.contains("seed-client"), "{on_disk}");
        assert!(!on_disk.contains("discord"), "{on_disk}");
        assert!(!on_disk.contains("new-github-client"), "{on_disk}");

        assert_eq!(
            oidc_live_secret_plaintext(&state, &library, "github")
                .await
                .as_deref(),
            Some("original-github-secret")
        );
        assert!(oidc_live_secret_plaintext(&state, &library, "discord")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn oidc_config_put_rolls_back_when_secret_delete_fails() {
        let (_lock, _failpoint) = acquire_secret_failpoint().await;
        let (state, app, library, dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let seed = serde_json::json!({
            "enabled": true,
            "providers": [{
                "id": "github",
                "name": "GitHub",
                "preset": "github",
                "client_id": "keep-client",
                "client_secret": "keep-github-secret",
                "provision": "any",
                "default_role": "member"
            }]
        });
        let (status, json) = put_oidc_json(app.clone(), &cookie, &seed).await;
        assert_eq!(status, StatusCode::OK, "{json:?}");

        SECRET_MUTATION_SUCCESSES_REMAINING.store(-1, std::sync::atomic::Ordering::SeqCst);
        CONFIG_WRITE_SUCCESSES_REMAINING.store(0, std::sync::atomic::Ordering::SeqCst);
        let clear = serde_json::json!({
            "enabled": false,
            "providers": []
        });
        let (status, json) = put_oidc_json(app, &cookie, &clear).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json:?}");
        assert!(
            json["message"]
                .as_str()
                .unwrap_or("")
                .contains("injected oidc config write failure"),
            "{json:?}"
        );

        let live = state.config.read().await;
        assert!(live.auth.oidc.enabled);
        assert_eq!(live.auth.oidc.providers[0].client_id, "keep-client");
        drop(live);
        let on_disk = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(on_disk.contains("keep-client"), "{on_disk}");
        assert_eq!(
            oidc_live_secret_plaintext(&state, &library, "github")
                .await
                .as_deref(),
            Some("keep-github-secret")
        );
    }

    #[tokio::test]
    async fn oidc_config_put_fails_after_secret_mutation_at_config_write() {
        let (_lock, _failpoint) = acquire_secret_failpoint().await;
        let (state, app, library, dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let seed = serde_json::json!({
            "enabled": true,
            "providers": [{
                "id": "github",
                "name": "GitHub",
                "preset": "github",
                "client_id": "seed-client",
                "client_secret": "original-github-secret",
                "provision": "any",
                "default_role": "member"
            }]
        });
        let (status, json) = put_oidc_json(app.clone(), &cookie, &seed).await;
        assert_eq!(status, StatusCode::OK, "{json:?}");
        let published_gen = state.config.read().await.auth.oidc.secret_generation;
        assert!(published_gen > 0);

        CONFIG_WRITE_SUCCESSES_REMAINING.store(0, std::sync::atomic::Ordering::SeqCst);
        let update = serde_json::json!({
            "enabled": true,
            "providers": [{
                "id": "github",
                "name": "GitHub",
                "preset": "github",
                "client_id": "new-github-client",
                "client_secret": "new-github-secret",
                "provision": "any",
                "default_role": "member"
            }]
        });
        let (status, json) = put_oidc_json(app, &cookie, &update).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json:?}");
        assert!(
            json["message"]
                .as_str()
                .unwrap_or("")
                .contains("injected oidc config write failure"),
            "{json:?}"
        );

        let live = state.config.read().await;
        assert_eq!(live.auth.oidc.secret_generation, published_gen);
        assert_eq!(live.auth.oidc.providers[0].client_id, "seed-client");
        drop(live);

        let on_disk = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(on_disk.contains("seed-client"), "{on_disk}");
        assert!(!on_disk.contains("new-github-client"), "{on_disk}");

        assert_eq!(
            oidc_live_secret_plaintext(&state, &library, "github")
                .await
                .as_deref(),
            Some("original-github-secret")
        );
        assert!(oidc_store_secret_plaintext(
            &library,
            &oidc_secret_store_name("github", published_gen.saturating_add(1))
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn oidc_config_put_serializes_concurrent_revisions() {
        let (_lock, _failpoint) = acquire_secret_failpoint().await;
        let (state, app, library, _dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let body_a = serde_json::json!({
            "enabled": true,
            "providers": [{
                "id": "github",
                "name": "GitHub",
                "preset": "github",
                "client_id": "from-a",
                "client_secret": "secret-a",
                "provision": "any",
                "default_role": "member"
            }]
        });
        let body_b = serde_json::json!({
            "enabled": false,
            "providers": [{
                "id": "discord",
                "name": "Discord",
                "preset": "discord",
                "client_id": "from-b",
                "client_secret": "secret-b",
                "provision": "any",
                "default_role": "member"
            }]
        });
        let app_a = app.clone();
        let app_b = app;
        let cookie_a = cookie.clone();
        let cookie_b = cookie;
        let task_a = tokio::spawn(async move { put_oidc_json(app_a, &cookie_a, &body_a).await });
        let task_b = tokio::spawn(async move { put_oidc_json(app_b, &cookie_b, &body_b).await });
        let (status_a, json_a) = task_a.await.expect("put a");
        let (status_b, json_b) = task_b.await.expect("put b");
        assert_eq!(status_a, StatusCode::OK, "{json_a:?}");
        assert_eq!(status_b, StatusCode::OK, "{json_b:?}");

        let live = state.config.read().await;
        let github_revision = live.auth.oidc.enabled
            && live.auth.oidc.providers.len() == 1
            && live.auth.oidc.providers[0].id == "github"
            && live.auth.oidc.providers[0].client_id == "from-a";
        let discord_revision = !live.auth.oidc.enabled
            && live.auth.oidc.providers.len() == 1
            && live.auth.oidc.providers[0].id == "discord"
            && live.auth.oidc.providers[0].client_id == "from-b";
        assert!(
            github_revision || discord_revision,
            "mixed OIDC revision: enabled={} providers={:?}",
            live.auth.oidc.enabled,
            live.auth.oidc.providers
        );
        drop(live);

        let github_secret = oidc_live_secret_plaintext(&state, &library, "github").await;
        let discord_secret = oidc_live_secret_plaintext(&state, &library, "discord").await;
        if github_revision {
            assert_eq!(github_secret.as_deref(), Some("secret-a"));
            assert!(discord_secret.is_none());
        } else {
            assert_eq!(discord_secret.as_deref(), Some("secret-b"));
            assert!(github_secret.is_none());
        }
    }

    #[tokio::test]
    async fn settings_patch_and_oidc_put_keep_resolvable_secrets() {
        let (state, app, library, _dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let seed = serde_json::json!({
            "enabled": true,
            "providers": [{
                "id": "github",
                "name": "GitHub",
                "preset": "github",
                "client_id": "seed-client",
                "client_secret": "seed-secret",
                "provision": "any",
                "default_role": "member"
            }]
        });
        let (status, json) = put_oidc_json(app.clone(), &cookie, &seed).await;
        assert_eq!(status, StatusCode::OK, "{json:?}");

        let patch_app = app.clone();
        let oidc_app = app;
        let cookie_oidc = cookie;
        let patch_task = tokio::spawn(async move {
            patch_app
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri("/api/settings")
                        .header(axum::http::header::CONTENT_TYPE, "application/json")
                        .header(axum::http::header::AUTHORIZATION, "Bearer op-token-oidc")
                        .body(Body::from(
                            r#"{"settings":[{"key":"library.auto_acquire","value":"true"}]}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        });
        let oidc_task = tokio::spawn(async move {
            put_oidc_json(
                oidc_app,
                &cookie_oidc,
                &serde_json::json!({
                    "enabled": true,
                    "providers": [{
                        "id": "github",
                        "name": "GitHub",
                        "preset": "github",
                        "client_id": "from-put",
                        "client_secret": "put-secret",
                        "provision": "any",
                        "default_role": "member"
                    }]
                }),
            )
            .await
        });
        let patch_status = patch_task.await.expect("settings patch");
        let (oidc_status, oidc_json) = oidc_task.await.expect("oidc put");
        assert_eq!(oidc_status, StatusCode::OK, "{oidc_json:?}");
        assert!(
            patch_status.is_success() || patch_status == StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected settings patch status {patch_status}"
        );

        let secret = oidc_live_secret_plaintext(&state, &library, "github").await;
        assert!(
            secret.as_deref() == Some("put-secret") || secret.as_deref() == Some("seed-secret"),
            "live OIDC generation must still unseal, got {secret:?}"
        );
    }

    #[tokio::test]
    async fn migrate_apply_and_oidc_put_keep_resolvable_secrets() {
        let (state, app, library, _dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let seed = serde_json::json!({
            "enabled": true,
            "providers": [{
                "id": "github",
                "name": "GitHub",
                "preset": "github",
                "client_id": "seed-client",
                "client_secret": "seed-secret",
                "provision": "any",
                "default_role": "member"
            }]
        });
        let (status, json) = put_oidc_json(app.clone(), &cookie, &seed).await;
        assert_eq!(status, StatusCode::OK, "{json:?}");

        let plugin = state.config.read().await.database.plugin.clone();
        let apply_state = state.clone();
        let apply_plugin = plugin.clone();
        let oidc_app = app;
        let cookie_oidc = cookie;
        let apply_task = tokio::spawn(async move {
            crate::api::apply_migrated_database_plugin(&apply_state, apply_plugin).await
        });
        let oidc_task = tokio::spawn(async move {
            put_oidc_json(
                oidc_app,
                &cookie_oidc,
                &serde_json::json!({
                    "enabled": true,
                    "providers": [{
                        "id": "github",
                        "name": "GitHub",
                        "preset": "github",
                        "client_id": "from-put",
                        "client_secret": "put-secret",
                        "provision": "any",
                        "default_role": "member"
                    }]
                }),
            )
            .await
        });
        let apply_res = apply_task.await.expect("migrate apply");
        let (oidc_status, oidc_json) = oidc_task.await.expect("oidc put");
        assert!(apply_res.is_ok(), "{apply_res:?}");
        assert_eq!(oidc_status, StatusCode::OK, "{oidc_json:?}");

        let live_plugin = state.config.read().await.database.plugin.clone();
        assert_eq!(live_plugin, plugin);
        let secret = oidc_live_secret_plaintext(&state, &library, "github").await;
        assert!(
            secret.as_deref() == Some("put-secret") || secret.as_deref() == Some("seed-secret"),
            "live OIDC generation must still unseal, got {secret:?}"
        );
    }

    #[tokio::test]
    async fn oidc_config_put_rejects_operator_role() {
        let (_state, app, library, _dir, _dek) = persist_harness().await;
        let cookie =
            portal_cookie_for_user(&library, bookclerk_library::UserRole::Owner, "Owner").await;
        let body = serde_json::json!({
            "enabled": true,
            "providers": [{
                "id": "corp",
                "name": "Corp",
                "issuer": "https://idp.example/realms/corp",
                "client_id": "bookclerk",
                "provision": "mapped_role",
                "default_role": "operator"
            }]
        });
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/oidc/config")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let json = json_body(res).await;
        let message = json["message"].as_str().unwrap_or("");
        assert!(message.contains("operator"), "{message}");
    }

    #[tokio::test]
    async fn oidc_config_get_operator_bearer() {
        let (_state, app, _library) = harness(false, vec![]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/config")
                    .header(axum::http::header::AUTHORIZATION, "Bearer op-token-oidc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = json_body(res).await;
        assert_eq!(json["enabled"], false);
        assert_eq!(json["providers"].as_array().unwrap().len(), 0);
    }

    fn apple_provider() -> OidcProviderConfig {
        OidcProviderConfig {
            id: "apple".into(),
            name: "Apple".into(),
            preset: Some("apple".into()),
            client_id: "com.example.bookclerk".into(),
            apple_team_id: Some("TEAM123".into()),
            apple_key_id: Some("KEY456".into()),
            provision: OidcProvisionMode::Any,
            ..OidcProviderConfig::default()
        }
    }

    #[tokio::test]
    async fn apple_login_uses_form_post() {
        let (_state, app, _library) = harness(true, vec![apple_provider()]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login?provider=apple")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(loc.contains("response_mode=form_post"), "{loc}");
        assert!(loc.contains("code_challenge_method=S256"), "{loc}");
        assert!(loc.contains("nonce="), "{loc}");
    }

    #[tokio::test]
    async fn apple_callback_form_post_expired_state() {
        let (_state, app, _library) = harness(true, vec![apple_provider()]).await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/oidc/callback")
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .header(axum::http::header::COOKIE, "bookclerk_oidc_tx=missing")
                    .body(Body::from("code=abc&state=missing&user=%7B%7D"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(loc.contains("sso_error=expired"), "{loc}");
    }

    #[tokio::test]
    async fn apple_form_post_not_blocked_by_csrf_with_session_cookie() {
        let (state, app, _library) = harness(true, vec![apple_provider()]).await;
        state.config.write().await.integrations.public_origin =
            Some(String::from("https://bookclerk.example"));

        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::ORIGIN, "https://bookclerk.example")
                    .body(Body::from(r#"{"token":"op-token-oidc"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK, "{login:?}");
        let op_cookie = login
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|s| {
                s.split(';')
                    .next()
                    .filter(|part| part.starts_with("bookclerk_operator_session="))
                    .map(str::to_string)
            })
            .expect("operator session cookie");

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/oidc/callback")
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .header(axum::http::header::ORIGIN, "https://appleid.apple.com")
                    .header(
                        axum::http::header::COOKIE,
                        format!("{op_cookie}; bookclerk_oidc_tx=missing"),
                    )
                    .body(Body::from("code=abc&state=missing"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::FORBIDDEN, "{res:?}");
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{res:?}");
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(loc.contains("sso_error="), "{loc}");
    }

    #[tokio::test]
    async fn oidc_callback_without_tx_cookie_is_csrf() {
        let (_state, app, _library) = harness(true, vec![github_provider()]).await;
        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/callback?code=ok&state=abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::SEE_OTHER);
        let loc = missing
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(loc.contains("sso_error=csrf"), "{loc}");

        let mismatch = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/callback?code=ok&state=abc")
                    .header(axum::http::header::COOKIE, "bookclerk_oidc_tx=other")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mismatch.status(), StatusCode::SEE_OTHER);
        let loc = mismatch
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(loc.contains("sso_error=csrf"), "{loc}");
    }

    #[tokio::test]
    async fn oidc_config_put_stale_owner_requires_password() {
        use bookclerk_library::hash_token;
        use chrono::{Duration as ChronoDuration, Utc};
        use uuid::Uuid;

        let (_state, app, library, _dir, _dek) = persist_harness().await;
        let owner = library
            .create_user(bookclerk_library::UserRole::Owner, Some("Owner"), None)
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(owner.id, Some("Owner"))
            .await
            .unwrap();
        let raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(12),
            )
            .await
            .unwrap();
        library
            .set_portal_session_created_at(
                &hash_token(&raw),
                Utc::now() - ChronoDuration::minutes(20),
            )
            .await
            .unwrap();
        let cookie = format!("{}={raw}", crate::auth::PORTAL_SESSION_COOKIE);
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/oidc/config")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::from(r#"{"enabled":false,"providers":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mock_oidc_jit_login_issues_session() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let idp = MockServer::start().await;
        let issuer = idp.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery_doc(&issuer)))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(crate::oidc_verify::test_jwks_json()),
            )
            .mount(&idp)
            .await;

        let provider = OidcProviderConfig {
            id: "corp".into(),
            name: "Corp".into(),
            issuer: Some(issuer.clone()),
            client_id: "bookclerk".into(),
            provision: OidcProvisionMode::Any,
            default_role: "member".into(),
            ..OidcProviderConfig::default()
        };
        let (_state, app, library) = harness(true, vec![provider]).await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login?provider=corp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let tx_cookie = oidc_tx_cookie_header(&res);
        let parsed = url::Url::parse(&loc).unwrap();
        let state_raw = parsed
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("state");
        let nonce = parsed
            .query_pairs()
            .find(|(k, _)| k == "nonce")
            .map(|(_, v)| v.into_owned())
            .expect("nonce");
        let now = chrono::Utc::now().timestamp();
        let id_token = crate::oidc_verify::test_id_token(&serde_json::json!({
            "iss": issuer,
            "aud": "bookclerk",
            "sub": "corp-user-1",
            "exp": now + 600,
            "iat": now,
            "nonce": nonce,
            "email": "member@corp.example",
            "email_verified": true,
            "name": "Corp User"
        }));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-1",
                "token_type": "Bearer",
                "id_token": id_token
            })))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sub": "corp-user-1",
                "email": "member@corp.example",
                "email_verified": true,
                "name": "Corp User"
            })))
            .mount(&idp)
            .await;

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/auth/oidc/callback?code=ok&state={state_raw}"))
                    .header(axum::http::header::COOKIE, &tx_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{res:?}");
        let set_cookie = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            set_cookie.contains(crate::auth::PORTAL_SESSION_COOKIE),
            "{set_cookie}"
        );
        let users = library.list_users().await.unwrap();
        assert!(
            users
                .iter()
                .any(|u| u.email.as_deref() == Some("member@corp.example")),
            "{users:?}"
        );
    }

    #[tokio::test]
    async fn mock_oidc_token_exchange_does_not_follow_redirect() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let idp = MockServer::start().await;
        let issuer = idp.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery_doc(&issuer)))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(crate::oidc_verify::test_jwks_json()),
            )
            .mount(&idp)
            .await;

        let provider = OidcProviderConfig {
            id: "corp".into(),
            name: "Corp".into(),
            issuer: Some(issuer.clone()),
            client_id: "bookclerk".into(),
            provision: OidcProvisionMode::Any,
            default_role: "member".into(),
            ..OidcProviderConfig::default()
        };
        let (_state, app, library) = harness(true, vec![provider]).await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login?provider=corp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let tx_cookie = oidc_tx_cookie_header(&res);
        let parsed = url::Url::parse(&loc).unwrap();
        let state_raw = parsed
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("state");
        let nonce = parsed
            .query_pairs()
            .find(|(k, _)| k == "nonce")
            .map(|(_, v)| v.into_owned())
            .expect("nonce");
        let now = chrono::Utc::now().timestamp();
        let id_token = crate::oidc_verify::test_id_token(&serde_json::json!({
            "iss": issuer,
            "aud": "bookclerk",
            "sub": "corp-user-1",
            "exp": now + 600,
            "iat": now,
            "nonce": nonce,
            "email": "member@corp.example",
            "email_verified": true
        }));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", format!("{issuer}/stolen")),
            )
            .mount(&idp)
            .await;
        Mock::given(method("POST"))
            .and(path("/stolen"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-stolen",
                "token_type": "Bearer",
                "id_token": id_token
            })))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sub": "corp-user-1",
                "email": "member@corp.example",
                "email_verified": true
            })))
            .mount(&idp)
            .await;

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/auth/oidc/callback?code=ok&state={state_raw}"))
                    .header(axum::http::header::COOKIE, &tx_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY, "{res:?}");
        let users = library.list_users().await.unwrap();
        assert!(
            users
                .iter()
                .all(|u| u.email.as_deref() != Some("member@corp.example")),
            "{users:?}"
        );
    }

    #[tokio::test]
    async fn mock_oidc_maps_role_from_id_token_groups_only() {
        use std::collections::BTreeMap;

        use bookclerk_library::UserRole;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let idp = MockServer::start().await;
        let issuer = idp.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery_doc(&issuer)))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(crate::oidc_verify::test_jwks_json()),
            )
            .mount(&idp)
            .await;

        let mut role_map = BTreeMap::new();
        role_map.insert("bookclerk-admins".into(), "administrator".into());
        let provider = OidcProviderConfig {
            id: "corp".into(),
            name: "Corp".into(),
            issuer: Some(issuer.clone()),
            client_id: "bookclerk".into(),
            provision: OidcProvisionMode::MappedRole,
            role_map,
            ..OidcProviderConfig::default()
        };
        let (_state, app, library) = harness(true, vec![provider]).await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login?provider=corp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let tx_cookie = oidc_tx_cookie_header(&res);
        let parsed = url::Url::parse(&loc).unwrap();
        let state_raw = parsed
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("state");
        let nonce = parsed
            .query_pairs()
            .find(|(k, _)| k == "nonce")
            .map(|(_, v)| v.into_owned())
            .expect("nonce");
        let now = chrono::Utc::now().timestamp();
        let id_token = crate::oidc_verify::test_id_token(&serde_json::json!({
            "iss": issuer,
            "aud": "bookclerk",
            "sub": "corp-admin-1",
            "exp": now + 600,
            "iat": now,
            "nonce": nonce,
            "email": "admin@corp.example",
            "email_verified": true,
            "groups": ["bookclerk-admins"]
        }));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-1",
                "token_type": "Bearer",
                "id_token": id_token
            })))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sub": "corp-admin-1",
                "email": "admin@corp.example",
                "email_verified": true
            })))
            .mount(&idp)
            .await;

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/auth/oidc/callback?code=ok&state={state_raw}"))
                    .header(axum::http::header::COOKIE, &tx_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{res:?}");
        let users = library.list_users().await.unwrap();
        assert!(
            users.iter().any(|u| {
                u.email.as_deref() == Some("admin@corp.example")
                    && u.role == UserRole::Administrator
            }),
            "{users:?}"
        );
    }

    #[tokio::test]
    async fn mock_oidc_rejects_unverified_email_link() {
        use bookclerk_library::UserRole;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let idp = MockServer::start().await;
        let issuer = idp.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery_doc(&issuer)))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(crate::oidc_verify::test_jwks_json()),
            )
            .mount(&idp)
            .await;

        let owner = {
            let provider = OidcProviderConfig {
                id: "corp".into(),
                name: "Corp".into(),
                issuer: Some(issuer.clone()),
                client_id: "bookclerk".into(),
                provision: OidcProvisionMode::InviteOnly,
                link_by_email: true,
                ..OidcProviderConfig::default()
            };
            let (_state, app, library) = harness(true, vec![provider]).await;
            library
                .create_user_with_profile(
                    UserRole::Owner,
                    Some("Owner"),
                    None,
                    Some("owner@corp.example"),
                    None,
                )
                .await
                .unwrap();
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/auth/oidc/login?provider=corp")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let loc = res
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let tx_cookie = oidc_tx_cookie_header(&res);
            let parsed = url::Url::parse(&loc).unwrap();
            let state_raw = parsed
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            let nonce = parsed
                .query_pairs()
                .find(|(k, _)| k == "nonce")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            let now = chrono::Utc::now().timestamp();
            let id_token = crate::oidc_verify::test_id_token(&serde_json::json!({
                "iss": issuer,
                "aud": "bookclerk",
                "sub": "attacker",
                "exp": now + 600,
                "iat": now,
                "nonce": nonce,
                "email": "owner@corp.example",
                "email_verified": false
            }));
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-1",
                    "id_token": id_token
                })))
                .mount(&idp)
                .await;
            let res = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/auth/oidc/callback?code=ok&state={state_raw}"))
                        .header(axum::http::header::COOKIE, &tx_cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER);
            let loc = res
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap();
            assert!(loc.contains("sso_error=no_role"), "{loc}");
            library
        };
        let identities = owner
            .list_portal_identities_for_user(owner.list_users().await.unwrap()[0].id)
            .await
            .unwrap();
        assert!(
            identities.iter().all(|p| !p.provider.starts_with("oidc:")),
            "{identities:?}"
        );
    }
}
