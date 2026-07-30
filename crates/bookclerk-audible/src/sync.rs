//! Library scan: fetch Audible library via audible-rs and upsert into bookclerk-library.

use audible_rs::api::client::Client;
use audible_rs::api::paginator;
use audible_rs::library_sync::DEFAULT_RESPONSE_GROUPS;
use audible_rs::models::library as lib_model;
use bookclerk_library::{NewBook, SourceScope};
use bookclerk_source::{ScanOptions, ScanSummary};
use chrono::{DateTime, NaiveDate, Utc};
use futures::TryStreamExt;
use reqwest::Method;

use crate::db::{list_audible_accounts_from_db, load_authenticator_from_db};
use crate::error::{AudibleError, Result};

/// Sync Audible library for configured accounts into `library`.
///
/// Accounts are resolved from the `encrypted_secrets` table (DB-backed); no
/// `Accounts/*.audible.auth` files are read.
pub async fn scan_library(scope: &SourceScope, options: ScanOptions) -> Result<ScanSummary> {
    let explicit = !options.accounts.is_empty();
    let all = list_audible_accounts_from_db(scope).await?;

    let targets: Vec<String> = if explicit {
        options
            .accounts
            .iter()
            .filter(|needle| {
                all.iter().any(|(id, name)| {
                    id.eq_ignore_ascii_case(needle) || name.eq_ignore_ascii_case(needle)
                }) || {
                    // Allow requesting accounts not yet in DB; will surface as "no credentials".
                    true
                }
            })
            .cloned()
            .collect()
    } else {
        all.into_iter().map(|(id, _name)| id).collect()
    };

    if targets.is_empty() {
        return Err(AudibleError::NoAccounts(
            "no Audible accounts configured — run `bookclerk auth login` first".into(),
        ));
    }

    let mut summary = ScanSummary::default();

    for account_id in targets {
        let Some(auth) = load_authenticator_from_db(scope, &account_id).await? else {
            tracing::warn!(account = %account_id, "no Audible credentials in DB — skipping");
            continue;
        };

        let marketplace = auth.locale().country_code.to_string();
        let resolved_id = auth
            .customer_id()
            .map(str::to_string)
            .unwrap_or_else(|| account_id.clone());

        // Honor per-account scan_enabled unless specific accounts were requested.
        if !explicit {
            if let Some(acct) = scope.get_account(&resolved_id).await? {
                if !acct.scan_enabled {
                    tracing::info!(
                        account = %resolved_id,
                        "skipping account — scan_enabled=false"
                    );
                    summary.skipped_disabled += 1;
                    continue;
                }
            }
        }

        scope
            .ensure_account(&resolved_id, &marketplace, Some(&account_id))
            .await?;

        let client = Client::new(auth).map_err(AudibleError::from)?;
        let (books, pages) = scan_account_into_library(
            scope,
            &client,
            &resolved_id,
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
            account = %resolved_id,
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
    scope: &SourceScope,
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
            book.source = String::from("audible");
            book.asin = Some(asin.to_string());
            book.isbn = item
                .get("isbn")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
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
            scope.upsert_book(&book).await?;
            books_upserted += 1;
        }
    }

    Ok((books_upserted, pages))
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
