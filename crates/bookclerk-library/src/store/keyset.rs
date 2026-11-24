//! Keyset paging of host reads under the database plugin's result limits.
//!
//! A read that can return more rows than one reply holds (`maxResultRows`,
//! `maxResultBytes`) is read in pages. Pages follow a unique key rather
//! than an `OFFSET`: rows read are never re-counted, so a row deleted
//! between two page reads cannot shift a later row past the next page's
//! start, and a row inserted ahead of the cursor cannot repeat one already
//! read.
//!
//! Composite keys are compared as an `OR` of prefix matches, because
//! row-value comparisons (`(a, b) > (?, ?)`) are outside the canonical
//! SQL-v1 contract (`docs/plugins/sql-contract/v1.md`).

use bookclerk_plugin_abi::LimitExceeded;
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, ModelTrait, QueryFilter, QueryOrder, QuerySelect, Select,
    Value,
};

use crate::column_read::SelectColumnExt;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Which way a keyset read walks its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyOrder {
    /// Smallest key first.
    Ascending,
    /// Largest key first.
    Descending,
}

/// The rows ordered after `last` by `key`: the lexicographic
/// `(key[0], key[1], …) > (last[0], last[1], …)`, written as
/// `key[0] > last[0] OR (key[0] = last[0] AND key[1] > last[1]) OR …`.
///
/// # Arguments
///
/// * `key` - The key columns, most significant first. Every column must be
///   `NOT NULL`, since a `NULL` compares as neither greater nor equal.
/// * `last` - The key values of the last row read, one per column of `key`.
///
/// # Returns
///
/// A condition binding `n (n + 1) / 2` values for an `n`-column key.
pub(crate) fn after_key<C: ColumnTrait>(key: &[C], last: &[Value]) -> Condition {
    past_key(key, last, KeyOrder::Ascending)
}

/// The rows a walk of `key` in `order` reaches after `last`: those
/// ordered after it ([`after_key`]) when ascending, before it when
/// descending.
///
/// # Arguments
///
/// * `key` - The key columns, most significant first, all `NOT NULL`.
/// * `last` - The key values of the last row read, one per column of `key`.
/// * `order` - Which way the walk runs.
///
/// # Returns
///
/// A condition binding `n (n + 1) / 2` values for an `n`-column key.
pub(crate) fn past_key<C: ColumnTrait>(key: &[C], last: &[Value], order: KeyOrder) -> Condition {
    let mut any = Condition::any();
    let mut prefix = Condition::all();
    for (column, value) in key.iter().zip(last) {
        let past = match order {
            KeyOrder::Ascending => column.gt(value.clone()),
            KeyOrder::Descending => column.lt(value.clone()),
        };
        any = any.add(prefix.clone().add(past));
        prefix = prefix.add(column.eq(value.clone()));
    }
    any
}

/// Every row `select` reads, in keyset pages of at most `*page` rows
/// ordered by `key`.
///
/// `select` must not carry an order of its own: the pages are ordered by
/// `key` alone. A page the plugin refuses as over a negotiated limit (a
/// reply too large for `maxResultRows` or `maxResultBytes`) is read again
/// at half the rows, and `*page` keeps the smaller size, so a caller
/// reading several selects in turn starts each at the size that last
/// fitted.
///
/// # Arguments
///
/// * `store` - The store to read.
/// * `select` - The rows to read, filtered but not ordered or limited.
/// * `key` - Columns that are unique together among the rows `select`
///   reads and all `NOT NULL`, most significant first.
/// * `page` - Rows per read, at least 1; halved on a refused reply.
///
/// # Returns
///
/// The rows in `key` order.
///
/// # Errors
///
/// Returns the read error classified by [`LibraryError::from_db_err`]
/// ([`LibraryError::LimitExceeded`] when a single row is still over a
/// limit, [`LibraryError::Unavailable`] while the database is locked).
pub(crate) async fn read_keyset_pages<E>(
    store: &LibraryStore,
    select: Select<E>,
    key: &[E::Column],
    page: &mut u64,
) -> Result<Vec<E::Model>>
where
    E: EntityTrait,
{
    read_keyset_walk(store, select, key, KeyOrder::Ascending, page, u64::MAX).await
}

/// The first `limit` rows `select` reads in `key` `order`, in keyset
/// pages of at most one result page of `store` ([`result_page`]), the way
/// `ORDER BY key LIMIT limit` reads them in one reply where the plugin
/// allows it.
///
/// # Arguments
///
/// * `store` - The store to read.
/// * `select` - The rows to read, filtered but not ordered or limited.
/// * `key` - Columns that are unique together among the rows `select`
///   reads and all `NOT NULL`, most significant first.
/// * `order` - Which way to walk `key`.
/// * `limit` - Most rows to return.
///
/// # Errors
///
/// Returns the read error, as [`read_keyset_pages`] does.
pub(crate) async fn read_keyset_limit<E>(
    store: &LibraryStore,
    select: Select<E>,
    key: &[E::Column],
    order: KeyOrder,
    limit: u64,
) -> Result<Vec<E::Model>>
where
    E: EntityTrait,
{
    let mut page = result_page(store);
    read_keyset_walk(store, select, key, order, &mut page, limit).await
}

/// The walk behind [`read_keyset_pages`] and [`read_keyset_limit`]: at
/// most `limit` rows of `select` in `key` `order`, in pages of at most
/// `*page` rows, halved on a refused reply.
///
/// # Errors
///
/// Returns the read error, as [`read_keyset_pages`] does.
async fn read_keyset_walk<E>(
    store: &LibraryStore,
    select: Select<E>,
    key: &[E::Column],
    order: KeyOrder,
    page: &mut u64,
    limit: u64,
) -> Result<Vec<E::Model>>
where
    E: EntityTrait,
{
    let mut ordered = select;
    for column in key {
        ordered = match order {
            KeyOrder::Ascending => ordered.order_by_asc(*column),
            KeyOrder::Descending => ordered.order_by_desc(*column),
        };
    }
    *page = (*page).max(1);
    let mut out = Vec::new();
    let mut last: Option<Vec<Value>> = None;
    loop {
        let left = limit.saturating_sub(u64::try_from(out.len()).unwrap_or(u64::MAX));
        if left == 0 {
            return Ok(out);
        }
        let take = (*page).min(left);
        let mut query = ordered.clone();
        if let Some(last) = &last {
            query = query.filter(past_key(key, last, order));
        }
        match query.limit(take).all(store.db()).await {
            Ok(rows) => {
                let full = u64::try_from(rows.len()).unwrap_or(u64::MAX) >= take;
                last = rows
                    .last()
                    .map(|row| key.iter().map(|column| row.get(*column)).collect());
                out.extend(rows);
                if !full || last.is_none() {
                    return Ok(out);
                }
            }
            Err(err) if take > 1 && LimitExceeded::from_message(&err.to_string()).is_some() => {
                *page = take / 2;
            }
            Err(err) => return Err(LibraryError::from_db_err(err)),
        }
    }
}

/// [`read_keyset_pages`] in pages of at most one result page of `store`
/// ([`result_page`]).
///
/// # Errors
///
/// Returns the read error, as [`read_keyset_pages`] does.
pub(crate) async fn read_keyset<E>(
    store: &LibraryStore,
    select: Select<E>,
    key: &[E::Column],
) -> Result<Vec<E::Model>>
where
    E: EntityTrait,
{
    let mut page = result_page(store);
    read_keyset_pages(store, select, key, &mut page).await
}

/// The `column` values of every row `select` reads, in ascending order,
/// read as a projection of that column alone in keyset pages of at most
/// one result page of `store` ([`result_page`]); a page refused as over
/// a negotiated limit is read again at half the rows.
///
/// # Arguments
///
/// * `store` - The store to read.
/// * `select` - The rows to read, filtered but not ordered or limited.
/// * `column` - A text column unique and `NOT NULL` among those rows.
///
/// # Errors
///
/// Returns the read error, as [`read_keyset_pages`] does.
pub(crate) async fn read_keyset_column<E>(
    store: &LibraryStore,
    select: Select<E>,
    column: E::Column,
) -> Result<Vec<String>>
where
    E: EntityTrait,
{
    let mut page = result_page(store);
    let mut out: Vec<String> = Vec::new();
    loop {
        let mut query = select.clone();
        if let Some(last) = out.last() {
            query = query.filter(column.gt(last.clone()));
        }
        match query
            .order_by_asc(column)
            .limit(page)
            .into_column::<String, _>(column)
            .all(store.db())
            .await
        {
            Ok(values) => {
                let full = u64::try_from(values.len()).unwrap_or(u64::MAX) >= page;
                out.extend(values);
                if !full {
                    return Ok(out);
                }
            }
            Err(err) if page > 1 && LimitExceeded::from_message(&err.to_string()).is_some() => {
                page /= 2;
            }
            Err(err) => return Err(LibraryError::from_db_err(err)),
        }
    }
}

/// Most rows one keyset page reads, whatever the plugin allows.
const MAX_PAGE: u64 = 256;

/// Rows per keyset page on `store`: its database plugin's `maxResultRows`,
/// at most [`MAX_PAGE`] (and that when it sets none).
pub(crate) fn result_page(store: &LibraryStore) -> u64 {
    match store.db_capabilities().max_result_rows {
        0 => MAX_PAGE,
        rows => u64::from(rows).clamp(1, MAX_PAGE),
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait, ModelTrait};

    use super::*;
    use crate::entities::plugin_kv;
    use crate::test_support::{sqlite, Profile};

    /// The composite key the tests page by.
    const KEY: [plugin_kv::Column; 3] = [
        plugin_kv::Column::PluginKey,
        plugin_kv::Column::Namespace,
        plugin_kv::Column::Key,
    ];

    /// Inserts one `plugin_kv` row keyed `(plugin, namespace, key)`.
    async fn put(store: &LibraryStore, plugin: &str, namespace: &str, key: &str) {
        put_value(store, plugin, namespace, key, "").await;
    }

    /// Inserts one `plugin_kv` row keyed `(plugin, namespace, key)` holding
    /// `value`.
    async fn put_value(
        store: &LibraryStore,
        plugin: &str,
        namespace: &str,
        key: &str,
        value: &str,
    ) {
        plugin_kv::ActiveModel {
            plugin_key: Set(plugin.into()),
            namespace: Set(namespace.into()),
            key: Set(key.into()),
            value: Set(value.into()),
            metadata: Set(String::new()),
            expires_at: Set(String::new()),
            updated_at: Set(String::new()),
        }
        .insert(store.db())
        .await
        .unwrap();
    }

    /// Rows keyed so every column of the composite key decides some order.
    async fn seeded(profile: Profile) -> LibraryStore {
        let store = sqlite::memory(profile).await.into_store();
        for plugin in ["a", "b"] {
            for namespace in ["x", "y"] {
                for key in ["1", "2", "3"] {
                    put(&store, plugin, namespace, key).await;
                }
            }
        }
        store
    }

    /// The key of `row` as a tuple, for comparisons.
    fn key_of(row: &plugin_kv::Model) -> (String, String, String) {
        (
            row.plugin_key.clone(),
            row.namespace.clone(),
            row.key.clone(),
        )
    }

    /// Keyset pages of any size read every row once, in key order, under
    /// every database profile.
    #[tokio::test]
    async fn pages_read_every_row_once_in_key_order() {
        for profile in Profile::ALL {
            let store = seeded(profile).await;
            let mut full: Vec<_> =
                crate::test_support::uncapped(plugin_kv::Entity::find().all(store.db()))
                    .await
                    .unwrap()
                    .iter()
                    .map(key_of)
                    .collect();
            full.sort();
            for size in [1, 2, 5, 12, 100] {
                let mut page = size;
                let rows = read_keyset_pages(&store, plugin_kv::Entity::find(), &KEY, &mut page)
                    .await
                    .unwrap();
                let got: Vec<_> = rows.iter().map(key_of).collect();
                assert_eq!(got, full, "{}: pages of {size}", profile.name());
            }
            let filtered = read_keyset(
                &store,
                plugin_kv::Entity::find().filter(plugin_kv::Column::Namespace.eq("y")),
                &KEY,
            )
            .await
            .unwrap();
            assert_eq!(filtered.len(), 6, "{}", profile.name());
        }
    }

    /// The next page starts after the last row read even when that row
    /// was deleted since: no unread row is skipped, where an `OFFSET`
    /// would now skip one.
    #[tokio::test]
    async fn a_deleted_row_does_not_shift_the_next_page() {
        let store = seeded(Profile::Minimum).await;
        let mut page = 4;
        let first = crate::test_support::uncapped(
            plugin_kv::Entity::find()
                .order_by_asc(KEY[0])
                .order_by_asc(KEY[1])
                .order_by_asc(KEY[2])
                .limit(page)
                .all(store.db()),
        )
        .await
        .unwrap();
        let last = first.last().unwrap().clone();
        let values: Vec<Value> = KEY.iter().map(|column| last.get(*column)).collect();
        for row in &first {
            row.clone().delete(store.db()).await.unwrap();
        }
        let rest = read_keyset_pages(
            &store,
            plugin_kv::Entity::find().filter(after_key(&KEY, &values)),
            &KEY,
            &mut page,
        )
        .await
        .unwrap();
        let got: Vec<_> = rest.iter().map(key_of).collect();
        let mut expected: Vec<_> =
            crate::test_support::uncapped(plugin_kv::Entity::find().all(store.db()))
                .await
                .unwrap()
                .iter()
                .map(key_of)
                .collect();
        expected.sort();
        assert_eq!(got.len(), 8);
        assert_eq!(got, expected);
    }

    /// A page the plugin refuses as over its reply limit is read again at
    /// half the rows until it fits, and the smaller size is kept; a single
    /// row still over the limit is an error.
    #[tokio::test]
    async fn a_refused_page_is_read_again_at_half_the_rows() {
        let _lock = crate::master_key::master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let (store, caps) = crate::control_plane::capped_store(dir.path()).await;
        let value = "v".repeat(100);
        for key in ["1", "2", "3", "4", "5"] {
            put_value(&store, "a", "x", key, &value).await;
        }
        // Stored before the cap applies: the insert reads its row back.
        put_value(&store, "b", "x", "1", &"w".repeat(400)).await;
        caps.reply.store(250, std::sync::atomic::Ordering::SeqCst);
        let mut page = 12;
        let narrow = plugin_kv::Entity::find().filter(plugin_kv::Column::PluginKey.eq("a"));
        let rows = read_keyset_pages(&store, narrow, &KEY, &mut page)
            .await
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(page, 1, "12, 6 and 3 rows are over the cap");

        let err = read_keyset_pages(&store, plugin_kv::Entity::find(), &KEY, &mut page)
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::LimitExceeded(_)), "{err}");
    }

    /// A limited walk returns the first rows of `key` in either order, in
    /// pages of the profile's result rows, and a column walk the key
    /// column alone, under every profile.
    #[tokio::test]
    async fn limited_and_column_walks_read_the_first_rows_in_order() {
        for profile in Profile::ALL {
            let store = seeded(profile).await;
            let mut full: Vec<_> =
                crate::test_support::uncapped(plugin_kv::Entity::find().all(store.db()))
                    .await
                    .unwrap()
                    .iter()
                    .map(key_of)
                    .collect();
            full.sort();
            for limit in [0, 1, 5, 12, 20] {
                let take = usize::try_from(limit).unwrap().min(full.len());
                let up = read_keyset_limit(
                    &store,
                    plugin_kv::Entity::find(),
                    &KEY,
                    KeyOrder::Ascending,
                    limit,
                )
                .await
                .unwrap();
                let got: Vec<_> = up.iter().map(key_of).collect();
                assert_eq!(got, full[..take], "{}: first {limit}", profile.name());
                let down = read_keyset_limit(
                    &store,
                    plugin_kv::Entity::find(),
                    &KEY,
                    KeyOrder::Descending,
                    limit,
                )
                .await
                .unwrap();
                let got: Vec<_> = down.iter().map(key_of).collect();
                let last: Vec<_> = full.iter().rev().take(take).cloned().collect();
                assert_eq!(got, last, "{}: last {limit}", profile.name());
            }
            let keys = read_keyset_column(
                &store,
                plugin_kv::Entity::find()
                    .filter(plugin_kv::Column::PluginKey.eq("a"))
                    .filter(plugin_kv::Column::Namespace.eq("x")),
                plugin_kv::Column::Key,
            )
            .await
            .unwrap();
            assert_eq!(keys, ["1", "2", "3"], "{}", profile.name());
        }
    }

    /// A column walk refused for its reply size reads again at half the
    /// rows; a single value still over the limit is an error.
    #[tokio::test]
    async fn a_refused_column_page_is_read_again_at_half_the_rows() {
        let _lock = crate::master_key::master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let (store, caps) = crate::control_plane::capped_store(dir.path()).await;
        let keys: Vec<String> = ["1", "2", "3"]
            .map(|n| format!("{n}{}", "k".repeat(99)))
            .into();
        for key in &keys {
            put(&store, "a", "x", key).await;
        }
        put(&store, "b", "x", &"w".repeat(400)).await;
        caps.reply.store(250, std::sync::atomic::Ordering::SeqCst);
        let read = read_keyset_column(
            &store,
            plugin_kv::Entity::find().filter(plugin_kv::Column::PluginKey.eq("a")),
            plugin_kv::Column::Key,
        )
        .await
        .unwrap();
        assert_eq!(read, keys);
        let err = read_keyset_column(
            &store,
            plugin_kv::Entity::find().filter(plugin_kv::Column::PluginKey.eq("b")),
            plugin_kv::Column::Key,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LibraryError::LimitExceeded(_)), "{err}");
    }
}
