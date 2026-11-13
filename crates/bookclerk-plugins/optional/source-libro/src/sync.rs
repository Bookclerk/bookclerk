//! Library scan: fetch Libro.fm library pages as structured scan rows.
//!
//! The guest returns the rows over the plugin ABI; the host writes them.

use bookclerk_plugin_sdk::ScanBook;
use bookclerk_storefront::abi::{abridgement_from_flag, rfc3339};
use bookclerk_storefront::records::{self, CreditRole, IdentifierScheme};
use chrono::{DateTime, NaiveDate, Utc};

use crate::client::{Audiobook, LibroClient};
use crate::error::{LibroError, Result};

/// Fetch one Libro.fm library page as scan rows (no DB writes).
///
/// # Arguments
///
/// * `client` - Authenticated Libro.fm client.
/// * `account_id` - Account the rows belong to.
/// * `marketplace` - Marketplace stamped on every row.
/// * `page` - 1-based library page.
///
/// # Returns
///
/// The page's visible titles and the next page number, `None` on the last
/// page.
///
/// # Errors
///
/// Returns an error when the request fails or the API reports one.
pub async fn fetch_account_page(
    client: &LibroClient,
    account_id: &str,
    marketplace: &str,
    page: u32,
) -> Result<(Vec<ScanBook>, Option<u32>)> {
    let response = client.library_page(page).await?;
    if let Some(err) = response.error.as_deref() {
        return Err(LibroError::api(err.to_string()));
    }
    let books = response
        .audiobooks
        .iter()
        .filter(|book| {
            !book
                .user_metadata
                .as_ref()
                .and_then(|m| m.hidden)
                .unwrap_or(false)
        })
        .map(|book| audiobook_to_scan_book(book, account_id, marketplace))
        .collect();
    let total = response.total_pages.max(1);
    let next = (page < total && !response.audiobooks.is_empty()).then(|| page.saturating_add(1));
    Ok((books, next))
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
    let mut page = Some(1u32);
    while let Some(current) = page {
        let (batch, next) = fetch_account_page(client, account_id, marketplace, current).await?;
        pages += 1;
        books.extend(batch);
        page = next;
    }
    Ok((books, pages))
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
        work_kind: bookclerk_storefront::records::WorkKind::Book,
        expression_kind: bookclerk_storefront::records::ExpressionKind::Narration,
        media_kind: bookclerk_storefront::records::MediaKind::Audio,
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
