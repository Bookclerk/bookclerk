//! SQL page for `GET /api/library/books`.
//!
//! Filtering, case-insensitive title order, and `LIMIT`/`OFFSET` run in the
//! database. The handler hydrates only the page. `total` is `COUNT(*)` of the
//! same `WHERE`, not the length of a fully loaded catalog.

use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, DbRow, DbValue, ExecuteRequest, TypedDbStatement,
};

use super::map_book;
use crate::entities::books;
use crate::error::{LibraryError, Result};
use crate::models::BookRecord;
use crate::store::LibraryStore;

/// Default page size for [`LibraryStore::list_books_filtered_page`].
pub const BOOK_PAGE_DEFAULT_LIMIT: u64 = 40;

/// Upper clamp for a library book page. Matches the HTTP handler.
pub const BOOK_PAGE_MAX_LIMIT: u64 = 500;

/// One page of books plus the unpaged match count.
#[derive(Debug, Clone)]
pub struct BookPage {
    /// Rows after `ORDER BY` and `LIMIT`/`OFFSET`.
    pub books: Vec<BookRecord>,
    /// Rows matching the filter before pagination.
    pub total: usize,
}

/// Placeholder counter for canonical `?` binds.
struct Binds {
    /// Next zero-based placeholder index. Canonical SQL always uses `?`.
    next: usize,
}

impl Binds {
    /// Allocates the next `?` placeholder.
    fn next(&mut self) -> String {
        self.next += 1;
        "?".to_string()
    }
}

impl LibraryStore {
    /// Pages books by account, acquire status, and title.
    ///
    /// `status` is the wire string from [`crate::AcquireStatus::as_str`].
    /// `limit` is clamped to 1..=500. `total` uses the same `WHERE` as the page.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Other`] when the read fails.
    pub async fn list_books_filtered_page(
        &self,
        account_id: Option<&str>,
        status: Option<&str>,
        limit: u64,
        offset: u64,
    ) -> Result<BookPage> {
        self.query_book_page(&[], account_id, status, limit, offset)
            .await
    }

    /// Pages books whose uuid is in `uuids`, then applies account and status.
    ///
    /// `uuids` is capped at [`BOOK_PAGE_MAX_LIMIT`] (the search-hit cap).
    /// Sort and `LIMIT`/`OFFSET` run in SQL, so a `limit` of 8 does not
    /// hydrate the rest of the hit list. An empty uuid list is an empty page.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Other`] when the read fails.
    pub async fn list_books_by_uuid_page(
        &self,
        uuids: &[String],
        account_id: Option<&str>,
        status: Option<&str>,
        limit: u64,
        offset: u64,
    ) -> Result<BookPage> {
        if uuids.is_empty() {
            return Ok(BookPage {
                books: Vec::new(),
                total: 0,
            });
        }
        let capped = uuids
            .len()
            .min(usize::try_from(BOOK_PAGE_MAX_LIMIT).unwrap_or(500));
        self.query_book_page(&uuids[..capped], account_id, status, limit, offset)
            .await
    }

    /// Runs the page `SELECT` and the matching `COUNT(*)`.
    async fn query_book_page(
        &self,
        uuids: &[String],
        account_id: Option<&str>,
        status: Option<&str>,
        limit: u64,
        offset: u64,
    ) -> Result<BookPage> {
        let limit = limit.clamp(1, BOOK_PAGE_MAX_LIMIT);
        let (page_sql, count_sql) = page_statements(
            !uuids.is_empty(),
            uuids.len(),
            account_id.is_some(),
            status.is_some(),
        );
        let filters = filter_values(uuids, account_id, status);
        let mut page_values = filters.clone();
        page_values.push(DbValue::Int64(i64::try_from(limit).unwrap_or(i64::MAX)));
        page_values.push(DbValue::Int64(i64::try_from(offset).unwrap_or(i64::MAX)));
        let cap = u32::try_from(limit).unwrap_or(u32::try_from(BOOK_PAGE_MAX_LIMIT).unwrap_or(500));
        let reply = self
            .execute_host_batch_limited(
                ExecuteRequest {
                    operation_id: format!("books-page-{}", uuid::Uuid::new_v4()),
                    request_hash: String::new(),
                    deadline_unix_ms: 0,
                    statements: vec![
                        select_stmt(&count_sql, filters, 1),
                        select_stmt(&page_sql, page_values, cap),
                    ],
                },
                u32::try_from(BOOK_PAGE_MAX_LIMIT).unwrap_or(500),
            )
            .await?;
        let total = count_from_reply(&reply)?;
        let books = reply
            .statements
            .get(1)
            .map(|stmt| stmt.rows.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(book_from_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(BookPage { books, total })
    }
}

/// One canonical `SELECT`. `max_rows` is the proven upper bound.
fn select_stmt(sql: &str, parameters: Vec<DbValue>, max_rows: u32) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.to_string(),
        parameters,
        kind: DbPlanStatementKind::Select,
        max_rows,
        result_selection: DbResultSelection::Rows,
    }
}

/// `SELECT` and `COUNT` for one filter shape. Placeholders follow
/// uuids, account, status, then limit and offset on the page statement only.
pub(crate) fn page_statements(
    has_uuids: bool,
    uuid_count: usize,
    has_account: bool,
    has_status: bool,
) -> (String, String) {
    let uuid_count = if has_uuids { uuid_count } else { 0 };
    let mut count_binds = Binds { next: 0 };
    let count_where = where_sql(&mut count_binds, uuid_count, has_account, has_status);
    let count_sql = format!("SELECT COUNT(*) FROM books{count_where}");

    let mut page_binds = Binds { next: 0 };
    let page_where = where_sql(&mut page_binds, uuid_count, has_account, has_status);
    let limit = page_binds.next();
    let offset = page_binds.next();
    let page_sql = format!(
        "SELECT * FROM books{page_where} ORDER BY title COLLATE NOCASE, uuid LIMIT {limit} OFFSET {offset}"
    );
    (page_sql, count_sql)
}

/// `WHERE` fragment, including the leading space, or empty when unrestricted.
fn where_sql(binds: &mut Binds, uuid_count: usize, has_account: bool, has_status: bool) -> String {
    let mut parts = Vec::new();
    if uuid_count > 0 {
        let marks = (0..uuid_count)
            .map(|_| binds.next())
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("uuid IN ({marks})"));
    }
    if has_account {
        let mark = binds.next();
        parts.push(format!("account_id = {mark}"));
    }
    if has_status {
        let mark = binds.next();
        parts.push(format!("acquire_status = {mark}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", parts.join(" AND "))
    }
}

/// Bind values for the filter prefix (uuids, then account, then status).
fn filter_values(uuids: &[String], account_id: Option<&str>, status: Option<&str>) -> Vec<DbValue> {
    let mut values = Vec::with_capacity(uuids.len() + 2);
    for uuid in uuids {
        values.push(DbValue::Text(uuid.clone()));
    }
    if let Some(account_id) = account_id {
        values.push(DbValue::Text(account_id.to_string()));
    }
    if let Some(status) = status {
        values.push(DbValue::Text(status.to_string()));
    }
    values
}

/// `COUNT(*)` from the first statement of a page batch.
fn count_from_reply(reply: &bookclerk_plugin_abi::ExecuteReply) -> Result<usize> {
    let row = reply
        .statements
        .first()
        .and_then(|stmt| stmt.rows.first())
        .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("books page count returned no row")))?;
    let count = match row.values.first() {
        Some(DbValue::Int64(value)) => *value,
        Some(other) => {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "books page count was {other:?}"
            )))
        }
        None => 0,
    };
    Ok(usize::try_from(count.max(0)).unwrap_or(0))
}

/// Maps one `SELECT *` row in catalog column order onto a [`BookRecord`].
fn book_from_row(row: &DbRow) -> Result<BookRecord> {
    let mut cells = Cells { row, index: 0 };
    map_book(books::Model {
        id: cells.int()?,
        uuid: cells.text()?,
        source: cells.text()?,
        account_id: cells.text()?,
        product_id: cells.text()?,
        asin: cells.text_opt()?,
        isbn: cells.text_opt()?,
        marketplace: cells.text()?,
        title: cells.text()?,
        authors: cells.text_opt()?,
        narrators: cells.text_opt()?,
        series: cells.text_opt()?,
        series_index: cells.text_opt()?,
        series_asin: cells.text_opt()?,
        acquire_status: cells.text()?,
        storage_key: cells.text_opt()?,
        error_message: cells.text_opt()?,
        purchased_at: cells.text_opt()?,
        tags: cells.text_opt()?,
        rating_overall: cells.float_opt()?,
        rating_performance: cells.float_opt()?,
        rating_story: cells.float_opt()?,
        is_finished: cells.int()?,
        pdf_status: cells.text()?,
        pdf_storage_key: cells.text_opt()?,
        publisher: cells.text_opt()?,
        length_minutes: cells.int_opt()?,
        is_abridged: cells.int()?,
        content_kind: cells.text()?,
        categories: cells.text_opt()?,
        subtitle: cells.text_opt()?,
        published_at: cells.text_opt()?,
        description: cells.text_opt()?,
        language: cells.text_opt()?,
        cover_url: cells.text_opt()?,
        subjects: cells.text_opt()?,
        enrich_source: cells.text_opt()?,
        enrich_confidence: cells.float_opt()?,
        enrich_updated_at: cells.text_opt()?,
        created_at: cells.text()?,
        updated_at: cells.text()?,
    })
}

/// Cursor over one positional result row.
struct Cells<'a> {
    /// Source row.
    row: &'a DbRow,
    /// Next cell index.
    index: usize,
}

impl Cells<'_> {
    /// Next cell, or an error when the row is short.
    fn next(&mut self) -> Result<&DbValue> {
        let cell = self.row.values.get(self.index).ok_or_else(|| {
            LibraryError::Other(anyhow::anyhow!(
                "books page row missing column {}",
                self.index
            ))
        })?;
        self.index += 1;
        Ok(cell)
    }

    /// Required text cell.
    fn text(&mut self) -> Result<String> {
        let index = self.index;
        match self.next()? {
            DbValue::Text(value) => Ok(value.clone()),
            other => Err(LibraryError::Other(anyhow::anyhow!(
                "books page column {index} was {other:?}"
            ))),
        }
    }

    /// Optional text cell. Null stays empty.
    fn text_opt(&mut self) -> Result<Option<String>> {
        let index = self.index;
        match self.next()? {
            DbValue::Null(_) => Ok(None),
            DbValue::Text(value) => Ok(Some(value.clone())),
            other => Err(LibraryError::Other(anyhow::anyhow!(
                "books page column {index} was {other:?}"
            ))),
        }
    }

    /// Required integer cell.
    fn int(&mut self) -> Result<i64> {
        let index = self.index;
        match self.next()? {
            DbValue::Int64(value) => Ok(*value),
            other => Err(LibraryError::Other(anyhow::anyhow!(
                "books page column {index} was {other:?}"
            ))),
        }
    }

    /// Optional integer cell.
    fn int_opt(&mut self) -> Result<Option<i64>> {
        let index = self.index;
        match self.next()? {
            DbValue::Null(_) => Ok(None),
            DbValue::Int64(value) => Ok(Some(*value)),
            other => Err(LibraryError::Other(anyhow::anyhow!(
                "books page column {index} was {other:?}"
            ))),
        }
    }

    /// Optional real cell.
    fn float_opt(&mut self) -> Result<Option<f64>> {
        let index = self.index;
        match self.next()? {
            DbValue::Null(_) => Ok(None),
            DbValue::Float64(value) => Ok(Some(*value)),
            other => Err(LibraryError::Other(anyhow::anyhow!(
                "books page column {index} was {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Statement, Value};

    use super::*;
    use crate::models::AcquireStatus;
    use crate::store::NewBook;

    async fn memory_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite"),
        )
    }

    fn envelope_book(i: u32) -> (NewBook, AcquireStatus) {
        let account = if i < 8_000 {
            "envelope-a"
        } else {
            "envelope-b"
        };
        let status = match i % 10 {
            0 => AcquireStatus::Error,
            1 => AcquireStatus::NotAcquired,
            _ => AcquireStatus::Acquired,
        };
        let book = NewBook::minimal(format!("B{i:05}"), account, "us", format!("Title {i:05}"));
        (book, status)
    }

    #[tokio::test]
    async fn filtered_page_sorts_counts_and_does_not_return_the_catalog() {
        let store = memory_store().await;
        store
            .upsert_account("envelope-a", "us", None, false, "audible")
            .await
            .unwrap();
        store
            .upsert_account("envelope-b", "us", None, false, "audible")
            .await
            .unwrap();
        for i in 0..30 {
            let (mut book, status) = envelope_book(i);
            if i >= 24 {
                book.account_id = "envelope-b".to_string();
            }
            let saved = store.upsert_book(&book).await.unwrap();
            store
                .set_acquire_status(&saved.uuid, &saved.account_id, status, None, None)
                .await
                .unwrap();
            let again = store.upsert_book(&book).await.unwrap();
            assert_eq!(again.uuid, saved.uuid);
            assert_eq!(again.acquire_status, status);
        }

        let page = store
            .list_books_filtered_page(None, None, 40, 0)
            .await
            .unwrap();
        assert_eq!(page.total, 30);
        assert_eq!(page.books.len(), 30);
        assert_eq!(page.books[0].title, "Title 00000");
        assert_eq!(page.books[29].title, "Title 00029");

        let acquired = store
            .list_books_filtered_page(None, Some(AcquireStatus::Acquired.as_str()), 2, 1)
            .await
            .unwrap();
        let expected_acquired = (0..30).filter(|i| i % 10 >= 2).count();
        assert_eq!(acquired.total, expected_acquired);
        assert_eq!(acquired.books.len(), 2);
        assert!(acquired
            .books
            .iter()
            .all(|book| book.acquire_status == AcquireStatus::Acquired));

        let account_b = store
            .list_books_filtered_page(Some("envelope-b"), None, 2, 0)
            .await
            .unwrap();
        assert_eq!(account_b.total, 6);
        assert_eq!(account_b.books.len(), 2);
        assert!(account_b
            .books
            .iter()
            .all(|book| book.account_id == "envelope-b"));

        let uuids: Vec<String> = store
            .list_books(None)
            .await
            .unwrap()
            .into_iter()
            .map(|book| book.uuid)
            .collect();
        let narrow = store
            .list_books_by_uuid_page(&uuids, None, Some("acquired"), 8, 0)
            .await
            .unwrap();
        assert_eq!(narrow.total, expected_acquired);
        assert_eq!(narrow.books.len(), 8);
    }

    #[tokio::test]
    async fn ten_thousand_page_plan_uses_the_nocase_index() {
        let store = memory_store().await;
        store
            .upsert_account("envelope-a", "us", None, false, "audible")
            .await
            .unwrap();
        store
            .upsert_account("envelope-b", "us", None, false, "audible")
            .await
            .unwrap();
        seed_envelope_books(&store, 10_000).await;
        let backend = store.db.get_database_backend();
        ConnectionTrait::execute_raw(
            &store.db,
            Statement::from_string(backend, "ANALYZE".to_string()),
        )
        .await
        .unwrap();

        for (account, status, label) in [
            (None, None, "unfiltered"),
            (None, Some("acquired"), "status"),
            (Some("envelope-b"), None, "account"),
            (Some("envelope-b"), Some("acquired"), "account+status"),
        ] {
            let (page_sql, _) = page_statements(false, 0, account.is_some(), status.is_some());
            let mut values = Vec::new();
            if let Some(account) = account {
                values.push(Value::String(Some(account.to_string())));
            }
            if let Some(status) = status {
                values.push(Value::String(Some(status.to_string())));
            }
            values.push(Value::BigInt(Some(40)));
            values.push(Value::BigInt(Some(0)));
            let plan = explain(&store, &page_sql, values).await;
            assert!(
                plan.to_ascii_uppercase().contains("INDEX"),
                "{label} plan does not use an index:\n{plan}\n{page_sql}"
            );
            assert!(
                !plan.to_ascii_uppercase().contains("USE TEMP B-TREE"),
                "{label} plan sorts the matching rows:\n{plan}\n{page_sql}"
            );
        }

        let page = store
            .list_books_filtered_page(None, Some("acquired"), 40, 8_000)
            .await
            .unwrap();
        assert_eq!(
            page.books.len(),
            0,
            "offset 8000 is past 8000 acquired rows"
        );
        assert_eq!(page.total, 8_000);
        let first = store
            .list_books_filtered_page(Some("envelope-b"), None, 40, 0)
            .await
            .unwrap();
        assert_eq!(first.total, 2_000);
        assert_eq!(first.books.len(), 40);
        assert!(first
            .books
            .iter()
            .all(|book| book.account_id == "envelope-b"));
    }

    /// Inserts `count` envelope rows with one multi-value statement per chunk.
    async fn seed_envelope_books(store: &LibraryStore, count: u32) {
        let backend = store.db.get_database_backend();
        let chunk = 100u32;
        let mut start = 0u32;
        while start < count {
            let end = (start + chunk).min(count);
            let mut sql = String::from(
                "INSERT INTO books (
                    uuid, source, account_id, product_id, asin, marketplace, title,
                    acquire_status, pdf_status, is_finished, is_abridged, content_kind,
                    created_at, updated_at
                ) VALUES ",
            );
            let mut values = Vec::new();
            for i in start..end {
                if i != start {
                    sql.push_str(", ");
                }
                sql.push_str("(?, 'audible', ?, ?, ?, 'us', ?, ?, 'not_acquired', 0, 0, 'book', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')");
                let account = if i < 8_000 {
                    "envelope-a"
                } else {
                    "envelope-b"
                };
                let status = match i % 10 {
                    0 => "error",
                    1 => "not_acquired",
                    _ => "acquired",
                };
                values.push(Value::String(Some(format!("uuid-{i:05}"))));
                values.push(Value::String(Some(account.to_string())));
                values.push(Value::String(Some(format!("B{i:05}"))));
                values.push(Value::String(Some(format!("B{i:05}"))));
                values.push(Value::String(Some(format!("Title {i:05}"))));
                values.push(Value::String(Some(status.to_string())));
            }
            ConnectionTrait::execute_raw(
                &store.db,
                Statement::from_sql_and_values(backend, sql, values),
            )
            .await
            .unwrap_or_else(|err| panic!("seed {start}..{end}: {err}"));
            start = end;
        }
    }

    /// Joins `EXPLAIN QUERY PLAN` detail text.
    async fn explain(store: &LibraryStore, sql: &str, values: Vec<Value>) -> String {
        let backend = store.db.get_database_backend();
        let stmt =
            Statement::from_sql_and_values(backend, format!("EXPLAIN QUERY PLAN {sql}"), values);
        let rows = ConnectionTrait::query_all_raw(&store.db, stmt)
            .await
            .expect("explain");
        let mut lines = Vec::new();
        for row in rows {
            for index in 0..8 {
                if let Ok(text) = row.try_get_by_index::<String>(index) {
                    lines.push(text);
                }
            }
        }
        lines.join("\n")
    }
}
