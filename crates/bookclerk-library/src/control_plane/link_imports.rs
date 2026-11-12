//! On a branch: the branch owner's own books, acquired for a library it
//! joined, imported into the branch's own library as well.
//!
//! A branch that acquires a title for the library it joined keeps a copy on
//! its own destinations when the title is its owner's
//! ([`bookclerk_node_proto::WorkJob::for_node_owner`]). Once the library has
//! committed the title, [`import_link_title`] records it here as an
//! ordinary local change: the flat row and its listing, an account holding
//! for the store account that bought it, the acquire status and the copy's
//! files, with the book-acquired event that change publishes. The holding
//! carries the provenance `link:<cluster_id>` ([`link_provenance`]), so the
//! catalog sync never sends the library a book it acquired itself
//! ([`books_imported_from_link`]).
//!
//! Only a store account connected on this install to the branch's person
//! ([`super::branch_person`]) receives such a book: a book belongs to the
//! account that bought it, and an account this install does not have, or
//! has for someone else, is left alone.

use std::collections::BTreeSet;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait as _, EntityTrait as _, FromQueryResult, QueryFilter as _, QuerySelect as _,
};

use super::carried_accounts::branch_person;
use super::trust::accounts_owned_by;
use crate::entities::{holdings, store_listings};
use crate::error::{LibraryError, Result};
use crate::models::{AcquireStatus, BookRecord};
use crate::store::AcquiredBookKey;
use crate::store::{FileRole, LibraryStore, NewBook};

/// Store listings looked up per `IN (...)` read of
/// [`books_imported_from_link`]; well under every profile's bind limit.
const LISTING_CHUNK: usize = 50;

/// The holding provenance of a book imported from the library
/// `cluster_id`: `link:<cluster_id>`.
#[must_use]
pub fn link_provenance(cluster_id: &str) -> String {
    format!("link:{cluster_id}")
}

/// One title the library committed, as the branch kept it.
#[derive(Debug, Clone, Copy)]
pub struct LinkImport<'a> {
    /// The library this branch acquired the title for.
    pub cluster_id: &'a str,
    /// The library's row of the title (its uuid is the library's and is not
    /// reused here).
    pub book: &'a BookRecord,
    /// Storage key of the primary audio file, the same on every
    /// destination of the copy.
    pub primary_key: &'a str,
    /// Storage key of the companion PDF, when one was acquired.
    pub pdf_key: Option<&'a str>,
    /// Every file of the copy: `(destination id, storage key)`, where the
    /// destination id is this install's (`local`, `s3`).
    pub files: &'a [(&'a str, &'a str)],
}

/// What [`import_link_title`] did.
#[derive(Debug, Clone)]
pub enum LinkImportOutcome {
    /// The title is in this library now, as this row.
    Imported(Box<BookRecord>),
    /// The store account that bought it is not connected here to the
    /// branch's person; nothing was written.
    AccountNotHere,
    /// This install has no single person a branch's books belong to (no
    /// active Owner, or several); nothing was written.
    NoPerson,
}

/// Imports a title the library committed into this install's own library
/// (see the module docs).
///
/// A file whose key another live edition here already holds is left
/// unrecorded (logged), as an acquire on this install would refuse it.
///
/// # Errors
///
/// Returns an error when a read or write fails; the writes before it stay.
pub async fn import_link_title(
    store: &LibraryStore,
    import: LinkImport<'_>,
) -> Result<LinkImportOutcome> {
    let Some(person) = branch_person(store, None).await? else {
        return Ok(LinkImportOutcome::NoPerson);
    };
    let book = import.book;
    let account = (book.plugin_key.clone(), book.account_id.clone());
    if !accounts_owned_by(store, person).await?.contains(&account) {
        return Ok(LinkImportOutcome::AccountNotHere);
    }
    let record = store.upsert_book(&new_book(book)).await?;
    store
        .set_acquire_status(
            &record.product_id,
            &record.account_id,
            AcquireStatus::Acquired,
            Some(import.primary_key),
            None,
        )
        .await?;
    if let Some(pdf_key) = import.pdf_key {
        store
            .set_pdf_status(
                &record.product_id,
                &record.account_id,
                AcquireStatus::Acquired,
                Some(pdf_key),
            )
            .await?;
    }
    if let Some(owner) = store.file_owner_of_book(&record.uuid).await? {
        for (destination, key) in import.files {
            let role = FileRole::for_key(key, Some(import.primary_key));
            if !store
                .record_edition_file(destination, key, &owner, role)
                .await?
            {
                tracing::warn!(
                    book_uuid = %record.uuid,
                    %destination,
                    %key,
                    "another edition here holds this key; the imported copy leaves it unrecorded"
                );
            }
        }
    }
    mark_holding(store, &record, &link_provenance(import.cluster_id)).await?;
    let stored = store
        .get_book_by_uuid(&record.uuid)
        .await?
        .unwrap_or(record);
    Ok(LinkImportOutcome::Imported(Box::new(stored)))
}

/// The library's row as an insert here: the same store identity and
/// metadata under this library's own uuid.
fn new_book(book: &BookRecord) -> NewBook {
    NewBook {
        uuid: None,
        product_id: book.product_id.clone(),
        plugin_key: book.plugin_key.clone(),
        source: book.source.clone(),
        account_id: book.account_id.clone(),
        asin: book.asin.clone(),
        isbn: book.isbn.clone(),
        marketplace: book.marketplace.clone(),
        title: book.title.clone(),
        authors: book.authors.clone(),
        narrators: book.narrators.clone(),
        series: book.series.clone(),
        series_index: book.series_index.clone(),
        series_asin: book.series_asin.clone(),
        purchased_at: book.purchased_at,
        publisher: book.publisher.clone(),
        length_minutes: book.length_minutes,
        is_abridged: book.is_abridged,
        content_kind: book.content_kind.clone(),
        categories: book.categories.clone(),
        subtitle: book.subtitle.clone(),
        published_at: book.published_at,
    }
}

/// Sets `provenance` on the account holding of `book`'s listing. A listing
/// the library model did not resolve has no holding and is left as it is.
///
/// # Errors
///
/// Returns an error when a read or write fails.
async fn mark_holding(store: &LibraryStore, book: &BookRecord, provenance: &str) -> Result<()> {
    let listing = store_listings::Entity::find()
        .filter(store_listings::Column::PluginKey.eq(book.plugin_key.as_str()))
        .filter(
            store_listings::Column::Marketplace
                .eq(bookclerk_resolve::stored_marketplace(&book.marketplace)),
        )
        .filter(
            store_listings::Column::ProductId
                .eq(bookclerk_resolve::stored_product_id(&book.product_id)),
        )
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let Some(listing) = listing else {
        return Ok(());
    };
    holdings::Entity::update_many()
        .col_expr(holdings::Column::Provenance, Expr::value(provenance))
        .filter(holdings::Column::ListingId.eq(listing.id))
        .filter(holdings::Column::PluginKey.eq(book.plugin_key.as_str()))
        .filter(holdings::Column::AccountId.eq(book.account_id.as_str()))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// One narrow `holdings` row as read; columns are matched by name.
#[derive(Debug, FromQueryResult)]
struct HolderRow {
    /// The listing held.
    listing_id: String,
    /// Holding store account; `None` for a branch node's holding.
    account_id: Option<String>,
}

/// A book this library holds, by store identity: `(plugin_key, stored
/// marketplace, stored product id, account_id)`.
pub type HeldBookKey = (String, String, String, String);

/// Every account holding imported from the library `cluster_id`
/// ([`link_provenance`]), by store identity: the books the catalog sync
/// must not send back to it.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn books_imported_from_link(
    store: &LibraryStore,
    cluster_id: &str,
) -> Result<BTreeSet<HeldBookKey>> {
    let rows = holdings::Entity::find()
        .select_only()
        .column(holdings::Column::ListingId)
        .column(holdings::Column::AccountId)
        .filter(holdings::Column::Provenance.eq(link_provenance(cluster_id)))
        .into_model::<HolderRow>()
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut holders: Vec<(String, String)> = Vec::new();
    for row in rows {
        if let Some(account) = row.account_id {
            holders.push((row.listing_id, account));
        }
    }
    let mut out = BTreeSet::new();
    for chunk in holders.chunks(LISTING_CHUNK) {
        let ids: Vec<&str> = chunk.iter().map(|(id, _)| id.as_str()).collect();
        let listings = store_listings::Entity::find()
            .filter(store_listings::Column::Id.is_in(ids))
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        for (listing_id, account) in chunk {
            if let Some(listing) = listings.iter().find(|l| &l.id == listing_id) {
                out.insert((
                    listing.plugin_key.clone(),
                    listing.marketplace.clone(),
                    listing.product_id.clone(),
                    account.clone(),
                ));
            }
        }
    }
    Ok(out)
}

/// Which of this install's acquired books the catalog sync sends the
/// library `cluster_id`: only the branch owner's own books, those held by
/// a store account connected here to the branch's person
/// ([`super::branch_person`]), and never one imported from that library
/// ([`books_imported_from_link`]). With no single person, nothing.
#[derive(Debug, Clone, Default)]
pub struct OwnerShareFilter {
    /// The person's store accounts: `(plugin_key, account_id)`.
    accounts: BTreeSet<super::AccountKey>,
    /// Books imported from the library, by store identity.
    imported: BTreeSet<HeldBookKey>,
}

impl OwnerShareFilter {
    /// Reads the filter for the library `cluster_id` now.
    ///
    /// # Errors
    ///
    /// Returns an error when a read fails.
    pub async fn load(store: &LibraryStore, cluster_id: &str) -> Result<Self> {
        let Some(person) = branch_person(store, None).await? else {
            return Ok(Self::default());
        };
        Ok(Self {
            accounts: accounts_owned_by(store, person).await?,
            imported: books_imported_from_link(store, cluster_id).await?,
        })
    }

    /// True when the book `key` names is sent.
    #[must_use]
    pub fn admits(&self, key: &AcquiredBookKey) -> bool {
        self.accounts
            .contains(&(key.plugin_key.clone(), key.account_id.clone()))
            && !self.imported.contains(&(
                key.plugin_key.clone(),
                bookclerk_resolve::stored_marketplace(&key.marketplace),
                bookclerk_resolve::stored_product_id(&key.product_id),
                key.account_id.clone(),
            ))
    }
}

#[cfg(test)]
#[path = "link_imports_tests.rs"]
mod tests;
