//! Library scan: fetch Audible library via audible-rs and upsert into libation-library.

use std::path::Path;

use audible_rs::api::client::Client;
use audible_rs::api::paginator;
use audible_rs::library_sync::DEFAULT_RESPONSE_GROUPS;
use audible_rs::models::library as lib_model;
use chrono::{DateTime, NaiveDate, Utc};
use futures::TryStreamExt;
use libation_library::{LibraryStore, NewBook};
use reqwest::Method;

use crate::accounts::resolve_auth_file_async;
use crate::auth::load_authenticator;
use crate::error::{AudibleError, Result};
use crate::paths::list_auth_files;

/// Options for a library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Limit to specific account nicknames / ids (LibationCli: `scan nick1 nick2`).
    /// When empty, scan all configured auth files (honoring `scan_enabled`).
    pub accounts: Vec<String>,
    pub page_size: u32,
    /// Import podcast episodes (`ImportEpisodes`).
    pub import_episodes: bool,
    /// Import Audible Plus / non-owned titles (`ImportPlusTitles`).
    pub import_plus_titles: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            page_size: 50,
            import_episodes: true,
            import_plus_titles: true,
        }
    }
}

/// Summary of a scan run.
#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub accounts: usize,
    pub books_upserted: usize,
    pub pages: u32,
    pub skipped_disabled: usize,
}

/// Sync Audible library for configured accounts into `library`.
pub async fn scan_library(
    files_dir: &Path,
    library: &LibraryStore,
    options: ScanOptions,
) -> Result<ScanSummary> {
    let explicit = !options.accounts.is_empty();
    let targets = resolve_targets(files_dir, &options.accounts).await?;
    if targets.is_empty() {
        return Err(AudibleError::Auth(
            "no accounts configured — run `libation auth login` first".into(),
        ));
    }

    let mut summary = ScanSummary::default();

    for (account_key, auth_path) in targets {
        let auth = load_authenticator(&auth_path, None).await?;
        let marketplace = auth.locale().country_code.to_string();
        let account_id = auth
            .customer_id()
            .map(str::to_string)
            .unwrap_or_else(|| account_key.clone());

        // Classic migrate may have stored email AccountId; remap onto customer_id.
        let aliases = [account_key.as_str()];
        for alias in aliases {
            if alias != account_id.as_str() {
                if let Ok(Some(_)) = library.get_account(alias) {
                    library.remap_account_id(alias, &account_id)?;
                    tracing::info!(
                        from = %alias,
                        to = %account_id,
                        "remapped library account id to Audible customer_id"
                    );
                }
            }
        }

        // Honor per-account scan_enabled unless specific accounts were requested.
        if !explicit {
            if let Some(acct) = library.get_account(&account_id)? {
                if !acct.scan_enabled {
                    tracing::info!(
                        account = %account_id,
                        "skipping account — scan_enabled=false"
                    );
                    summary.skipped_disabled += 1;
                    continue;
                }
            }
        }

        library.ensure_account(&account_id, &marketplace, Some(&account_key))?;

        let client = Client::new(auth).map_err(AudibleError::from)?;
        let (books, pages) = scan_account_into_library(
            library,
            &client,
            &account_id,
            &marketplace,
            options.page_size,
            options.import_episodes,
            options.import_plus_titles,
        )
        .await?;
        summary.accounts += 1;
        summary.books_upserted += books;
        summary.pages += pages;

        tracing::info!(
            account = %account_id,
            marketplace = %marketplace,
            books,
            pages,
            "library scan finished for account"
        );
    }

    Ok(summary)
}

/// Fetch library pages for one authenticated client and upsert into `library`.
///
/// Exposed for wiremock CI tests (inject a [`Client`] with `api_base_override`).
pub async fn scan_account_into_library(
    library: &LibraryStore,
    client: &Client,
    account_id: &str,
    marketplace: &str,
    page_size: u32,
    import_episodes: bool,
    import_plus_titles: bool,
) -> Result<(usize, u32)> {
    let page_size = page_size.to_string();
    let marketplace_q = marketplace.to_string();

    let stream = paginator::pages(|_continuation| {
        client
            .request(Method::GET, "/1.0/library")
            .country_code(&marketplace_q)
            .query("response_groups", DEFAULT_RESPONSE_GROUPS)
            .query("num_results", &page_size)
            .query("image_sizes", "500,1215")
            .query("include_pending", "true")
            .query("status", "Active")
    });
    futures::pin_mut!(stream);

    let mut books_upserted = 0usize;
    let mut pages = 0u32;

    while let Some(page) = stream.try_next().await.map_err(AudibleError::from)? {
        pages += 1;
        for item in lib_model::normalize_items(&page.body) {
            if lib_model::should_soft_delete(&item) {
                continue;
            }
            if !import_episodes && lib_model::item_kind(&item) == "episode" {
                continue;
            }
            if !import_plus_titles && lib_model::is_consumable_indefinitely(&item) == Some(false) {
                continue;
            }
            let Some(asin) = item.get("asin").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(title) = lib_model::build_full_title(&item) else {
                continue;
            };
            let series_entries = lib_model::extract_series(&item);
            let series = series_entries.first().map(|s| s.title.clone());
            let series_index = series_entries.first().and_then(|s| s.sequence.clone());
            let series_asin = series_entries.first().map(|s| s.asin.clone());

            let content_kind = lib_model::item_kind(&item).to_string();
            let subtitle = item
                .get("subtitle")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let publisher = item
                .get("publisher_name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let length_minutes = item
                .get("runtime_length_min")
                .and_then(|v| v.as_i64())
                .or_else(|| item.get("length_minutes").and_then(|v| v.as_i64()));
            let is_abridged = item
                .get("is_abridged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let categories = item
                .get("category_ladders")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty());
            let published_at = item
                .get("release_date")
                .and_then(|v| v.as_str())
                .and_then(parse_release_date);
            let purchased_at = item
                .get("purchase_date")
                .and_then(|v| v.as_str())
                .and_then(parse_release_date)
                .or_else(|| {
                    item.get("library_status")
                        .and_then(|s| s.get("date_added"))
                        .and_then(|v| v.as_str())
                        .and_then(parse_release_date)
                });

            let mut book = NewBook::minimal(asin, account_id, marketplace, title);
            book.authors = join_named_people(&item, "authors");
            book.narrators = join_named_people(&item, "narrators");
            book.series = series;
            book.series_index = series_index;
            book.series_asin = series_asin;
            book.content_kind = content_kind;
            book.subtitle = subtitle;
            book.publisher = publisher;
            book.length_minutes = length_minutes;
            book.is_abridged = is_abridged;
            book.categories = categories;
            book.published_at = published_at;
            book.purchased_at = purchased_at;
            library.upsert_book(&book)?;
            books_upserted += 1;
        }
    }

    Ok((books_upserted, pages))
}

async fn resolve_targets(
    files_dir: &Path,
    accounts: &[String],
) -> Result<Vec<(String, std::path::PathBuf)>> {
    if !accounts.is_empty() {
        let mut out = Vec::with_capacity(accounts.len());
        for account in accounts {
            let path = resolve_auth_file_async(files_dir, account).await?;
            let key = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(account.as_str())
                .to_string();
            out.push((key, path));
        }
        return Ok(out);
    }
    let mut out = Vec::new();
    for path in list_auth_files(files_dir)? {
        let key = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("account")
            .to_string();
        out.push((key, path));
    }
    Ok(out)
}

fn join_named_people(item: &serde_json::Value, field: &str) -> Option<String> {
    let arr = item.get(field)?.as_array()?;
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|entry| entry.get("name").and_then(|v| v.as_str()))
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

fn parse_release_date(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc());
    }
    None
}
