//! Library scan: paginate Chirp `currentUserAudiobooks` into [`SourceScope`].

use bookclerk_library::{NewBook, SourceScope};
use bookclerk_source::ScanSummary;
use chrono::{DateTime, NaiveDate, Utc};

use crate::auth::ChirpAuthFile;
use crate::client::{Audiobook, ChirpClient};
use crate::db::list_auth_from_db;
use crate::error::{ChirpError, Result};

/// Options for a Chirp library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Accounts.
    pub accounts: Vec<String>,
    /// Page size.
    pub page_size: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            page_size: 20,
        }
    }
}

impl From<&bookclerk_source::ScanOptions> for ScanOptions {
    fn from(opts: &bookclerk_source::ScanOptions) -> Self {
        Self {
            accounts: opts.accounts.clone(),
            page_size: opts.page_size.max(1),
        }
    }
}

/// Sync Chirp libraries for configured accounts into `library`.
///
/// Accounts are resolved from `encrypted_secrets` (DB-backed); no
/// `Accounts/*.chirp.auth` files are read.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn scan_library(
    library: &SourceScope,
    options: ScanOptions,
    graphql_url: Option<&str>,
) -> Result<ScanSummary> {
    let explicit = !options.accounts.is_empty();
    let all = list_auth_from_db(library).await?;
    let targets: Vec<(String, ChirpAuthFile)> = if explicit {
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
        return Err(ChirpError::no_accounts(
            "no Chirp accounts configured — run login first",
        ));
    }

    let mut summary = ScanSummary::default();
    let gql = graphql_url.unwrap_or(crate::client::DEFAULT_GRAPHQL_URL);

    for (account_id, auth) in targets {
        let marketplace = auth.marketplace.clone();

        if !explicit {
            if let Some(acct) = library.get_account(&account_id).await? {
                if !acct.scan_enabled {
                    tracing::info!(
                        account = %account_id,
                        "skipping Chirp account — scan_enabled=false"
                    );
                    summary.skipped_disabled += 1;
                    continue;
                }
            }
        }

        library
            .ensure_account(&account_id, &marketplace, auth.label.as_deref())
            .await?;

        let client = ChirpClient::new(gql).with_token(&auth.access_token);
        let (books, pages) = scan_account_into_library(
            library,
            &client,
            &account_id,
            &marketplace,
            options.page_size,
        )
        .await?;
        summary.accounts += 1;
        summary.books_upserted += books;
        summary.pages += pages;

        tracing::info!(
            account = %account_id,
            marketplace = %marketplace,
            books,
            pages,
            "Chirp library scan finished"
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
    client: &ChirpClient,
    account_id: &str,
    marketplace: &str,
    page_size: u32,
) -> Result<(Vec<NewBook>, u32)> {
    let mut books = Vec::new();
    let mut pages = 0u32;
    let mut page = 1u32;
    let page_size = page_size.max(1);

    loop {
        let items = client.library_page(page, page_size).await?;
        pages += 1;
        if items.is_empty() {
            break;
        }
        for item in &items {
            if item.archived.unwrap_or(false) {
                continue;
            }
            let Some(book) = item.audiobook.as_ref() else {
                continue;
            };
            books.push(audiobook_to_new_book(book, account_id, marketplace));
        }
        if items.len() < page_size as usize {
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
    client: &ChirpClient,
    account_id: &str,
    marketplace: &str,
    page_size: u32,
) -> Result<(usize, u32)> {
    let (books, pages) = collect_account_books(client, account_id, marketplace, page_size).await?;
    let mut books_upserted = 0usize;
    for book in &books {
        library.upsert_book(book).await?;
        books_upserted += 1;
    }
    Ok((books_upserted, pages))
}

/// Map a Chirp audiobook to a [`NewBook`] row.
#[must_use]
pub fn audiobook_to_new_book(book: &Audiobook, account_id: &str, marketplace: &str) -> NewBook {
    let length_minutes = book.duration_ms.map(|ms| (ms / 60_000) as i64);
    let published_at = book.released_on.as_deref().and_then(parse_chirp_date);

    NewBook {
        uuid: None,
        product_id: book.id.clone(),
        source: String::from("chirp"),
        account_id: account_id.to_string(),
        asin: None,
        isbn: None,
        marketplace: marketplace.to_string(),
        title: book
            .display_title
            .clone()
            .unwrap_or_else(|| book.id.clone()),
        authors: book.display_authors.clone(),
        narrators: book.display_narrators.clone(),
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: book.publisher.clone(),
        length_minutes,
        is_abridged: book.abridged.unwrap_or(false),
        content_kind: String::from("book"),
        categories: None,
        subtitle: book.sub_title.clone(),
        published_at,
    }
}

/// Parses a Chirp purchase/publish date as RFC 3339 or `YYYY-MM-DD` (midnight UTC).
fn parse_chirp_date(raw: &str) -> Option<DateTime<Utc>> {
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
