//! Bounded `books` reads that replace whole-library loads.
//!
//! Each reader here answers one question with an indexed lookup or a keyset
//! page, so no caller has to read every row (with its description and other
//! wide columns) into memory to find a few. A whole-library read fails the
//! adapters' streamed result cap once a library holds a few thousand
//! enriched titles; these reads stay within it at any size.

use sea_orm::sea_query::{Expr, ExprTrait, Func};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder};

use super::{map_book, LibraryStore};
use crate::entities::books;
use crate::error::{LibraryError, Result};
use crate::models::BookRecord;

/// The spellings of `value` an exact, indexed comparison tries: as given,
/// ASCII-lowercased and ASCII-uppercased, without duplicates.
///
/// Store ids are compared case-insensitively by the callers. Stored ids are
/// uniformly cased in practice (ASINs upper, uuids lower, ISBN digits), so
/// these three spellings find a row through its index where `lower(col)`
/// would scan the table; callers still check the match case-insensitively.
pub(super) fn case_variants(value: &str) -> Vec<String> {
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
    /// The first read compares each id column through its index (see
    /// `case_variants` for how case is handled), so it touches the matching
    /// rows only. When that finds nothing, a second read compares
    /// `lower(column)` so a stored id in mixed case (a slug such as
    /// `The-Dresden-Files`) still matches; that read may scan the table on
    /// the database side but still returns only the matching rows. The
    /// result is the set the acquire job used to find by filtering the
    /// whole library in memory.
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
        let exact = Condition::any()
            .add(books::Column::Uuid.is_in(variants.clone()))
            .add(books::Column::ProductId.is_in(variants.clone()))
            .add(books::Column::Isbn.is_in(variants.clone()))
            .add(books::Column::Asin.is_in(variants));
        let found = self.books_matching_id(account_id, id, exact).await?;
        if !found.is_empty() {
            return Ok(found);
        }
        let lowered = id.to_ascii_lowercase();
        let folded = |column: books::Column| {
            ExprTrait::eq(Expr::expr(Func::lower(Expr::col(column))), lowered.clone())
        };
        let folded = Condition::any()
            .add(folded(books::Column::Uuid))
            .add(folded(books::Column::ProductId))
            .add(folded(books::Column::Isbn))
            .add(folded(books::Column::Asin));
        self.books_matching_id(account_id, id, folded).await
    }

    /// Books matching `condition` on `account_id` (every account when
    /// `None`), in title order, kept only when one id column equals `id`
    /// ignoring ASCII case (the database's own case folding can be wider).
    async fn books_matching_id(
        &self,
        account_id: Option<&str>,
        id: &str,
        condition: Condition,
    ) -> Result<Vec<BookRecord>> {
        let mut query = books::Entity::find()
            .filter(condition)
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
}

#[cfg(test)]
mod tests {
    use super::case_variants;
    use crate::models::BookRecord;
    use crate::store::{LibraryStore, NewBook};
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
            let slug = book(&store, "a1", "The-Dresden-Files", None, None, "Slug").await;
            let found = store
                .find_books_by_any_id(None, "the-dresden-FILES")
                .await
                .unwrap();
            assert_eq!(uuids(&found), vec![slug.uuid.clone()], "{name}: mixed case");
            let found = store.find_books_by_any_id(None, "missing").await.unwrap();
            assert!(found.is_empty(), "{name}");
        }
    }
}
