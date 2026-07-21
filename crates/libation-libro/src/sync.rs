//! Library scan: fetch Libro.fm library pages and upsert into `LibraryStore`.

use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use libation_library::{LibraryStore, NewBook};
use libation_source::ScanSummary;

use crate::auth::{find_auth_file, list_auth_files, load_auth, LibroAuthFile};
use crate::client::{Audiobook, LibroClient};
use crate::error::{LibroError, Result};

/// Options for a Libro.fm library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Limit to specific account emails / labels / ids. Empty = all auth files.
    pub accounts: Vec<String>,
    /// Page size is fixed by the Libro.fm API; kept for ContentSource parity.
    pub page_size: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            page_size: 50,
        }
    }
}

impl From<&libation_source::ScanOptions> for ScanOptions {
    fn from(opts: &libation_source::ScanOptions) -> Self {
        Self {
            accounts: opts.accounts.clone(),
            page_size: opts.page_size,
        }
    }
}

/// Sync Libro.fm libraries for configured accounts into `library`.
pub async fn scan_library(
    files_dir: &Path,
    library: &LibraryStore,
    options: ScanOptions,
    base_url: Option<&str>,
) -> Result<ScanSummary> {
    let explicit = !options.accounts.is_empty();
    let targets = resolve_targets(files_dir, &options.accounts)?;
    if targets.is_empty() {
        return Err(LibroError::no_accounts(
            "no Libro.fm accounts configured — run login first",
        ));
    }

    let mut summary = ScanSummary::default();
    let base = base_url.unwrap_or(crate::client::DEFAULT_BASE_URL);

    for auth in targets {
        let account_id = auth.account_id().to_string();
        let marketplace = auth.marketplace.clone();

        if !explicit {
            if let Some(acct) = library.get_account(&account_id)? {
                if !acct.scan_enabled {
                    tracing::info!(
                        account = %account_id,
                        "skipping Libro.fm account — scan_enabled=false"
                    );
                    summary.skipped_disabled += 1;
                    continue;
                }
            }
        }

        library.ensure_account_with_source(
            &account_id,
            &marketplace,
            auth.label.as_deref(),
            "libro",
        )?;

        let client = LibroClient::new(base).with_token(&auth.access_token);
        let (books, pages) =
            scan_account_into_library(library, &client, &account_id, &marketplace).await?;
        summary.accounts += 1;
        summary.books_upserted += books;
        summary.pages += pages;

        tracing::info!(
            account = %account_id,
            marketplace = %marketplace,
            books,
            pages,
            "Libro.fm library scan finished"
        );
    }

    Ok(summary)
}

/// Fetch all library pages for one client and upsert books.
pub async fn scan_account_into_library(
    library: &LibraryStore,
    client: &LibroClient,
    account_id: &str,
    marketplace: &str,
) -> Result<(usize, u32)> {
    let mut books_upserted = 0usize;
    let mut pages = 0u32;
    let mut page = 1u32;

    loop {
        let response = client.library_page(page).await?;
        if let Some(err) = response.error.as_deref() {
            return Err(LibroError::api(err.to_string()));
        }
        pages += 1;

        for book in &response.audiobooks {
            if book
                .user_metadata
                .as_ref()
                .and_then(|m| m.hidden)
                .unwrap_or(false)
            {
                continue;
            }
            library.upsert_book(&audiobook_to_new_book(book, account_id, marketplace))?;
            books_upserted += 1;
        }

        let total = response.total_pages.max(1);
        if page >= total || response.audiobooks.is_empty() {
            break;
        }
        page += 1;
    }

    Ok((books_upserted, pages))
}

/// Map a Libro.fm audiobook JSON object to a [`NewBook`] row.
#[must_use]
pub fn audiobook_to_new_book(book: &Audiobook, account_id: &str, marketplace: &str) -> NewBook {
    let isbn = book.isbn.clone();
    let authors = book.authors.as_ref().map(|a| a.join(", "));
    let narrators = book
        .audiobook_info
        .as_ref()
        .and_then(|i| i.narrators.as_ref())
        .map(|n| n.join(", "));
    let length_minutes = book
        .audiobook_info
        .as_ref()
        .and_then(|i| i.duration)
        .map(|secs| (secs / 60) as i64);
    let categories = book.genres.as_ref().map(|genres| {
        genres
            .iter()
            .filter_map(|g| g.name.as_deref())
            .collect::<Vec<_>>()
            .join(", ")
    });
    let series_index = book.series_num.as_ref().and_then(|v| match v {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    });
    let published_at = book
        .publication_date
        .as_deref()
        .and_then(parse_libro_datetime);
    let purchased_at = book
        .user_metadata
        .as_ref()
        .and_then(|m| m.added_at.as_deref())
        .and_then(parse_libro_datetime);

    NewBook {
        uuid: None,
        product_id: isbn.clone(),
        source: String::from("libro"),
        account_id: account_id.to_string(),
        asin: None,
        isbn: Some(isbn),
        marketplace: marketplace.to_string(),
        title: book.title.clone(),
        authors,
        narrators,
        series: book.series.clone(),
        series_index,
        series_asin: None,
        purchased_at,
        publisher: book.publisher.clone(),
        length_minutes,
        is_abridged: book.abridged.unwrap_or(false),
        content_kind: String::from("book"),
        categories: categories.filter(|s| !s.is_empty()),
        subtitle: book.subtitle.clone(),
        published_at,
    }
}

fn parse_libro_datetime(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
        })
}

fn resolve_targets(files_dir: &Path, accounts: &[String]) -> Result<Vec<LibroAuthFile>> {
    if accounts.is_empty() {
        let mut out = Vec::new();
        for path in list_auth_files(files_dir)? {
            out.push(load_auth(&path)?);
        }
        return Ok(out);
    }

    let mut out = Vec::new();
    for key in accounts {
        let path = find_auth_file(files_dir, key)?;
        out.push(load_auth(&path)?);
    }
    Ok(out)
}
