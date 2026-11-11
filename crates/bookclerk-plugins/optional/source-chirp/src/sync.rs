//! Library scan: paginate Chirp `currentUserAudiobooks` into structured scan rows.
//!
//! The guest returns the rows over the plugin ABI; the host writes them.

use bookclerk_plugin_sdk::ScanBook;
use bookclerk_storefront::abi::{abridgement_from_flag, rfc3339};
use bookclerk_storefront::records::{self, CreditRole};
use chrono::{DateTime, NaiveDate, Utc};

use crate::client::{Audiobook, ChirpClient};
use crate::error::Result;

/// Fetch one Chirp library page as scan rows (no DB writes).
///
/// # Arguments
///
/// * `client` - Authenticated Chirp client.
/// * `account_id` - Account the rows belong to.
/// * `marketplace` - Marketplace stamped on every row.
/// * `page` - 1-based library page.
/// * `page_size` - Items per page (at least 1).
///
/// # Returns
///
/// The page's playable titles and the next page number, `None` on the last
/// page (a short or empty page).
///
/// # Errors
///
/// Returns an error when the request fails.
pub async fn fetch_account_page(
    client: &ChirpClient,
    account_id: &str,
    marketplace: &str,
    page: u32,
    page_size: u32,
) -> Result<(Vec<ScanBook>, Option<u32>)> {
    let page_size = page_size.max(1);
    let items = client.library_page(page, page_size).await?;
    let books = items
        .iter()
        .filter(|item| !item.archived.unwrap_or(false))
        .filter_map(|item| item.audiobook.as_ref())
        .map(|book| audiobook_to_scan_book(book, account_id, marketplace))
        .collect();
    let full = u32::try_from(items.len()).is_ok_and(|n| n >= page_size);
    Ok((books, full.then(|| page.saturating_add(1))))
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
) -> Result<(Vec<ScanBook>, u32)> {
    let mut books = Vec::new();
    let mut pages = 0u32;
    let mut page = Some(1u32);
    while let Some(current) = page {
        let (batch, next) =
            fetch_account_page(client, account_id, marketplace, current, page_size).await?;
        pages += 1;
        books.extend(batch);
        page = next;
    }
    Ok((books, pages))
}

/// Map a Chirp audiobook to a structured scan row. Chirp returns one display
/// string per role, so names are split with [`records::credits_from_display`].
#[must_use]
pub fn audiobook_to_scan_book(book: &Audiobook, account_id: &str, marketplace: &str) -> ScanBook {
    let length_minutes = book.duration_ms.map(|ms| (ms / 60_000) as i64);
    let published_at = book.released_on.as_deref().and_then(parse_chirp_date);
    let mut credits =
        records::credits_from_display(book.display_authors.as_deref(), CreditRole::Author);
    credits.extend(records::credits_from_display(
        book.display_narrators.as_deref(),
        CreditRole::Narrator,
    ));

    ScanBook {
        account_id: account_id.to_string(),
        product_id: book.id.clone(),
        title: book
            .display_title
            .clone()
            .unwrap_or_else(|| book.id.clone()),
        marketplace: Some(marketplace.to_string()),
        publisher: book.publisher.clone(),
        length_minutes,
        subtitle: book.sub_title.clone(),
        credits,
        abridgement: abridgement_from_flag(book.abridged),
        published_at: published_at.map(rfc3339),
        ..ScanBook::default()
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

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::{Abridgement, CreditRole};

    #[test]
    fn scan_row_splits_display_names_into_credits() {
        let book: Audiobook = serde_json::from_value(serde_json::json!({
            "id": "123",
            "displayTitle": "The Lincoln Lawyer",
            "displayAuthors": "Michael Connelly",
            "displayNarrators": "Dick Hill, Peter Giles",
            "durationMs": 600000,
            "abridged": false,
            "releasedOn": "2005-10-03",
            "tracks": []
        }))
        .expect("audiobook");
        let row = audiobook_to_scan_book(&book, "acct", "us");
        assert_eq!(row.product_id, "123");
        assert_eq!(row.length_minutes, Some(10));
        assert_eq!(row.abridgement, Abridgement::Unabridged);
        assert_eq!(row.published_at.as_deref(), Some("2005-10-03T00:00:00Z"));
        let narrators: Vec<&str> = row
            .credits
            .iter()
            .filter(|c| c.role == CreditRole::Narrator)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(narrators, ["Dick Hill", "Peter Giles"]);
        assert_eq!(row.credits[0].role, CreditRole::Author);
    }
}
