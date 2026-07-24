//! Library scan: fetch GraphicAudio products and upsert owned titles.

use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use libation_config::GraphicAudioAccess;
use libation_library::{LibraryStore, NewBook};
use libation_source::ScanSummary;

use crate::auth::{find_auth_file, list_auth_files, load_auth, GraphicAudioAuthFile};
use crate::client::{GraphicAudioClient, Product};
use crate::error::{GraphicAudioError, Result};
use crate::magento::{LibraryItem, MagentoClient};

/// Options for a GraphicAudio library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Limit to specific account emails / labels / ids. Empty = all auth files.
    pub accounts: Vec<String>,
    /// When true, also import promotional `Type=sample` entries (default: false).
    pub include_samples: bool,
    pub page_size: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            include_samples: false,
            page_size: 50,
        }
    }
}

impl From<&libation_source::ScanOptions> for ScanOptions {
    fn from(opts: &libation_source::ScanOptions) -> Self {
        Self {
            accounts: opts.accounts.clone(),
            include_samples: false,
            page_size: opts.page_size,
        }
    }
}

/// Runtime context for [`scan_library`] (Access App vs Magento).
#[derive(Debug, Clone, Copy)]
pub struct ScanContext<'a> {
    pub access_base_url: Option<&'a str>,
    pub store_base_url: Option<&'a str>,
    pub access: GraphicAudioAccess,
    pub magento_password: Option<&'a str>,
}

impl Default for ScanContext<'_> {
    fn default() -> Self {
        Self {
            access_base_url: None,
            store_base_url: None,
            access: GraphicAudioAccess::Web,
            magento_password: None,
        }
    }
}

/// Sync GraphicAudio libraries for configured accounts into `library`.
pub async fn scan_library(
    files_dir: &Path,
    library: &LibraryStore,
    options: ScanOptions,
    ctx: ScanContext<'_>,
) -> Result<ScanSummary> {
    let explicit = !options.accounts.is_empty();
    let targets = resolve_targets(files_dir, &options.accounts)?;
    if targets.is_empty() {
        return Err(GraphicAudioError::no_accounts(
            "no GraphicAudio accounts configured — run login first",
        ));
    }

    let mut summary = ScanSummary::default();
    let access_base = ctx
        .access_base_url
        .unwrap_or(crate::client::DEFAULT_BASE_URL);
    let store_base = ctx
        .store_base_url
        .unwrap_or(crate::magento::DEFAULT_STORE_URL);

    for auth in targets {
        let account_id = auth.account_id().to_string();
        let marketplace = auth.marketplace.clone();

        if !explicit {
            if let Some(acct) = library.get_account(&account_id)? {
                if !acct.scan_enabled {
                    tracing::info!(
                        account = %account_id,
                        "skipping GraphicAudio account — scan_enabled=false"
                    );
                    summary.skipped_disabled += 1;
                    continue;
                }
            }
        }

        library.ensure_account_with_source(
            &account_id,
            &marketplace,
            auth.label.as_deref(),
            "graphicaudio",
        )?;

        let books = scan_account_books(
            &auth,
            access_base,
            store_base,
            ctx.access,
            ctx.magento_password,
            options.include_samples,
            library,
            &account_id,
            &marketplace,
        )
        .await?;

        summary.accounts += 1;
        summary.books_upserted += books;
        summary.pages += 1;

        tracing::info!(
            account = %account_id,
            marketplace = %marketplace,
            books,
            access = ?ctx.access,
            "GraphicAudio library scan finished"
        );
    }

    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
async fn scan_account_books(
    auth: &GraphicAudioAuthFile,
    access_base: &str,
    store_base: &str,
    access: GraphicAudioAccess,
    magento_password: Option<&str>,
    include_samples: bool,
    library: &LibraryStore,
    account_id: &str,
    marketplace: &str,
) -> Result<usize> {
    // Prefer Access App catalog when a device token exists *and* access=device.
    // For web/zip (default), use Magento Browser Player library so we do not
    // depend on a device slot for ownership listing.
    let use_device = matches!(access, GraphicAudioAccess::Device) && auth.has_device_token();

    if use_device {
        return scan_access_products(
            access_base,
            auth,
            include_samples,
            library,
            account_id,
            marketplace,
        )
        .await;
    }

    // Fallback: if a legacy device token exists under web/zip, still allow Access
    // App listing when Magento password is unavailable.
    if auth.has_device_token() && magento_password.is_none() {
        return scan_access_products(
            access_base,
            auth,
            include_samples,
            library,
            account_id,
            marketplace,
        )
        .await;
    }

    let password = magento_password.ok_or_else(|| {
        GraphicAudioError::auth(
            "GraphicAudio Magento library scan requires LIBATION_GA_PASSWORD (or access=device with a saved token)",
        )
    })?;
    let store = MagentoClient::new(store_base)?;
    store.login(&auth.email, password).await?;
    let items = store.list_library().await?;
    let mut books = 0usize;
    for item in &items {
        library.upsert_book(&library_item_to_new_book(item, account_id, marketplace))?;
        books += 1;
    }
    Ok(books)
}

async fn scan_access_products(
    access_base: &str,
    auth: &GraphicAudioAuthFile,
    include_samples: bool,
    library: &LibraryStore,
    account_id: &str,
    marketplace: &str,
) -> Result<usize> {
    let client = GraphicAudioClient::new(access_base).with_token(&auth.token);
    let products = client.products().await?;
    let mut books = 0usize;
    for product in &products {
        if product.is_sample() && !include_samples {
            continue;
        }
        library.upsert_book(&product_to_new_book(product, account_id, marketplace))?;
        books += 1;
    }
    tracing::debug!(
        samples_skipped = products.iter().filter(|p| p.is_sample()).count(),
        "GraphicAudio Access App catalog scan"
    );
    Ok(books)
}

/// Map a GraphicAudio product JSON object to a [`NewBook`] row.
#[must_use]
pub fn product_to_new_book(product: &Product, account_id: &str, marketplace: &str) -> NewBook {
    let length_minutes = product
        .running_time
        .as_deref()
        .and_then(parse_running_time_minutes);
    let purchased_at = product.purchased_date.as_deref().and_then(parse_ga_date);
    let published_at = product.release_date.as_deref().and_then(parse_ga_date);

    NewBook {
        uuid: None,
        product_id: product.id.clone(),
        source: String::from("graphicaudio"),
        account_id: account_id.to_string(),
        asin: None,
        isbn: None,
        marketplace: marketplace.to_string(),
        title: product.display_title(),
        authors: product.author.clone(),
        narrators: None,
        series: product.series.clone(),
        series_index: product.episode.clone(),
        series_asin: None,
        purchased_at,
        publisher: Some(String::from("GraphicAudio")),
        length_minutes,
        is_abridged: false,
        content_kind: String::from("book"),
        categories: product.genre.clone(),
        subtitle: product.title.clone(),
        published_at,
    }
}

fn library_item_to_new_book(item: &LibraryItem, account_id: &str, marketplace: &str) -> NewBook {
    let title = item
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| format!("GraphicAudio {}", item.product_id));
    NewBook {
        uuid: None,
        product_id: item.product_id.clone(),
        source: String::from("graphicaudio"),
        account_id: account_id.to_string(),
        asin: None,
        isbn: None,
        marketplace: marketplace.to_string(),
        title,
        authors: None,
        narrators: None,
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: Some(String::from("GraphicAudio")),
        length_minutes: None,
        is_abridged: false,
        content_kind: String::from("book"),
        categories: None,
        subtitle: None,
        published_at: None,
    }
}

fn parse_running_time_minutes(raw: &str) -> Option<i64> {
    // Examples seen / expected: "10 hrs 30 mins", "630", "10:30"
    let lower = raw.to_ascii_lowercase();
    if let Some(hrs) = lower.find("hr") {
        let before = lower[..hrs].trim();
        let hours: i64 = before.split_whitespace().next_back()?.parse().ok()?;
        let mut minutes = hours * 60;
        if let Some(mpos) = lower.find("min") {
            let chunk = lower[..mpos].trim();
            if let Some(m) = chunk
                .split_whitespace()
                .next_back()
                .and_then(|s| s.parse::<i64>().ok())
            {
                minutes += m;
            }
        }
        return Some(minutes);
    }
    if lower.contains(':') {
        let parts: Vec<_> = lower.split(':').collect();
        if parts.len() >= 2 {
            let h: i64 = parts[0].trim().parse().ok()?;
            let m: i64 = parts[1].trim().parse().ok()?;
            return Some(h * 60 + m);
        }
    }
    raw.trim().parse::<i64>().ok()
}

fn parse_ga_date(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if let Ok(secs) = trimmed.parse::<i64>() {
        // Access App returns unix epoch seconds for Purchase/Release Date.
        if (1_000_000_000..4_000_000_000).contains(&secs) {
            return DateTime::from_timestamp(secs, 0);
        }
    }
    DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
        })
        .or_else(|| {
            NaiveDate::parse_from_str(trimmed, "%m/%d/%Y")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
        })
}

fn resolve_targets(files_dir: &Path, accounts: &[String]) -> Result<Vec<GraphicAudioAuthFile>> {
    if accounts.is_empty() {
        let mut out = Vec::new();
        for path in list_auth_files(files_dir)? {
            out.push(load_auth(&path)?);
        }
        return Ok(out);
    }

    let mut out = Vec::new();
    for key in accounts {
        let path = find_auth_file(files_dir, key)?;
        out.push(load_auth(&path)?);
    }
    Ok(out)
}
