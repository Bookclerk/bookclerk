//! Guest-process helpers for the external GraphicAudio source plugin.
//!
//! These APIs do **not** open the library DB — the host seals credentials and
//! upserts scan DTOs via [`bookclerk_plugin::ExternalSource`].

use std::collections::BTreeMap;
use std::path::Path;

use bookclerk_library::NewBook;
use bookclerk_plugin_sdk::{
    CatalogHitDto, FetchTitleParams, LoginParams, LoginResultDto, PlainPartDto, PurchaseHintDto,
    ScanBookDto, ScanParams, ScanSummaryDto, SourceAccountDto, SourceFetchDto,
};
use bookclerk_source::{CatalogHit, LoginOptions, PlainFetch, SourcePurchaseHint};
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

/// Map a library [`NewBook`] to the plugin-protocol scan DTO.
#[must_use]
pub fn new_book_to_scan(book: NewBook) -> ScanBookDto {
    ScanBookDto {
        account_id: book.account_id,
        product_id: book.product_id,
        title: book.title,
        marketplace: Some(book.marketplace),
        asin: book.asin,
        isbn: book.isbn,
        authors: book.authors,
        narrators: book.narrators,
        series: book.series,
        series_index: book.series_index,
        content_kind: Some(book.content_kind),
        publisher: book.publisher,
        length_minutes: book.length_minutes,
        subtitle: book.subtitle,
    }
}

/// Map a DRM-free fetch result to the plugin-protocol DTO.
#[must_use]
pub fn plain_to_dto(plain: PlainFetch) -> SourceFetchDto {
    SourceFetchDto::Plain {
        parts: plain
            .parts
            .into_iter()
            .map(|p| PlainPartDto {
                path: p.path.display().to_string(),
                title: p.title,
                duration_ms: p.duration_ms,
            })
            .collect(),
        m4b_path: plain.m4b_path.map(|p| p.display().to_string()),
        cover_path: plain.cover_path.map(|p| p.display().to_string()),
        chapters: plain.chapters,
        pdf_url: plain.pdf_url,
    }
}

/// Map a catalog hit to the plugin-protocol DTO.
#[must_use]
pub fn catalog_hit_to_dto(hit: CatalogHit) -> CatalogHitDto {
    CatalogHitDto {
        product_id: hit.product_id,
        title: hit.title,
        authors: hit.authors,
        narrators: hit.narrators,
        series: hit.series,
        series_index: hit.series_index,
        asin: hit.asin,
        isbn: hit.isbn,
        url: hit.url,
        origin: hit.origin,
    }
}

/// Map a purchase hint to the plugin-protocol DTO.
#[must_use]
pub fn purchase_hint_to_dto(hint: SourcePurchaseHint) -> PurchaseHintDto {
    PurchaseHintDto {
        product_id: hint.product_id,
        title: hint.title,
        url: hint.url,
        price_cents: hint.price_cents,
        currency: hint.currency,
        price_label: hint.price_label,
    }
}

/// Login against GraphicAudio and return account metadata + credential JSON.
///
/// - `access=web|zip`: Magento customer login only (no Access App device slot).
/// - `access=device`: Access App `activation/login` (registers a device).
///
/// Does not write to `encrypted_secrets` — the host seals `credentials`.
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

/// RPC login: build [`LoginOptions`] from params and return a protocol DTO.
pub async fn guest_login_rpc(
    access_base_url: &str,
    store_base_url: &str,
    access: GraphicAudioAccess,
    params: LoginParams,
) -> Result<LoginResultDto> {
    let (account_id, marketplace, label, scan_enabled, credentials) = guest_login(
        access_base_url,
        store_base_url,
        access,
        LoginOptions {
            marketplace: params.marketplace,
            label: params.label,
            email: params.email,
            password: params.password,
            force: params.force,
            callback_bind: params.callback_bind,
            ..Default::default()
        },
    )
    .await?;
    Ok(LoginResultDto {
        account: SourceAccountDto {
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

/// RPC scan: return protocol [`ScanSummaryDto`] (host upserts books).
pub async fn guest_scan_rpc(
    access_base_url: &str,
    store_base_url: &str,
    access: GraphicAudioAccess,
    magento_password: Option<&str>,
    params: &ScanParams,
) -> Result<ScanSummaryDto> {
    let (books, accounts, pages) = guest_scan(
        access_base_url,
        store_base_url,
        access,
        magento_password,
        &params.credentials,
        &params.accounts,
        params.import_plus_titles,
    )
    .await?;
    let n = books.len();
    Ok(ScanSummaryDto {
        accounts,
        books_upserted: n,
        pages,
        skipped_disabled: 0,
        books: books.into_iter().map(new_book_to_scan).collect(),
    })
}

/// Download one title into `cache_dir` using host-injected credentials.
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

/// RPC fetch: return protocol [`SourceFetchDto`].
#[allow(clippy::too_many_arguments)]
pub async fn guest_fetch_title_rpc(
    access_base_url: &str,
    store_base_url: &str,
    params: &FetchTitleParams,
    access: GraphicAudioAccess,
    bitrate: GraphicAudioBitrate,
    container: GraphicAudioContainer,
    magento_password: Option<&str>,
) -> Result<SourceFetchDto> {
    let creds = params
        .credentials
        .as_ref()
        .ok_or_else(|| GraphicAudioError::auth("fetch_title requires host credentials"))?;
    let plain = guest_fetch_title(
        access_base_url,
        store_base_url,
        creds,
        &params.title_id,
        Path::new(&params.cache_dir),
        access,
        bitrate,
        container,
        magento_password,
    )
    .await?;
    Ok(plain_to_dto(plain))
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

/// Canonical source id for handshake.
#[must_use]
pub fn source_id() -> &'static str {
    ID
}
