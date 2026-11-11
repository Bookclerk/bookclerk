//! Reads the node-local search index needs to follow library events.
//!
//! The search index holds one document per library title (`books` row). The
//! library model's events name editions and listings
//! (`edition_linked`, `edition_updated`, `holding_added`, and
//! `holding_removed` once holdings can be removed), not titles, so the
//! index consumer maps an event to the listings it touches, and each
//! listing to the titles that sell it. A listing is matched to a title the
//! way the resolver derives one: same plugin key, the stored marketplace and
//! the stored product id ([`ListingRef`]).
//!
//! Refreshing by listing rather than by title lets the index drop a title
//! whose row is gone: the consumer deletes every document carrying the
//! listing's [`ListingRef::term`] and re-adds the titles that still sell it.

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::{map_book, LibraryStore};
use crate::entities::{books, store_listings};
use crate::error::{LibraryError, Result};
use crate::models::BookRecord;

/// The identity of a store listing as the search index stores it: plugin
/// key, stored marketplace and stored product id.
///
/// Built either from a `store_listings` row or from a library title, and
/// both spell it the same way, so a title's document and its listing's
/// refresh meet on one [`Self::term`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListingRef {
    /// Provenance-qualified PluginKey of the storefront.
    plugin_key: String,
    /// Marketplace in its stored spelling (trimmed, lower-cased, default
    /// when blank).
    marketplace: String,
    /// The store's product id in its stored spelling (words joined by single
    /// spaces).
    product_id: String,
}

impl ListingRef {
    /// The listing a library title's row resolves to.
    ///
    /// # Arguments
    ///
    /// * `book` - The library title.
    #[must_use]
    pub fn of_book(book: &BookRecord) -> Self {
        Self::new(&book.plugin_key, &book.marketplace, &book.product_id)
    }

    /// A listing from raw parts, normalized the way listings are stored.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The storefront's PluginKey.
    /// * `marketplace` - The marketplace as reported (normalized here).
    /// * `product_id` - The product id as reported (normalized here).
    #[must_use]
    pub fn new(plugin_key: &str, marketplace: &str, product_id: &str) -> Self {
        Self {
            plugin_key: plugin_key.to_string(),
            marketplace: bookclerk_resolve::stored_marketplace(marketplace),
            product_id: bookclerk_resolve::stored_product_id(product_id),
        }
    }

    /// The listing of a stored `store_listings` row (already normalized).
    fn of_row(row: store_listings::Model) -> Self {
        Self {
            plugin_key: row.plugin_key,
            marketplace: row.marketplace,
            product_id: row.product_id,
        }
    }

    /// The single index term that names this listing: its three parts
    /// joined by the ASCII unit separator, which none of them contains.
    #[must_use]
    pub fn term(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            self.plugin_key, self.marketplace, self.product_id
        )
    }
}

impl LibraryStore {
    /// The listings that sell `edition_id`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn listing_refs_of_edition(&self, edition_id: &str) -> Result<Vec<ListingRef>> {
        Ok(store_listings::Entity::find()
            .filter(store_listings::Column::EditionId.eq(edition_id))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(ListingRef::of_row)
            .collect())
    }

    /// The listing stored under `listing_id`, or `None` when it is gone.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn listing_ref_by_id(&self, listing_id: &str) -> Result<Option<ListingRef>> {
        Ok(store_listings::Entity::find_by_id(listing_id.to_string())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(ListingRef::of_row))
    }

    /// Every library title whose row resolves to `listing`, one per
    /// account that holds it.
    ///
    /// A row keeps its product id as the store spelled it, padding
    /// included, while the listing keeps the normalized spelling, so the
    /// read narrows by plugin key and the product id's first word and the
    /// rest is compared normalized.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails, or a mapping error
    /// for a row the host cannot read.
    pub async fn books_of_listing(&self, listing: &ListingRef) -> Result<Vec<BookRecord>> {
        let first_word = listing
            .product_id
            .split_whitespace()
            .next()
            .unwrap_or_default();
        books::Entity::find()
            .filter(books::Column::PluginKey.eq(listing.plugin_key.as_str()))
            .filter(books::Column::ProductId.contains(first_word))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .filter(|row| {
                bookclerk_resolve::stored_product_id(&row.product_id) == listing.product_id
                    && bookclerk_resolve::stored_marketplace(&row.marketplace)
                        == listing.marketplace
            })
            .map(map_book)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewBook;

    #[test]
    fn a_title_and_its_raw_parts_share_one_term() {
        let a = ListingRef::new("platform:audible", " US ", "B00  TEST");
        let b = ListingRef::new("platform:audible", "us", "B00 TEST");
        assert_eq!(a, b);
        assert_eq!(a.term(), "platform:audible\u{1f}us\u{1f}B00 TEST");
        assert_ne!(
            a.term(),
            ListingRef::new("platform:libro", "us", "B00 TEST").term()
        );
    }

    #[tokio::test]
    async fn a_listing_finds_every_account_row_that_sells_it() {
        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite"),
        );
        for account in ["acct-a", "acct-b"] {
            library
                .upsert_account("audible", account, "us", None, true, "audible")
                .await
                .unwrap();
            library
                .upsert_book(&NewBook::minimal("B00SHARED", account, "us", "Shared"))
                .await
                .unwrap();
        }
        library
            .upsert_book(&NewBook::minimal("B00OTHER", "acct-a", "us", "Other"))
            .await
            .unwrap();
        let shared = library
            .get_book("B00SHARED", "acct-a")
            .await
            .unwrap()
            .unwrap();
        let listing = ListingRef::of_book(&shared);
        let mut accounts = library
            .books_of_listing(&listing)
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.account_id)
            .collect::<Vec<_>>();
        accounts.sort();
        assert_eq!(accounts, ["acct-a", "acct-b"]);

        // The resolver stored the listing and its edition from the rows.
        let edition = library
            .edition_of_book(&shared.uuid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            library.listing_refs_of_edition(&edition).await.unwrap(),
            vec![listing.clone()]
        );
        let row = store_listings::Entity::find()
            .filter(store_listings::Column::EditionId.eq(edition.as_str()))
            .one(library.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            library.listing_ref_by_id(&row.id).await.unwrap(),
            Some(listing)
        );
        assert_eq!(library.listing_ref_by_id("gone").await.unwrap(), None);
    }
}
