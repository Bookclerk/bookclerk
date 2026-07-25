//! Library scan: paginate Chirp `currentUserAudiobooks` into `LibraryStore`.

use std::path::Path;

use bookclerk_library::{LibraryStore, NewBook};
use bookclerk_source::ScanSummary;
use chrono::{DateTime, NaiveDate, Utc};

use crate::auth::{find_auth_file, list_auth_files, load_auth, ChirpAuthFile};
use crate::client::{Audiobook, ChirpClient};
use crate::error::{ChirpError, Result};

/// Options for a Chirp library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub accounts: Vec<String>,
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
pub async fn scan_library(
    files_dir: &Path,
    library: &LibraryStore,
    options: ScanOptions,
    graphql_url: Option<&str>,
) -> Result<ScanSummary> {
    let explicit = !options.accounts.is_empty();
    let targets = resolve_targets(files_dir, &options.accounts)?;
    if targets.is_empty() {
        return Err(ChirpError::no_accounts(
            "no Chirp accounts configured — run login first",
        ));
    }

    let mut summary = ScanSummary::default();
    let gql = graphql_url.unwrap_or(crate::client::DEFAULT_GRAPHQL_URL);

    for auth in targets {
        let account_id = auth.account_id().to_string();
        let marketplace = auth.marketplace.clone();

        if !explicit {
            if let Some(acct) = library.get_account(&account_id)? {
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

        library.ensure_account_with_source(
            &account_id,
            &marketplace,
            auth.label.as_deref(),
            "chirp",
        )?;

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

async fn scan_account_into_library(
    library: &LibraryStore,
    client: &ChirpClient,
    account_id: &str,
    marketplace: &str,
    page_size: u32,
) -> Result<(usize, u32)> {
    let mut books_upserted = 0usize;
    let mut pages = 0u32;
    let mut page = 1u32;

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
            library.upsert_book(&audiobook_to_new_book(book, account_id, marketplace))?;
            books_upserted += 1;
        }
        if items.len() < page_size as usize {
            break;
        }
        page += 1;
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

fn resolve_targets(files_dir: &Path, accounts: &[String]) -> Result<Vec<ChirpAuthFile>> {
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
