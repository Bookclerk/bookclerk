//! On a branch: the branch owner's own books, acquired for a library it
//! joined, imported into the branch's own library as well.
//!
//! A branch that acquires a title for the library it joined keeps a copy on
//! its own destinations when the title is its owner's
//! ([`bookclerk_node_proto::WorkJob::for_node_owner`]). Once the library has
//! committed the title, [`import_link_title`] records it here as an
//! ordinary local change: the flat row and its listing, an account holding
//! for the store account that bought it, the acquire status and the copy's
//! files, with the book-acquired event that change publishes. The import is
//! also recorded for that library in `link_imports`, once per library a
//! book came from, so the catalog sync never sends a library a book it
//! acquired itself ([`books_imported_from_link`]).
//!
//! Only a store account connected on this install to the branch's person
//! ([`super::branch_person`]) receives such a book: a book belongs to the
//! account that bought it, and an account this install does not have, or
//! has for someone else, is left alone.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::{NotSet, Set},
    ColumnTrait as _, EntityTrait as _, FromQueryResult, QueryFilter as _, QueryOrder as _,
    QuerySelect as _,
};

use super::carried_accounts::branch_person;
use super::trust::accounts_owned_by;
use crate::entities::{account_links, link_imports};
use crate::error::{LibraryError, Result};
use crate::models::{AcquireStatus, BookRecord};
use crate::store::AcquiredBookKey;
use crate::store::{FileRole, LibraryStore, NewBook};

/// Rows per read of [`books_imported_from_link`] when the database plugin
/// advertises no smaller `maxResultRows`.
const IMPORT_PAGE: u64 = 256;

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
    /// Another live edition here holds the copy's primary file key, so the
    /// copy is not this title's: its row stays (not acquired) and nothing
    /// else was recorded.
    KeyHeldElsewhere,
}

/// Imports a title the library committed into this install's own library
/// (see the module docs).
///
/// A file whose key another live edition here already holds is left
/// unrecorded (logged), as an acquire on this install would refuse it; when
/// that is the primary file, the title is not marked acquired
/// ([`LinkImportOutcome::KeyHeldElsewhere`]); when it is the PDF, the PDF
/// is not marked acquired.
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
    // The files first, the primary one before the rest: a primary file
    // another live edition here holds is not this copy's, so the title is
    // not marked acquired at it and no other file is claimed for it.
    let mut pdf_refused = false;
    if let Some(owner) = store.file_owner_of_book(&record.uuid).await? {
        let mut files = import.files.to_vec();
        files.sort_by_key(|(_, key)| *key != import.primary_key);
        for (destination, key) in files {
            let role = FileRole::for_key(key, Some(import.primary_key));
            if store
                .record_edition_file(destination, key, &owner, role)
                .await?
            {
                continue;
            }
            tracing::warn!(
                book_uuid = %record.uuid,
                %destination,
                %key,
                "another edition here holds this key; the imported copy leaves it unrecorded"
            );
            if key == import.primary_key {
                return Ok(LinkImportOutcome::KeyHeldElsewhere);
            }
            pdf_refused |= import.pdf_key == Some(key);
        }
    }
    store
        .set_acquire_status(
            &record.uuid,
            &record.account_id,
            AcquireStatus::Acquired,
            Some(import.primary_key),
            None,
        )
        .await?;
    if let Some(pdf_key) = import.pdf_key.filter(|_| !pdf_refused) {
        store
            .set_pdf_status(
                &record.uuid,
                &record.account_id,
                AcquireStatus::Acquired,
                Some(pdf_key),
            )
            .await?;
    }
    record_import(store, import.cluster_id, &record).await?;
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

/// Records that `book` was imported from the library `cluster_id`. A book
/// already recorded for that library is left as it is; one imported from
/// several libraries is recorded once for each.
///
/// # Errors
///
/// Returns an error when the write fails.
async fn record_import(store: &LibraryStore, cluster_id: &str, book: &BookRecord) -> Result<()> {
    let row = link_imports::ActiveModel {
        id: NotSet,
        cluster_id: Set(cluster_id.to_string()),
        plugin_key: Set(book.plugin_key.clone()),
        account_id: Set(book.account_id.clone()),
        marketplace: Set(bookclerk_resolve::stored_marketplace(&book.marketplace)),
        product_id: Set(bookclerk_resolve::stored_product_id(&book.product_id)),
        created_at: Set(Utc::now().to_rfc3339()),
    };
    link_imports::Entity::insert(row)
        .on_conflict(
            OnConflict::columns([
                link_imports::Column::ClusterId,
                link_imports::Column::PluginKey,
                link_imports::Column::AccountId,
                link_imports::Column::Marketplace,
                link_imports::Column::ProductId,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// One narrow `link_imports` row as read; columns are matched by name.
#[derive(Debug, FromQueryResult)]
struct ImportRow {
    /// Keyset position.
    id: i64,
    /// Plugin key.
    plugin_key: String,
    /// Store account id.
    account_id: String,
    /// Stored marketplace.
    marketplace: String,
    /// Stored product id.
    product_id: String,
}

/// A book this library holds, by store identity: `(plugin_key, stored
/// marketplace, stored product id, account_id)`.
pub type HeldBookKey = (String, String, String, String);

/// Every book imported from the library `cluster_id`, by store identity:
/// the books the catalog sync must not send back to it. Read in keyset
/// pages no larger than the database plugin's `maxResultRows`.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn books_imported_from_link(
    store: &LibraryStore,
    cluster_id: &str,
) -> Result<BTreeSet<HeldBookKey>> {
    let page = match store.db_capabilities().max_result_rows {
        0 => IMPORT_PAGE,
        rows => u64::from(rows).clamp(1, IMPORT_PAGE),
    };
    let mut out = BTreeSet::new();
    let mut after: Option<i64> = None;
    loop {
        let mut query = link_imports::Entity::find()
            .select_only()
            .column(link_imports::Column::Id)
            .column(link_imports::Column::PluginKey)
            .column(link_imports::Column::AccountId)
            .column(link_imports::Column::Marketplace)
            .column(link_imports::Column::ProductId)
            .filter(link_imports::Column::ClusterId.eq(cluster_id))
            .order_by_asc(link_imports::Column::Id);
        if let Some(after) = after {
            query = query.filter(link_imports::Column::Id.gt(after));
        }
        let rows = query
            .limit(page)
            .into_model::<ImportRow>()
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let full = u64::try_from(rows.len()).unwrap_or(u64::MAX) >= page;
        after = rows.last().map(|row| row.id);
        for row in rows {
            out.insert((
                row.plugin_key,
                row.marketplace,
                row.product_id,
                row.account_id,
            ));
        }
        if !full {
            return Ok(out);
        }
    }
}

/// The books the library `cluster_id` asked for that this branch still
/// lists to it: none once the link is gone or "Share this branch's books"
/// is off, and a member's only while "List my members' books" is on and
/// they did not opt out (the rules a request was accepted under).
///
/// # Errors
///
/// Returns an error when a read fails.
async fn still_listed_requests(
    store: &LibraryStore,
    cluster_id: &str,
) -> Result<BTreeMap<HeldBookKey, DateTime<Utc>>> {
    let Some(link) = super::load_cluster_link(store, cluster_id).await? else {
        return Ok(BTreeMap::new());
    };
    if !link.share_books {
        return Ok(BTreeMap::new());
    }
    let requested = super::books_requested_by_link(store, cluster_id).await?;
    let accounts: BTreeSet<super::AccountKey> = requested
        .keys()
        .map(|(plugin, _, _, account)| (plugin.clone(), account.clone()))
        .collect();
    let listable =
        super::catalog_lookup::listable_accounts(store, &accounts, link.list_members_books).await?;
    Ok(requested
        .into_iter()
        .filter(|((plugin, _, _, account), _)| {
            listable.contains(&(plugin.clone(), account.clone()))
        })
        .collect())
}

/// The store accounts this branch lists to the library `cluster_id` now,
/// under the catalog answers' rules (`super::catalog_lookup::listable_accounts`):
/// none once the link is gone or "Share this branch's books" is off.
///
/// # Errors
///
/// Returns an error when a read fails.
async fn listed_accounts(
    store: &LibraryStore,
    cluster_id: &str,
) -> Result<BTreeSet<super::AccountKey>> {
    let Some(link) = super::load_cluster_link(store, cluster_id).await? else {
        return Ok(BTreeSet::new());
    };
    if !link.share_books {
        return Ok(BTreeSet::new());
    }
    let linked = linked_accounts(store).await?;
    super::catalog_lookup::listable_accounts(store, &linked, link.list_members_books).await
}

/// Every store account linked to someone on this install, in keyset pages
/// no larger than the database plugin's `maxResultRows`.
///
/// # Errors
///
/// Returns an error when a read fails.
async fn linked_accounts(store: &LibraryStore) -> Result<BTreeSet<super::AccountKey>> {
    let page = super::trust::result_page(store);
    let mut out = BTreeSet::new();
    let mut after: Option<i64> = None;
    loop {
        let mut query = account_links::Entity::find().order_by_asc(account_links::Column::Id);
        if let Some(after) = after {
            query = query.filter(account_links::Column::Id.gt(after));
        }
        let links = query
            .limit(page)
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let full = u64::try_from(links.len()).unwrap_or(u64::MAX) >= page;
        after = links.last().map(|link| link.id);
        out.extend(
            links
                .into_iter()
                .map(|link| (link.plugin_key, link.account_id)),
        );
        if !full {
            return Ok(out);
        }
    }
}

/// Which of this install's acquired books the catalog sync sends the
/// library `cluster_id`: the branch owner's own books, those held by a
/// store account connected here to the branch's person
/// ([`super::branch_person`]); the books that library asked for through an
/// interlibrary request and this branch still lists to it
/// (`still_listed_requests`); and, while that library acquires all of this
/// branch's members' books, the books of every store account this branch
/// lists to it (`listed_accounts`). Never one imported from that library
/// ([`books_imported_from_link`]). With no single person, the owner's
/// books are not among them.
#[derive(Debug, Clone, Default)]
pub struct OwnerShareFilter {
    /// The person's store accounts: `(plugin_key, account_id)`.
    accounts: BTreeSet<super::AccountKey>,
    /// With the library's "Acquire all of this branch's members' books"
    /// on, the store accounts this branch lists to it; otherwise empty.
    listed: BTreeSet<super::AccountKey>,
    /// Books the library asked for, by store identity, with when it last
    /// asked.
    requested: BTreeMap<HeldBookKey, DateTime<Utc>>,
    /// Books imported from the library, by store identity.
    imported: BTreeSet<HeldBookKey>,
}

impl OwnerShareFilter {
    /// Reads the filter for the library `cluster_id` now.
    ///
    /// # Arguments
    ///
    /// * `members_books` - That library's "Acquire all of this branch's
    ///   members' books", from its catalog session.
    ///
    /// # Errors
    ///
    /// Returns an error when a read fails.
    pub async fn load(store: &LibraryStore, cluster_id: &str, members_books: bool) -> Result<Self> {
        let accounts = match branch_person(store, None).await? {
            Some(person) => accounts_owned_by(store, person).await?,
            None => BTreeSet::new(),
        };
        let listed = if members_books {
            listed_accounts(store, cluster_id).await?
        } else {
            BTreeSet::new()
        };
        Ok(Self {
            accounts,
            listed,
            requested: still_listed_requests(store, cluster_id).await?,
            imported: books_imported_from_link(store, cluster_id).await?,
        })
    }

    /// True when the book `key` names is sent.
    #[must_use]
    pub fn admits(&self, key: &AcquiredBookKey) -> bool {
        let held = held_key(key);
        let account = (key.plugin_key.clone(), key.account_id.clone());
        let sent = self.accounts.contains(&account) || self.listed.contains(&account);
        (sent || self.requested.contains_key(&held)) && !self.imported.contains(&held)
    }

    /// When the library last asked for the book `key` names, if it asked
    /// and this branch still lists it. The catalog sync orders a requested
    /// book no earlier than this, so a book already past the sync's cursor
    /// still goes up on the next pass.
    #[must_use]
    pub fn requested_at(&self, key: &AcquiredBookKey) -> Option<DateTime<Utc>> {
        self.requested.get(&held_key(key)).copied()
    }
}

/// The store identity of an acquired book, normalized as stored.
fn held_key(key: &AcquiredBookKey) -> HeldBookKey {
    (
        key.plugin_key.clone(),
        bookclerk_resolve::stored_marketplace(&key.marketplace),
        bookclerk_resolve::stored_product_id(&key.product_id),
        key.account_id.clone(),
    )
}

#[cfg(test)]
#[path = "link_imports_tests.rs"]
mod tests;
