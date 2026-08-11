//! Audible metadata enrichment for non-Audible ownership rows.
//!
//! Matching follows AudioBookshelf: public catalog title/author search, then
//! Audnexus enrichment, then confidence scoring (duration / title / author),
//! with ISBN / narrator / subtitle signals when available.
//! No Audible account is required.

use bookclerk_library::{LibraryStore, NewBook};
use serde::{Deserialize, Serialize};

use crate::error::{EnrichError, Result};
use crate::match_score::{
    calculate_match_confidence, is_valid_asin, normalize_isbn, MatchQuery, ScoreInput,
};
use crate::public_meta::{
    fetch_audnexus_book, normalize_region, public_http_client, search_catalog_asins,
    search_catalog_keywords,
};

/// Default minimum confidence percent (AudioBookshelf-style 0–100 scale).
pub const DEFAULT_ENRICH_MIN_CONFIDENCE: u8 = 90;

/// Metadata pulled from Audible / Audnexus for a confident match.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Enrichment {
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    pub asin: String,
    /// Display title as shown on the storefront or library card.
    pub title: String,
    /// Comma-separated author names when the storefront provides them.
    pub authors: Option<String>,
    /// Comma-separated narrator names when known.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a named series.
    pub series: Option<String>,
    /// Position within the series (e.g. `1`, `1.5`) when known.
    pub series_index: Option<String>,
    /// Audible series ASIN when the catalog associates one.
    pub series_asin: Option<String>,
    /// Audiobook runtime in whole minutes when known.
    pub length_minutes: Option<i64>,
    /// Publisher imprint as reported by the catalog.
    pub publisher: Option<String>,
    /// Optional subtitle when the catalog distinguishes it from the title.
    pub subtitle: Option<String>,
    /// HTTPS URL for cover art when the catalog exposes one.
    pub cover_url: Option<String>,
    /// Canonical ISBN-13 (or ISBN-10 normalized) when published.
    pub isbn: Option<String>,
    /// ISO-8601 / RFC3339 timestamp when known (`releaseDate`).
    pub published_at: Option<String>,
    /// ABS-style genre string (`;`-separated).
    pub categories: Option<String>,
    /// Publisher or storefront synopsis; may be truncated for embeddings.
    pub description: Option<String>,
    /// BCP-47 / storefront language code (`en`, `de`, …) when known.
    pub language: Option<String>,
    /// Match confidence in `[0.0, 1.0]` (omit/`None` for legacy callers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// A scored Audible match candidate.
#[derive(Debug, Clone)]
pub struct ScoredMatch {
    /// Filled metadata fields from the public catalog match.
    pub enrichment: Enrichment,
    /// Match confidence in `[0.0, 1.0]`; higher means a stronger ASIN match.
    pub confidence: f64,
}

/// Look up the best Audible match for the given query metadata via public APIs.
///
/// Returns [`None`] when no candidate meets `min_confidence` (0.0–1.0).
///
/// # Arguments
///
/// * `query` - Query vector or free-text search string.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `min_confidence` - Minimum match confidence in `[0.0, 1.0]` to apply enrichment.
///
/// # Returns
///
/// On success, the inner `Option<ScoredMatch>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn lookup_by_metadata(
    query: &MatchQuery<'_>,
    region: &str,
    min_confidence: f64,
) -> Result<Option<ScoredMatch>> {
    let http = public_http_client()?;
    lookup_by_metadata_with_client(&http, query, region, min_confidence).await
}

/// Catalog + Audnexus lookup against an already-built HTTP client.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `query` - Query vector or free-text search string.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `min_confidence` - Minimum match confidence in `[0.0, 1.0]` to apply enrichment.
///
/// # Returns
///
/// On success, the inner `Option<ScoredMatch>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn lookup_by_metadata_with_client(
    http: &reqwest::Client,
    query: &MatchQuery<'_>,
    region: &str,
    min_confidence: f64,
) -> Result<Option<ScoredMatch>> {
    let title = query.title.trim();
    let isbn_norm = query.isbn.map(normalize_isbn).filter(|s| !s.is_empty());
    if title.is_empty() && isbn_norm.is_none() {
        return Ok(None);
    }
    let region = normalize_region(region);
    let author_query = query.author.map(primary_author).filter(|s| !s.is_empty());
    let title_is_asin = !title.is_empty() && is_valid_asin(&title.to_ascii_uppercase());

    let mut asins = Vec::new();
    if title_is_asin {
        asins.push(title.to_ascii_uppercase());
    } else {
        if !title.is_empty() {
            asins = search_catalog_asins(http, &region, title, author_query.as_deref()).await?;
            // Retry title-only if author-constrained search returned nothing.
            if asins.is_empty() && author_query.is_some() {
                asins = search_catalog_asins(http, &region, title, None).await?;
            }
        }
        // ISBN keyword search surfaces candidates title search may miss. Exact ISBN
        // is scored as a boost later (not an auto-accept — multi-ASIN ISBNs exist).
        if let Some(isbn) = isbn_norm.as_deref() {
            for asin in search_catalog_keywords(http, &region, isbn).await? {
                if !asins.iter().any(|a| a.eq_ignore_ascii_case(&asin)) {
                    asins.push(asin);
                }
            }
        }
    }

    let mut best: Option<ScoredMatch> = None;
    for asin in asins {
        let Some(item) = fetch_audnexus_book(http, &asin, &region).await? else {
            continue;
        };
        let Some((mut enrichment, candidate_isbn)) = enrichment_from_audnexus(&item) else {
            continue;
        };
        let score_input = ScoreInput {
            title: enrichment.title.as_str(),
            subtitle: enrichment.subtitle.as_deref(),
            author: enrichment.authors.as_deref(),
            narrator: enrichment.narrators.as_deref(),
            isbn: candidate_isbn.as_deref(),
            duration_minutes: enrichment.length_minutes.map(|n| n as f64),
        };
        let confidence = calculate_match_confidence(&score_input, query);
        enrichment.confidence = Some(confidence);
        if confidence < min_confidence {
            tracing::debug!(
                asin = %enrichment.asin,
                confidence,
                min_confidence,
                isbn_match = crate::match_score::isbn_exact_match(
                    query.isbn,
                    candidate_isbn.as_deref()
                ),
                "Audible candidate below confidence threshold"
            );
            continue;
        }
        match &best {
            Some(prev) if prev.confidence >= confidence => {}
            _ => {
                best = Some(ScoredMatch {
                    enrichment,
                    confidence,
                });
            }
        }
    }
    Ok(best)
}

/// Apply a confident Audible catalog match onto a non-Audible ownership row.
///
/// Preserves `source`, `product_id`, `uuid`, `account_id`, and an existing
/// `asin` (sets ASIN only when the row does not already have one). Catalog
/// fields win for display metadata (title, contributors, series, ISBN, genres,
/// publish date). Store-native `length_minutes` is kept when present — Audible
/// runtimes include brand intro/outro that plain Acquire files omit.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `book_uuid` - String `book_uuid` for this call.
/// * `enrichment` - Catalog fields to merge onto the book row.
///
/// # Returns
///
/// On success, the inner `bookclerk_library::BookRecord` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn apply_enrichment_to_book(
    library: &LibraryStore,
    book_uuid: &str,
    enrichment: &Enrichment,
) -> Result<bookclerk_library::BookRecord> {
    let existing = library
        .get_book_by_uuid(book_uuid)
        .await?
        .ok_or_else(|| EnrichError::Sync(format!("book not found: {book_uuid}")))?;

    let title = if enrichment.title.is_empty() {
        existing.title.clone()
    } else {
        enrichment.title.clone()
    };

    let published_at = enrichment
        .published_at
        .as_deref()
        .and_then(parse_release_date)
        .or(existing.published_at);

    let book = NewBook {
        uuid: Some(existing.uuid.clone()),
        product_id: existing.product_id.clone(),
        source: existing.source.clone(),
        account_id: existing.account_id.clone(),
        asin: existing
            .asin
            .clone()
            .or_else(|| Some(enrichment.asin.clone())),
        isbn: enrichment.isbn.clone().or(existing.isbn.clone()),
        marketplace: existing.marketplace.clone(),
        title,
        authors: enrichment.authors.clone().or(existing.authors.clone()),
        narrators: enrichment.narrators.clone().or(existing.narrators.clone()),
        series: enrichment.series.clone().or(existing.series.clone()),
        series_index: enrichment
            .series_index
            .clone()
            .or(existing.series_index.clone()),
        series_asin: enrichment
            .series_asin
            .clone()
            .or(existing.series_asin.clone()),
        purchased_at: existing.purchased_at,
        publisher: enrichment.publisher.clone().or(existing.publisher.clone()),
        // Prefer the store-native runtime. Audible catalog length includes
        // Audible pre/post-roll that plain DRM-free files do not have.
        length_minutes: existing.length_minutes.or(enrichment.length_minutes),
        is_abridged: existing.is_abridged,
        content_kind: existing.content_kind.clone(),
        categories: enrichment
            .categories
            .clone()
            .or(existing.categories.clone()),
        subtitle: enrichment.subtitle.clone().or(existing.subtitle.clone()),
        published_at,
    };

    library.upsert_book(&book).await?;

    library
        .update_catalog_enrichment(
            book_uuid,
            &bookclerk_library::CatalogEnrichmentFields {
                description: enrichment.description.clone(),
                language: enrichment.language.clone(),
                cover_url: enrichment.cover_url.clone(),
                subjects: None,
                categories: enrichment.categories.clone(),
                enrich_source: Some(String::from("audible")),
                enrich_confidence: enrichment.confidence,
                enrich_updated_at: Some(chrono::Utc::now()),
            },
        )
        .await?;

    library
        .get_book_by_uuid(book_uuid)
        .await?
        .ok_or_else(|| EnrichError::Sync(format!("book not found after enrich: {book_uuid}")))
}

/// Enrich library rows that lack an ASIN via public Audible catalog + Audnexus.
///
/// Source-agnostic: any non-`audible` row with title and/or ISBN is considered.
/// Uses title, author, narrator, subtitle, ISBN, and duration when present.
/// `min_confidence_percent` is 0–100 (default [`DEFAULT_ENRICH_MIN_CONFIDENCE`]).
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `min_confidence_percent` - `min_confidence_percent` input for this call.
///
/// # Returns
///
/// On success, the inner `usize` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn enrich_books_from_audible(
    library: &LibraryStore,
    min_confidence_percent: u8,
) -> Result<usize> {
    let min_confidence = (min_confidence_percent.min(100) as f64) / 100.0;
    let http = public_http_client()?;
    let mut enriched = 0usize;

    for book in library.list_books(None).await? {
        // Skip Audible-native rows (asin already equals product_id) and rows
        // that already carry an enrichment ASIN.
        if book.asin.as_deref() == Some(book.product_id.as_str()) {
            continue;
        }
        if book.asin.is_some() {
            continue;
        }
        if book.title.trim().is_empty() && book.isbn.is_none() {
            continue;
        }
        let region = book.marketplace.as_str();
        let query = MatchQuery {
            title: book.title.as_str(),
            subtitle: book.subtitle.as_deref(),
            author: book.authors.as_deref(),
            narrator: book.narrators.as_deref(),
            isbn: book.isbn.as_deref(),
            duration_minutes: book.length_minutes.map(|n| n as f64),
        };
        match lookup_by_metadata_with_client(&http, &query, region, min_confidence).await? {
            Some(matched) => {
                apply_enrichment_to_book(library, &book.uuid, &matched.enrichment).await?;
                enriched += 1;
                tracing::info!(
                    uuid = %book.uuid,
                    source = %book.source,
                    title = %book.title,
                    isbn = ?book.isbn,
                    asin = %matched.enrichment.asin,
                    confidence = matched.confidence,
                    "enriched book from Audible metadata"
                );
            }
            None => {
                tracing::debug!(
                    uuid = %book.uuid,
                    source = %book.source,
                    title = %book.title,
                    isbn = ?book.isbn,
                    min_confidence,
                    "no Audible metadata match above confidence threshold"
                );
            }
        }
    }

    Ok(enriched)
}

/// Convert a 0–100 percent threshold to a 0.0–1.0 fraction.
///
/// # Arguments
///
/// * `percent` - Confidence threshold expressed as 0–100.
///
/// # Returns
///
/// `f64` result.
#[must_use]
pub fn confidence_percent_to_fraction(percent: u8) -> f64 {
    f64::from(percent.min(100)) / 100.0
}

/// Fetch Audnexus metadata for a known ASIN (no confidence scoring).
///
/// Used by Discover / Wishlist / Library detail views to surface description,
/// runtime, publisher, and related fields without requiring a library row.
///
/// # Arguments
///
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// On success, the inner `Option<Enrichment>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn enrichment_for_asin(asin: &str, region: &str) -> Result<Option<Enrichment>> {
    let http = public_http_client()?;
    enrichment_for_asin_with_client(&http, asin, region).await
}

/// Same as [`enrichment_for_asin`] with a shared HTTP client.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// On success, the inner `Option<Enrichment>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn enrichment_for_asin_with_client(
    http: &reqwest::Client,
    asin: &str,
    region: &str,
) -> Result<Option<Enrichment>> {
    let Some(item) = fetch_audnexus_book(http, asin, region).await? else {
        return Ok(None);
    };
    Ok(enrichment_from_audnexus(&item).map(|(e, _)| e))
}

fn enrichment_from_audnexus(item: &serde_json::Value) -> Option<(Enrichment, Option<String>)> {
    let asin = item.get("asin")?.as_str()?.to_string();
    let title = item
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let authors = join_named_people(item, "authors");
    let narrators = join_named_people(item, "narrators");
    let series = item
        .get("seriesPrimary")
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let series_index = item
        .get("seriesPrimary")
        .and_then(|s| s.get("position"))
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_f64().map(|n| n.to_string()))
        })
        .filter(|s| !s.is_empty());
    let series_asin = item
        .get("seriesPrimary")
        .and_then(|s| s.get("asin"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let length_minutes = item
        .get("runtimeLengthMin")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            item.get("runtimeLengthMin")
                .and_then(|v| v.as_f64())
                .map(|n| n.round() as i64)
        });
    let publisher = item
        .get("publisherName")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let subtitle = item
        .get("subtitle")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let cover_url = item
        .get("image")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let isbn = item
        .get("isbn")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .filter(|s| !s.is_empty());
    let published_at = item
        .get("releaseDate")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let categories = genres_from_audnexus(item);
    let description = item
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            item.get("summary")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    let language = item
        .get("language")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Some((
        Enrichment {
            asin,
            title,
            authors,
            narrators,
            series,
            series_index,
            series_asin,
            length_minutes,
            publisher,
            subtitle,
            cover_url,
            isbn: isbn.clone(),
            published_at,
            categories,
            description,
            language,
            confidence: None,
        },
        isbn,
    ))
}

fn genres_from_audnexus(item: &serde_json::Value) -> Option<String> {
    let arr = item.get("genres")?.as_array()?;
    let mut names = Vec::new();
    for entry in arr {
        let kind = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("genre");
        // Prefer catalog genres; still accept tags when no genres present.
        if kind.eq_ignore_ascii_case("genre") || kind.eq_ignore_ascii_case("tag") {
            if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
                if !name.is_empty() && !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names.join("; "))
    }
}

fn parse_release_date(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // Audnexus sometimes returns `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS.sssZ` variants.
    if let Ok(dt) = chrono::NaiveDate::parse_from_str(&raw[..raw.len().min(10)], "%Y-%m-%d") {
        return dt.and_hms_opt(0, 0, 0).map(|naive| naive.and_utc());
    }
    None
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

/// First author from a comma-separated list (for catalog search query).
fn primary_author(authors: &str) -> String {
    authors
        .split(',')
        .next()
        .unwrap_or(authors)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::NewBook;

    #[tokio::test]
    async fn enrichment_keeps_libro_runtime_over_audible() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("user@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut seed = NewBook::minimal("9781234567890", "user@example.com", "us", "Libro Title");
        seed.source = "libro".into();
        seed.asin = None;
        seed.isbn = Some("9781234567890".into());
        seed.length_minutes = Some(900);
        let row = store.upsert_book(&seed).await.unwrap();
        let enrichment = Enrichment {
            asin: "B00TEST01".into(),
            title: "Audible Title".into(),
            authors: Some("Ann Author".into()),
            narrators: Some("Ned Narrator".into()),
            series: Some("Foundation".into()),
            series_index: Some("2".into()),
            series_asin: Some("B00SERIES1".into()),
            length_minutes: Some(970),
            publisher: Some("Pub".into()),
            subtitle: Some("A Subtitle".into()),
            cover_url: None,
            isbn: Some("9781234567890".into()),
            published_at: Some("2011-01-15T00:00:00.000Z".into()),
            categories: Some("Science Fiction; Classics".into()),
            description: Some("Blurb".into()),
            language: Some("english".into()),
            confidence: Some(0.95),
        };
        let updated = apply_enrichment_to_book(&store, &row.uuid, &enrichment)
            .await
            .unwrap();
        assert_eq!(updated.asin.as_deref(), Some("B00TEST01"));
        assert_eq!(updated.authors.as_deref(), Some("Ann Author"));
        assert_eq!(updated.length_minutes, Some(900), "Libro runtime must win");
        assert_eq!(updated.series.as_deref(), Some("Foundation"));
        assert_eq!(updated.series_index.as_deref(), Some("2"));
        assert_eq!(updated.series_asin.as_deref(), Some("B00SERIES1"));
        assert_eq!(updated.isbn.as_deref(), Some("9781234567890"));
        assert_eq!(updated.subtitle.as_deref(), Some("A Subtitle"));
        assert_eq!(
            updated.categories.as_deref(),
            Some("Science Fiction; Classics")
        );
        assert!(updated.published_at.is_some());
    }

    #[tokio::test]
    async fn enrichment_preserves_existing_asin() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("user@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut seed = NewBook::minimal("9781234567890", "user@example.com", "us", "Libro Title");
        seed.source = "libro".into();
        seed.asin = Some("B00EXIST01".into());
        seed.length_minutes = Some(900);
        let row = store.upsert_book(&seed).await.unwrap();
        let enrichment = Enrichment {
            asin: "B00NEWASIN".into(),
            title: "Audible Title".into(),
            authors: Some("Ann Author".into()),
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            length_minutes: Some(970),
            publisher: None,
            subtitle: None,
            cover_url: None,
            isbn: None,
            published_at: None,
            categories: None,
            description: None,
            language: None,
            confidence: Some(0.95),
        };
        let updated = apply_enrichment_to_book(&store, &row.uuid, &enrichment)
            .await
            .unwrap();
        assert_eq!(updated.asin.as_deref(), Some("B00EXIST01"));
        assert_eq!(updated.title, "Audible Title");
    }

    #[test]
    fn audnexus_parse_extracts_fields() {
        let item = serde_json::json!({
            "asin": "B00TEST01",
            "title": "Test Title",
            "subtitle": "A Subtitle",
            "publisherName": "Pub Co",
            "runtimeLengthMin": 320,
            "isbn": "9781234567890",
            "releaseDate": "2019-12-03T00:00:00.000Z",
            "language": "english",
            "description": "A long blurb",
            "authors": [{"name": "Ann Author"}],
            "narrators": [{"name": "Ned Narrator"}],
            "seriesPrimary": {"name": "Series A", "position": "1", "asin": "B00SER01"},
            "genres": [
                {"name": "Literature & Fiction", "type": "genre"},
                {"name": "Adventure", "type": "tag"}
            ],
            "image": "https://img.example/cover.jpg"
        });
        let (e, isbn) = enrichment_from_audnexus(&item).unwrap();
        assert_eq!(e.asin, "B00TEST01");
        assert_eq!(e.title, "Test Title");
        assert_eq!(e.authors.as_deref(), Some("Ann Author"));
        assert_eq!(e.narrators.as_deref(), Some("Ned Narrator"));
        assert_eq!(e.length_minutes, Some(320));
        assert_eq!(e.series.as_deref(), Some("Series A"));
        assert_eq!(e.series_index.as_deref(), Some("1"));
        assert_eq!(e.series_asin.as_deref(), Some("B00SER01"));
        assert_eq!(
            e.categories.as_deref(),
            Some("Literature & Fiction; Adventure")
        );
        assert_eq!(e.description.as_deref(), Some("A long blurb"));
        assert_eq!(e.language.as_deref(), Some("english"));
        assert_eq!(
            e.cover_url.as_deref(),
            Some("https://img.example/cover.jpg")
        );
        assert_eq!(isbn.as_deref(), Some("9781234567890"));
        assert_eq!(e.isbn.as_deref(), Some("9781234567890"));
    }

    #[test]
    fn confidence_percent_conversion() {
        assert!((confidence_percent_to_fraction(90) - 0.9).abs() < f64::EPSILON);
        assert!((confidence_percent_to_fraction(100) - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    #[ignore = "network: public Audible catalog + Audnexus"]
    async fn live_forward_the_foundation_matches_above_90() {
        let query = MatchQuery {
            title: "Forward the Foundation",
            subtitle: None,
            author: Some("Isaac Asimov"),
            narrator: Some("Larry McKeever"),
            isbn: Some("9780307970626"),
            duration_minutes: Some(970.0),
        };
        let matched = lookup_by_metadata(&query, "us", 0.90)
            .await
            .expect("lookup")
            .expect("should match");
        assert_eq!(matched.enrichment.asin, "B005WWT30E");
        assert!(
            matched.confidence >= 0.90,
            "confidence={}",
            matched.confidence
        );
    }
}
