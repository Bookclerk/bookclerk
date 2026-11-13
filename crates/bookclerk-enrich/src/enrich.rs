//! Audible metadata enrichment for non-Audible ownership rows.
//!
//! Matching follows AudioBookshelf: public catalog title/author search, then
//! a metadata-provider lookup of each candidate ASIN (the Audnexus plugin),
//! then confidence scoring (duration / title / author), with ISBN / narrator
//! / subtitle signals when available. No Audible account is required.

use bookclerk_library::{LibraryStore, NewBook};
use bookclerk_plugin_sdk::http::Client;
use bookclerk_plugin_sdk::{
    CreditRole, IdentifierScheme, MediaKind, MetadataLookupParams, MetadataRecord,
};
use serde::{Deserialize, Serialize};

use crate::error::{EnrichError, Result};
use crate::match_score::{
    calculate_match_confidence, is_asin_title, is_valid_asin, normalize_isbn, MatchQuery,
    ScoreInput,
};
use crate::providers::{asin_identifier, record_identifier, MetadataProviders};
use bookclerk_audible_catalog::{
    normalize_region, public_http_client, search_catalog_asins, search_catalog_keywords,
};

/// Default minimum confidence percent (AudioBookshelf-style 0–100 scale).
pub const DEFAULT_ENRICH_MIN_CONFIDENCE: u8 = 90;

/// Metadata pulled from the Audible catalog and a metadata provider for a confident match.
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

/// Look up the best Audible match for the given query metadata.
///
/// Candidate ASINs come from the public Audible catalog search; each one's
/// record comes from the deployed metadata providers. Returns [`None`] when
/// no candidate meets `min_confidence` (0.0–1.0), or when no provider has a
/// record for any candidate.
///
/// # Arguments
///
/// * `providers` - Deployed metadata providers asked for each candidate.
/// * `query` - Title, author, ISBN and runtime of the row to match.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `min_confidence` - Minimum match confidence in `[0.0, 1.0]` to apply enrichment.
///
/// # Returns
///
/// The highest-scoring candidate at or above `min_confidence`.
///
/// # Errors
///
/// Returns an error when the HTTP client cannot be built or a catalog search fails.
pub async fn lookup_by_metadata(
    providers: &dyn MetadataProviders,
    query: &MatchQuery<'_>,
    region: &str,
    min_confidence: f64,
) -> Result<Option<ScoredMatch>> {
    let http = public_http_client()?;
    lookup_by_metadata_with_client(&http, providers, query, region, min_confidence).await
}

/// Catalog search + provider lookup against an already-built HTTP client.
///
/// # Arguments
///
/// * `http` - Client for the public Audible catalog search.
/// * `providers` - Deployed metadata providers asked for each candidate.
/// * `query` - Title, author, ISBN and runtime of the row to match.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `min_confidence` - Minimum match confidence in `[0.0, 1.0]` to apply enrichment.
///
/// # Returns
///
/// The highest-scoring candidate at or above `min_confidence`.
///
/// # Errors
///
/// Returns an error when a catalog search fails.
pub async fn lookup_by_metadata_with_client(
    http: &Client,
    providers: &dyn MetadataProviders,
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
    let title_is_asin = is_asin_title(title);

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
        // Every provider's audio record for this ASIN is scored, so a
        // sparse first record cannot hide a complete later one. Ebook and
        // print records never describe the Audible edition being matched.
        let candidates = records_for_asin(providers, &asin, &region)
            .await
            .into_iter()
            .filter(|record| record.media_kind == MediaKind::Audio)
            .filter_map(|record| enrichment_from_record(&record, &asin));
        for enrichment in candidates {
            best = better_match(best, enrichment, query, min_confidence);
        }
    }
    Ok(best)
}

/// Scores `enrichment` against `query` and keeps it when it clears
/// `min_confidence` and beats `best`.
///
/// # Arguments
///
/// * `best` - The best match so far.
/// * `enrichment` - A candidate record's fields.
/// * `query` - The library row being matched.
/// * `min_confidence` - Lowest accepted confidence (0.0–1.0).
///
/// # Returns
///
/// The better of `best` and the scored candidate.
fn better_match(
    best: Option<ScoredMatch>,
    mut enrichment: Enrichment,
    query: &MatchQuery<'_>,
    min_confidence: f64,
) -> Option<ScoredMatch> {
    let candidate_isbn = enrichment.isbn.clone();
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
        return best;
    }
    match best {
        Some(prev) if prev.confidence >= confidence => Some(prev),
        _ => Some(ScoredMatch {
            enrichment,
            confidence,
        }),
    }
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
        plugin_key: existing.plugin_key.clone(),
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

    // Metadata only: enrichment never asserts that the account holds the
    // title, so it cannot bring back a holding a scan removed.
    library.update_book_metadata(&book).await?;

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

/// Enrich library rows that lack an ASIN via the public Audible catalog and
/// the deployed metadata providers.
///
/// Source-agnostic: any non-`audible` row with title and/or ISBN is considered.
/// Uses title, author, narrator, subtitle, ISBN, and duration when present.
/// `min_confidence_percent` is 0–100 (default [`DEFAULT_ENRICH_MIN_CONFIDENCE`]).
/// With no provider that answers ASIN lookups, nothing matches.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `providers` - Deployed metadata providers asked for each candidate ASIN.
/// * `min_confidence_percent` - Minimum match confidence, 0–100.
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
    providers: &dyn MetadataProviders,
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
        match lookup_by_metadata_with_client(&http, providers, &query, region, min_confidence)
            .await?
        {
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

/// Provider metadata for a known ASIN (no confidence scoring).
///
/// Used by Discover / Wishlist / Library detail views to surface description,
/// runtime, publisher, and related fields without requiring a library row.
/// The first provider record that carries a title wins.
///
/// # Arguments
///
/// * `providers` - Deployed metadata providers.
/// * `asin` - Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// The record's fields, or `None` when the ASIN is malformed or no provider
/// has a record. A provider failure is logged and treated as no record.
pub async fn enrichment_for_asin(
    providers: &dyn MetadataProviders,
    asin: &str,
    region: &str,
) -> Option<Enrichment> {
    let asin = asin.trim().to_ascii_uppercase();
    if !is_valid_asin(&asin) {
        return None;
    }
    records_for_asin(providers, &asin, region)
        .await
        .iter()
        .find_map(|record| enrichment_from_record(record, &asin))
}

/// Every provider record for `asin`, in provider order, without those that
/// name a different ASIN.
///
/// A record that names no ASIN is kept: the provider answered a lookup by
/// this ASIN. A record that names another one describes another edition and
/// would store the wrong ASIN on the matched row.
///
/// # Arguments
///
/// * `providers` - Deployed metadata providers.
/// * `asin` - Upper-case ASIN, already validated.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// The matching records. A provider failure is logged and contributes none.
async fn records_for_asin(
    providers: &dyn MetadataProviders,
    asin: &str,
    region: &str,
) -> Vec<MetadataRecord> {
    let region = normalize_region(region);
    let answer = providers
        .lookup(&MetadataLookupParams {
            identifiers: vec![asin_identifier(asin, &region)],
            marketplace: Some(region.clone()),
            limit: Some(1),
        })
        .await;
    for failure in &answer.failures {
        tracing::debug!(
            provider = %failure.provider,
            asin = %asin,
            error = %failure.message,
            "metadata provider lookup failed"
        );
    }
    answer
        .records
        .into_iter()
        .map(|provided| provided.record)
        .filter(|record| {
            record_identifier(record, IdentifierScheme::Asin)
                .is_none_or(|named| named.eq_ignore_ascii_case(asin))
        })
        .collect()
}

/// Maps a provider record into enrichment fields.
///
/// # Arguments
///
/// * `record` - The provider's record.
/// * `asin` - ASIN asked for; used when the record does not name one.
///
/// # Returns
///
/// `None` when the record has no title.
#[must_use]
pub fn enrichment_from_record(record: &MetadataRecord, asin: &str) -> Option<Enrichment> {
    let title = non_empty(Some(record.title.as_str()))?;
    let series = record.series_entries.first();
    Some(Enrichment {
        asin: record_identifier(record, IdentifierScheme::Asin)
            .map_or_else(|| asin.to_string(), str::to_string),
        title,
        authors: join_credits(record, CreditRole::Author),
        narrators: join_credits(record, CreditRole::Narrator),
        series: series.and_then(|s| non_empty(Some(s.name.as_str()))),
        series_index: series.and_then(|s| non_empty(s.position.as_deref())),
        series_asin: series.and_then(|s| non_empty(s.store_series_id.as_deref())),
        length_minutes: record.length_minutes.map(i64::from),
        publisher: non_empty(record.publisher.as_deref()),
        subtitle: non_empty(record.subtitle.as_deref()),
        cover_url: non_empty(record.cover_url.as_deref()),
        isbn: record_identifier(record, IdentifierScheme::Isbn).map(str::to_string),
        published_at: non_empty(record.published_at.as_deref()),
        categories: non_empty(record.categories.as_deref()),
        description: non_empty(record.description.as_deref()),
        language: non_empty(record.language.as_deref()),
        confidence: None,
    })
}

/// Trimmed copy of `value`, or `None` when it is absent or blank.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Joins the names credited with `role`, in billing order, with `, `.
fn join_credits(record: &MetadataRecord, role: CreditRole) -> Option<String> {
    let names: Vec<&str> = record
        .credits
        .iter()
        .filter(|c| c.role == role)
        .map(|c| c.name.trim())
        .filter(|n| !n.is_empty())
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// Parses RFC 3339 or `YYYY-MM-DD` release dates as UTC midnight; empty/invalid yield `None`.
fn parse_release_date(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // Providers sometimes print `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS.sssZ` variants.
    if let Ok(dt) = chrono::NaiveDate::parse_from_str(raw.get(..10).unwrap_or(raw), "%Y-%m-%d") {
        return dt.and_hms_opt(0, 0, 0).map(|naive| naive.and_utc());
    }
    None
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
    use crate::providers::{
        ChaptersAnswer, MetadataAnswer, NoMetadataProviders, ProvidedRecord, ProviderFailure,
    };
    use bookclerk_library::NewBook;
    use bookclerk_plugin_sdk::{Credit, MetadataChaptersParams, MetadataSearchParams, SeriesEntry};

    #[tokio::test]
    async fn enrichment_keeps_libro_runtime_over_audible() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("libro", "user@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut seed = NewBook::minimal("9781234567890", "user@example.com", "us", "Libro Title");
        seed.source = "libro".into();
        seed.plugin_key = "libro".into();
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
            .upsert_account("libro", "user@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut seed = NewBook::minimal("9781234567890", "user@example.com", "us", "Libro Title");
        seed.source = "libro".into();
        seed.plugin_key = "libro".into();
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

    /// Answers each ASIN lookup with the record registered for it.
    struct ByAsin(Vec<(String, MetadataRecord)>);

    #[async_trait::async_trait]
    impl MetadataProviders for ByAsin {
        async fn lookup(&self, params: &MetadataLookupParams) -> MetadataAnswer {
            let asin = &params.identifiers[0].value;
            MetadataAnswer {
                records: self
                    .0
                    .iter()
                    .filter(|(a, _)| a == asin)
                    .map(|(_, record)| ProvidedRecord {
                        provider: "audnexus".into(),
                        record: record.clone(),
                    })
                    .collect(),
                failures: vec![ProviderFailure {
                    provider: "broken".into(),
                    message: "down".into(),
                }],
            }
        }

        async fn search(&self, _params: &MetadataSearchParams) -> MetadataAnswer {
            MetadataAnswer::default()
        }

        async fn chapters(&self, _params: &MetadataChaptersParams) -> ChaptersAnswer {
            ChaptersAnswer::default()
        }
    }

    fn credit(name: &str, role: CreditRole) -> Credit {
        Credit {
            name: name.into(),
            role,
            ..Credit::default()
        }
    }

    fn full_record() -> MetadataRecord {
        MetadataRecord {
            title: "Test Title".into(),
            subtitle: Some("A Subtitle".into()),
            publisher: Some("Pub Co".into()),
            length_minutes: Some(320),
            published_at: Some("2019-12-03T00:00:00.000Z".into()),
            language: Some("english".into()),
            description: Some("A long blurb".into()),
            categories: Some("Literature & Fiction; Adventure".into()),
            cover_url: Some("https://img.example/cover.jpg".into()),
            identifiers: vec![
                asin_identifier("B00TEST001", "us"),
                crate::providers::isbn_identifier("9781234567890"),
            ],
            credits: vec![
                credit("Ann Author", CreditRole::Author),
                credit("Ned Narrator", CreditRole::Narrator),
                credit(" ", CreditRole::Author),
                credit("Bea Author", CreditRole::Author),
            ],
            series_entries: vec![SeriesEntry {
                name: "Series A".into(),
                position: Some("1".into()),
                store_series_id: Some("B00SER0001".into()),
            }],
            work_kind: bookclerk_plugin_sdk::WorkKind::Book,
            expression_kind: bookclerk_plugin_sdk::ExpressionKind::Narration,
            media_kind: MediaKind::Audio,
            ..MetadataRecord::default()
        }
    }

    #[test]
    fn record_maps_to_enrichment() {
        let e = enrichment_from_record(&full_record(), "B00ASKED01").unwrap();
        assert_eq!(e.asin, "B00TEST001");
        assert_eq!(e.title, "Test Title");
        assert_eq!(e.authors.as_deref(), Some("Ann Author, Bea Author"));
        assert_eq!(e.narrators.as_deref(), Some("Ned Narrator"));
        assert_eq!(e.length_minutes, Some(320));
        assert_eq!(e.series.as_deref(), Some("Series A"));
        assert_eq!(e.series_index.as_deref(), Some("1"));
        assert_eq!(e.series_asin.as_deref(), Some("B00SER0001"));
        assert_eq!(e.publisher.as_deref(), Some("Pub Co"));
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
        assert_eq!(e.isbn.as_deref(), Some("9781234567890"));
        assert_eq!(e.confidence, None);
    }

    #[test]
    fn sparse_record_falls_back_to_the_asked_asin() {
        let record = MetadataRecord {
            title: " Bare ".into(),
            subtitle: Some("  ".into()),
            ..MetadataRecord::default()
        };
        let e = enrichment_from_record(&record, "B00ASKED01").unwrap();
        assert_eq!(e.asin, "B00ASKED01");
        assert_eq!(e.title, "Bare");
        assert_eq!(e.subtitle, None);
        assert_eq!(e.authors, None);
        assert_eq!(e.series, None);
        assert!(enrichment_from_record(&MetadataRecord::default(), "B00ASKED01").is_none());
    }

    #[tokio::test]
    async fn asin_lookup_uses_the_first_titled_record() {
        let untitled = MetadataRecord::default();
        let providers = ByAsin(vec![
            ("B00TEST001".into(), untitled),
            ("B00TEST001".into(), full_record()),
        ]);
        let e = enrichment_for_asin(&providers, " b00test001 ", "us")
            .await
            .unwrap();
        assert_eq!(e.title, "Test Title");
        assert!(enrichment_for_asin(&providers, "B00OTHER01", "us")
            .await
            .is_none());
        assert!(enrichment_for_asin(&providers, "not an asin", "us")
            .await
            .is_none());
        assert!(
            enrichment_for_asin(&NoMetadataProviders, "B00TEST001", "us")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn asin_titles_are_scored_without_catalog_search() {
        let providers = ByAsin(vec![("B00TEST001".into(), full_record())]);
        let http = public_http_client().unwrap();
        let query = MatchQuery {
            title: "B00TEST001",
            subtitle: None,
            author: Some("Ann Author"),
            narrator: None,
            isbn: None,
            duration_minutes: Some(320.0),
        };
        let matched = lookup_by_metadata_with_client(&http, &providers, &query, "us", 0.0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(matched.enrichment.asin, "B00TEST001");
        assert_eq!(matched.enrichment.confidence, Some(matched.confidence));
        let none = lookup_by_metadata_with_client(&http, &providers, &query, "us", 1.01)
            .await
            .unwrap();
        assert!(none.is_none(), "nothing reaches a confidence above 1");
        let blank = MatchQuery {
            title: "  ",
            subtitle: None,
            author: None,
            narrator: None,
            isbn: None,
            duration_minutes: None,
        };
        assert!(
            lookup_by_metadata_with_client(&http, &providers, &blank, "us", 0.0)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn release_dates_parse_both_shapes() {
        assert!(parse_release_date("2019-12-03T00:00:00.000Z").is_some());
        assert!(parse_release_date("2019-12-03").is_some());
        assert!(parse_release_date("  ").is_none());
        assert!(parse_release_date("soon").is_none());
    }

    #[test]
    fn confidence_percent_conversion() {
        assert!((confidence_percent_to_fraction(90) - 0.9).abs() < f64::EPSILON);
        assert!((confidence_percent_to_fraction(100) - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    #[ignore = "network: public Audible catalog; needs a provider that answers ASINs"]
    async fn live_forward_the_foundation_matches_above_90() {
        let query = MatchQuery {
            title: "Forward the Foundation",
            subtitle: None,
            author: Some("Isaac Asimov"),
            narrator: Some("Larry McKeever"),
            isbn: Some("9780307970626"),
            duration_minutes: Some(970.0),
        };
        // A provider that knows the book under whichever ASIN the catalog
        // search offers, so the scoring itself is what is tested.
        let record = MetadataRecord {
            title: "Forward the Foundation".into(),
            length_minutes: Some(970),
            identifiers: vec![crate::providers::isbn_identifier("9780307970626")],
            credits: vec![
                credit("Isaac Asimov", CreditRole::Author),
                credit("Larry McKeever", CreditRole::Narrator),
            ],
            ..MetadataRecord::default()
        };
        let matched = lookup_by_metadata(&EveryAsin(record), &query, "us", 0.90)
            .await
            .expect("lookup")
            .expect("a match above 0.90");
        assert!(matched.confidence >= 0.90, "{}", matched.confidence);
        assert!(is_valid_asin(&matched.enrichment.asin));
    }

    /// Answers every ASIN lookup with one record, under the asked ASIN.
    struct EveryAsin(MetadataRecord);

    #[async_trait::async_trait]
    impl MetadataProviders for EveryAsin {
        async fn lookup(&self, _params: &MetadataLookupParams) -> MetadataAnswer {
            MetadataAnswer {
                records: vec![ProvidedRecord {
                    provider: "fake".into(),
                    record: self.0.clone(),
                }],
                failures: Vec::new(),
            }
        }

        async fn search(&self, _params: &MetadataSearchParams) -> MetadataAnswer {
            MetadataAnswer::default()
        }

        async fn chapters(&self, _params: &MetadataChaptersParams) -> ChaptersAnswer {
            ChaptersAnswer::default()
        }
    }

    #[tokio::test]
    async fn asin_matching_skips_other_asins_and_non_audiobooks_and_scores_every_record() {
        let http = public_http_client().unwrap();
        let query = MatchQuery {
            title: "B00TEST001",
            subtitle: None,
            author: Some("Ann Author"),
            narrator: Some("Ned Narrator"),
            isbn: None,
            duration_minutes: Some(320.0),
        };
        let mut other_asin = full_record();
        other_asin.identifiers = vec![asin_identifier("B00OTHER01", "us")];
        let mut ebook = full_record();
        ebook.media_kind = MediaKind::Ebook;
        for refused in [other_asin.clone(), ebook] {
            let providers = ByAsin(vec![("B00TEST001".into(), refused)]);
            assert!(
                lookup_by_metadata_with_client(&http, &providers, &query, "us", 0.0)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            enrichment_for_asin(
                &ByAsin(vec![("B00TEST001".into(), other_asin)]),
                "B00TEST001",
                "us"
            )
            .await
            .is_none(),
            "Discover ignores a record for another ASIN too"
        );

        // A sparse first record does not hide a complete later one.
        let sparse = MetadataRecord {
            title: "Unrelated".into(),
            ..MetadataRecord::default()
        };
        let providers = ByAsin(vec![
            ("B00TEST001".into(), sparse),
            ("B00TEST001".into(), full_record()),
        ]);
        let matched = lookup_by_metadata_with_client(&http, &providers, &query, "us", 0.0)
            .await
            .unwrap()
            .expect("a record matches");
        let full_only = ByAsin(vec![("B00TEST001".into(), full_record())]);
        let full = lookup_by_metadata_with_client(&http, &full_only, &query, "us", 0.0)
            .await
            .unwrap()
            .expect("the complete record matches");
        assert!(
            matched.confidence >= full.confidence,
            "{} < {}",
            matched.confidence,
            full.confidence
        );
    }
}
