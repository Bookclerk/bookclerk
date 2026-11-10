//! Guest-process helpers for the external Chirp source plugin.
//!
//! These APIs do **not** open the library DB — the host seals credentials and
//! upserts scan rows via `bookclerk_plugin_host::ExternalSource`. Conversions
//! between storefront types and the typed ABI live in [`bookclerk_storefront::abi`].

use std::path::Path;

use bookclerk_plugin_sdk::{
    FetchTitleParams, LoginParams, LoginResult, PlainFetch as AbiPlainFetch, ScanPage, ScanParams,
    SourceAccount,
};
use bookclerk_storefront::abi::{
    credentials_from_bytes, credentials_to_bytes, full_pass_page, page_cursor, scan_credential,
};
use bookclerk_storefront::{LoginOptions, PlainFetch};
use serde_json::Value;

use crate::auth::ChirpAuthFile;
use crate::client::{ChirpClient, DEFAULT_GRAPHQL_URL};
use crate::download::fetch_title_materials;
use crate::error::{ChirpError, Result};
use crate::source::ID;
use crate::sync::fetch_account_page;

/// Default GraphQL page size used when the host does not specify one.
const DEFAULT_GUEST_PAGE_SIZE: u32 = 20;

/// Login against Chirp and return account metadata + credential JSON.
///
/// Does not write to `encrypted_secrets` — the host seals `credentials`.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_login(
    graphql_url: &str,
    opts: LoginOptions,
) -> Result<(String, String, Option<String>, bool, Value)> {
    let email = opts
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ChirpError::auth("Chirp login requires email"))?;
    let password = opts
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ChirpError::auth("Chirp login requires password"))?;

    let mut client = ChirpClient::new(graphql_url);
    let user = client.login(email, password).await?;

    let marketplace = if opts.marketplace.trim().is_empty() {
        String::from("us")
    } else {
        opts.marketplace.trim().to_ascii_lowercase()
    };

    let auth = ChirpAuthFile {
        access_token: user.token,
        web_token: user.web_token,
        email: user.email,
        user_id: Some(user.id),
        marketplace: marketplace.clone(),
        label: opts.label.clone(),
    };
    let account_id = auth.account_id().to_string();
    let label = auth.label.clone().or_else(|| Some(auth.email.clone()));
    let credentials = serde_json::to_value(&auth)
        .map_err(|e| ChirpError::auth(format!("serialize Chirp auth: {e}")))?;
    Ok((account_id, marketplace, label, true, credentials))
}

/// RPC login: build [`LoginOptions`] from typed params and return the ABI result.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_login_rpc(graphql_url: &str, params: LoginParams) -> Result<LoginResult> {
    let (account_id, marketplace, label, scan_enabled, credentials) =
        guest_login(graphql_url, LoginOptions::from(params)).await?;
    let credentials =
        credentials_to_bytes(&credentials).map_err(|e| ChirpError::auth(e.to_string()))?;
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
/// Chirp has no delta API, so every pass lists the whole library and ends
/// `complete`.
///
/// # Arguments
///
/// * `graphql_url` - Chirp GraphQL endpoint.
/// * `params` - The pass's account, credential, page size and cursor.
/// * `page` - The store page the cursor names
///   ([`bookclerk_storefront::abi::page_from_cursor`]).
///
/// # Errors
///
/// Returns an error when the credential is missing or invalid, or the page
/// request fails.
pub async fn guest_scan_page(
    graphql_url: &str,
    params: &ScanParams,
    page: u32,
) -> Result<ScanPage> {
    let creds = scan_credential(params).map_err(|e| ChirpError::auth(e.to_string()))?;
    let auth: ChirpAuthFile = serde_json::from_value(creds).map_err(|e| {
        ChirpError::auth(format!(
            "invalid Chirp credentials for {}: {e}",
            params.account_id
        ))
    })?;
    let page_size = if params.page_size == 0 {
        DEFAULT_GUEST_PAGE_SIZE
    } else {
        params.page_size
    };
    let client = ChirpClient::new(graphql_url).with_token(&auth.access_token);
    let (books, next) = fetch_account_page(
        &client,
        &params.account_id,
        &auth.marketplace,
        page,
        page_size,
    )
    .await?;
    Ok(full_pass_page(books, next.map(page_cursor), 1))
}

/// Download one title into `cache_dir` using host-injected credentials.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_fetch_title(
    graphql_url: &str,
    credentials: &Value,
    title_id: &str,
    cache_dir: &Path,
) -> Result<PlainFetch> {
    let auth: ChirpAuthFile = serde_json::from_value(credentials.clone())
        .map_err(|e| ChirpError::auth(format!("invalid Chirp credentials: {e}")))?;
    let client = ChirpClient::new(graphql_url).with_token(&auth.access_token);
    fetch_title_materials(&client, title_id, cache_dir).await
}

/// RPC fetch: return the typed plain fetch result.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_fetch_title_rpc(
    graphql_url: &str,
    params: &FetchTitleParams,
) -> Result<AbiPlainFetch> {
    let creds = params
        .credentials
        .as_deref()
        .ok_or_else(|| ChirpError::auth("fetch_title requires host credentials"))?;
    let creds = credentials_from_bytes(creds).map_err(|e| ChirpError::auth(e.to_string()))?;
    let work_dir = bookclerk_plugin_sdk::fetch_work_dir(params)
        .map_err(|err| ChirpError::api(format!("fetch work directory: {err}")))?;
    let plain = guest_fetch_title(graphql_url, &creds, &params.title_id, &work_dir).await?;
    Ok(plain.into())
}

/// Resolve GraphQL endpoint (tests may override via the plugin settings document).
#[must_use]
pub fn resolve_graphql_url(source_config: &Value) -> String {
    source_config
        .get("graphql_url")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GRAPHQL_URL.to_string())
}

/// Source id returned in describe(); must match [`crate::source::ID`].
#[must_use]
pub fn source_id() -> &'static str {
    ID
}
