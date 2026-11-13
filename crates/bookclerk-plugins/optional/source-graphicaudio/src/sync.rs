//! Library scan: list owned GraphicAudio titles as structured scan rows.
//!
//! The guest returns the rows over the plugin ABI; the host writes them.

use crate::options::GraphicAudioAccess;
use bookclerk_plugin_sdk::ScanBook;
use bookclerk_storefront::abi::rfc3339;
use bookclerk_storefront::records::{self, CreditRole, ExpressionKind};
use chrono::{DateTime, NaiveDate, Utc};

use crate::auth::GraphicAudioAuthFile;
use crate::client::{GraphicAudioClient, Product};
use crate::error::{GraphicAudioError, Result};
use crate::magento::{LibraryItem, MagentoClient};

/// Collect owned titles for one account (no DB writes).
///
/// Prefer Access App catalog when `access=device` and a device token exists.
/// For web/zip (default), use Magento Browser Player library so we do not
/// depend on a device slot for ownership listing. Falls back to Access App
/// when a legacy device token exists and Magento password is unavailable.
///
/// # Errors
///
/// Returns an error when the operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn collect_account_books(
    auth: &GraphicAudioAuthFile,
    access_base: &str,
    store_base: &str,
    access: GraphicAudioAccess,
    magento_password: Option<&str>,
    include_samples: bool,
    account_id: &str,
    marketplace: &str,
) -> Result<(Vec<ScanBook>, u32)> {
    let use_device = matches!(access, GraphicAudioAccess::Device) && auth.has_device_token();

    if use_device {
        let books =
            collect_access_products(access_base, auth, include_samples, account_id, marketplace)
                .await?;
        return Ok((books, 1));
    }

    if auth.has_device_token() && magento_password.is_none() {
        let books =
            collect_access_products(access_base, auth, include_samples, account_id, marketplace)
                .await?;
        return Ok((books, 1));
    }

    let password = magento_password.ok_or_else(|| {
        GraphicAudioError::auth(
            "GraphicAudio Magento library scan requires the account password (connect in Accounts) or access=device with a saved token",
        )
    })?;
    let store = MagentoClient::new(store_base)?;
    store.login(&auth.email, password).await?;
    let items = store.list_library().await?;
    let books = items
        .iter()
        .map(|item| library_item_to_scan_book(item, account_id, marketplace))
        .collect();
    Ok((books, 1))
}

/// Lists Access App products as structured scan rows, optionally skipping sample SKUs.
async fn collect_access_products(
    access_base: &str,
    auth: &GraphicAudioAuthFile,
    include_samples: bool,
    account_id: &str,
    marketplace: &str,
) -> Result<Vec<ScanBook>> {
    let client = GraphicAudioClient::new(access_base).with_token(&auth.token);
    let products = client.products().await?;
    let books = products
        .iter()
        .filter(|product| include_samples || !product.is_sample())
        .map(|product| product_to_scan_book(product, account_id, marketplace))
        .collect();
    tracing::debug!(
        samples_skipped = products.iter().filter(|p| p.is_sample()).count(),
        "GraphicAudio Access App catalog scan"
    );
    Ok(books)
}

/// Map a GraphicAudio product JSON object to a structured scan row.
///
/// Every GraphicAudio production is a full-cast dramatisation of the work,
/// so the expression kind is [`ExpressionKind::Dramatisation`].
#[must_use]
pub fn product_to_scan_book(product: &Product, account_id: &str, marketplace: &str) -> ScanBook {
    let length_minutes = product
        .running_time
        .as_deref()
        .and_then(parse_running_time_minutes);
    let purchased_at = product.purchased_date.as_deref().and_then(parse_ga_date);
    let published_at = product.release_date.as_deref().and_then(parse_ga_date);

    ScanBook {
        account_id: account_id.to_string(),
        product_id: product.id.clone(),
        title: product.display_title(),
        marketplace: Some(marketplace.to_string()),
        publisher: Some(String::from("GraphicAudio")),
        length_minutes,
        subtitle: product.title.clone(),
        expression_kind: ExpressionKind::Dramatisation,
        credits: records::credits_from_display(product.author.as_deref(), CreditRole::Author),
        series_entries: records::series_entries(
            product.series.as_deref(),
            product.episode.as_deref(),
        ),
        published_at: published_at.map(rfc3339),
        purchased_at: purchased_at.map(rfc3339),
        categories: product.genre.clone(),
        work_kind: bookclerk_storefront::records::WorkKind::Book,
        media_kind: bookclerk_storefront::records::MediaKind::Audio,
        ..ScanBook::default()
    }
}

/// Maps a Browser Player library card onto a structured scan row (title
/// falls back to `GraphicAudio {id}`).
fn library_item_to_scan_book(item: &LibraryItem, account_id: &str, marketplace: &str) -> ScanBook {
    let title = item
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| format!("GraphicAudio {}", item.product_id));
    ScanBook {
        account_id: account_id.to_string(),
        product_id: item.product_id.clone(),
        title,
        marketplace: Some(marketplace.to_string()),
        publisher: Some(String::from("GraphicAudio")),
        expression_kind: ExpressionKind::Dramatisation,
        work_kind: bookclerk_storefront::records::WorkKind::Book,
        media_kind: bookclerk_storefront::records::MediaKind::Audio,
        ..ScanBook::default()
    }
}

/// Parses `10 hrs 30 mins`, `10:30`, or a bare minute count into minutes; `None` when unparseable.
fn parse_running_time_minutes(raw: &str) -> Option<i64> {
    // Examples seen / expected: "10 hrs 30 mins", "630", "10:30"
    let lower = raw.to_ascii_lowercase();
    if let Some(hrs) = lower.find("hr") {
        let before = lower[..hrs].trim();
        let hours: i64 = before.split_whitespace().next_back()?.parse().ok()?;
        let mut minutes = hours * 60;
        if let Some(mpos) = lower.find("min") {
            let chunk = lower[..mpos].trim();
            if let Some(m) = chunk
                .split_whitespace()
                .next_back()
                .and_then(|s| s.parse::<i64>().ok())
            {
                minutes += m;
            }
        }
        return Some(minutes);
    }
    if lower.contains(':') {
        let parts: Vec<_> = lower.split(':').collect();
        if parts.len() >= 2 {
            let h: i64 = parts[0].trim().parse().ok()?;
            let m: i64 = parts[1].trim().parse().ok()?;
            return Some(h * 60 + m);
        }
    }
    raw.trim().parse::<i64>().ok()
}

/// Parses Access App unix seconds, RFC 3339, or `YYYY-MM-DD` into UTC; `None` when unparseable.
fn parse_ga_date(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if let Ok(secs) = trimmed.parse::<i64>() {
        // Access App returns unix epoch seconds for Purchase/Release Date.
        if (1_000_000_000..4_000_000_000).contains(&secs) {
            return DateTime::from_timestamp(secs, 0);
        }
    }
    DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
        })
        .or_else(|| {
            NaiveDate::parse_from_str(trimmed, "%m/%d/%Y")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::{CreditRole, ExpressionKind};

    #[test]
    fn scan_rows_are_dramatisations() {
        let product: Product = serde_json::from_value(serde_json::json!({
            "Id": "ga-1",
            "Title": "Part 1",
            "ProductName": "Red Rising",
            "Author": "Pierce Brown",
            "Series": "Red Rising",
            "Episode": "1",
            "Genre": "Science Fiction",
            "Purchased Date": "1700000000"
        }))
        .expect("product");
        let row = product_to_scan_book(&product, "acct", "us");
        assert_eq!(row.expression_kind, ExpressionKind::Dramatisation);
        assert_eq!(row.credits[0].name, "Pierce Brown");
        assert_eq!(row.credits[0].role, CreditRole::Author);
        assert_eq!(row.series_entries[0].position.as_deref(), Some("1"));
        assert_eq!(row.purchased_at.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(row.categories.as_deref(), Some("Science Fiction"));

        let card = LibraryItem {
            product_id: "ga-2".into(),
            title: Some(" ".into()),
            listen_path: None,
        };
        let row = library_item_to_scan_book(&card, "acct", "us");
        assert_eq!(row.title, "GraphicAudio ga-2");
        assert_eq!(row.expression_kind, ExpressionKind::Dramatisation);
    }
}
