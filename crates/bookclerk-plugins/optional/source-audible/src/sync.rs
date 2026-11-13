//! Library scan: fetch the Audible library via audible-rs into structured scan rows.

use audible_rs::api::client::Client;
use audible_rs::api::paginator;
use audible_rs::library_sync::DEFAULT_RESPONSE_GROUPS;
use audible_rs::models::library as lib_model;
use bookclerk_plugin_sdk::http::Method;
use bookclerk_plugin_sdk::{Abridgement, ScanBook, SeriesEntry};
use bookclerk_storefront::abi::rfc3339;
use bookclerk_storefront::records::{self, CreditRole, IdentifierScheme};
use chrono::{DateTime, NaiveDate, Utc};

use crate::error::{AudibleError, Result};

/// Library listing filters and paging for one `/1.0/library` request.
#[derive(Debug, Clone, Default)]
pub struct LibraryPageRequest {
    /// Continuation token of the page to fetch; `None` for the first page.
    pub continuation: Option<String>,
    /// Delta state token from the account's last pass; `None` for a full pass.
    pub state_token: Option<String>,
    /// Items per page (`num_results`).
    pub page_size: u32,
    /// When true, keep podcast episodes.
    pub import_episodes: bool,
    /// When true, keep Plus catalog titles that are not owned outright.
    pub import_plus_titles: bool,
}

/// One `/1.0/library` page mapped to scan rows.
#[derive(Debug, Clone, Default)]
pub struct LibraryPageRows {
    /// Titles to upsert.
    pub books: Vec<ScanBook>,
    /// ASINs the delta reports as gone from the library (empty on a full pass).
    pub removed: Vec<String>,
    /// Continuation token of the next page; `None` on the last page.
    pub next_continuation: Option<String>,
    /// State token the response carried (Audible sends it on the first page).
    pub state_token: Option<String>,
}

/// Fetch one library page for one account without touching the library DB.
///
/// A request with a `state_token` is a delta: it asks for active and revoked
/// items changed since that token, and revoked or soft-deleted items come
/// back in [`LibraryPageRows::removed`]. Without one it lists the active
/// library and skips soft-deleted items.
///
/// # Errors
///
/// Returns an error when the request fails.
pub async fn fetch_library_page(
    client: &Client,
    account_id: &str,
    marketplace: &str,
    request: &LibraryPageRequest,
) -> Result<LibraryPageRows> {
    let page_size = request.page_size.max(1).to_string();
    let delta = request.state_token.as_deref();
    let mut make_request = |_continuation: Option<&str>| {
        let mut builder = client
            .request(Method::GET, "/1.0/library")
            .country_code(marketplace)
            .query("response_groups", DEFAULT_RESPONSE_GROUPS)
            .query("num_results", &page_size)
            .query("image_sizes", "500,1215")
            .query("include_pending", "true")
            .query(
                "status",
                if delta.is_some() {
                    "Active,Revoked"
                } else {
                    "Active"
                },
            );
        if let Some(token) = delta {
            builder = builder.query("state_token", token);
        }
        builder
    };
    let (page, next) = paginator::page_at(&mut make_request, request.continuation.as_deref())
        .await
        .map_err(AudibleError::from)?;
    let mut rows = LibraryPageRows {
        next_continuation: next,
        state_token: page.state_token.clone(),
        ..LibraryPageRows::default()
    };
    if delta.is_some() {
        rows.removed
            .extend(lib_model::extract_removed_asins(&page.body));
    }
    for item in lib_model::normalize_items(&page.body) {
        if lib_model::should_soft_delete(&item) {
            if delta.is_some() {
                if let Some(asin) = item.get("asin").and_then(|v| v.as_str()) {
                    rows.removed.push(asin.to_string());
                }
            }
            continue;
        }
        if !request.import_episodes && lib_model::item_kind(&item) == "episode" {
            continue;
        }
        if !request.import_plus_titles
            && lib_model::is_consumable_indefinitely(&item) == Some(false)
        {
            continue;
        }
        if let Some(book) = item_to_scan_book(&item, account_id, marketplace) {
            rows.books.push(book);
        }
    }
    rows.removed.sort();
    rows.removed.dedup();
    Ok(rows)
}

/// Fetch every library page for one account without touching the library DB
/// (a full pass).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn collect_account_books(
    client: &Client,
    account_id: &str,
    marketplace: &str,
    page_size: u32,
    import_episodes: bool,
    import_plus_titles: bool,
) -> Result<(Vec<ScanBook>, u32)> {
    let mut request = LibraryPageRequest {
        page_size,
        import_episodes,
        import_plus_titles,
        ..LibraryPageRequest::default()
    };
    let mut books = Vec::new();
    let mut pages = 0u32;
    loop {
        let rows = fetch_library_page(client, account_id, marketplace, &request).await?;
        pages += 1;
        books.extend(rows.books);
        match rows.next_continuation {
            Some(next) => request.continuation = Some(next),
            None => break,
        }
    }
    Ok((books, pages))
}

/// Maps one library item to a scan row; `None` without an ASIN or a title.
fn item_to_scan_book(
    item: &serde_json::Value,
    account_id: &str,
    marketplace: &str,
) -> Option<ScanBook> {
    let asin = item.get("asin").and_then(|v| v.as_str())?;
    let title = lib_model::build_full_title(item)?;
    let series_entries: Vec<SeriesEntry> = lib_model::extract_series(item)
        .into_iter()
        .map(|entry| SeriesEntry {
            name: entry.title,
            position: entry.sequence,
            store_series_id: Some(entry.asin).filter(|a| !a.is_empty()),
        })
        .collect();

    let label = lib_model::item_kind(item);
    let work_kind = records::work_kind_from_label(label);
    // Only an episode names its show: a season also carries a `parent`
    // relationship, and keeping it would file the season as an episode.
    let parent = bookclerk_storefront::content_kind::is_episode(label)
        .then(|| parent_product_id(item))
        .flatten();
    let (expression_kind, media_kind) = records::audio_kinds(work_kind);
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
    let abridgement = match item.get("is_abridged").and_then(|v| v.as_bool()) {
        None => Abridgement::Unknown,
        Some(false) => Abridgement::Unabridged,
        Some(true) => Abridgement::Abridged,
    };
    let language = item
        .get("language")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
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

    let mut identifiers: Vec<_> =
        records::identifier(IdentifierScheme::Asin, asin, Some(marketplace))
            .into_iter()
            .collect();
    identifiers.extend(
        item.get("isbn")
            .and_then(|v| v.as_str())
            .and_then(|isbn| records::identifier(IdentifierScheme::Isbn, isbn, None)),
    );
    let mut credits =
        records::credits_from_names(named_people(item, "authors"), CreditRole::Author);
    credits.extend(records::credits_from_names(
        named_people(item, "narrators"),
        CreditRole::Narrator,
    ));
    let book = ScanBook {
        account_id: account_id.to_string(),
        product_id: asin.to_string(),
        title,
        marketplace: Some(marketplace.to_string()),
        publisher,
        length_minutes,
        subtitle,
        work_kind,
        expression_kind,
        media_kind,
        identifiers,
        credits,
        series_entries,
        parent_product_id: parent,
        language,
        abridgement,
        published_at: published_at.map(rfc3339),
        purchased_at: purchased_at.map(rfc3339),
        categories,
    };
    Some(book)
}

/// Non-empty `name` fields from a library-item authors/narrators array, in order.
fn named_people<'a>(item: &'a serde_json::Value, field: &str) -> Vec<&'a str> {
    item.get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| entry.get("name").and_then(|v| v.as_str()))
                .filter(|n| !n.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// ASIN of the podcast show (or season) a podcast episode belongs to, from
/// the item's `parent` relationship.
fn parent_product_id(item: &serde_json::Value) -> Option<String> {
    item.get("relationships")?
        .as_array()?
        .iter()
        .find(|rel| rel.get("relationship_to_product").and_then(|v| v.as_str()) == Some("parent"))
        .and_then(|rel| rel.get("asin").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|asin| !asin.is_empty())
        .map(str::to_string)
}

/// Parses RFC 3339 or `YYYY-MM-DD` into UTC; other formats yield `None`.
fn parse_release_date(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc());
    }
    None
}
