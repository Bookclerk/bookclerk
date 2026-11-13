//! Guest-process helpers for the external Libro.fm source plugin.
//!
//! These APIs do **not** open the library DB — the host seals credentials and
//! upserts scan rows via `bookclerk_plugin_host::ExternalSource`. Conversions
//! between storefront types and the typed ABI live in [`bookclerk_storefront::abi`].

use std::path::Path;

use bookclerk_plugin_sdk::{
    scan_page, FetchTitleParams, KvNamespace, LoginParams, LoginResult,
    PlainFetch as AbiPlainFetch, PluginError, ScanPage, ScanParams, SourceAccount, StorePage,
};
use bookclerk_storefront::abi::{
    credentials_from_bytes, credentials_to_bytes, scan_credential, store_page_cursor,
    store_page_from_cursor,
};
use bookclerk_storefront::scan_delta::ScanDelta;
use bookclerk_storefront::{LoginOptions, PlainFetch};
use chrono::{Duration, TimeZone, Utc};
use serde_json::Value;

use crate::auth::LibroAuthFile;
use crate::client::{LibroClient, DEFAULT_BASE_URL};
use crate::container::LibroContainer;
use crate::download::fetch_title_materials_with;
use crate::error::{LibroError, Result};
use crate::source::ID;
use crate::sync::fetch_account_page;

/// Login against Libro.fm and return account metadata + credential JSON.
///
/// Does not write to `encrypted_secrets` — the host seals `credentials`.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_login(
    base_url: &str,
    opts: LoginOptions,
) -> Result<(String, String, Option<String>, bool, Value)> {
    let email = opts
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LibroError::auth("Libro.fm login requires email"))?;
    let password = opts
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LibroError::auth("Libro.fm login requires password"))?;

    let mut client = LibroClient::new(base_url);
    let token = client.login(email, password).await?;

    let expires_at = match (token.created_at, token.expires_in) {
        (Some(created), Some(expires_in)) => Utc
            .timestamp_opt(created, 0)
            .single()
            .map(|t| t + Duration::seconds(expires_in)),
        (None, Some(expires_in)) => Some(Utc::now() + Duration::seconds(expires_in)),
        _ => None,
    };

    let marketplace = if opts.marketplace.trim().is_empty() {
        String::from("us")
    } else {
        opts.marketplace.trim().to_ascii_lowercase()
    };

    let auth = LibroAuthFile {
        access_token: token.access_token,
        token_type: token.token_type,
        expires_at,
        email: email.to_string(),
        user_id: None,
        marketplace: marketplace.clone(),
        label: opts.label.clone(),
    };
    let account_id = auth.account_id().to_string();
    let label = auth.label.clone().or_else(|| Some(auth.email.clone()));
    let credentials = serde_json::to_value(&auth)
        .map_err(|e| LibroError::auth(format!("serialize Libro auth: {e}")))?;
    Ok((account_id, marketplace, label, true, credentials))
}

/// RPC login: build [`LoginOptions`] from typed params and return the ABI result.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_login_rpc(base_url: &str, params: LoginParams) -> Result<LoginResult> {
    let (account_id, marketplace, label, scan_enabled, credentials) =
        guest_login(base_url, LoginOptions::from(params)).await?;
    let credentials =
        credentials_to_bytes(&credentials).map_err(|e| LibroError::auth(e.to_string()))?;
    Ok(LoginResult {
        account: SourceAccount {
            account_id,
            source: ID.into(),
            marketplace,
            label,
            scan_enabled,
        },
        credentials: Some(credentials),
    })
}

/// RPC scan: one page of one account's full pass (the host upserts books).
///
/// Libro.fm has no delta API, so every pass fetches the whole library. With
/// the `SCAN_STATE` namespace, [`ScanDelta`] sends the host only titles that
/// are new or changed since the last pass, and the product ids that
/// disappeared; without it every pass is full and ends `complete`. The SDK's
/// [`scan_page`] helper pages the store's numbered library pages into scan
/// replies.
///
/// # Arguments
///
/// * `base_url` - Libro.fm API base URL.
/// * `params` - The pass's account, credential, cursor and sync token.
/// * `scan_state` - The `SCAN_STATE` namespace, when granted.
///
/// # Errors
///
/// Returns `invalid_cursor` for a cursor or sync token this plugin cannot
/// use, the namespace's error when scan state cannot be kept, and
/// `internal` when the credential is missing or invalid or the page request
/// fails.
pub async fn guest_scan_page(
    base_url: &str,
    params: &ScanParams,
    scan_state: Option<&dyn KvNamespace>,
) -> std::result::Result<ScanPage, PluginError> {
    let delta = &ScanDelta::open(scan_state, params).await?;
    scan_page(params, |store_cursor, _| async move {
        let page = store_page_from_cursor(&store_cursor)?;
        let (books, next) = fetch_scan_page(base_url, params, page)
            .await
            .map_err(|err| PluginError::internal(err.to_string()))?;
        let next = store_page_cursor(next);
        let page = delta.page(&store_cursor, books, next.is_empty()).await?;
        Ok(StorePage {
            books: page.books,
            removed: page.removed,
            next,
            sync_token: page.sync_token,
            complete: page.complete,
            store_pages: None,
        })
    })
    .await
}

/// Fetches one numbered library page with the pass's credential.
///
/// # Errors
///
/// Returns an error when the credential is missing or invalid, or the page
/// request fails.
async fn fetch_scan_page(
    base_url: &str,
    params: &ScanParams,
    page: u32,
) -> Result<(Vec<bookclerk_plugin_sdk::ScanBook>, Option<u32>)> {
    let creds = scan_credential(params).map_err(|e| LibroError::auth(e.to_string()))?;
    let auth: LibroAuthFile = serde_json::from_value(creds).map_err(|e| {
        LibroError::auth(format!(
            "invalid Libro credentials for {}: {e}",
            params.account_id
        ))
    })?;
    let client = LibroClient::new(base_url).with_token(&auth.access_token);
    fetch_account_page(&client, &params.account_id, &auth.marketplace, page).await
}

/// Download one title into `cache_dir` using host-injected credentials.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_fetch_title(
    base_url: &str,
    credentials: &Value,
    title_id: &str,
    cache_dir: &Path,
    container: LibroContainer,
) -> Result<PlainFetch> {
    let auth: LibroAuthFile = serde_json::from_value(credentials.clone())
        .map_err(|e| LibroError::auth(format!("invalid Libro credentials: {e}")))?;
    let client = LibroClient::new(base_url).with_token(&auth.access_token);
    fetch_title_materials_with(&client, title_id, cache_dir, container).await
}

/// RPC fetch: return the typed plain fetch result.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_fetch_title_rpc(
    base_url: &str,
    params: &FetchTitleParams,
    container: LibroContainer,
) -> Result<AbiPlainFetch> {
    let creds = params
        .credentials
        .as_deref()
        .ok_or_else(|| LibroError::auth("fetch_title requires host credentials"))?;
    let creds = credentials_from_bytes(creds).map_err(|e| LibroError::auth(e.to_string()))?;
    let work_dir = bookclerk_plugin_sdk::fetch_work_dir(params)
        .map_err(|err| LibroError::api(format!("fetch work directory: {err}")))?;
    let plain = guest_fetch_title(base_url, &creds, &params.title_id, &work_dir, container).await?;
    Ok(plain.into())
}

/// Resolve API base URL (tests may override).
#[must_use]
pub fn resolve_base_url(source_config: &Value) -> String {
    source_config
        .get("base_url")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// Resolve download container from the plugin settings document.
#[must_use]
pub fn resolve_container(source_config: &Value) -> LibroContainer {
    source_config
        .get("container")
        .and_then(|v| v.as_str())
        .and_then(LibroContainer::parse)
        .unwrap_or_default()
}

/// Source id returned in describe(); must match [`crate::source::ID`].
#[must_use]
pub fn source_id() -> &'static str {
    ID
}
