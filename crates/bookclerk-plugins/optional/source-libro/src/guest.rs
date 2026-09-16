//! Guest-process helpers for the external Libro.fm source plugin.
//!
//! These APIs do **not** open the library DB — the host seals credentials and
//! upserts scan rows via `bookclerk_plugin_host::ExternalSource`. Conversions
//! between storefront types and the typed ABI live in [`bookclerk_source::abi`].

use std::collections::BTreeMap;
use std::path::Path;

use bookclerk_library::NewBook;
use bookclerk_plugin_sdk::{
    FetchTitleParams, LoginParams, LoginResult, PlainFetch as AbiPlainFetch, ScanParams,
    ScanSummary, SourceAccount,
};
use bookclerk_source::abi::{
    account_credentials_json, credentials_from_bytes, credentials_to_bytes, scan_summary,
};
use bookclerk_source::{LoginOptions, PlainFetch};
use chrono::{Duration, TimeZone, Utc};
use serde_json::Value;

use crate::auth::LibroAuthFile;
use crate::client::{LibroClient, DEFAULT_BASE_URL};
use crate::container::LibroContainer;
use crate::download::fetch_title_materials_with;
use crate::error::{LibroError, Result};
use crate::source::ID;
use crate::sync::collect_account_books;

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

/// Scan libraries for the credential blobs the host injected.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_scan(
    base_url: &str,
    credentials: &BTreeMap<String, Value>,
    account_filter: &[String],
) -> Result<(Vec<NewBook>, usize, u32)> {
    if credentials.is_empty() {
        return Err(LibroError::no_accounts(
            "no Libro.fm credentials from host — run login first",
        ));
    }
    let explicit = !account_filter.is_empty();
    let mut books = Vec::new();
    let mut accounts = 0usize;
    let mut pages = 0u32;

    for (account_id, creds) in credentials {
        if explicit
            && !account_filter
                .iter()
                .any(|n| n.eq_ignore_ascii_case(account_id))
        {
            continue;
        }
        let auth: LibroAuthFile = serde_json::from_value(creds.clone()).map_err(|e| {
            LibroError::auth(format!("invalid Libro credentials for {account_id}: {e}"))
        })?;
        let client = LibroClient::new(base_url).with_token(&auth.access_token);
        let (batch, p) = collect_account_books(&client, account_id, &auth.marketplace).await?;
        pages = pages.saturating_add(p);
        accounts += 1;
        books.extend(batch);
    }

    if accounts == 0 {
        return Err(LibroError::no_accounts(
            "no matching Libro.fm accounts in host credentials",
        ));
    }
    Ok((books, accounts, pages))
}

/// RPC scan: return the typed [`ScanSummary`] (host upserts books).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_scan_rpc(base_url: &str, params: &ScanParams) -> Result<ScanSummary> {
    let credentials = account_credentials_json(&params.credentials)
        .map_err(|e| LibroError::auth(e.to_string()))?;
    let (books, accounts, pages) = guest_scan(base_url, &credentials, &params.accounts).await?;
    Ok(scan_summary(books, accounts, pages))
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

/// Resolve download container from `[sources.libro]` JSON.
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
