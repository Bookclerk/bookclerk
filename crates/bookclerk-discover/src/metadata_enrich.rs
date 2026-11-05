//! Fill library metadata gaps from the deployed metadata providers.
//!
//! The Open Library plugin is the usual provider here: a title with an ISBN
//! is looked up by ISBN, and one without is searched by title and author. The
//! answer fills the title's missing description, subjects, cover and
//! language, and its work's description, subjects and cover plus the Open
//! Library work id. Lookups are low-volume: each run asks at most
//! [`MetadataEnrichOptions::max_requests`] times and pauses between asks, as
//! Open Library's API guidelines ask (bulk data comes from their dumps).
//!
//! The work is written before the title is marked enriched, so a work write
//! that fails leaves the title unmarked and the next run tries again.

use std::time::Duration;

use bookclerk_enrich::{
    isbn_identifier, normalize_isbn, record_identifier, MetadataAnswer, MetadataProviders,
    ProvidedRecord,
};
use bookclerk_library::{BookRecord, CatalogEnrichmentFields, LibraryStore, WorkEnrichment};
use bookclerk_plugin_sdk::{IdentifierScheme, MetadataLookupParams, MetadataSearchParams};
use chrono::Utc;
use tokio::time::sleep;

use crate::error::Result;

/// Default pause between provider asks (~2.5 per second, under Open Library's
/// identified cap).
const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(400);
/// Default cap on provider asks in one run.
pub const DEFAULT_MAX_REQUESTS_PER_RUN: usize = 25;
/// Most search hits asked for when a title has no ISBN.
const SEARCH_LIMIT: u32 = 5;
/// `enrich_source` prefix marking a title filled by a metadata provider; the
/// provider's plugin key follows it.
pub const METADATA_ENRICH_SOURCE_PREFIX: &str = "metadata:";
/// `enrich_source` the in-process Open Library code wrote before providers
/// moved into plugins; such titles count as already enriched.
const LEGACY_OPENLIBRARY_SOURCE: &str = "openlibrary";
/// Confidence recorded for a provider fill (an ISBN or title match, not scored).
const PROVIDER_CONFIDENCE: f64 = 0.7;

/// Pacing for one enrichment run.
#[derive(Debug, Clone)]
pub struct MetadataEnrichOptions {
    /// Pause before each provider ask.
    pub min_interval: Duration,
    /// Most provider asks in one run; titles past the cap wait for the next run.
    pub max_requests: usize,
}

impl Default for MetadataEnrichOptions {
    fn default() -> Self {
        Self {
            min_interval: DEFAULT_MIN_INTERVAL,
            max_requests: DEFAULT_MAX_REQUESTS_PER_RUN,
        }
    }
}

/// Which of a title's catalog fields are empty.
#[derive(Debug, Clone, Copy)]
struct Gaps {
    /// No subjects.
    subjects: bool,
    /// No description.
    description: bool,
    /// No cover URL.
    cover: bool,
    /// No language.
    language: bool,
}

impl Gaps {
    /// The empty fields of `book`.
    fn of(book: &BookRecord) -> Self {
        let blank = |v: &Option<String>| v.as_deref().unwrap_or("").trim().is_empty();
        Self {
            subjects: blank(&book.subjects),
            description: blank(&book.description),
            cover: blank(&book.cover_url),
            language: blank(&book.language),
        }
    }

    /// True when any field is empty.
    fn any(self) -> bool {
        self.subjects || self.description || self.cover || self.language
    }
}

/// Fills missing description, subjects, cover and language from the
/// deployed metadata providers.
///
/// Skips titles a provider already filled. With no provider deployed, every
/// answer is empty and nothing changes.
///
/// # Arguments
///
/// * `library` - Library store read and written.
/// * `providers` - Deployed metadata providers.
/// * `opts` - Pacing for this run.
///
/// # Returns
///
/// The number of titles filled.
///
/// # Errors
///
/// Returns an error when a library read or write fails.
pub async fn enrich_books_from_metadata(
    library: &LibraryStore,
    providers: &dyn MetadataProviders,
    opts: &MetadataEnrichOptions,
) -> Result<usize> {
    let mut enriched = 0usize;
    let mut requests = 0usize;

    for book in library.list_books(None).await? {
        if already_enriched(&book) {
            continue;
        }
        let gaps = Gaps::of(&book);
        if !gaps.any() {
            continue;
        }
        let isbn = book
            .isbn
            .as_deref()
            .map(normalize_isbn)
            .filter(|s| !s.is_empty());
        if isbn.is_none() && book.title.trim().is_empty() {
            continue;
        }
        if requests >= opts.max_requests {
            tracing::info!(
                requests,
                max = opts.max_requests,
                "metadata enrichment capped for this run"
            );
            break;
        }
        sleep(opts.min_interval).await;
        requests += 1;
        let (answer, matched) = match isbn.as_deref() {
            Some(isbn) => {
                let answer = providers
                    .lookup(&MetadataLookupParams {
                        identifiers: vec![isbn_identifier(isbn)],
                        marketplace: None,
                        limit: Some(1),
                    })
                    .await;
                let matched = answer.records.first().cloned();
                (answer, matched)
            }
            None => {
                let answer = providers
                    .search(&MetadataSearchParams {
                        title: book.title.trim().to_string(),
                        author: book
                            .authors
                            .as_deref()
                            .and_then(primary_author)
                            .map(str::to_string),
                        language: None,
                        marketplace: None,
                        limit: Some(SEARCH_LIMIT),
                    })
                    .await;
                let matched = first_title_match(&answer, &book.title);
                (answer, matched)
            }
        };
        for failure in &answer.failures {
            tracing::debug!(
                uuid = %book.uuid,
                provider = %failure.provider,
                error = %failure.message,
                "metadata provider failed for this title"
            );
        }
        let Some(matched) = matched else {
            continue;
        };
        if apply_record(library, &book, gaps, &matched).await? {
            enriched += 1;
            tracing::info!(
                uuid = %book.uuid,
                title = %book.title,
                provider = %matched.provider,
                "filled title metadata from a metadata provider"
            );
        }
    }

    Ok(enriched)
}

/// True when a provider already filled `book`.
fn already_enriched(book: &BookRecord) -> bool {
    book.enrich_source.as_deref().is_some_and(|source| {
        source.starts_with(METADATA_ENRICH_SOURCE_PREFIX) || source == LEGACY_OPENLIBRARY_SOURCE
    })
}

/// The first search hit whose title contains, or is contained in, `title`
/// (case-insensitive).
fn first_title_match(answer: &MetadataAnswer, title: &str) -> Option<ProvidedRecord> {
    let wanted = title.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    answer
        .records
        .iter()
        .find(|provided| {
            let got = provided.record.title.trim().to_lowercase();
            !got.is_empty() && (got.contains(&wanted) || wanted.contains(&got))
        })
        .cloned()
}

/// Writes the fields `matched` fills: the work first, then the title.
///
/// # Returns
///
/// `false` when the record fills none of the title's gaps.
async fn apply_record(
    library: &LibraryStore,
    book: &BookRecord,
    gaps: Gaps,
    matched: &ProvidedRecord,
) -> Result<bool> {
    let record = &matched.record;
    let fill = |gap: bool, value: Option<&String>| -> Option<String> {
        value
            .filter(|_| gap)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let subjects = fill(gaps.subjects, record.categories.as_ref());
    let description = fill(gaps.description, record.description.as_ref());
    let cover_url = fill(gaps.cover, record.cover_url.as_ref());
    let language = fill(gaps.language, record.language.as_ref());
    if subjects.is_none() && description.is_none() && cover_url.is_none() && language.is_none() {
        return Ok(false);
    }

    // The work first: if one of its writes fails, the title stays unmarked
    // and the next run asks again (#640).
    if let Some(work_id) = library.work_id_for_book(&book.uuid).await? {
        let work = WorkEnrichment {
            description: description.clone().or_else(|| book.description.clone()),
            subjects: subjects.clone().or_else(|| book.subjects.clone()),
            categories: book.categories.clone(),
            cover_url: cover_url.clone().or_else(|| book.cover_url.clone()),
        };
        library.set_work_enrichment(&work_id, &work).await?;
        if let Some(ol) = record_identifier(record, IdentifierScheme::OpenLibraryWork) {
            library
                .add_work_identifier(
                    &work_id,
                    bookclerk_resolve::IdentifierScheme::OpenLibraryWork,
                    ol,
                )
                .await?;
        }
    }

    library
        .update_catalog_enrichment(
            &book.uuid,
            &CatalogEnrichmentFields {
                description,
                language,
                cover_url,
                subjects,
                categories: None,
                enrich_source: Some(format!(
                    "{METADATA_ENRICH_SOURCE_PREFIX}{}",
                    matched.provider
                )),
                enrich_confidence: Some(PROVIDER_CONFIDENCE),
                enrich_updated_at: Some(Utc::now()),
            },
        )
        .await?;
    Ok(true)
}

/// First non-empty author token from a comma/semicolon/ampersand-separated list.
fn primary_author(authors: &str) -> Option<&str> {
    authors
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use bookclerk_enrich::{ChaptersAnswer, ProviderFailure};
    use bookclerk_library::NewBook;
    use bookclerk_plugin_sdk::{Identifier, MetadataChaptersParams, MetadataRecord};

    use super::*;

    /// Answers ISBN lookups and title searches from fixed records, and
    /// counts the asks.
    #[derive(Default)]
    struct Fixed {
        by_isbn: Vec<(String, MetadataRecord)>,
        search: Vec<MetadataRecord>,
        fail: bool,
        asks: Mutex<Vec<String>>,
    }

    impl Fixed {
        fn answer(&self, records: Vec<MetadataRecord>) -> MetadataAnswer {
            MetadataAnswer {
                records: records
                    .into_iter()
                    .map(|record| ProvidedRecord {
                        provider: "openlibrary".into(),
                        record,
                    })
                    .collect(),
                failures: if self.fail {
                    vec![ProviderFailure {
                        provider: "broken".into(),
                        message: "down".into(),
                    }]
                } else {
                    Vec::new()
                },
            }
        }
    }

    #[async_trait]
    impl MetadataProviders for Fixed {
        async fn lookup(&self, params: &MetadataLookupParams) -> MetadataAnswer {
            let isbn = params.identifiers[0].value.clone();
            self.asks.lock().unwrap().push(format!("isbn:{isbn}"));
            self.answer(
                self.by_isbn
                    .iter()
                    .filter(|(i, _)| *i == isbn)
                    .map(|(_, r)| r.clone())
                    .collect(),
            )
        }

        async fn search(&self, params: &MetadataSearchParams) -> MetadataAnswer {
            self.asks.lock().unwrap().push(format!(
                "search:{}:{}",
                params.title,
                params.author.as_deref().unwrap_or("")
            ));
            self.answer(self.search.clone())
        }

        async fn chapters(&self, _params: &MetadataChaptersParams) -> ChaptersAnswer {
            ChaptersAnswer::default()
        }
    }

    fn opts(max_requests: usize) -> MetadataEnrichOptions {
        MetadataEnrichOptions {
            min_interval: Duration::ZERO,
            max_requests,
        }
    }

    fn ol_record(title: &str) -> MetadataRecord {
        MetadataRecord {
            title: title.into(),
            description: Some("A blurb".into()),
            categories: Some("Fiction; Space".into()),
            cover_url: Some("https://covers.openlibrary.org/b/id/1-L.jpg".into()),
            language: Some("eng".into()),
            identifiers: vec![Identifier {
                scheme: IdentifierScheme::OpenLibraryWork,
                value: "OL45883W".into(),
                marketplace: None,
            }],
            ..MetadataRecord::default()
        }
    }

    async fn store_with(books: &[(&str, &str, Option<&str>)]) -> (LibraryStore, Vec<String>) {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("libro", "user@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut uuids = Vec::new();
        for (product_id, title, isbn) in books {
            let mut seed = NewBook::minimal(*product_id, "user@example.com", "us", *title);
            seed.source = "libro".into();
            seed.plugin_key = "libro".into();
            seed.asin = None;
            seed.isbn = isbn.map(str::to_string);
            seed.authors = Some("Ann Author, Bob Other".into());
            uuids.push(store.upsert_book(&seed).await.unwrap().uuid);
        }
        (store, uuids)
    }

    #[tokio::test]
    async fn isbn_titles_are_looked_up_and_marked() {
        let (store, uuids) = store_with(&[("p1", "Dune", Some("978-0-441-17271-9"))]).await;
        let providers = Fixed {
            by_isbn: vec![("9780441172719".into(), ol_record("Dune"))],
            ..Fixed::default()
        };
        let n = enrich_books_from_metadata(&store, &providers, &opts(5))
            .await
            .unwrap();
        assert_eq!(n, 1);
        let book = store.get_book_by_uuid(&uuids[0]).await.unwrap().unwrap();
        assert_eq!(book.description.as_deref(), Some("A blurb"));
        assert_eq!(book.subjects.as_deref(), Some("Fiction; Space"));
        assert_eq!(book.language.as_deref(), Some("eng"));
        assert_eq!(
            book.cover_url.as_deref(),
            Some("https://covers.openlibrary.org/b/id/1-L.jpg")
        );
        assert_eq!(book.enrich_source.as_deref(), Some("metadata:openlibrary"));

        // A second run skips the marked title without asking again.
        let again = enrich_books_from_metadata(&store, &providers, &opts(5))
            .await
            .unwrap();
        assert_eq!(again, 0);
        assert_eq!(providers.asks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn titles_without_isbn_are_searched_and_must_overlap() {
        let (store, uuids) =
            store_with(&[("p1", "Foundation", None), ("p2", "Unrelated Book", None)]).await;
        let providers = Fixed {
            search: vec![
                ol_record("Something Else"),
                ol_record("Foundation and Empire"),
            ],
            ..Fixed::default()
        };
        let n = enrich_books_from_metadata(&store, &providers, &opts(5))
            .await
            .unwrap();
        assert_eq!(n, 1, "only the overlapping title is filled");
        let filled = store.get_book_by_uuid(&uuids[0]).await.unwrap().unwrap();
        assert_eq!(filled.description.as_deref(), Some("A blurb"));
        let skipped = store.get_book_by_uuid(&uuids[1]).await.unwrap().unwrap();
        assert_eq!(skipped.description, None);
        assert_eq!(
            skipped.enrich_source, None,
            "an unmatched title stays unmarked"
        );
        let asks = providers.asks.lock().unwrap().clone();
        assert!(
            asks.contains(&"search:Foundation:Ann Author".to_string()),
            "{asks:?}"
        );
    }

    #[tokio::test]
    async fn the_cap_stops_the_run() {
        let (store, _) = store_with(&[
            ("p1", "One", Some("9780441172719")),
            ("p2", "Two", Some("9780553293357")),
        ])
        .await;
        let providers = Fixed::default();
        enrich_books_from_metadata(&store, &providers, &opts(1))
            .await
            .unwrap();
        assert_eq!(providers.asks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_work_gets_the_fill_even_when_another_provider_failed() {
        let (store, uuids) = store_with(&[("p1", "Dune", Some("9780441172719"))]).await;
        let work_id = store
            .work_id_for_book(&uuids[0])
            .await
            .unwrap()
            .expect("upsert resolves a work");
        let providers = Fixed {
            by_isbn: vec![("9780441172719".into(), ol_record("Dune"))],
            fail: true,
            ..Fixed::default()
        };
        assert_eq!(
            enrich_books_from_metadata(&store, &providers, &opts(5))
                .await
                .unwrap(),
            1
        );
        let work = store.get_work(&work_id).await.unwrap().expect("work");
        assert_eq!(work.description.as_deref(), Some("A blurb"));
        assert_eq!(work.subjects.as_deref(), Some("Fiction; Space"));
        assert_eq!(
            work.cover_url.as_deref(),
            Some("https://covers.openlibrary.org/b/id/1-L.jpg")
        );
    }

    #[tokio::test]
    async fn complete_and_legacy_rows_are_skipped() {
        let (store, uuids) = store_with(&[
            ("p1", "Full", Some("9780441172719")),
            ("p2", "Legacy", Some("9780553293357")),
            ("p3", "  ", None),
        ])
        .await;
        store
            .update_catalog_enrichment(
                &uuids[0],
                &CatalogEnrichmentFields {
                    description: Some("d".into()),
                    language: Some("en".into()),
                    cover_url: Some("https://x/c.jpg".into()),
                    subjects: Some("s".into()),
                    ..CatalogEnrichmentFields::default()
                },
            )
            .await
            .unwrap();
        store
            .update_catalog_enrichment(
                &uuids[1],
                &CatalogEnrichmentFields {
                    enrich_source: Some("openlibrary".into()),
                    ..CatalogEnrichmentFields::default()
                },
            )
            .await
            .unwrap();
        let providers = Fixed::default();
        assert_eq!(
            enrich_books_from_metadata(&store, &providers, &opts(5))
                .await
                .unwrap(),
            0
        );
        assert!(providers.asks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_record_that_fills_nothing_leaves_the_title_unmarked() {
        let (store, uuids) = store_with(&[("p1", "Dune", Some("9780441172719"))]).await;
        let providers = Fixed {
            by_isbn: vec![(
                "9780441172719".into(),
                MetadataRecord {
                    title: "Dune".into(),
                    description: Some("   ".into()),
                    ..MetadataRecord::default()
                },
            )],
            ..Fixed::default()
        };
        assert_eq!(
            enrich_books_from_metadata(&store, &providers, &opts(5))
                .await
                .unwrap(),
            0
        );
        let book = store.get_book_by_uuid(&uuids[0]).await.unwrap().unwrap();
        assert_eq!(book.enrich_source, None);
    }

    #[test]
    fn helpers() {
        assert_eq!(primary_author(" & Ann; Bob"), Some("Ann"));
        assert_eq!(primary_author(" , "), None);
        let answer = MetadataAnswer {
            records: vec![ProvidedRecord {
                provider: "p".into(),
                record: MetadataRecord::default(),
            }],
            failures: Vec::new(),
        };
        assert!(
            first_title_match(&answer, "Dune").is_none(),
            "blank hit titles never match"
        );
        assert!(first_title_match(&answer, "  ").is_none());
    }
}
