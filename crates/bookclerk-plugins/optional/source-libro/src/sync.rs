//! Library scan: fetch Libro.fm library pages and upsert via [`SourceScope`].

use bookclerk_library::{NewBook, SourceScope};
use bookclerk_source::ScanSummary;
use chrono::{DateTime, NaiveDate, Utc};

use crate::auth::LibroAuthFile;
use crate::client::{Audiobook, LibroClient};
use crate::db::list_auth_from_db;
use crate::error::{LibroError, Result};

/// Options for a Libro.fm library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Limit to specific account emails / labels / ids. Empty = all accounts.
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

impl From<&bookclerk_source::ScanOptions> for ScanOptions {
    fn from(opts: &bookclerk_source::ScanOptions) -> Self {
        Self {
            accounts: opts.accounts.clone(),
            page_size: opts.page_size,
        }
    }
}

/// Sync Libro.fm libraries for configured accounts into `library`.
///
/// Accounts are resolved from `encrypted_secrets` (DB-backed); no
/// `Accounts/*.libro.auth` files are read.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn scan_library(
    library: &SourceScope,
    options: ScanOptions,
    base_url: Option<&str>,
) -> Result<ScanSummary> {
    let explicit = !options.accounts.is_empty();
    let all = list_auth_from_db(library).await?;
    let targets: Vec<(String, LibroAuthFile)> = if explicit {
        all.into_iter()
            .filter(|(id, auth)| {
                options.accounts.iter().any(|needle| {
                    id.eq_ignore_ascii_case(needle)
                        || auth
                            .label
                            .as_deref()
                            .is_some_and(|l| l.eq_ignore_ascii_case(needle))
                        || auth.email.eq_ignore_ascii_case(needle)
                })
            })
            .collect()
    } else {
        all
    };

    if targets.is_empty() {
        return Err(LibroError::no_accounts(
            "no Libro.fm accounts configured — run login first",
        ));
    }

    let mut summary = ScanSummary::default();
    let base = base_url.unwrap_or(crate::client::DEFAULT_BASE_URL);

    for (account_id, auth) in targets {
        let marketplace = auth.marketplace.clone();

        if !explicit {
            if let Some(acct) = library.get_account(&account_id).await? {
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

        library
            .ensure_account(&account_id, &marketplace, auth.label.as_deref())
            .await?;

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

/// Fetch all library pages for one client and collect books (no DB writes).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn collect_account_books(
    client: &LibroClient,
    account_id: &str,
    marketplace: &str,
) -> Result<(Vec<NewBook>, u32)> {
    let mut books = Vec::new();
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
            books.push(audiobook_to_new_book(book, account_id, marketplace));
        }

        let total = response.total_pages.max(1);
        if page >= total || response.audiobooks.is_empty() {
            break;
        }
        page += 1;
    }

    Ok((books, pages))
}

/// Fetch all library pages for one client and upsert books.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn scan_account_into_library(
    library: &SourceScope,
    client: &LibroClient,
    account_id: &str,
    marketplace: &str,
) -> Result<(usize, u32)> {
    let (books, pages) = collect_account_books(client, account_id, marketplace).await?;
    let mut books_upserted = 0usize;
    for book in &books {
        library.upsert_book(book).await?;
        books_upserted += 1;
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

/// Parses a Libro.fm date as RFC 3339 or `YYYY-MM-DD` (midnight UTC); `None` when unparseable.
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
