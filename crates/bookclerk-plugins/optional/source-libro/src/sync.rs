//! Library scan: fetch Libro.fm library pages and upsert via [`SourceScope`].

use bookclerk_library::SourceScope;
use bookclerk_plugin_sdk::ScanBook;
use bookclerk_source::abi::{abridgement_from_flag, rfc3339, scan_book_to_new};
use bookclerk_source::records::{self, CreditRole, IdentifierScheme};
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
) -> Result<(Vec<ScanBook>, u32)> {
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
            books.push(audiobook_to_scan_book(book, account_id, marketplace));
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
    for new in books
        .into_iter()
        .filter_map(|book| scan_book_to_new("libro", book))
    {
        library.upsert_book(&new).await?;
        books_upserted += 1;
    }
    Ok((books_upserted, pages))
}

/// Map a Libro.fm audiobook JSON object to a structured scan row.
#[must_use]
pub fn audiobook_to_scan_book(book: &Audiobook, account_id: &str, marketplace: &str) -> ScanBook {
    let mut credits =
        records::credits_from_names(book.authors.iter().flatten(), CreditRole::Author);
    credits.extend(records::credits_from_names(
        book.audiobook_info
            .iter()
            .filter_map(|i| i.narrators.as_ref())
            .flatten(),
        CreditRole::Narrator,
    ));
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

    ScanBook {
        account_id: account_id.to_string(),
        product_id: book.isbn.clone(),
        title: book.title.clone(),
        marketplace: Some(marketplace.to_string()),
        publisher: book.publisher.clone(),
        length_minutes,
        subtitle: book.subtitle.clone(),
        identifiers: records::identifier(IdentifierScheme::Isbn, &book.isbn, None)
            .into_iter()
            .collect(),
        credits,
        series_entries: records::series_entries(book.series.as_deref(), series_index.as_deref()),
        abridgement: abridgement_from_flag(book.abridged),
        published_at: published_at.map(rfc3339),
        purchased_at: purchased_at.map(rfc3339),
        categories: categories.filter(|s| !s.is_empty()),
        ..ScanBook::default()
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

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::{Abridgement, CreditRole, IdentifierScheme};

    #[test]
    fn scan_row_carries_one_credit_per_listed_name() {
        let book: Audiobook = serde_json::from_value(serde_json::json!({
            "isbn": "9781980053446",
            "title": "Ashes of Man",
            "authors": ["Christopher Ruocchio", "  "],
            "abridged": true,
            "series": "Sun Eater",
            "series_num": 5,
            "genres": [{"name": "Science Fiction"}],
            "audiobook_info": {"narrators": ["Samuel Roukin"], "duration": 3600},
            "publication_date": "2022-07-05",
            "user_metadata": {"added_at": "2023-01-02T03:04:05Z"}
        }))
        .expect("audiobook");
        let row = audiobook_to_scan_book(&book, "acct", "us");
        assert_eq!(row.identifiers[0].scheme, IdentifierScheme::Isbn);
        assert_eq!(row.identifiers[0].value, "9781980053446");
        let names: Vec<(CreditRole, &str)> = row
            .credits
            .iter()
            .map(|c| (c.role, c.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                (CreditRole::Author, "Christopher Ruocchio"),
                (CreditRole::Narrator, "Samuel Roukin"),
            ]
        );
        assert_eq!(row.abridgement, Abridgement::Abridged);
        assert_eq!(row.series_entries[0].position.as_deref(), Some("5"));
        assert_eq!(row.length_minutes, Some(60));
        assert_eq!(row.purchased_at.as_deref(), Some("2023-01-02T03:04:05Z"));
        assert_eq!(row.categories.as_deref(), Some("Science Fiction"));
    }
}
