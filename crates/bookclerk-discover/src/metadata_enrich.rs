//! Fill library metadata gaps from the deployed metadata providers.
//!
//! The Open Library plugin is the usual provider here: a held edition with
//! an ISBN is looked up by ISBN, and one without is searched by title and
//! author. The answer fills what the edition lacks: its work's description,
//! subjects and cover, its expression's language and its own cover, plus
//! the work's Open Library id. Lookups are low-volume: each run asks at most
//! [`MetadataEnrichOptions::max_requests`] times and pauses between asks, as
//! Open Library's API guidelines ask (bulk data comes from their dumps).
//!
//! The edition is marked enriched only after the rest is written, so a
//! write that fails leaves it unmarked and the next run tries again.

use std::time::Duration;

use bookclerk_enrich::{
    isbn_identifier, normalize_isbn, record_identifier, MetadataAnswer, MetadataProviders,
    ProvidedRecord,
};
use bookclerk_library::{EditionEnrichment, EnrichCandidate, LibraryStore};
use bookclerk_plugin_sdk::{
    IdentifierScheme, MetadataLookupParams, MetadataRecord, MetadataSearchParams,
};
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
/// moved into plugins; such editions count as already enriched.
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

/// Which of an edition's catalog fields are empty.
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
    /// The empty fields of `edition`.
    fn of(edition: &EnrichCandidate) -> Self {
        let blank = |v: &Option<String>| v.as_deref().unwrap_or("").trim().is_empty();
        Self {
            subjects: blank(&edition.subjects),
            description: blank(&edition.description),
            cover: blank(&edition.cover_url),
            language: blank(&edition.language),
        }
    }

    /// True when any field is empty.
    fn any(self) -> bool {
        self.subjects || self.description || self.cover || self.language
    }

    /// True when `record` has a non-blank value for at least one gap.
    fn filled_by(self, record: &MetadataRecord) -> bool {
        let has =
            |gap: bool, value: Option<&String>| gap && value.is_some_and(|v| !v.trim().is_empty());
        has(self.subjects, record.categories.as_ref())
            || has(self.description, record.description.as_ref())
            || has(self.cover, record.cover_url.as_ref())
            || has(self.language, record.language.as_ref())
    }
}

/// Editions read per library page while looking for gaps to fill.
#[cfg(not(test))]
const ENRICH_PAGE: u64 = 200;

/// Editions read per library page in tests, small so the tests' few
/// editions span several pages.
#[cfg(test)]
const ENRICH_PAGE: u64 = 2;

/// Fills missing description, subjects, cover and language from the
/// deployed metadata providers.
///
/// Walks the held editions and skips those a provider already filled. With
/// no provider deployed, every answer is empty and nothing changes.
///
/// # Arguments
///
/// * `library` - Library store read and written.
/// * `providers` - Deployed metadata providers.
/// * `opts` - Pacing for this run.
///
/// # Returns
///
/// The number of editions filled.
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

    // The library is read a page at a time, and reading stops once the
    // run's request cap is spent.
    let mut after: Option<String> = None;
    'pages: loop {
        // A spent budget reads no further page, even when the editions left
        // would not have needed a request.
        if requests >= opts.max_requests {
            break;
        }
        let page = library
            .enrich_candidates_page(after.as_deref(), ENRICH_PAGE)
            .await?;
        for book in page.candidates {
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
                break 'pages;
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
                    let matched = answer
                        .records
                        .iter()
                        .find(|provided| gaps.filled_by(&provided.record))
                        .cloned();
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
                    let matched = first_title_match(&answer, &book.title, gaps);
                    (answer, matched)
                }
            };
            for failure in &answer.failures {
                tracing::debug!(
                    edition_id = %book.edition_id,
                    provider = %failure.provider,
                    error = %failure.message,
                    "metadata provider failed for this edition"
                );
            }
            let Some(matched) = matched else {
                continue;
            };
            if apply_record(library, &book, gaps, &matched).await? {
                enriched += 1;
                tracing::info!(
                    edition_id = %book.edition_id,
                    title = %book.title,
                    provider = %matched.provider,
                    "filled edition metadata from a metadata provider"
                );
            }
        }
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }

    Ok(enriched)
}

/// True when a provider already filled `book`.
fn already_enriched(book: &EnrichCandidate) -> bool {
    book.enrich_source.as_deref().is_some_and(|source| {
        source.starts_with(METADATA_ENRICH_SOURCE_PREFIX) || source == LEGACY_OPENLIBRARY_SOURCE
    })
}

/// The first search hit with the same title as `title`, once both are
/// normalized, that fills one of `gaps`.
///
/// Titles must be equal, not merely overlap: `Foundation` must not take the
/// blurb, cover and work id of `Foundation and Empire`.
fn first_title_match(answer: &MetadataAnswer, title: &str, gaps: Gaps) -> Option<ProvidedRecord> {
    let wanted = normalized_title(title);
    if wanted.is_empty() {
        return None;
    }
    answer
        .records
        .iter()
        .find(|provided| {
            normalized_title(&provided.record.title) == wanted && gaps.filled_by(&provided.record)
        })
        .cloned()
}

/// `title` lower-cased, with punctuation dropped and whitespace collapsed,
/// so `Dune: Messiah` and `dune  messiah` compare equal.
fn normalized_title(title: &str) -> String {
    title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Writes the fields `matched` fills onto the edition, its work and its
/// expression, then marks the edition.
///
/// # Returns
///
/// `false` when the record fills none of the edition's gaps.
async fn apply_record(
    library: &LibraryStore,
    book: &EnrichCandidate,
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

    // The work's Open Library id first: the edition is marked last, so a
    // failed write leaves it unmarked and the next run asks again (#640).
    if let (Some(work_id), Some(ol)) = (
        &book.work_id,
        record_identifier(record, IdentifierScheme::OpenLibraryWork),
    ) {
        library
            .add_work_identifier(
                work_id,
                bookclerk_resolve::IdentifierScheme::OpenLibraryWork,
                ol,
            )
            .await?;
    }
    library
        .enrich_edition(
            &book.edition_id,
            &EditionEnrichment {
                description,
                subjects,
                cover_url,
                language,
                source: format!("{METADATA_ENRICH_SOURCE_PREFIX}{}", matched.provider),
                confidence: PROVIDER_CONFIDENCE,
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
    use bookclerk_library::{NewBook, WorkEnrichment};
    use bookclerk_plugin_sdk::{Identifier, MetadataChaptersParams};

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
    async fn titles_without_isbn_are_searched_and_must_match_exactly() {
        let (store, uuids) =
            store_with(&[("p1", "Foundation", None), ("p2", "Unrelated Book", None)]).await;
        let mut sequel = ol_record("Foundation and Empire");
        sequel.description = Some("The sequel's blurb".into());
        let providers = Fixed {
            search: vec![
                ol_record("Something Else"),
                sequel,
                ol_record(" FOUNDATION. "),
            ],
            ..Fixed::default()
        };
        let n = enrich_books_from_metadata(&store, &providers, &opts(5))
            .await
            .unwrap();
        assert_eq!(n, 1, "only the exactly matching title is filled");
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

    /// A library longer than one page is read to the end, each title once.
    #[tokio::test]
    async fn every_page_of_the_library_is_read() {
        let isbns = [
            "9780441172719",
            "9780553293357",
            "9780141439518",
            "9780141439600",
            "9780140449136",
        ];
        let books: Vec<(String, String)> = isbns
            .iter()
            .enumerate()
            .map(|(i, isbn)| (format!("p{i}"), (*isbn).to_string()))
            .collect();
        let seeds: Vec<(&str, &str, Option<&str>)> = books
            .iter()
            .map(|(p, isbn)| (p.as_str(), p.as_str(), Some(isbn.as_str())))
            .collect();
        let (store, _) = store_with(&seeds).await;
        let providers = Fixed {
            by_isbn: isbns
                .iter()
                .map(|isbn| ((*isbn).to_string(), ol_record("Any")))
                .collect(),
            ..Fixed::default()
        };
        let n = enrich_books_from_metadata(&store, &providers, &opts(50))
            .await
            .unwrap();
        assert_eq!(n, isbns.len(), "every title on every page was filled");
        assert_eq!(providers.asks.lock().unwrap().len(), isbns.len());
    }

    /// A run whose budget is spent reads no further page of the library.
    #[tokio::test]
    async fn a_spent_budget_reads_no_further_page() {
        let (store, _) = store_with(&[
            ("p1", "A", Some("9780441172719")),
            ("p2", "B", Some("9780553293357")),
            ("p3", "C", Some("9780141439518")),
        ])
        .await;
        let providers = Fixed::default();
        // Nothing is asked with no budget; with one request, only the first
        // page's first title is.
        enrich_books_from_metadata(&store, &providers, &opts(0))
            .await
            .unwrap();
        assert!(providers.asks.lock().unwrap().is_empty());
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
        // Every field filled by someone else, and a legacy Open Library mark.
        let full = store.edition_of_book(&uuids[0]).await.unwrap().unwrap();
        store
            .enrich_edition(
                &full,
                &EditionEnrichment {
                    description: Some("d".into()),
                    language: Some("en".into()),
                    cover_url: Some("https://x/c.jpg".into()),
                    subjects: Some("s".into()),
                    source: "audible".into(),
                    confidence: 0.9,
                },
            )
            .await
            .unwrap();
        let legacy = store.edition_of_book(&uuids[1]).await.unwrap().unwrap();
        store
            .enrich_edition(
                &legacy,
                &EditionEnrichment {
                    source: "openlibrary".into(),
                    ..EditionEnrichment::default()
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
        let all = Gaps {
            subjects: true,
            description: true,
            cover: true,
            language: true,
        };
        assert!(
            first_title_match(&answer, "Dune", all).is_none(),
            "blank hit titles never match"
        );
        assert!(first_title_match(&answer, "  ", all).is_none());
        assert_eq!(normalized_title("  Dune:  Messiah! "), "dune messiah");
    }

    #[test]
    fn gaps_report_each_field_on_its_own() {
        let none = Gaps {
            subjects: false,
            description: false,
            cover: false,
            language: false,
        };
        assert!(!none.any());
        let one = |field: usize| Gaps {
            subjects: field == 0,
            description: field == 1,
            cover: field == 2,
            language: field == 3,
        };
        let filled = |field: usize, value: &str| {
            let value = Some(value.to_string());
            match field {
                0 => MetadataRecord {
                    categories: value,
                    ..MetadataRecord::default()
                },
                1 => MetadataRecord {
                    description: value,
                    ..MetadataRecord::default()
                },
                2 => MetadataRecord {
                    cover_url: value,
                    ..MetadataRecord::default()
                },
                _ => MetadataRecord {
                    language: value,
                    ..MetadataRecord::default()
                },
            }
        };
        for field in 0..4 {
            assert!(one(field).any(), "gap {field} alone counts");
            assert!(
                one(field).filled_by(&filled(field, "x")),
                "gap {field} filled"
            );
            assert!(
                !one(field).filled_by(&filled(field, "  ")),
                "gap {field} blank"
            );
            assert!(
                !one(field).filled_by(&filled((field + 1) % 4, "x")),
                "gap {field} is not filled by another field"
            );
        }
    }

    #[tokio::test]
    async fn a_record_that_fills_one_gap_marks_the_title() {
        let (store, uuids) = store_with(&[("p1", "Dune", Some("9780441172719"))]).await;
        let providers = Fixed {
            by_isbn: vec![(
                "9780441172719".into(),
                MetadataRecord {
                    title: "Dune".into(),
                    language: Some("en".into()),
                    ..MetadataRecord::default()
                },
            )],
            ..Fixed::default()
        };
        assert_eq!(
            enrich_books_from_metadata(&store, &providers, &opts(5))
                .await
                .unwrap(),
            1
        );
        let book = store.get_book_by_uuid(&uuids[0]).await.unwrap().unwrap();
        assert_eq!(book.language.as_deref(), Some("en"));
        assert_eq!(book.description, None);
        assert!(book.enrich_source.is_some(), "the title is marked");
    }

    #[tokio::test]
    async fn the_first_record_that_fills_a_gap_is_used() {
        let (store, uuids) = store_with(&[("p1", "Dune", Some("9780441172719"))]).await;
        let providers = Fixed {
            by_isbn: vec![
                (
                    "9780441172719".into(),
                    MetadataRecord {
                        title: "Dune".into(),
                        ..MetadataRecord::default()
                    },
                ),
                ("9780441172719".into(), ol_record("Dune")),
            ],
            ..Fixed::default()
        };
        assert_eq!(
            enrich_books_from_metadata(&store, &providers, &opts(5))
                .await
                .unwrap(),
            1
        );
        let book = store.get_book_by_uuid(&uuids[0]).await.unwrap().unwrap();
        assert_eq!(book.description.as_deref(), Some("A blurb"));
    }

    #[tokio::test]
    async fn a_described_work_keeps_its_own_values() {
        let (store, uuids) = store_with(&[("p1", "Dune", Some("9780441172719"))]).await;
        let work_id = store
            .work_id_for_book(&uuids[0])
            .await
            .unwrap()
            .expect("upsert resolves a work");
        store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    description: Some("Another edition's blurb".into()),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap();
        let providers = Fixed {
            by_isbn: vec![("9780441172719".into(), ol_record("Dune"))],
            ..Fixed::default()
        };
        assert_eq!(
            enrich_books_from_metadata(&store, &providers, &opts(5))
                .await
                .unwrap(),
            1
        );
        let work = store.get_work(&work_id).await.unwrap().expect("work");
        assert_eq!(work.description.as_deref(), Some("Another edition's blurb"));
        assert_eq!(
            work.cover_url.as_deref(),
            Some("https://covers.openlibrary.org/b/id/1-L.jpg"),
            "the work still takes what it lacked"
        );
        // The work's own description was no gap, so the edition's flat row
        // takes only what was filled.
        let book = store.get_book_by_uuid(&uuids[0]).await.unwrap().unwrap();
        assert_eq!(book.description, None);
        assert_eq!(
            book.cover_url.as_deref(),
            Some("https://covers.openlibrary.org/b/id/1-L.jpg")
        );
    }
}
