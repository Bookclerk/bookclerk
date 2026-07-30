//! Open Library enrichment — low-volume, cached, ToS-compliant.
//!
//! Per https://openlibrary.org/developers/api :
//! - Identify with User-Agent including contact email
//! - ≤ ~1 req/s unidentified, ≤ ~3/s when identified
//! - Cache responses; do not bulk-harvest (use monthly dumps for bulk)
//! - Prefer search.json over hundreds of single-book GETs

use std::time::Duration;

use bookclerk_enrich::normalize_isbn;
use bookclerk_library::{CatalogEnrichmentFields, LibraryStore};
use chrono::Utc;
use serde::Deserialize;
use tokio::time::sleep;

use crate::error::Result;

const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(400);
const DEFAULT_MAX_REQUESTS_PER_RUN: usize = 25;

/// Options for a polite Open Library enrichment pass.
#[derive(Debug, Clone)]
pub struct OpenLibraryOptions {
    /// Contact email embedded in User-Agent (required for identified rate limit).
    pub contact_email: Option<String>,
    pub min_interval: Duration,
    pub max_requests: usize,
}

impl Default for OpenLibraryOptions {
    fn default() -> Self {
        Self {
            contact_email: None,
            min_interval: DEFAULT_MIN_INTERVAL,
            max_requests: DEFAULT_MAX_REQUESTS_PER_RUN,
        }
    }
}

#[derive(Debug, Deserialize)]
struct OlSearchResponse {
    #[serde(default)]
    docs: Vec<OlDoc>,
}

#[derive(Debug, Deserialize)]
struct OlDoc {
    key: Option<String>,
    title: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    author_name: Vec<String>,
    #[serde(default)]
    subject: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    isbn: Vec<String>,
    cover_i: Option<i64>,
    #[serde(default)]
    language: Vec<String>,
    first_sentence: Option<OlFirstSentence>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OlFirstSentence {
    Text(String),
    List(Vec<String>),
}

impl OlFirstSentence {
    fn as_text(&self) -> Option<String> {
        match self {
            Self::Text(s) => Some(s.clone()),
            Self::List(v) => v.first().cloned(),
        }
    }
}

/// Fill missing description/subjects/cover/language from Open Library.
///
/// Skips rows already enriched from Open Library. Caps remote calls per run and
/// spaces requests to respect Open Library rate guidelines.
pub async fn enrich_books_from_openlibrary(library: &LibraryStore) -> Result<usize> {
    enrich_books_from_openlibrary_with(library, &OpenLibraryOptions::default()).await
}

/// Same as [`enrich_books_from_openlibrary`] with explicit politeness options.
pub async fn enrich_books_from_openlibrary_with(
    library: &LibraryStore,
    opts: &OpenLibraryOptions,
) -> Result<usize> {
    let http = ol_http_client(opts.contact_email.as_deref())?;
    let mut enriched = 0usize;
    let mut requests = 0usize;

    for book in library.list_books(None).await? {
        if requests >= opts.max_requests {
            tracing::info!(
                requests,
                max = opts.max_requests,
                "open library enrich capped for this run (use dumps for bulk)"
            );
            break;
        }

        // Already enriched via OL — do not re-hit the API (cache via provenance).
        if book.enrich_source.as_deref() == Some("openlibrary") {
            continue;
        }

        let needs_subjects = book.subjects.as_deref().unwrap_or("").trim().is_empty();
        let needs_description = book.description.as_deref().unwrap_or("").trim().is_empty();
        let needs_cover = book.cover_url.as_deref().unwrap_or("").trim().is_empty();
        let needs_language = book.language.as_deref().unwrap_or("").trim().is_empty();
        if !(needs_subjects || needs_description || needs_cover || needs_language) {
            continue;
        }

        // Prefer ISBN lookup (one search.json hit) over title search.
        let doc = if let Some(isbn) = book
            .isbn
            .as_deref()
            .map(normalize_isbn)
            .filter(|s| !s.is_empty())
        {
            sleep(opts.min_interval).await;
            requests += 1;
            lookup_by_isbn(&http, &isbn).await?
        } else if !book.title.trim().is_empty() {
            sleep(opts.min_interval).await;
            requests += 1;
            lookup_by_title_author(
                &http,
                &book.title,
                book.authors.as_deref().and_then(primary_author),
            )
            .await?
        } else {
            None
        };

        let Some(doc) = doc else {
            continue;
        };

        let subjects = if needs_subjects && !doc.subject.is_empty() {
            Some(
                doc.subject
                    .iter()
                    .take(24)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        } else {
            None
        };
        let description = if needs_description {
            doc.first_sentence
                .as_ref()
                .and_then(OlFirstSentence::as_text)
        } else {
            None
        };
        let cover_url = if needs_cover {
            doc.cover_i
                .map(|id| format!("https://covers.openlibrary.org/b/id/{id}-L.jpg"))
        } else {
            None
        };
        let language = if needs_language {
            doc.language.first().cloned()
        } else {
            None
        };

        if subjects.is_none() && description.is_none() && cover_url.is_none() && language.is_none()
        {
            continue;
        }

        let openlibrary_id = doc.key.clone();
        library
            .update_catalog_enrichment(
                &book.uuid,
                &CatalogEnrichmentFields {
                    description,
                    language,
                    cover_url,
                    subjects,
                    categories: None,
                    enrich_source: Some(String::from("openlibrary")),
                    enrich_confidence: Some(0.7),
                    enrich_updated_at: Some(Utc::now()),
                },
            )
            .await?;

        if let (Some(ol), Some(work_id)) =
            (openlibrary_id, library.work_id_for_book(&book.uuid).await?)
        {
            if let Some(work) = library.get_work(&work_id).await? {
                let mut nw = bookclerk_library::NewWork {
                    id: Some(work.id),
                    canonical_asin: work.canonical_asin,
                    canonical_isbn: work.canonical_isbn,
                    title: work.title,
                    authors: work.authors,
                    narrators: work.narrators,
                    description: work.description,
                    subjects: work.subjects,
                    categories: work.categories,
                    language: work.language,
                    series: work.series,
                    series_index: work.series_index,
                    cover_url: work.cover_url,
                    openlibrary_id: Some(ol),
                };
                if let Some(b) = library.get_book_by_uuid(&book.uuid).await? {
                    nw.description = b.description.or(nw.description);
                    nw.subjects = b.subjects.or(nw.subjects);
                    nw.language = b.language.or(nw.language);
                    nw.cover_url = b.cover_url.or(nw.cover_url);
                }
                library.upsert_work(&nw).await?;
            }
        }

        enriched += 1;
        tracing::info!(
            uuid = %book.uuid,
            title = %book.title,
            "enriched book from Open Library"
        );
    }

    Ok(enriched)
}

fn ol_http_client(contact_email: Option<&str>) -> Result<reqwest::Client> {
    let ua = match contact_email.map(str::trim).filter(|s| !s.is_empty()) {
        Some(email) => format!(
            "Bookclerk/{} ({email}; https://github.com/fritz-fritz/bookclerk)",
            env!("CARGO_PKG_VERSION")
        ),
        None => {
            tracing::warn!(
                "discovery.openlibrary_contact_email unset — Open Library rate limit stays at \
                 1 req/s; set a contact email in User-Agent per their API guidelines"
            );
            format!(
                "Bookclerk/{} (https://github.com/fritz-fritz/bookclerk)",
                env!("CARGO_PKG_VERSION")
            )
        }
    };
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(ua)
        .build()?)
}

async fn lookup_by_isbn(http: &reqwest::Client, isbn: &str) -> Result<Option<OlDoc>> {
    let url = format!("https://openlibrary.org/search.json?isbn={isbn}&limit=1");
    let resp: OlSearchResponse = http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.docs.into_iter().next())
}

async fn lookup_by_title_author(
    http: &reqwest::Client,
    title: &str,
    author: Option<&str>,
) -> Result<Option<OlDoc>> {
    let mut url = format!(
        "https://openlibrary.org/search.json?title={}&limit=5",
        urlencoding_lite(title)
    );
    if let Some(a) = author {
        url.push_str(&format!("&author={}", urlencoding_lite(a)));
    }
    let resp: OlSearchResponse = http
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let title_l = title.to_lowercase();
    Ok(resp.docs.into_iter().find(|d| {
        d.title
            .as_deref()
            .map(|t| t.to_lowercase().contains(&title_l) || title_l.contains(&t.to_lowercase()))
            .unwrap_or(false)
    }))
}

fn primary_author(authors: &str) -> Option<&str> {
    authors
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

fn urlencoding_lite(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
