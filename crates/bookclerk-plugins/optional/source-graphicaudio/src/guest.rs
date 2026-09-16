//! Guest-process helpers for the external GraphicAudio source plugin.
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
use serde_json::Value;

use crate::auth::GraphicAudioAuthFile;
use crate::client::{GraphicAudioClient, DEFAULT_BASE_URL};
use crate::download::{
    fetch_title_with_mode, password_from_env, product_title_for, TitleFetchRequest,
};
use crate::error::{GraphicAudioError, Result};
use crate::magento::{MagentoClient, DEFAULT_STORE_URL};
use crate::options::{GraphicAudioAccess, GraphicAudioBitrate, GraphicAudioContainer};
use crate::source::ID;
use crate::sync::collect_account_books;

/// Login against GraphicAudio and return account metadata + credential JSON.
///
/// - `access=web|zip`: Magento customer login only (no Access App device slot).
/// - `access=device`: Access App `activation/login` (registers a device).
///
/// Does not write to `encrypted_secrets` — the host seals `credentials`.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_login(
    access_base_url: &str,
    store_base_url: &str,
    access: GraphicAudioAccess,
    opts: LoginOptions,
) -> Result<(String, String, Option<String>, bool, Value)> {
    let email = opts
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GraphicAudioError::auth("GraphicAudio login requires email"))?;
    let password = opts
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GraphicAudioError::auth("GraphicAudio login requires password"))?;

    let marketplace = if opts.marketplace.trim().is_empty() {
        String::from("us")
    } else {
        opts.marketplace.trim().to_ascii_lowercase()
    };

    let client_id = format!("bookclerk-{}", uuid::Uuid::new_v4());
    let (token, client_id) = match access {
        GraphicAudioAccess::Device => {
            let mut client = GraphicAudioClient::new(access_base_url);
            let token = client.login(email, password, &client_id).await?;
            (token, client_id)
        }
        GraphicAudioAccess::Web | GraphicAudioAccess::Zip => {
            let store = MagentoClient::new(store_base_url)?;
            store.login(email, password).await?;
            // Magento-only: empty Access App token (same as in-process source).
            (String::new(), client_id)
        }
    };

    let auth = GraphicAudioAuthFile {
        token,
        client_id,
        email: email.to_string(),
        marketplace: marketplace.clone(),
        label: opts.label.clone(),
    };
    let account_id = auth.account_id().to_string();
    let label = auth.label.clone().or_else(|| Some(auth.email.clone()));
    let credentials = serde_json::to_value(&auth)
        .map_err(|e| GraphicAudioError::auth(format!("serialize GraphicAudio auth: {e}")))?;
    Ok((account_id, marketplace, label, true, credentials))
}

/// RPC login: build [`LoginOptions`] from typed params and return the ABI result.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_login_rpc(
    access_base_url: &str,
    store_base_url: &str,
    access: GraphicAudioAccess,
    params: LoginParams,
) -> Result<LoginResult> {
    let (account_id, marketplace, label, scan_enabled, credentials) = guest_login(
        access_base_url,
        store_base_url,
        access,
        LoginOptions::from(params),
    )
    .await?;
    let credentials =
        credentials_to_bytes(&credentials).map_err(|e| GraphicAudioError::auth(e.to_string()))?;
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
/// `magento_password` is required for Magento web/zip listing unless a device
/// token is present on the credentials (legacy Access App fallback).
///
/// # Errors
///
/// Returns an error when the operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn guest_scan(
    access_base_url: &str,
    store_base_url: &str,
    access: GraphicAudioAccess,
    magento_password: Option<&str>,
    credentials: &BTreeMap<String, Value>,
    account_filter: &[String],
    include_samples: bool,
) -> Result<(Vec<NewBook>, usize, u32)> {
    if credentials.is_empty() {
        return Err(GraphicAudioError::no_accounts(
            "no GraphicAudio credentials from host — run login first",
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
        let auth: GraphicAudioAuthFile = serde_json::from_value(creds.clone()).map_err(|e| {
            GraphicAudioError::auth(format!(
                "invalid GraphicAudio credentials for {account_id}: {e}"
            ))
        })?;
        let (batch, p) = collect_account_books(
            &auth,
            access_base_url,
            store_base_url,
            access,
            magento_password,
            include_samples,
            account_id,
            &auth.marketplace,
        )
        .await?;
        pages = pages.saturating_add(p);
        accounts += 1;
        books.extend(batch);
    }

    if accounts == 0 {
        return Err(GraphicAudioError::no_accounts(
            "no matching GraphicAudio accounts in host credentials",
        ));
    }
    Ok((books, accounts, pages))
}

/// RPC scan: return the typed [`ScanSummary`] (host upserts books).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_scan_rpc(
    access_base_url: &str,
    store_base_url: &str,
    access: GraphicAudioAccess,
    magento_password: Option<&str>,
    params: &ScanParams,
) -> Result<ScanSummary> {
    let credentials = account_credentials_json(&params.credentials)
        .map_err(|e| GraphicAudioError::auth(e.to_string()))?;
    let (books, accounts, pages) = guest_scan(
        access_base_url,
        store_base_url,
        access,
        magento_password,
        &credentials,
        &params.accounts,
        params.import_plus_titles,
    )
    .await?;
    Ok(scan_summary(books, accounts, pages))
}

/// Download one title into `cache_dir` using host-injected credentials.
///
/// # Errors
///
/// Returns an error when the operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn guest_fetch_title(
    access_base_url: &str,
    store_base_url: &str,
    credentials: &Value,
    title_id: &str,
    cache_dir: &Path,
    access: GraphicAudioAccess,
    bitrate: GraphicAudioBitrate,
    container: GraphicAudioContainer,
    magento_password: Option<&str>,
) -> Result<PlainFetch> {
    let auth: GraphicAudioAuthFile = serde_json::from_value(credentials.clone())
        .map_err(|e| GraphicAudioError::auth(format!("invalid GraphicAudio credentials: {e}")))?;
    let client = GraphicAudioClient::new(access_base_url).with_token(&auth.token);
    let prefer_hi = bitrate.prefers_hi();

    let product_title = if matches!(access, GraphicAudioAccess::Zip) && auth.has_device_token() {
        match product_title_for(&client, title_id).await {
            Ok(t) => t,
            Err(err) => {
                tracing::debug!(error = %err, "could not resolve GraphicAudio product title");
                None
            }
        }
    } else {
        None
    };

    let password = magento_password
        .map(str::to_string)
        .or_else(password_from_env);

    fetch_title_with_mode(
        &client,
        TitleFetchRequest {
            store_base_url,
            email: &auth.email,
            product_id: title_id,
            product_title: product_title.as_deref(),
            cache_dir,
            prefer_hi,
            mode: access,
            password: password.as_deref(),
            zip_container: container,
        },
    )
    .await
}

/// RPC fetch: return the typed plain fetch result.
///
/// # Errors
///
/// Returns an error when the operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn guest_fetch_title_rpc(
    access_base_url: &str,
    store_base_url: &str,
    params: &FetchTitleParams,
    access: GraphicAudioAccess,
    bitrate: GraphicAudioBitrate,
    container: GraphicAudioContainer,
    magento_password: Option<&str>,
) -> Result<AbiPlainFetch> {
    let creds = params
        .credentials
        .as_deref()
        .ok_or_else(|| GraphicAudioError::auth("fetch_title requires host credentials"))?;
    let creds =
        credentials_from_bytes(creds).map_err(|e| GraphicAudioError::auth(e.to_string()))?;
    let work_dir = bookclerk_plugin_sdk::fetch_work_dir(params)
        .map_err(|err| GraphicAudioError::api(format!("fetch work directory: {err}")))?;
    let plain = guest_fetch_title(
        access_base_url,
        store_base_url,
        &creds,
        &params.title_id,
        &work_dir,
        access,
        bitrate,
        container,
        magento_password,
    )
    .await?;
    Ok(plain.into())
}

/// Resolve Access App API base URL from `[sources.graphicaudio]` JSON.
#[must_use]
pub fn resolve_access_base_url(source_config: &Value) -> String {
    source_config
        .get("base_url")
        .or_else(|| source_config.get("access_base_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// Resolve Magento storefront base URL from `[sources.graphicaudio]` JSON.
#[must_use]
pub fn resolve_store_base_url(source_config: &Value) -> String {
    source_config
        .get("store_url")
        .or_else(|| source_config.get("store_base_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_STORE_URL.to_string())
}

/// Resolve download access path from `[sources.graphicaudio]` JSON / env.
#[must_use]
pub fn resolve_access(source_config: &Value) -> GraphicAudioAccess {
    source_config
        .get("access")
        .and_then(|v| v.as_str())
        .and_then(GraphicAudioAccess::parse)
        .or_else(GraphicAudioAccess::from_env)
        .unwrap_or_default()
}

/// Resolve device bitrate preference from `[sources.graphicaudio]` JSON.
#[must_use]
pub fn resolve_bitrate(source_config: &Value) -> GraphicAudioBitrate {
    source_config
        .get("bitrate")
        .and_then(|v| v.as_str())
        .and_then(GraphicAudioBitrate::parse)
        .unwrap_or_default()
}

/// Resolve ZIP container preference from `[sources.graphicaudio]` JSON.
#[must_use]
pub fn resolve_container(source_config: &Value) -> GraphicAudioContainer {
    source_config
        .get("container")
        .and_then(|v| v.as_str())
        .and_then(GraphicAudioContainer::parse)
        .unwrap_or_default()
}

/// Source id returned in describe(); must match [`crate::source::ID`].
#[must_use]
pub fn source_id() -> &'static str {
    ID
}
