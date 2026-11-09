//! Bounded `books` reads that replace whole-library loads.
//!
//! Each reader here answers one question with an indexed lookup or a keyset
//! page, so no caller has to read every row (with its description and other
//! wide columns) into memory to find a few. A whole-library read fails the
//! adapters' streamed result cap once a library holds a few thousand
//! enriched titles; these reads stay within it at any size.

use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, QuerySelect,
};

use super::{map_book, parse_dt, LibraryStore};
use crate::entities::books;
use crate::error::{LibraryError, Result};
use crate::models::{AcquireStatus, BookRecord};

/// Upper clamp for one [`LibraryStore::acquired_book_keys_page`] read.
///
/// A key row is five short columns (well under 1 KiB on the wire), so this
/// many stay far below the 256 KiB inline result cap of an adapter that
/// does not stream.
pub const ACQUIRED_KEYS_MAX_PAGE: u64 = 256;

/// The identity of one acquired book with a stored file, without its
/// metadata.
///
/// Read in keyset pages by [`LibraryStore::acquired_book_keys_page`]; the
/// full row is loaded only when it is needed, by
/// [`LibraryStore::get_book_by_uuid`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquiredBookKey {
    /// Surrogate row id; the keyset cursor of the page reader.
    pub id: i64,
    /// Public book id.
    pub uuid: String,
    /// Store alias the book came from (`audible`, `libro`, ...), as stored.
    pub source: String,
    /// Store product id.
    pub product_id: String,
    /// Last change of the row. An unparsable stored value reads as the
    /// time of the read, the same fallback [`BookRecord`] uses.
    pub updated_at: DateTime<Utc>,
}

/// One narrow `books` row as read; columns are matched by name.
///
/// Positional tuples are not used: the database proxy hands rows over as
/// maps ordered by column name, so a tuple would bind columns out of order.
#[derive(Debug, FromQueryResult)]
struct KeyRow {
    /// Surrogate row id.
    id: i64,
    /// Public book id.
    uuid: String,
    /// Store alias.
    source: String,
    /// Store product id.
    product_id: String,
    /// RFC 3339 last change, as stored.
    updated_at: String,
}

/// The spellings of `value` an exact, indexed comparison tries: as given,
/// ASCII-lowercased and ASCII-uppercased, without duplicates.
///
/// Store ids are compared case-insensitively by the callers. Stored ids are
/// uniformly cased in practice (ASINs upper, uuids lower, ISBN digits), so
/// these three spellings find a row through its index where `lower(col)`
/// would scan the table; callers still check the match case-insensitively.
fn case_variants(value: &str) -> Vec<String> {
    let mut out = vec![value.to_string()];
    for variant in [value.to_ascii_lowercase(), value.to_ascii_uppercase()] {
        if !out.contains(&variant) {
            out.push(variant);
        }
    }
    out
}

impl LibraryStore {
    /// Books whose uuid, product id, ISBN or ASIN equals `id`, ignoring
    /// ASCII case, optionally on one account, in title order.
    ///
    /// Every comparison uses that column's index (see `case_variants` for
    /// how case is handled), so the read touches the matching rows only.
    /// The result is the same set the acquire job used to find by
    /// filtering the whole library in memory, except a stored id cased
    /// differently from all three spellings tried.
    ///
    /// # Arguments
    ///
    /// * `account_id` - Store account to search, or every account when `None`.
    /// * `id` - A book uuid, store product id, ISBN or ASIN.
    ///
    /// # Returns
    ///
    /// The matching books ordered by title; empty when none match.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn find_books_by_any_id(
        &self,
        account_id: Option<&str>,
        id: &str,
    ) -> Result<Vec<BookRecord>> {
        let variants = case_variants(id);
        let any = Condition::any()
            .add(books::Column::Uuid.is_in(variants.clone()))
            .add(books::Column::ProductId.is_in(variants.clone()))
            .add(books::Column::Isbn.is_in(variants.clone()))
            .add(books::Column::Asin.is_in(variants));
        let mut query = books::Entity::find()
            .filter(any)
            .order_by_asc(books::Column::Title)
            .order_by_asc(books::Column::Id);
        if let Some(account_id) = account_id {
            query = query.filter(books::Column::AccountId.eq(account_id));
        }
        let rows = query.all(&self.db).await.map_err(LibraryError::Orm)?;
        let wanted = |value: Option<&str>| value.is_some_and(|v| v.eq_ignore_ascii_case(id));
        rows.into_iter()
            .filter(|m| {
                wanted(Some(&m.uuid))
                    || wanted(Some(&m.product_id))
                    || wanted(m.isbn.as_deref())
                    || wanted(m.asin.as_deref())
            })
            .map(map_book)
            .collect()
    }

    /// The first book, in title order, whose product id or ASIN is exactly
    /// `id`, or else whose primary storage key is exactly `storage_key`.
    ///
    /// The id comparison uses the `product_id` and `asin` indexes; the
    /// storage-key comparison reads at most one row.
    ///
    /// # Arguments
    ///
    /// * `id` - A store product id or ASIN.
    /// * `storage_key` - The primary file's storage key, matched only when
    ///   no id matches; an empty key matches nothing.
    ///
    /// # Returns
    ///
    /// The book, or `None` when neither matches.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn find_book_by_store_id_or_key(
        &self,
        id: &str,
        storage_key: &str,
    ) -> Result<Option<BookRecord>> {
        let by_id = books::Entity::find()
            .filter(
                Condition::any()
                    .add(books::Column::ProductId.eq(id))
                    .add(books::Column::Asin.eq(id)),
            )
            .order_by_asc(books::Column::Title)
            .order_by_asc(books::Column::Id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(model) = by_id {
            return map_book(model).map(Some);
        }
        if storage_key.is_empty() {
            return Ok(None);
        }
        books::Entity::find()
            .filter(books::Column::StorageKey.eq(storage_key))
            .order_by_asc(books::Column::Title)
            .order_by_asc(books::Column::Id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_book)
            .transpose()
    }

    /// One keyset page of the acquired books that have a stored file,
    /// identity columns only, by surrogate id.
    ///
    /// # Arguments
    ///
    /// * `after_id` - The last [`AcquiredBookKey::id`] of the previous page,
    ///   or `None` for the first page.
    /// * `limit` - Rows wanted, clamped to 1..=[`ACQUIRED_KEYS_MAX_PAGE`].
    ///
    /// # Returns
    ///
    /// Up to `limit` keys in ascending id order; fewer than `limit` means
    /// the last page.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn acquired_book_keys_page(
        &self,
        after_id: Option<i64>,
        limit: u64,
    ) -> Result<Vec<AcquiredBookKey>> {
        let mut query = books::Entity::find()
            .select_only()
            .column(books::Column::Id)
            .column(books::Column::Uuid)
            .column(books::Column::Source)
            .column(books::Column::ProductId)
            .column(books::Column::UpdatedAt)
            .filter(books::Column::AcquireStatus.eq(AcquireStatus::Acquired.as_str()))
            .filter(books::Column::StorageKey.is_not_null())
            .order_by_asc(books::Column::Id);
        if let Some(after_id) = after_id {
            query = query.filter(books::Column::Id.gt(after_id));
        }
        let rows = query
            .limit(limit.clamp(1, ACQUIRED_KEYS_MAX_PAGE))
            .into_model::<KeyRow>()
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .map(|row| AcquiredBookKey {
                id: row.id,
                uuid: row.uuid,
                source: row.source,
                product_id: row.product_id,
                updated_at: parse_dt(&row.updated_at),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::case_variants;
    use crate::models::{AcquireStatus, BookRecord};
    use crate::store::{LibraryStore, NewBook};
    use crate::test_support::paging;
    use crate::test_support::Profile;

    /// An empty store that behaves like one on an external plugin
    /// advertising `profile`.
    async fn kit(profile: Profile) -> LibraryStore {
        crate::test_support::sqlite::memory(profile)
            .await
            .into_store()
    }

    /// Writes one book on `account` and returns it.
    async fn book(
        store: &LibraryStore,
        account: &str,
        product: &str,
        asin: Option<&str>,
        isbn: Option<&str>,
        title: &str,
    ) -> BookRecord {
        store
            .upsert_account("audible", account, "us", Some(account), true, "audible")
            .await
            .unwrap();
        let mut new = NewBook::minimal(product, account, "us", title);
        new.asin = asin.map(str::to_string);
        new.isbn = isbn.map(str::to_string);
        store.upsert_book(&new).await.unwrap()
    }

    /// Uuids of `books`, in order.
    fn uuids(books: &[BookRecord]) -> Vec<String> {
        books.iter().map(|b| b.uuid.clone()).collect()
    }

    #[test]
    fn case_variants_keep_the_given_spelling_first_and_drop_duplicates() {
        assert_eq!(case_variants("b0ABc"), vec!["b0ABc", "b0abc", "B0ABC"]);
        assert_eq!(case_variants("B0ABC"), vec!["B0ABC", "b0abc"]);
        assert_eq!(case_variants("9781"), vec!["9781"]);
    }

    /// Any id column matches, ASCII case aside, in title order, and the
    /// account filter narrows the match.
    #[tokio::test]
    async fn any_id_lookup_matches_every_id_column_ignoring_case() {
        for profile in Profile::ALL {
            let store = kit(profile).await;
            let name = profile.name();
            let zed = book(&store, "a1", "P-1", Some("B0AAA"), Some("978111"), "Zed").await;
            let abe = book(&store, "a2", "p-2", Some("B0AAA"), None, "Abe").await;
            let other = book(&store, "a1", "P-3", Some("B0CCC"), Some("978333"), "Mid").await;

            let found = store.find_books_by_any_id(None, "b0aaa").await.unwrap();
            assert_eq!(
                uuids(&found),
                vec![abe.uuid.clone(), zed.uuid.clone()],
                "{name}"
            );
            let found = store
                .find_books_by_any_id(Some("a1"), "B0AAA")
                .await
                .unwrap();
            assert_eq!(uuids(&found), vec![zed.uuid.clone()], "{name}");
            let found = store.find_books_by_any_id(None, "P-2").await.unwrap();
            assert_eq!(uuids(&found), vec![abe.uuid.clone()], "{name}");
            let found = store.find_books_by_any_id(None, "978333").await.unwrap();
            assert_eq!(uuids(&found), vec![other.uuid.clone()], "{name}");
            let upper = other.uuid.to_ascii_uppercase();
            let found = store.find_books_by_any_id(None, &upper).await.unwrap();
            assert_eq!(uuids(&found), vec![other.uuid.clone()], "{name}");
            let found = store.find_books_by_any_id(None, "missing").await.unwrap();
            assert!(found.is_empty(), "{name}");
        }
    }

    /// Product id or ASIN wins over the storage key; the key is a fallback
    /// and an empty key matches nothing.
    #[tokio::test]
    async fn store_id_or_key_lookup_prefers_ids_then_the_stored_file() {
        for profile in Profile::ALL {
            let store = kit(profile).await;
            let name = profile.name();
            let first = book(&store, "a1", "P-1", Some("B0AAA"), None, "First").await;
            let second = book(&store, "a1", "P-2", None, None, "Second").await;
            store
                .set_acquire_status(
                    "P-2",
                    "a1",
                    AcquireStatus::Acquired,
                    Some("x/P-1.m4b"),
                    None,
                )
                .await
                .unwrap();

            let found = store.find_book_by_store_id_or_key("P-1", "x/P-1.m4b").await;
            assert_eq!(
                found.unwrap().map(|b| b.uuid),
                Some(first.uuid.clone()),
                "{name}"
            );
            let found = store.find_book_by_store_id_or_key("B0AAA", "").await;
            assert_eq!(
                found.unwrap().map(|b| b.uuid),
                Some(first.uuid.clone()),
                "{name}"
            );
            let found = store
                .find_book_by_store_id_or_key("nope", "x/P-1.m4b")
                .await;
            assert_eq!(
                found.unwrap().map(|b| b.uuid),
                Some(second.uuid.clone()),
                "{name}"
            );
            let found = store.find_book_by_store_id_or_key("nope", "").await;
            assert!(found.unwrap().is_none(), "{name}");
            let found = store
                .find_book_by_store_id_or_key("nope", "y/none.m4b")
                .await;
            assert!(found.unwrap().is_none(), "{name}");
        }
    }

    /// The keyset walk yields exactly the acquired books with a stored
    /// file, in id order, at every page size.
    #[tokio::test]
    async fn acquired_key_pages_match_the_filtered_full_listing() {
        for profile in Profile::ALL {
            let store = kit(profile).await;
            for i in 0..7 {
                let product = format!("P-{i}");
                book(&store, "a1", &product, None, None, &format!("T{i}")).await;
                let (status, key) = match i % 3 {
                    0 => (AcquireStatus::Acquired, Some(format!("k/{product}.m4b"))),
                    1 => (AcquireStatus::Acquired, None),
                    _ => (AcquireStatus::NotAcquired, None),
                };
                store
                    .set_acquire_status(&product, "a1", status, key.as_deref(), None)
                    .await
                    .unwrap();
            }
            let mut expected: Vec<BookRecord> = store
                .list_books(None)
                .await
                .unwrap()
                .into_iter()
                .filter(|b| b.acquire_status == AcquireStatus::Acquired && b.storage_key.is_some())
                .collect();
            expected.sort_by_key(|b| b.id);
            assert_eq!(expected.len(), 3, "{}", profile.name());
            let full: Vec<(String, String, String)> = expected
                .iter()
                .map(|b| (b.uuid.clone(), b.source.clone(), b.product_id.clone()))
                .collect();
            for size in [1, 2, 100] {
                paging::assert_pages_match_full(&full, size, |after: Option<i64>, size| {
                    let store = &store;
                    async move {
                        let page = store.acquired_book_keys_page(after, size).await.unwrap();
                        let next = (u64::try_from(page.len()).unwrap() >= size)
                            .then(|| page.last().map(|k| k.id))
                            .flatten();
                        let rows = page
                            .into_iter()
                            .map(|k| (k.uuid, k.source, k.product_id))
                            .collect();
                        (rows, next)
                    }
                })
                .await;
            }
            let first = store.acquired_book_keys_page(None, 1).await.unwrap();
            assert_eq!(
                first[0].updated_at,
                expected[0].updated_at,
                "{}",
                profile.name()
            );
        }
    }
}
