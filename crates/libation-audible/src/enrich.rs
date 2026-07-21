//! Audible catalog ISBN enrichment for Libro (and other) ownership rows.

use std::path::Path;

use audible_rs::api::client::Client;
use audible_rs::models::library as lib_model;
use libation_library::{LibraryStore, NewBook};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::accounts::list_accounts;
use crate::download::open_account_client;
use crate::error::{AudibleError, Result};

/// Response groups for ISBN catalog lookup (product details + media).
const ISBN_LOOKUP_RESPONSE_GROUPS: &str =
    "product_details,contributors,product_attrs,media,product_desc";

/// Metadata pulled from Audible catalog for a matching ISBN.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Enrichment {
    pub asin: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub length_minutes: Option<i64>,
    pub publisher: Option<String>,
    pub subtitle: Option<String>,
    pub cover_url: Option<String>,
}

/// Look up an Audible catalog product by ISBN for `account`.
///
/// Exact-matches on the product `isbn` field after normalizing digits.
pub async fn lookup_by_isbn(
    files_dir: &Path,
    account: &str,
    isbn: &str,
) -> Result<Option<Enrichment>> {
    let client = open_account_client(files_dir, account).await?;
    lookup_by_isbn_with_client(&client.client, &client.marketplace, isbn).await
}

/// Catalog ISBN lookup against an already-open client (tests / batch reuse).
pub async fn lookup_by_isbn_with_client(
    client: &Client,
    marketplace: &str,
    isbn: &str,
) -> Result<Option<Enrichment>> {
    let needle = normalize_isbn(isbn);
    if needle.is_empty() {
        return Ok(None);
    }

    let response = client
        .request(Method::GET, "/1.0/catalog/products")
        .country_code(marketplace)
        .query("keywords", isbn)
        .query("num_results", "50")
        .query("response_groups", ISBN_LOOKUP_RESPONSE_GROUPS)
        .query("image_sizes", "500,1215")
        .send()
        .await
        .map_err(AudibleError::from)?;
    let response = response
        .error_for_status()
        .map_err(|err| AudibleError::Sync(err.to_string()))?;
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|err| AudibleError::Sync(err.to_string()))?;

    let Some(products) = body.get("products").and_then(|v| v.as_array()) else {
        return Ok(None);
    };

    for product in products {
        let Some(product_isbn) = product.get("isbn").and_then(|v| v.as_str()) else {
            continue;
        };
        if normalize_isbn(product_isbn) != needle {
            continue;
        }
        return Ok(parse_enrichment(product));
    }
    Ok(None)
}

/// Merge Audible catalog metadata into an existing ownership row.
///
/// Preserves `source`, `product_id`, `uuid`, and `account_id`. May set `asin`
/// from the Audible match when the row does not already have one.
pub fn apply_enrichment_to_book(
    library: &LibraryStore,
    book_uuid: &str,
    enrichment: &Enrichment,
) -> Result<libation_library::BookRecord> {
    let existing = library
        .get_book_by_uuid(book_uuid)?
        .ok_or_else(|| AudibleError::Sync(format!("book not found: {book_uuid}")))?;

    let title = if enrichment.title.is_empty() {
        existing.title.clone()
    } else if existing.title.is_empty()
        || existing
            .title
            .eq_ignore_ascii_case(existing.product_id.as_str())
    {
        enrichment.title.clone()
    } else {
        // Prefer catalog title when the row already has a real title — Libro
        // titles can be sparse; Audible is usually richer.
        enrichment.title.clone()
    };

    let book = NewBook {
        uuid: Some(existing.uuid.clone()),
        product_id: existing.product_id.clone(),
        source: existing.source.clone(),
        account_id: existing.account_id.clone(),
        asin: existing
            .asin
            .clone()
            .or_else(|| Some(enrichment.asin.clone())),
        isbn: existing.isbn.clone(),
        marketplace: existing.marketplace.clone(),
        title,
        authors: enrichment.authors.clone().or(existing.authors.clone()),
        narrators: enrichment.narrators.clone().or(existing.narrators.clone()),
        series: enrichment.series.clone().or(existing.series.clone()),
        series_index: existing.series_index.clone(),
        series_asin: existing.series_asin.clone(),
        purchased_at: existing.purchased_at,
        publisher: enrichment.publisher.clone().or(existing.publisher.clone()),
        length_minutes: enrichment.length_minutes.or(existing.length_minutes),
        is_abridged: existing.is_abridged,
        content_kind: existing.content_kind.clone(),
        categories: existing.categories.clone(),
        subtitle: enrichment.subtitle.clone().or(existing.subtitle.clone()),
        published_at: existing.published_at,
    };

    Ok(library.upsert_book(&book)?)
}

/// Enrich Libro rows that have an ISBN but no ASIN via Audible catalog.
///
/// Uses the first available Audible auth account under `files_dir`.
pub async fn enrich_libro_books_by_isbn(files_dir: &Path, library: &LibraryStore) -> Result<usize> {
    let accounts = list_accounts(files_dir).await?;
    let Some(account) = accounts.first() else {
        tracing::debug!("no Audible accounts available for ISBN enrichment");
        return Ok(0);
    };

    let client = open_account_client(files_dir, &account.account_id).await?;
    let mut enriched = 0usize;

    for book in library.list_books(None)? {
        if !book.source.eq_ignore_ascii_case("libro") {
            continue;
        }
        let Some(isbn) = book.isbn.as_deref() else {
            continue;
        };
        if book.asin.is_some() {
            continue;
        }
        match lookup_by_isbn_with_client(&client.client, &client.marketplace, isbn).await? {
            Some(enrichment) => {
                apply_enrichment_to_book(library, &book.uuid, &enrichment)?;
                enriched += 1;
                tracing::info!(
                    uuid = %book.uuid,
                    isbn,
                    asin = %enrichment.asin,
                    "enriched Libro book from Audible catalog"
                );
            }
            None => {
                tracing::debug!(uuid = %book.uuid, isbn, "no Audible catalog match for ISBN");
            }
        }
    }

    Ok(enriched)
}

fn parse_enrichment(product: &serde_json::Value) -> Option<Enrichment> {
    let asin = product.get("asin")?.as_str()?.to_string();
    let title = product
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| lib_model::build_full_title(product))?;

    let series_entries = lib_model::extract_series(product);
    let series = series_entries.first().map(|s| s.title.clone());
    let cover_url = product
        .get("product_images")
        .and_then(|imgs| {
            imgs.get("500")
                .or_else(|| imgs.get("1215"))
                .or_else(|| imgs.as_object().and_then(|m| m.values().next()))
        })
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Some(Enrichment {
        asin,
        title,
        authors: join_named_people(product, "authors"),
        narrators: join_named_people(product, "narrators"),
        series,
        length_minutes: product
            .get("runtime_length_min")
            .and_then(|v| v.as_i64())
            .or_else(|| product.get("length_minutes").and_then(|v| v.as_i64())),
        publisher: product
            .get("publisher_name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        subtitle: product
            .get("subtitle")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        cover_url,
    })
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

/// Digits-only ISBN for exact matching (strips hyphens / spaces / `ISBN` prefix).
fn normalize_isbn(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_prefix = trimmed
        .strip_prefix("ISBN-13:")
        .or_else(|| trimmed.strip_prefix("ISBN-10:"))
        .or_else(|| trimmed.strip_prefix("ISBN:"))
        .or_else(|| trimmed.strip_prefix("isbn:"))
        .unwrap_or(trimmed)
        .trim();
    without_prefix
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
        .collect::<String>()
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_isbn_strips_hyphens() {
        assert_eq!(normalize_isbn("978-1-234-56789-0"), "9781234567890");
        assert_eq!(normalize_isbn("ISBN: 9781234567890"), "9781234567890");
    }

    #[test]
    fn parse_enrichment_extracts_fields() {
        let product = serde_json::json!({
            "asin": "B00TEST01",
            "isbn": "9781234567890",
            "title": "Test Title",
            "subtitle": "A Subtitle",
            "publisher_name": "Pub Co",
            "runtime_length_min": 320,
            "authors": [{"name": "Ann Author"}],
            "narrators": [{"name": "Ned Narrator"}],
            "product_images": {"500": "https://img.example/500.jpg"}
        });
        let e = parse_enrichment(&product).unwrap();
        assert_eq!(e.asin, "B00TEST01");
        assert_eq!(e.title, "Test Title");
        assert_eq!(e.authors.as_deref(), Some("Ann Author"));
        assert_eq!(e.narrators.as_deref(), Some("Ned Narrator"));
        assert_eq!(e.length_minutes, Some(320));
        assert_eq!(e.cover_url.as_deref(), Some("https://img.example/500.jpg"));
    }
}
