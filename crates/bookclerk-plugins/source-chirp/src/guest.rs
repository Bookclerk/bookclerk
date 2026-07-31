//! Guest-process helpers for the external Chirp source plugin.
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

use crate::auth::ChirpAuthFile;
use crate::client::{ChirpClient, DEFAULT_GRAPHQL_URL};
use crate::download::fetch_title_materials;
use crate::error::{ChirpError, Result};
use crate::source::ID;
use crate::sync::collect_account_books;

/// Default GraphQL page size used when the host does not specify one.
const DEFAULT_GUEST_PAGE_SIZE: u32 = 20;

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

/// Login against Chirp and return account metadata + credential JSON.
///
/// Does not write to `encrypted_secrets` — the host seals `credentials`.
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

/// RPC login: build [`LoginOptions`] from params and return a protocol DTO.
pub async fn guest_login_rpc(graphql_url: &str, params: LoginParams) -> Result<LoginResultDto> {
    let (account_id, marketplace, label, scan_enabled, credentials) = guest_login(
        graphql_url,
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
pub async fn guest_scan(
    graphql_url: &str,
    credentials: &BTreeMap<String, Value>,
    account_filter: &[String],
    page_size: u32,
) -> Result<(Vec<NewBook>, usize, u32)> {
    if credentials.is_empty() {
        return Err(ChirpError::no_accounts(
            "no Chirp credentials from host — run login first",
        ));
    }
    let explicit = !account_filter.is_empty();
    let page_size = if page_size == 0 {
        DEFAULT_GUEST_PAGE_SIZE
    } else {
        page_size
    };
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
        let auth: ChirpAuthFile = serde_json::from_value(creds.clone()).map_err(|e| {
            ChirpError::auth(format!("invalid Chirp credentials for {account_id}: {e}"))
        })?;
        let client = ChirpClient::new(graphql_url).with_token(&auth.access_token);
        let (batch, p) =
            collect_account_books(&client, account_id, &auth.marketplace, page_size).await?;
        pages = pages.saturating_add(p);
        accounts += 1;
        books.extend(batch);
    }

    if accounts == 0 {
        return Err(ChirpError::no_accounts(
            "no matching Chirp accounts in host credentials",
        ));
    }
    Ok((books, accounts, pages))
}

/// RPC scan: return protocol [`ScanSummaryDto`] (host upserts books).
pub async fn guest_scan_rpc(graphql_url: &str, params: &ScanParams) -> Result<ScanSummaryDto> {
    let (books, accounts, pages) = guest_scan(
        graphql_url,
        &params.credentials,
        &params.accounts,
        params.page_size,
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

/// RPC fetch: return protocol [`SourceFetchDto`].
pub async fn guest_fetch_title_rpc(
    graphql_url: &str,
    params: &FetchTitleParams,
) -> Result<SourceFetchDto> {
    let creds = params
        .credentials
        .as_ref()
        .ok_or_else(|| ChirpError::auth("fetch_title requires host credentials"))?;
    let work_dir = bookclerk_plugin_sdk::fetch_work_dir(params)
        .map_err(|err| ChirpError::api(format!("fetch work directory: {err}")))?;
    let plain = guest_fetch_title(graphql_url, creds, &params.title_id, &work_dir).await?;
    Ok(plain_to_dto(plain))
}

/// Resolve GraphQL endpoint (tests may override via `[sources.chirp]` JSON).
#[must_use]
pub fn resolve_graphql_url(source_config: &Value) -> String {
    source_config
        .get("graphql_url")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GRAPHQL_URL.to_string())
}

/// Canonical source id for handshake.
#[must_use]
pub fn source_id() -> &'static str {
    ID
}
