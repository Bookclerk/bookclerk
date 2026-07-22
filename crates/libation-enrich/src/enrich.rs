//! Audible metadata enrichment for non-Audible ownership rows.
//!
//! Matching follows AudioBookshelf: public catalog title/author search, then
//! Audnexus enrichment, then confidence scoring (duration / title / author),
//! with ISBN / narrator / subtitle signals when available.
//! No Audible account is required.

use libation_library::{LibraryStore, NewBook};
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
    pub asin: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub length_minutes: Option<i64>,
    pub publisher: Option<String>,
    pub subtitle: Option<String>,
    pub cover_url: Option<String>,
    /// Match confidence in `[0.0, 1.0]` (omit/`None` for legacy callers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// A scored Audible match candidate.
#[derive(Debug, Clone)]
pub struct ScoredMatch {
    pub enrichment: Enrichment,
    pub confidence: f64,
}

/// Look up the best Audible match for the given query metadata via public APIs.
///
/// Returns [`None`] when no candidate meets `min_confidence` (0.0–1.0).
pub async fn lookup_by_metadata(
    query: &MatchQuery<'_>,
    region: &str,
    min_confidence: f64,
) -> Result<Option<ScoredMatch>> {
    let http = public_http_client()?;
    lookup_by_metadata_with_client(&http, query, region, min_confidence).await
}

/// Catalog + Audnexus lookup against an already-built HTTP client.
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
        .ok_or_else(|| EnrichError::Sync(format!("book not found: {book_uuid}")))?;

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
        // Prefer the store-native runtime. Audible catalog length includes
        // Audible pre/post-roll that Libro DRM-free files do not have; using it
        // for Libro rows mis-reports duration and invites bad chapter alignment.
        length_minutes: existing.length_minutes.or(enrichment.length_minutes),
        is_abridged: existing.is_abridged,
        content_kind: existing.content_kind.clone(),
        categories: existing.categories.clone(),
        subtitle: enrichment.subtitle.clone().or(existing.subtitle.clone()),
        published_at: existing.published_at,
    };

    Ok(library.upsert_book(&book)?)
}

/// Enrich library rows that lack an ASIN via public Audible catalog + Audnexus.
///
/// Source-agnostic: any non-`audible` row with title and/or ISBN is considered.
/// Uses title, author, narrator, subtitle, ISBN, and duration when present.
/// `min_confidence_percent` is 0–100 (default [`DEFAULT_ENRICH_MIN_CONFIDENCE`]).
pub async fn enrich_books_from_audible(
    library: &LibraryStore,
    min_confidence_percent: u8,
) -> Result<usize> {
    let min_confidence = (min_confidence_percent.min(100) as f64) / 100.0;
    let http = public_http_client()?;
    let mut enriched = 0usize;

    for book in library.list_books(None)? {
        if book.source.eq_ignore_ascii_case("audible") {
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
                apply_enrichment_to_book(library, &book.uuid, &matched.enrichment)?;
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
#[must_use]
pub fn confidence_percent_to_fraction(percent: u8) -> f64 {
    f64::from(percent.min(100)) / 100.0
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

    Some((
        Enrichment {
            asin,
            title,
            authors,
            narrators,
            series,
            length_minutes,
            publisher,
            subtitle,
            cover_url,
            confidence: None,
        },
        isbn,
    ))
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
    use libation_library::NewBook;

    #[test]
    fn enrichment_keeps_libro_runtime_over_audible() {
        let store = LibraryStore::open_in_memory().unwrap();
        store
            .upsert_account_with_source("user@example.com", "us", None, true, "libro")
            .unwrap();
        let mut seed = NewBook::minimal("9781234567890", "user@example.com", "us", "Libro Title");
        seed.source = "libro".into();
        seed.asin = None;
        seed.isbn = Some("9781234567890".into());
        seed.length_minutes = Some(900);
        let row = store.upsert_book(&seed).unwrap();
        let enrichment = Enrichment {
            asin: "B00TEST01".into(),
            title: "Audible Title".into(),
            authors: Some("Ann Author".into()),
            narrators: Some("Ned Narrator".into()),
            series: Some("Foundation".into()),
            length_minutes: Some(970),
            publisher: Some("Pub".into()),
            subtitle: None,
            cover_url: None,
            confidence: Some(0.95),
        };
        let updated = apply_enrichment_to_book(&store, &row.uuid, &enrichment).unwrap();
        assert_eq!(updated.asin.as_deref(), Some("B00TEST01"));
        assert_eq!(updated.authors.as_deref(), Some("Ann Author"));
        assert_eq!(updated.length_minutes, Some(900), "Libro runtime must win");
        assert_eq!(updated.series.as_deref(), Some("Foundation"));
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
            "authors": [{"name": "Ann Author"}],
            "narrators": [{"name": "Ned Narrator"}],
            "seriesPrimary": {"name": "Series A", "position": "1"},
            "image": "https://img.example/cover.jpg"
        });
        let (e, isbn) = enrichment_from_audnexus(&item).unwrap();
        assert_eq!(e.asin, "B00TEST01");
        assert_eq!(e.title, "Test Title");
        assert_eq!(e.authors.as_deref(), Some("Ann Author"));
        assert_eq!(e.narrators.as_deref(), Some("Ned Narrator"));
        assert_eq!(e.length_minutes, Some(320));
        assert_eq!(e.series.as_deref(), Some("Series A"));
        assert_eq!(
            e.cover_url.as_deref(),
            Some("https://img.example/cover.jpg")
        );
        assert_eq!(isbn.as_deref(), Some("9781234567890"));
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
