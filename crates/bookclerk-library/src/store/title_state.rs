//! `bookclerk library export-state`: the user state that only Bookclerk knows
//! about each title, written against the current `books` schema so that it
//! can be restored after the title tables are rebuilt (library data model
//! ADR, `docs/adr/library-data-model.md`, section 7).
//!
//! Everything is keyed by store listing (`plugin_key`, `account_id`,
//! `product_id`, `marketplace`) plus the identifiers a rescan brings back
//! (ASIN, ISBN), never by `books.uuid` or `works.id`, which the rebuild
//! replaces.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};

use crate::entities::ignored_titles;
use crate::error::{LibraryError, Result};
use crate::models::BookRecord;
use crate::store::LibraryStore;

/// Value of [`TitleStateExport::format`]; names the document type.
pub const TITLE_STATE_FORMAT: &str = "bookclerk.title-state";

/// Value of [`TitleStateExport::version`] this build writes.
pub const TITLE_STATE_VERSION: u32 = 1;

/// One exported document: user state for every title that has any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleStateExport {
    /// Always [`TITLE_STATE_FORMAT`], so an import can reject other JSON.
    pub format: String,
    /// Document version; [`TITLE_STATE_VERSION`] for this build.
    pub version: u32,
    /// When the export was taken.
    pub exported_at: DateTime<Utc>,
    /// Titles with tags, ratings or a finished flag, in listing order.
    pub titles: Vec<TitleState>,
    /// Titles on an account's ignore list.
    pub ignored: Vec<IgnoredTitle>,
    /// Listening-progress rows that point at a title, with the title named
    /// by listing instead of by `books.uuid`.
    pub listening_links: Vec<ListeningLink>,
    /// Title requests resolved to a title, named by listing.
    pub request_links: Vec<RequestLink>,
}

/// A store listing: the key that survives a rebuild and a rescan.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ListingKey {
    /// Storefront plugin key (`audible`, `libro`, …).
    pub plugin_key: String,
    /// Store account id the title belongs to.
    pub account_id: String,
    /// The store's own product id.
    pub product_id: String,
    /// Store marketplace (`us`, `uk`, …).
    pub marketplace: String,
}

impl ListingKey {
    /// The listing that `book` came from.
    ///
    /// # Arguments
    ///
    /// * `book` - A `books` row.
    ///
    /// # Returns
    ///
    /// The row's plugin, account, product id and marketplace.
    fn of(book: &BookRecord) -> Self {
        Self {
            plugin_key: book.plugin_key.clone(),
            account_id: book.account_id.clone(),
            product_id: book.product_id.clone(),
            marketplace: book.marketplace.clone(),
        }
    }
}

/// User state recorded on one title.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleState {
    /// Where the title was listed.
    pub listing: ListingKey,
    /// ASIN, when the store gave one; a fallback match key for the import.
    pub asin: Option<String>,
    /// ISBN, when the store gave one; a fallback match key for the import.
    pub isbn: Option<String>,
    /// Title text, for people reading the file; never used as a key.
    pub title: String,
    /// Author text, for people reading the file; never used as a key.
    pub authors: Option<String>,
    /// Tags as stored (serialized string), when any.
    pub tags: Option<String>,
    /// Overall rating, 0–5, when set.
    pub rating_overall: Option<f32>,
    /// Performance rating, 0–5, when set.
    pub rating_performance: Option<f32>,
    /// Story rating, 0–5, when set.
    pub rating_story: Option<f32>,
    /// Whether the title is marked finished.
    pub finished: bool,
}

impl TitleState {
    /// The state on `book`, or `None` when it carries none worth keeping.
    ///
    /// # Arguments
    ///
    /// * `book` - A `books` row.
    ///
    /// # Returns
    ///
    /// `Some` when the row has non-empty tags, any rating, or is finished.
    fn of(book: &BookRecord) -> Option<Self> {
        let tags = book
            .tags
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "[]")
            .map(str::to_string);
        let has_state = tags.is_some()
            || book.rating_overall.is_some()
            || book.rating_performance.is_some()
            || book.rating_story.is_some()
            || book.is_finished;
        has_state.then(|| Self {
            listing: ListingKey::of(book),
            asin: book.asin.clone(),
            isbn: book.isbn.clone(),
            title: book.title.clone(),
            authors: book.authors.clone(),
            tags,
            rating_overall: book.rating_overall,
            rating_performance: book.rating_performance,
            rating_story: book.rating_story,
            finished: book.is_finished,
        })
    }
}

/// One ignore-list entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredTitle {
    /// Storefront plugin key.
    pub plugin_key: String,
    /// Store account id.
    pub account_id: String,
    /// The store's product id.
    pub product_id: String,
    /// Operator note, when one was given.
    pub reason: Option<String>,
    /// When the title was ignored, as stored (RFC 3339).
    pub created_at: String,
}

/// A listening-progress row's link to a title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListeningLink {
    /// Progress provider (`audiobookshelf`, …).
    pub provider: String,
    /// The provider's user id.
    pub external_user_id: String,
    /// The provider's item id; with the two fields above, the row's key.
    pub external_item_id: String,
    /// The title the row points at.
    pub listing: ListingKey,
}

/// A title request's link to the title that resolved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLink {
    /// The request's uuid, which the rebuild keeps.
    pub request_uuid: String,
    /// The title that resolved the request.
    pub listing: ListingKey,
}

impl LibraryStore {
    /// Collects the user state on every title for
    /// `bookclerk library export-state`.
    ///
    /// Rows without state are left out. A listening-progress row or title
    /// request whose `book_uuid` no longer names a title is left out too,
    /// since there is nothing to re-link it to.
    ///
    /// # Arguments
    ///
    /// * `exported_at` - Timestamp written into the document.
    ///
    /// # Returns
    ///
    /// The document, with every list in a stable order.
    ///
    /// # Errors
    ///
    /// Returns a crate error when a read fails.
    pub async fn export_title_state(&self, exported_at: DateTime<Utc>) -> Result<TitleStateExport> {
        let books = self.list_books(None).await?;
        let by_uuid: BTreeMap<&str, &BookRecord> =
            books.iter().map(|b| (b.uuid.as_str(), b)).collect();

        let mut titles: Vec<TitleState> = books.iter().filter_map(TitleState::of).collect();
        titles.sort_by(|a, b| a.listing.cmp(&b.listing));

        let mut ignored: Vec<IgnoredTitle> = ignored_titles::Entity::find()
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|m| IgnoredTitle {
                plugin_key: m.plugin_key,
                account_id: m.account_id,
                product_id: m.product_id,
                reason: m.reason,
                created_at: m.created_at,
            })
            .collect();
        ignored.sort_by(|a, b| {
            (&a.plugin_key, &a.account_id, &a.product_id).cmp(&(
                &b.plugin_key,
                &b.account_id,
                &b.product_id,
            ))
        });

        let mut listening_links: Vec<ListeningLink> = self
            .list_listening_progress(None)
            .await?
            .into_iter()
            .filter_map(|row| {
                let book = by_uuid.get(row.book_uuid.as_deref()?)?;
                Some(ListeningLink {
                    provider: row.provider,
                    external_user_id: row.external_user_id,
                    external_item_id: row.external_item_id,
                    listing: ListingKey::of(book),
                })
            })
            .collect();
        listening_links.sort_by(|a, b| {
            (&a.provider, &a.external_user_id, &a.external_item_id).cmp(&(
                &b.provider,
                &b.external_user_id,
                &b.external_item_id,
            ))
        });

        let mut request_links: Vec<RequestLink> = self
            .list_title_requests(None)
            .await?
            .into_iter()
            .filter_map(|req| {
                let book = by_uuid.get(req.resolved_book_uuid.as_deref()?)?;
                Some(RequestLink {
                    request_uuid: req.uuid,
                    listing: ListingKey::of(book),
                })
            })
            .collect();
        request_links.sort_by(|a, b| a.request_uuid.cmp(&b.request_uuid));

        Ok(TitleStateExport {
            format: TITLE_STATE_FORMAT.to_string(),
            version: TITLE_STATE_VERSION,
            exported_at,
            titles,
            ignored,
            listening_links,
            request_links,
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::models::RequestStatus;
    use crate::store::{NewBook, NewListeningProgress, NewTitleRequest, UserBookFields};

    /// A fresh in-memory store with one Audible account.
    async fn store() -> LibraryStore {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        store
            .upsert_account("audible", "acct", "us", Some("Main"), true, "audible")
            .await
            .unwrap();
        store
    }

    /// A listening-progress row for `item` pointing at `book_uuid`.
    fn progress(item: &str, book_uuid: Option<String>) -> NewListeningProgress {
        NewListeningProgress {
            identity_id: None,
            provider: "audiobookshelf".into(),
            external_user_id: "u1".into(),
            book_uuid,
            work_id: None,
            external_item_id: item.into(),
            title: None,
            authors: None,
            asin: None,
            isbn: None,
            progress: Some(0.5),
            current_time_seconds: None,
            duration_seconds: None,
            is_finished: false,
            last_listened_at: None,
        }
    }

    /// A title request resolved to `book_uuid`.
    fn request(uuid: &str, book_uuid: Option<String>) -> NewTitleRequest {
        NewTitleRequest {
            uuid: Some(uuid.into()),
            identity_id: None,
            title: "Wanted".into(),
            authors: None,
            asin: None,
            isbn: None,
            notes: None,
            status: RequestStatus::Open,
            work_key: String::new(),
            work_id: None,
            resolved_book_uuid: book_uuid,
            cover_url: None,
        }
    }

    #[tokio::test]
    async fn export_keeps_only_titles_with_state_keyed_by_listing() {
        let store = store().await;
        store
            .upsert_book(&NewBook::minimal("B0PLAIN", "acct", "us", "Plain"))
            .await
            .unwrap();
        store
            .upsert_book(&NewBook::minimal("B0EMPTY", "acct", "us", "Empty tags"))
            .await
            .unwrap();
        let mut tagged = NewBook::minimal("B0TAGGED", "acct", "us", "Tagged");
        tagged.authors = Some("Ann Author".into());
        let tagged = store.upsert_book(&tagged).await.unwrap();
        for (id, tags) in [
            ("B0PLAIN", "  "),
            ("B0EMPTY", "[]"),
            ("B0TAGGED", "[\"sci-fi\"]"),
        ] {
            store
                .update_user_fields(
                    id,
                    "acct",
                    &UserBookFields {
                        tags: Some(tags.into()),
                        ..UserBookFields::default()
                    },
                )
                .await
                .unwrap();
        }
        let finished = store
            .upsert_book(&NewBook::minimal("B0DONE", "acct", "us", "Done"))
            .await
            .unwrap();
        store
            .update_user_fields(
                "B0DONE",
                "acct",
                &UserBookFields {
                    is_finished: Some(true),
                    rating_story: Some(4.0),
                    ..UserBookFields::default()
                },
            )
            .await
            .unwrap();
        store
            .set_ignored("B0PLAIN", "acct", true, Some("duplicate"))
            .await
            .unwrap();
        store
            .upsert_listening_progress(&progress("item-1", Some(tagged.uuid.clone())))
            .await
            .unwrap();
        store
            .upsert_listening_progress(&progress("item-2", Some("gone".into())))
            .await
            .unwrap();
        store
            .upsert_listening_progress(&progress("item-3", None))
            .await
            .unwrap();
        store
            .create_title_request(&request("req-1", Some(finished.uuid.clone())))
            .await
            .unwrap();
        store
            .create_title_request(&request("req-2", None))
            .await
            .unwrap();

        let at = Utc.with_ymd_and_hms(2026, 11, 4, 3, 0, 0).unwrap();
        let doc = store.export_title_state(at).await.unwrap();

        assert_eq!(doc.format, TITLE_STATE_FORMAT);
        assert_eq!(doc.version, TITLE_STATE_VERSION);
        assert_eq!(doc.exported_at, at);
        let products: Vec<&str> = doc
            .titles
            .iter()
            .map(|t| t.listing.product_id.as_str())
            .collect();
        assert_eq!(products, ["B0DONE", "B0TAGGED"]);
        let done = &doc.titles[0];
        assert!(done.finished);
        assert_eq!(done.rating_story, Some(4.0));
        assert_eq!(done.tags, None);
        assert_eq!(done.listing.plugin_key, "audible");
        assert_eq!(done.listing.marketplace, "us");
        let tagged_state = &doc.titles[1];
        assert_eq!(tagged_state.tags.as_deref(), Some("[\"sci-fi\"]"));
        assert_eq!(tagged_state.asin.as_deref(), Some("B0TAGGED"));
        assert_eq!(tagged_state.authors.as_deref(), Some("Ann Author"));
        assert!(!tagged_state.finished);

        assert_eq!(doc.ignored.len(), 1);
        assert_eq!(doc.ignored[0].product_id, "B0PLAIN");
        assert_eq!(doc.ignored[0].reason.as_deref(), Some("duplicate"));

        assert_eq!(doc.listening_links.len(), 1);
        assert_eq!(doc.listening_links[0].external_item_id, "item-1");
        assert_eq!(doc.listening_links[0].listing.product_id, "B0TAGGED");

        assert_eq!(doc.request_links.len(), 1);
        assert_eq!(doc.request_links[0].request_uuid, "req-1");
        assert_eq!(doc.request_links[0].listing.product_id, "B0DONE");

        let json = serde_json::to_string(&doc).unwrap();
        let back: TitleStateExport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, doc);
    }

    #[tokio::test]
    async fn export_of_an_empty_library_is_an_empty_document() {
        let store = store().await;
        let at = Utc.with_ymd_and_hms(2026, 11, 4, 3, 0, 0).unwrap();
        let doc = store.export_title_state(at).await.unwrap();
        assert!(doc.titles.is_empty());
        assert!(doc.ignored.is_empty());
        assert!(doc.listening_links.is_empty());
        assert!(doc.request_links.is_empty());
    }
}
