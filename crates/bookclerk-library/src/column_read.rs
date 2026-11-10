//! Column reads that stay correct on the host's proxied store connection.
//!
//! The host reaches every database plugin through SeaORM's proxy connection,
//! and SeaORM's `ProxyRow` keeps a row as a `BTreeMap<String, Value>`. A
//! positional read therefore sees the columns in alphabetical order by name,
//! not in `SELECT` order: `select_only().column(b).column(a).into_tuple()`
//! fills the tuple with `a` first (#854). Every adapter returns its columns in
//! order; the order is lost when the host builds the proxy row.
//!
//! `crates/bookclerk-library/clippy.toml` disallows the positional readers
//! (`Select::into_tuple`, `QueryResult::try_get_by_index`,
//! `QueryResult::try_get_many_by_index` and their trait forms), so host code
//! reads by name instead:
//!
//! - several columns: a `#[derive(FromQueryResult)]` struct read with
//!   `into_model::<T>()`, which looks each field up by its column name;
//! - one column of an entity query: [`SelectColumnExt::into_column`];
//! - the only column of a raw statement's row: [`only_column`].

use sea_orm::{
    ColumnTrait, DbErr, EntityTrait, QueryResult, QuerySelect, Select, SelectGetableTuple,
    Selector, TryGetable,
};

/// Reads one column of an entity query without a positional tuple.
pub trait SelectColumnExt {
    /// Selects only `column` and reads it into `T`.
    ///
    /// Any columns selected earlier are replaced, so the result always has
    /// exactly one column and its position cannot be mistaken. Filters,
    /// ordering, `DISTINCT` and limits applied before the call are kept.
    ///
    /// `T` is a single value ([`TryGetable`]), never a tuple: a read of
    /// several columns is a `FromQueryResult` struct instead.
    ///
    /// # Arguments
    ///
    /// * `column` - The one column to read.
    ///
    /// # Returns
    ///
    /// A selector whose items are the column's values.
    fn into_column<T, C>(self, column: C) -> Selector<SelectGetableTuple<T>>
    where
        T: TryGetable,
        C: ColumnTrait;
}

impl<E> SelectColumnExt for Select<E>
where
    E: EntityTrait,
{
    fn into_column<T, C>(self, column: C) -> Selector<SelectGetableTuple<T>>
    where
        T: TryGetable,
        C: ColumnTrait,
    {
        // `select_only` drops every earlier column, so the row has exactly
        // one column and index 0 is that column whatever its name.
        #[allow(clippy::disallowed_methods)]
        self.select_only().column(column).into_tuple()
    }
}

/// Reads the only column of a raw statement's row.
///
/// The value is looked up by the column's own name, so the read does not
/// depend on column order.
///
/// # Arguments
///
/// * `row` - A row of a statement that selects exactly one column, such as
///   `SELECT COUNT(*) FROM …`.
///
/// # Returns
///
/// The column's value.
///
/// # Errors
///
/// Returns [`DbErr::Type`] when the row does not have exactly one column
/// (read several columns by name instead), or the decode error when the value
/// is not a `T`.
pub fn only_column<T>(row: &QueryResult) -> Result<T, DbErr>
where
    T: TryGetable,
{
    // A proxy row is a map: read its one entry without cloning the row,
    // which `column_names` does.
    if let Some(proxy) = row.try_as_proxy_row() {
        let mut cells = proxy.values.keys();
        return match (cells.next(), cells.next()) {
            (Some(name), None) => row.try_get("", name),
            _ => Err(wrong_width(
                &proxy.values.keys().cloned().collect::<Vec<_>>(),
            )),
        };
    }
    let names = row.column_names();
    match names.as_slice() {
        [name] => row.try_get("", name),
        _ => Err(wrong_width(&names)),
    }
}

/// The error [`only_column`] returns for a row whose width is not one.
///
/// # Arguments
///
/// * `names` - The row's column names, in the order the row reports them.
fn wrong_width(names: &[String]) -> DbErr {
    DbErr::Type(format!(
        "only_column needs a row with one column, got {} ({}); read several columns by name",
        names.len(),
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    //! Reads through the proxy under every test profile (#854).

    use super::*;
    use crate::entities::books;
    use crate::test_support::{sqlite, Profile};
    use sea_orm::{ConnectionTrait, FromQueryResult, Statement};

    /// Two columns whose `SELECT` order is the reverse of their name order.
    #[derive(Debug, PartialEq, Eq, FromQueryResult)]
    struct UuidAndProductId {
        /// `books.uuid`, selected first.
        uuid: String,
        /// `books.product_id`, selected second and sorted first by name.
        product_id: String,
    }

    /// Seeds one book whose `uuid` and `product_id` cannot be confused.
    async fn seeded(profile: Profile) -> crate::test_support::ProfileStore {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        store
            .upsert_account("audible", "acct", "us", None, false, "audible")
            .await
            .unwrap();
        let mut book = crate::NewBook::minimal("the-product", "acct", "us", "Title");
        book.uuid = Some("the-uuid".into());
        store.upsert_book(&book).await.unwrap();
        kit
    }

    /// The hazard itself: a positional read of the proxy row sees the
    /// columns sorted by name. If SeaORM ever keeps `SELECT` order this
    /// fails, and the lint can be revisited.
    #[tokio::test]
    async fn the_proxy_row_is_ordered_by_name_not_by_select_order() {
        for profile in Profile::ALL {
            let kit = seeded(profile).await;
            let row = kit
                .connection()
                .query_one_raw(Statement::from_string(
                    kit.connection().get_database_backend(),
                    "SELECT uuid, product_id FROM books",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                row.column_names(),
                ["product_id", "uuid"],
                "{}",
                profile.name()
            );
            #[allow(clippy::disallowed_methods)]
            let first: String = row.try_get_by_index(0).unwrap();
            assert_eq!(first, "the-product", "{}", profile.name());
        }
    }

    /// Reading several columns by name returns each in its own field.
    #[tokio::test]
    async fn a_from_query_result_struct_reads_out_of_order_columns_by_name() {
        for profile in Profile::ALL {
            let kit = seeded(profile).await;
            let rows = books::Entity::find()
                .select_only()
                .column(books::Column::Uuid)
                .column(books::Column::ProductId)
                .into_model::<UuidAndProductId>()
                .all(kit.connection())
                .await
                .unwrap();
            assert_eq!(
                rows,
                [UuidAndProductId {
                    uuid: "the-uuid".into(),
                    product_id: "the-product".into(),
                }],
                "{}",
                profile.name()
            );
        }
    }

    /// `into_column` reads the named column even when another column was
    /// selected first and sorts before it.
    #[tokio::test]
    async fn into_column_replaces_earlier_columns() {
        for profile in Profile::ALL {
            let kit = seeded(profile).await;
            let uuids: Vec<String> = books::Entity::find()
                .select_only()
                .column(books::Column::ProductId)
                .into_column(books::Column::Uuid)
                .all(kit.connection())
                .await
                .unwrap();
            assert_eq!(uuids, ["the-uuid"], "{}", profile.name());
        }
    }

    /// `only_column` reads a one-column row and refuses a wider one.
    #[tokio::test]
    async fn only_column_refuses_a_row_with_several_columns() {
        for profile in Profile::ALL {
            let kit = seeded(profile).await;
            let backend = kit.connection().get_database_backend();
            let one = kit
                .connection()
                .query_one_raw(Statement::from_string(
                    backend,
                    "SELECT COUNT(*) FROM books",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(only_column::<i64>(&one).unwrap(), 1, "{}", profile.name());
            let two = kit
                .connection()
                .query_one_raw(Statement::from_string(
                    backend,
                    "SELECT uuid, product_id FROM books",
                ))
                .await
                .unwrap()
                .unwrap();
            let err = only_column::<String>(&two).unwrap_err();
            assert!(
                err.to_string().contains("product_id, uuid"),
                "{}: {err}",
                profile.name()
            );
        }
    }
}
