//! The library model's view of one title an account holds, as the acquire
//! pipeline reads it (book model step 6, PR 4c).
//!
//! An [`AcquireTitle`] is built from the account's holding of a store
//! listing, the listing's store text, the edition it sells, that edition's
//! identifiers and its acquire state, so acquire can stop reading the flat
//! `books` row. While `books` still exists the view also names the flat
//! row's UUID, which the status writes are still keyed by.

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::LibraryStore;
use crate::entities::{
    books, edition_acquisitions, edition_identifiers, editions, expressions, holdings,
    store_listings, works,
};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;

/// One title an account holds, as acquire reads it from the library model.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquireTitle {
    /// The edition the listing sells: the key the title's files and acquire
    /// state belong to.
    pub edition_id: String,
    /// `store_listings.id`.
    pub listing_id: String,
    /// Provenance-qualified PluginKey of the store.
    pub plugin_key: String,
    /// The holding account's id at that store.
    pub account_id: String,
    /// Lower-cased marketplace (empty for a store with one).
    pub marketplace: String,
    /// The store's own product id: what its download and fetch APIs take.
    pub product_id: String,
    /// The edition's ASIN, preferring one scoped to the marketplace.
    pub asin: Option<String>,
    /// The edition's lowest ISBN, when it has one.
    pub isbn: Option<String>,
    /// Title as the store lists it.
    pub title: String,
    /// Subtitle as the store lists it, else the edition's.
    pub subtitle: Option<String>,
    /// Authors as the store prints them, comma-separated.
    pub authors: Option<String>,
    /// Narrators as the store prints them, comma-separated.
    pub narrators: Option<String>,
    /// Series name as the store lists it.
    pub series: Option<String>,
    /// Position in that series as the store writes it.
    pub series_index: Option<String>,
    /// The store's product id for the series or podcast show.
    pub series_product_id: Option<String>,
    /// Publisher as the store lists it.
    pub publisher: Option<String>,
    /// Store category labels (serialized as the scan stored them).
    pub categories: Option<String>,
    /// Running time in whole minutes, from the store or the edition.
    pub length_minutes: Option<i64>,
    /// True when the store or the expression marks it abridged.
    pub is_abridged: bool,
    /// What the title is, in the flat row's labels: `episode` for a
    /// podcast episode, `podcast` for a show, `book` otherwise.
    pub content_kind: &'static str,
    /// When the account bought the title, when the store reported it.
    pub purchased_at: Option<DateTime<Utc>>,
    /// Publication time as the store reported it, else the edition's.
    pub published_at: Option<DateTime<Utc>>,
    /// The edition's acquire state; not acquired when no title of it was
    /// ever queued.
    pub status: AcquireStatus,
    /// Primary destination key of the edition's audio once acquired.
    pub storage_key: Option<String>,
    /// The edition's companion PDF state.
    pub pdf_status: AcquireStatus,
    /// Destination key of the companion PDF once acquired.
    pub pdf_storage_key: Option<String>,
    /// The flat `books` row of this account's title, while that table
    /// exists: the status writes are still keyed by it.
    pub book_uuid: Option<String>,
}

impl AcquireTitle {
    /// Naming and display fallback: ASIN, else ISBN, else the store's
    /// product id (the same order as the flat row's).
    #[must_use]
    pub fn asin_or_isbn(&self) -> &str {
        self.asin
            .as_deref()
            .or(self.isbn.as_deref())
            .unwrap_or(self.product_id.as_str())
    }
}

/// An RFC 3339 or date-only (`YYYY-MM-DD`) time, at midnight UTC for a
/// date; `None` when blank or unreadable.
fn parse_time(text: Option<&str>) -> Option<DateTime<Utc>> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Some(at.with_timezone(&Utc));
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc())
}

impl LibraryStore {
    /// The title `account_id` holds at a store, as acquire reads it.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The store's PluginKey.
    /// * `account_id` - The account at that store.
    /// * `marketplace` - The store's marketplace (any case; blank for one).
    /// * `product_id` - The store's product id.
    ///
    /// # Returns
    ///
    /// `None` when the store's listing is not resolved yet or the account
    /// does not hold it.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn acquire_title(
        &self,
        plugin_key: &str,
        account_id: &str,
        marketplace: &str,
        product_id: &str,
    ) -> Result<Option<AcquireTitle>> {
        let marketplace = bookclerk_resolve::stored_marketplace(marketplace);
        let product_id = bookclerk_resolve::stored_product_id(product_id);
        let Some(listing) = store_listings::Entity::find()
            .filter(store_listings::Column::PluginKey.eq(plugin_key))
            .filter(store_listings::Column::Marketplace.eq(marketplace.as_str()))
            .filter(store_listings::Column::ProductId.eq(product_id.as_str()))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let Some(holding) = super::holdings::find_holdings()
            .filter(holdings::Column::ListingId.eq(listing.id.as_str()))
            .filter(holdings::Column::HolderKind.eq("account"))
            .filter(holdings::Column::PluginKey.eq(plugin_key))
            .filter(holdings::Column::AccountId.eq(account_id))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let edition = editions::Entity::find_by_id(listing.edition_id.clone())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        let expression = match &edition {
            Some(edition) => expressions::Entity::find_by_id(edition.expression_id.clone())
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?,
            None => None,
        };
        let work = match &expression {
            Some(expression) => works::Entity::find_by_id(expression.work_id.clone())
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?,
            None => None,
        };
        let identifiers = edition_identifiers::Entity::find()
            .filter(edition_identifiers::Column::EditionId.eq(listing.edition_id.as_str()))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        let acquisition = edition_acquisitions::Entity::find_by_id(listing.edition_id.clone())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        let book_uuid = books::Entity::find()
            .filter(books::Column::PluginKey.eq(plugin_key))
            .filter(books::Column::AccountId.eq(account_id))
            .filter(books::Column::ProductId.eq(product_id.as_str()))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(|book| book.uuid);

        let asins = || identifiers.iter().filter(|m| m.scheme == "asin");
        let asin = asins()
            .find(|m| m.marketplace == listing.marketplace)
            .or_else(|| asins().find(|m| m.marketplace.is_empty()))
            .or_else(|| asins().next())
            .map(|m| m.value.clone());
        let isbn = identifiers
            .iter()
            .filter(|m| m.scheme == "isbn")
            .map(|m| m.value.clone())
            .min();
        let (status, storage_key, pdf_status, pdf_storage_key) = match acquisition {
            Some(row) => (
                AcquireStatus::parse(&row.status).unwrap_or_default(),
                row.storage_key,
                AcquireStatus::parse(&row.pdf_status).unwrap_or_default(),
                row.pdf_storage_key,
            ),
            None => (
                AcquireStatus::NotAcquired,
                None,
                AcquireStatus::NotAcquired,
                None,
            ),
        };
        let (edition_subtitle, edition_published, edition_minutes) = edition
            .map(|e| (e.subtitle, e.published_at, e.duration_minutes))
            .unwrap_or_default();
        let abridged_expression = expression
            .as_ref()
            .is_some_and(|e| e.abridgement == bookclerk_resolve::Abridgement::Abridged.as_str());
        let content_kind = match work.as_ref() {
            Some(w) if w.kind == bookclerk_resolve::WorkKind::Podcast.as_str() => {
                if w.parent_work_id.is_some() {
                    "episode"
                } else {
                    "podcast"
                }
            }
            _ => "book",
        };
        Ok(Some(AcquireTitle {
            edition_id: listing.edition_id,
            listing_id: listing.id,
            plugin_key: listing.plugin_key,
            account_id: account_id.to_string(),
            marketplace: listing.marketplace,
            product_id: listing.product_id,
            asin,
            isbn,
            title: listing.title,
            subtitle: listing.subtitle.or(edition_subtitle),
            authors: listing.authors,
            narrators: listing.narrators,
            series: listing.series,
            series_index: listing.series_index,
            series_product_id: listing.series_product_id,
            publisher: listing.publisher,
            categories: listing.categories,
            length_minutes: listing.length_minutes.or(edition_minutes),
            is_abridged: listing.is_abridged == Some(1) || abridged_expression,
            content_kind,
            purchased_at: parse_time(holding.purchased_at.as_deref()),
            published_at: parse_time(
                listing
                    .published_at
                    .as_deref()
                    .or(edition_published.as_deref()),
            ),
            status,
            storage_key,
            pdf_status,
            pdf_storage_key,
            book_uuid,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewBook;

    #[test]
    fn times_read_as_rfc3339_or_a_date() {
        assert_eq!(
            parse_time(Some("2024-03-01T10:00:00+02:00")).map(|t| t.to_rfc3339()),
            Some("2024-03-01T08:00:00+00:00".to_string())
        );
        assert_eq!(
            parse_time(Some("2024-03-01")).map(|t| t.to_rfc3339()),
            Some("2024-03-01T00:00:00+00:00".to_string())
        );
        assert_eq!(parse_time(Some("  ")), None);
        assert_eq!(parse_time(Some("March")), None);
        assert_eq!(parse_time(None), None);
    }

    #[tokio::test]
    async fn the_view_matches_the_flat_row_under_every_profile() {
        for profile in crate::test_support::Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            store
                .upsert_account("audible", "acct", "us", None, true, "audible")
                .await
                .unwrap();
            store
                .upsert_account("audible", "other", "us", None, true, "audible")
                .await
                .unwrap();
            let mut book = NewBook::minimal("B0VIEW001", "acct", "US", "Viewed");
            book.authors = Some("Ann Author".into());
            book.narrators = Some("Ned Narrator".into());
            book.series = Some("Saga".into());
            book.series_index = Some("2".into());
            book.series_asin = Some("B0SAGA".into());
            book.publisher = Some("House".into());
            book.categories = Some("Fantasy".into());
            book.length_minutes = Some(605);
            book.is_abridged = true;
            book.purchased_at = Some("2023-05-06T07:08:09Z".parse().unwrap());
            book.published_at = Some("2020-01-02T00:00:00Z".parse().unwrap());
            let flat = store.upsert_book(&book).await.unwrap();
            store
                .set_acquire_status(
                    "B0VIEW001",
                    "acct",
                    AcquireStatus::Acquired,
                    Some("Ann Author/Viewed.m4b"),
                    None,
                )
                .await
                .unwrap();
            store
                .set_pdf_status("B0VIEW001", "acct", AcquireStatus::Queued, None)
                .await
                .unwrap();

            let view = store
                .acquire_title("audible", "acct", "US", "B0VIEW001")
                .await
                .unwrap()
                .unwrap();
            let flat = store.get_book_by_uuid(&flat.uuid).await.unwrap().unwrap();
            let name = profile.name();
            assert_eq!(
                view.book_uuid.as_deref(),
                Some(flat.uuid.as_str()),
                "{name}"
            );
            assert_eq!(
                Some(view.edition_id.clone()),
                store.edition_of_book(&flat.uuid).await.unwrap()
            );
            assert_eq!(view.plugin_key, flat.plugin_key);
            assert_eq!(view.account_id, flat.account_id);
            assert_eq!(view.marketplace, flat.marketplace.to_lowercase());
            assert_eq!(view.product_id, flat.download_product_id());
            assert_eq!(view.asin, flat.asin, "{name}");
            assert_eq!(view.isbn, flat.isbn);
            assert_eq!(view.asin_or_isbn(), flat.asin_or_isbn());
            assert_eq!(view.title, flat.title);
            assert_eq!(view.authors, flat.authors);
            assert_eq!(view.narrators, flat.narrators);
            assert_eq!(view.series, flat.series);
            assert_eq!(view.series_index, flat.series_index);
            assert_eq!(view.series_product_id, flat.series_asin);
            assert_eq!(view.publisher, flat.publisher);
            assert_eq!(view.categories, flat.categories);
            assert_eq!(view.length_minutes, flat.length_minutes, "{name}");
            assert_eq!(view.is_abridged, flat.is_abridged);
            assert_eq!(view.content_kind, "book");
            assert_eq!(view.purchased_at, flat.purchased_at, "{name}");
            assert_eq!(view.published_at, flat.published_at, "{name}");
            assert_eq!(view.status, flat.acquire_status);
            assert_eq!(view.storage_key, flat.storage_key);
            assert_eq!(view.pdf_status, flat.pdf_status);
            assert_eq!(view.pdf_storage_key, flat.pdf_storage_key);

            // Another account of the store that does not hold it, and an
            // unknown product, have no view.
            assert_eq!(
                store
                    .acquire_title("audible", "other", "us", "B0VIEW001")
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                store
                    .acquire_title("audible", "acct", "us", "B0NOPE")
                    .await
                    .unwrap(),
                None
            );
        }
    }

    #[tokio::test]
    async fn an_isbn_store_title_names_its_isbn() {
        let kit = crate::test_support::sqlite::memory(crate::test_support::Profile::ALL[0]).await;
        let store = kit.store();
        store
            .upsert_account("libro", "acct", "", None, true, "libro")
            .await
            .unwrap();
        let mut book = NewBook::minimal("9780000000019", "acct", "", "Printed");
        book.plugin_key = "libro".into();
        book.source = "libro".into();
        book.asin = None;
        book.isbn = Some("9780000000019".into());
        let flat = store.upsert_book(&book).await.unwrap();
        let view = store
            .acquire_title(&flat.plugin_key, "acct", "", "9780000000019")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.asin, flat.asin);
        assert_eq!(view.isbn, flat.isbn);
        assert_eq!(view.asin_or_isbn(), flat.asin_or_isbn());
        assert_eq!(view.status, AcquireStatus::NotAcquired);
        assert_eq!(view.storage_key, None);
    }

    #[tokio::test]
    async fn a_podcast_episode_and_its_show_name_their_kinds() {
        let kit = crate::test_support::sqlite::memory(crate::test_support::Profile::ALL[0]).await;
        let store = kit.store();
        store
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut show = NewBook::minimal("B0SHOW", "acct", "us", "Show");
        show.content_kind = "podcast".into();
        store.upsert_book(&show).await.unwrap();
        let mut episode = NewBook::minimal("B0EP0001", "acct", "us", "Episode 1");
        episode.content_kind = "episode".into();
        episode.series_asin = Some("B0SHOW".into());
        store.upsert_book(&episode).await.unwrap();
        let kind = |product: &'static str| {
            let store = store.clone();
            async move {
                store
                    .acquire_title("audible", "acct", "us", product)
                    .await
                    .unwrap()
                    .unwrap()
                    .content_kind
            }
        };
        assert_eq!(kind("B0SHOW").await, "podcast");
        assert_eq!(kind("B0EP0001").await, "episode");
    }
}
