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

/// Rows per guest `books` result, and uuids per search-hit `IN` batch.
///
/// A sparse `books` row is about 1.2 KiB on the Cap'n wire, so 64 sparse rows
/// stay under the sqlite guest `maxResultBytes` (256 KiB). Enriched rows do
/// not. Callers start at this width and halve when a result would exceed the
/// cap. Search hydration uses the same width for narrow key reads.
const BOOK_PAGE_CHUNK: usize = 64;

/// `books` columns in [`book_from_row`] order.
///
/// Named so a later column added at the end of the table cannot shift this
/// positional map. The list matches the catalog row the handler returns.
const BOOK_PAGE_COLUMNS: &str = "id, uuid, source, account_id, product_id, asin, isbn, \
marketplace, title, authors, narrators, series, series_index, series_asin, acquire_status, \
storage_key, error_message, purchased_at, tags, rating_overall, rating_performance, \
rating_story, is_finished, pdf_status, pdf_storage_key, publisher, length_minutes, \
is_abridged, content_kind, categories, subtitle, published_at, description, language, \
cover_url, subjects, enrich_source, enrich_confidence, enrich_updated_at, created_at, \
updated_at";

/// How a uuid list is matched.
#[derive(Clone, Copy)]
enum UuidMatch {
    /// No uuid predicate.
    None,
    /// `lower(uuid) IN (lower(?), …)` for search hits.
    Folded,
    /// `uuid IN (?, …)` for one already chosen stored id.
    Exact,
}

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
    /// The page is read in pieces of at most 64 rows so a requested limit of
    /// 256 stays under the guest result-byte cap. A piece is halved when that
    /// result would exceed the cap, and later pieces keep that smaller width.
    /// `limit`, `offset`, and `total` are the caller's page, not one of those
    /// pieces. `total` is counted once.
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
        let limit = limit.clamp(1, BOOK_PAGE_MAX_LIMIT);
        let total = self.count_book_matches(&[], account_id, status).await?;
        let books = self
            .read_book_rows_adaptive(&[], UuidMatch::None, account_id, status, limit, offset)
            .await?;
        Ok(BookPage { books, total })
    }

    /// Pages books whose uuid is in `uuids`, then applies account and status.
    ///
    /// `uuids` is capped at [`BOOK_PAGE_MAX_LIMIT`] (the search-hit cap).
    /// Lists longer than 64 uuids are ordered from narrow `uuid, title` reads,
    /// then only the requested page is loaded. Shorter lists sort and page in
    /// SQL, in pieces that shrink when a result would exceed the guest byte
    /// cap. An empty uuid list is an empty page. Two stored uuids that differ
    /// only by ASCII case are different books and both hydrate.
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
        let uuids = &uuids[..capped];
        let limit = limit.clamp(1, BOOK_PAGE_MAX_LIMIT);
        if uuids.len() <= BOOK_PAGE_CHUNK {
            let total = self.count_book_matches(uuids, account_id, status).await?;
            let books = self
                .read_book_rows_adaptive(
                    uuids,
                    UuidMatch::Folded,
                    account_id,
                    status,
                    limit,
                    offset,
                )
                .await?;
            return Ok(BookPage { books, total });
        }
        let mut keys = self.collect_book_keys(uuids, account_id, status).await?;
        keys.sort_by(nocase_key_order);
        let total = keys.len();
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(keys.len());
        let width = usize::try_from(limit).unwrap_or(keys.len());
        let end = start.saturating_add(width).min(keys.len());
        let books = self
            .hydrate_book_keys(&keys[start..end], account_id, status)
            .await?;
        Ok(BookPage { books, total })
    }

    /// `COUNT(*)` for one filter shape.
    async fn count_book_matches(
        &self,
        uuids: &[String],
        account_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<usize> {
        let (_, count_sql) = page_statements(
            !uuids.is_empty(),
            uuids.len(),
            account_id.is_some(),
            status.is_some(),
        );
        let filters = filter_values(uuids, true, account_id, status);
        let reply = self
            .execute_reads(vec![select_stmt(&count_sql, filters, 1)])
            .await?;
        count_from_reply(&reply)
    }

    /// Reads `[offset, offset+limit)` in pieces, halving a piece that exceeds
    /// the guest byte cap.
    async fn read_book_rows_adaptive(
        &self,
        uuids: &[String],
        uuid_match: UuidMatch,
        account_id: Option<&str>,
        status: Option<&str>,
        limit: u64,
        offset: u64,
    ) -> Result<Vec<BookRecord>> {
        let chunk = u64::try_from(BOOK_PAGE_CHUNK).unwrap_or(64);
        let mut books = Vec::new();
        let mut remaining = limit;
        let mut next_offset = offset;
        let mut width = chunk;
        while remaining > 0 {
            let mut take = remaining.min(width);
            let rows = loop {
                match self
                    .query_book_rows(uuids, uuid_match, account_id, status, take, next_offset)
                    .await
                {
                    Ok(rows) => break rows,
                    Err(err) if is_result_too_large(&err) && take > 1 => {
                        take /= 2;
                        width = take;
                    }
                    Err(err) => return Err(err),
                }
            };
            let n = u64::try_from(rows.len()).unwrap_or(0);
            books.extend(rows);
            if n < take {
                break;
            }
            remaining -= n;
            next_offset = next_offset.saturating_add(n);
        }
        Ok(books)
    }

    /// One page `SELECT` (no count).
    async fn query_book_rows(
        &self,
        uuids: &[String],
        uuid_match: UuidMatch,
        account_id: Option<&str>,
        status: Option<&str>,
        limit: u64,
        offset: u64,
    ) -> Result<Vec<BookRecord>> {
        let (page_sql, _) = statements_for(
            BOOK_PAGE_COLUMNS,
            uuid_match,
            uuids.len(),
            account_id.is_some(),
            status.is_some(),
        );
        let mut values = filter_values(uuids, true, account_id, status);
        values.push(DbValue::Int64(i64::try_from(limit).unwrap_or(i64::MAX)));
        values.push(DbValue::Int64(i64::try_from(offset).unwrap_or(i64::MAX)));
        let cap = u32::try_from(limit).unwrap_or(u32::try_from(BOOK_PAGE_MAX_LIMIT).unwrap_or(500));
        let reply = self
            .execute_reads(vec![select_stmt(&page_sql, values, cap)])
            .await?;
        rows_from_reply(&reply)
    }

    /// Narrow `uuid, title` rows for a long search-hit list.
    ///
    /// The same stored uuid is kept once when two hit chunks both match it.
    async fn collect_book_keys(
        &self,
        uuids: &[String],
        account_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<BookKey>> {
        let mut keys = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for chunk in uuids.chunks(BOOK_PAGE_CHUNK) {
            let sql = key_sql(chunk.len(), account_id.is_some(), status.is_some());
            let values = filter_values(chunk, true, account_id, status);
            let width = chunk.len().saturating_mul(4).clamp(1, 500);
            let cap = u32::try_from(width).unwrap_or(500);
            let reply = self
                .execute_reads(vec![select_stmt(&sql, values, cap)])
                .await?;
            for key in keys_from_reply(&reply)? {
                if seen.insert(key.uuid.clone()) {
                    keys.push(key);
                }
            }
        }
        Ok(keys)
    }

    /// Loads full rows for `keys`, shrinking the batch when a result is too big.
    ///
    /// SQL `ORDER BY` would reshuffle the merged key order, so the rows are
    /// put back into `keys` order after the read.
    async fn hydrate_book_keys(
        &self,
        keys: &[BookKey],
        account_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<BookRecord>> {
        let mut books = Vec::with_capacity(keys.len());
        let mut start = 0usize;
        let mut width = BOOK_PAGE_CHUNK;
        while start < keys.len() {
            let mut take = (keys.len() - start).min(width);
            let rows = loop {
                let slice = &keys[start..start + take];
                match self.query_exact_rows(slice, account_id, status).await {
                    Ok(rows) => break rows,
                    Err(err) if is_result_too_large(&err) && take > 1 => {
                        take /= 2;
                        width = take;
                    }
                    Err(err) => return Err(err),
                }
            };
            books.extend(rows);
            start += take;
        }
        Ok(order_like_keys(keys, books))
    }

    /// Full rows whose stored uuid is one of `keys`.
    async fn query_exact_rows(
        &self,
        keys: &[BookKey],
        account_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<BookRecord>> {
        let uuids = keys.iter().map(|key| key.uuid.clone()).collect::<Vec<_>>();
        let (page_sql, _) = statements_for(
            BOOK_PAGE_COLUMNS,
            UuidMatch::Exact,
            uuids.len(),
            account_id.is_some(),
            status.is_some(),
        );
        let limit = u64::try_from(uuids.len().max(1)).unwrap_or(1);
        let mut values = filter_values(&uuids, false, account_id, status);
        values.push(DbValue::Int64(i64::try_from(limit).unwrap_or(i64::MAX)));
        values.push(DbValue::Int64(0));
        let cap = u32::try_from(limit).unwrap_or(1);
        let reply = self
            .execute_reads(vec![select_stmt(&page_sql, values, cap)])
            .await?;
        rows_from_reply(&reply)
    }

    /// Runs `statements`, waiting out a lock held by a concurrent writer.
    ///
    /// A result that exceeds `maxResultBytes` is returned immediately so the
    /// caller can shrink the page.
    async fn execute_reads(
        &self,
        statements: Vec<TypedDbStatement>,
    ) -> Result<bookclerk_plugin_abi::ExecuteReply> {
        super::lock_retry::retry_read_lock(|| async {
            self.execute_host_batch_limited(
                ExecuteRequest {
                    operation_id: format!("books-page-{}", uuid::Uuid::new_v4()),
                    request_hash: String::new(),
                    deadline_unix_ms: 0,
                    statements: statements.clone(),
                },
                u32::try_from(BOOK_PAGE_MAX_LIMIT).unwrap_or(500),
            )
            .await
        })
        .await
    }
}

/// `uuid` and `title` used to order a long search-hit list before hydration.
struct BookKey {
    /// Stored uuid, including its original case.
    uuid: String,
    /// Stored title.
    title: String,
}

/// ASCII case-fold, then `uuid` code points. Matches `title COLLATE NOCASE, uuid`.
fn nocase_key_order(left: &BookKey, right: &BookKey) -> std::cmp::Ordering {
    left.title
        .to_ascii_lowercase()
        .cmp(&right.title.to_ascii_lowercase())
        .then_with(|| left.uuid.cmp(&right.uuid))
}

/// True when `err` is a guest result-byte cap, not a lock.
pub(crate) fn is_result_too_large(err: &LibraryError) -> bool {
    let upper = err.to_string().to_ascii_uppercase();
    upper.contains("MAXRESULTBYTES") || upper.contains("QUERY RESULT IS")
}

/// Puts `books` back into `keys` order.
fn order_like_keys(keys: &[BookKey], books: Vec<BookRecord>) -> Vec<BookRecord> {
    let mut by_uuid = std::collections::HashMap::new();
    for book in books {
        by_uuid.insert(book.uuid.clone(), book);
    }
    keys.iter()
        .filter_map(|key| by_uuid.remove(&key.uuid))
        .collect()
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
    let mode = if has_uuids {
        UuidMatch::Folded
    } else {
        UuidMatch::None
    };
    statements_for(
        BOOK_PAGE_COLUMNS,
        mode,
        if has_uuids { uuid_count } else { 0 },
        has_account,
        has_status,
    )
}

/// Page `SELECT` plus `COUNT(*)` for one column list and uuid match.
fn statements_for(
    columns: &str,
    uuid_match: UuidMatch,
    uuid_count: usize,
    has_account: bool,
    has_status: bool,
) -> (String, String) {
    let mut count_binds = Binds { next: 0 };
    let count_where = where_sql(
        &mut count_binds,
        uuid_count,
        uuid_match,
        has_account,
        has_status,
    );
    let count_sql = format!("SELECT COUNT(*) FROM books{count_where}");

    let mut page_binds = Binds { next: 0 };
    let page_where = where_sql(
        &mut page_binds,
        uuid_count,
        uuid_match,
        has_account,
        has_status,
    );
    let limit = page_binds.next();
    let offset = page_binds.next();
    // `COLLATE NOCASE` is the SQLite spelling of ASCII case-fold order.
    // Postgres lowering rewrites the fold to `lower(title COLLATE "C")`
    // and the `uuid` tie-break to `(uuid COLLATE "C")`.
    let page_sql = format!(
        "SELECT {columns} FROM books{page_where} ORDER BY title COLLATE NOCASE, uuid LIMIT {limit} OFFSET {offset}"
    );
    (page_sql, count_sql)
}

/// `SELECT uuid, title` for one search-hit chunk, capped so case-variant
/// matches of those hits still fit.
fn key_sql(uuid_count: usize, has_account: bool, has_status: bool) -> String {
    let mut binds = Binds { next: 0 };
    let page_where = where_sql(
        &mut binds,
        uuid_count,
        UuidMatch::Folded,
        has_account,
        has_status,
    );
    let width = uuid_count.saturating_mul(4).clamp(1, 500);
    format!("SELECT uuid, title FROM books{page_where} LIMIT {width}")
}

/// `WHERE` fragment, including the leading space, or empty when unrestricted.
fn where_sql(
    binds: &mut Binds,
    uuid_count: usize,
    uuid_match: UuidMatch,
    has_account: bool,
    has_status: bool,
) -> String {
    let mut parts = Vec::new();
    if uuid_count > 0 {
        match uuid_match {
            UuidMatch::None => {}
            // Search hits store a lowercased uuid. `lower` on both sides still
            // finds a row whose stored uuid keeps its original case (`u-Alpha`).
            // `idx_books_uuid_lower` serves this predicate. Distinct stored
            // uuids that fold together are different books and both match.
            UuidMatch::Folded => {
                let marks = (0..uuid_count)
                    .map(|_| format!("lower({})", binds.next()))
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!("lower(uuid) IN ({marks})"));
            }
            UuidMatch::Exact => {
                let marks = (0..uuid_count)
                    .map(|_| binds.next())
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!("uuid IN ({marks})"));
            }
        }
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
fn filter_values(
    uuids: &[String],
    fold_uuids: bool,
    account_id: Option<&str>,
    status: Option<&str>,
) -> Vec<DbValue> {
    let mut values = Vec::with_capacity(uuids.len() + 2);
    for uuid in uuids {
        let text = if fold_uuids {
            uuid.to_ascii_lowercase()
        } else {
            uuid.clone()
        };
        values.push(DbValue::Text(text));
    }
    if let Some(account_id) = account_id {
        values.push(DbValue::Text(account_id.to_string()));
    }
    if let Some(status) = status {
        values.push(DbValue::Text(status.to_string()));
    }
    values
}

/// Book rows from the first statement of a page read.
fn rows_from_reply(reply: &bookclerk_plugin_abi::ExecuteReply) -> Result<Vec<BookRecord>> {
    reply
        .statements
        .first()
        .map(|stmt| stmt.rows.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(book_from_row)
        .collect()
}

/// Key rows from a narrow `uuid, title` read.
fn keys_from_reply(reply: &bookclerk_plugin_abi::ExecuteReply) -> Result<Vec<BookKey>> {
    reply
        .statements
        .first()
        .map(|stmt| stmt.rows.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(key_from_row)
        .collect()
}

/// One `uuid, title` cell pair.
fn key_from_row(row: &DbRow) -> Result<BookKey> {
    let mut cells = Cells { row, index: 0 };
    Ok(BookKey {
        uuid: cells.text()?,
        title: cells.text()?,
    })
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

/// Maps one explicit `books` column list, in [`BOOK_PAGE_COLUMNS`] order.
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

    /// ASCII case-fold order, then `uuid`. Binary order would put `Gamma`
    /// before `beta` because `G` < `b`.
    async fn assert_mixed_case_pages(store: &LibraryStore) {
        store
            .upsert_account("case", "us", None, false, "audible")
            .await
            .unwrap();
        for (uuid, title) in [
            ("u-gamma", "Gamma"),
            ("u-beta", "beta"),
            ("u-BETA", "BETA"),
            ("u-alpha", "alpha"),
            ("u-Alpha", "Alpha"),
        ] {
            let mut book = NewBook::minimal(uuid, "case", "us", title);
            book.uuid = Some(uuid.to_string());
            store.upsert_book(&book).await.unwrap();
        }
        let titles = |page: BookPage| -> Vec<String> {
            page.books.into_iter().map(|book| book.title).collect()
        };
        let all = store
            .list_books_filtered_page(Some("case"), None, 10, 0)
            .await
            .unwrap();
        assert_eq!(all.total, 5, "unpaged match count");
        assert_eq!(
            titles(all),
            ["Alpha", "alpha", "BETA", "beta", "Gamma"],
            "case-fold order then uuid"
        );
        let boundary = store
            .list_books_filtered_page(Some("case"), None, 2, 2)
            .await
            .unwrap();
        assert_eq!(boundary.total, 5);
        assert_eq!(
            titles(boundary),
            ["BETA", "beta"],
            "offset 2 is the first row after the alpha group"
        );
        let last = store
            .list_books_filtered_page(Some("case"), None, 1, 4)
            .await
            .unwrap();
        assert_eq!(titles(last), ["Gamma"]);
        let hits = ["u-gamma", "u-beta", "u-BETA", "u-alpha", "u-Alpha"]
            .into_iter()
            .map(|uuid| uuid.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let hydrated = store
            .list_books_by_uuid_page(&hits, Some("case"), None, 10, 0)
            .await
            .unwrap();
        assert_eq!(hydrated.total, 5);
        assert_eq!(
            titles(hydrated),
            ["Alpha", "alpha", "BETA", "beta", "Gamma"]
        );
    }

    #[tokio::test]
    async fn mixed_case_titles_share_page_boundaries() {
        let store = memory_store().await;
        assert_mixed_case_pages(&store).await;
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
    async fn postgres_mixed_case_titles_share_page_boundaries() {
        let Some(store) = postgres_page_store().await else {
            return;
        };
        assert_mixed_case_pages(&store).await;
        let plan = postgres_page_plan(&store).await;
        assert!(
            plan.contains("idx_books_page"),
            "expression index was not used:\n{plan}"
        );
        assert!(
            !plan.contains("Sort"),
            "mixed-case page order sorted instead of using the expression index:\n{plan}"
        );
    }

    async fn postgres_page_store() -> Option<LibraryStore> {
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let Some(url) = url else {
            assert!(
                std::env::var("BOOKCLERK_REQUIRE_POSTGRES_TESTS")
                    .ok()
                    .as_deref()
                    != Some("1"),
                "BOOKCLERK_TEST_POSTGRES_URL is required when BOOKCLERK_REQUIRE_POSTGRES_TESTS=1"
            );
            return None;
        };
        let db_name = format!("page_{}", uuid::Uuid::new_v4().as_simple());
        let admin = sea_orm::Database::connect(url.as_str())
            .await
            .unwrap_or_else(|err| panic!("connect BOOKCLERK_TEST_POSTGRES_URL: {err}"));
        let backend = admin.get_database_backend();
        admin
            .execute_raw(Statement::from_string(
                backend,
                format!("CREATE DATABASE {db_name}"),
            ))
            .await
            .unwrap_or_else(|err| panic!("CREATE DATABASE {db_name}: {err}"));
        let (base, query) = match url.split_once('?') {
            Some((base, q)) => (base, Some(q)),
            None => (url.as_str(), None),
        };
        let trimmed = base.trim_end_matches('/');
        let slash = trimmed
            .rfind('/')
            .unwrap_or_else(|| panic!("BOOKCLERK_TEST_POSTGRES_URL has no database path: {url}"));
        let db_url = match query {
            Some(q) => format!("{}/{db_name}?{q}", &trimmed[..slash]),
            None => format!("{}/{db_name}", &trimmed[..slash]),
        };
        let db = sea_orm::Database::connect(&db_url)
            .await
            .unwrap_or_else(|err| panic!("connect throwaway {db_name}: {err}"));
        crate::apply_host_schema(&db)
            .await
            .expect("apply host schema");
        Some(LibraryStore::from_connection(db).with_in_process_sql())
    }

    /// Production page SQL after the same desugar, typecheck, and Postgres
    /// lowering the adapter applies. Equality prefixes stay bare.
    fn lowered_account_page_sql() -> String {
        let (page_sql, _) = page_statements(false, 0, true, false);
        let desugared = bookclerk_plugin_abi::desugar_canonical_sql(&page_sql);
        let env = crate::migrations::host_sql_type_env();
        let req = ExecuteRequest {
            operation_id: "page-plan".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![select_stmt(
                &desugared,
                vec![
                    DbValue::Text("case".into()),
                    DbValue::Int64(2),
                    DbValue::Int64(2),
                ],
                2,
            )],
        };
        let proofs = bookclerk_plugin_abi::typecheck_execute_request_proofs(&req, &env)
            .unwrap_or_else(|err| panic!("page sql typecheck: {err}"));
        bookclerk_db_exec::lower_canonical_sql_typed(
            sea_orm::DatabaseBackend::Postgres,
            &desugared,
            Some(&proofs[0]),
        )
        .unwrap_or_else(|err| panic!("lower page sql: {err}"))
    }

    #[test]
    fn production_page_sql_keeps_equality_prefixes_bare() {
        let lowered = lowered_account_page_sql();
        let predicate = lowered
            .split_once(" WHERE ")
            .and_then(|(_, rest)| rest.split_once(" ORDER BY "))
            .map(|(pred, _)| pred)
            .unwrap_or("");
        assert!(
            predicate.contains("account_id = $1") && !predicate.contains("COLLATE"),
            "equality prefix must stay bare so the page index matches ({} bytes)",
            lowered.len()
        );
        assert!(
            lowered.contains("(lower(title COLLATE \"C\"))"),
            "production fold missing ({} bytes)",
            lowered.len()
        );
        assert!(
            lowered.contains("(uuid COLLATE \"C\")"),
            "production tie-break missing ({} bytes)",
            lowered.len()
        );
        assert!(
            !lowered.to_ascii_uppercase().contains("NOCASE"),
            "postgres must not receive COLLATE NOCASE ({} bytes)",
            lowered.len()
        );
    }

    /// Exact hydration `uuid IN (?, …)` stays bare so Postgres can use
    /// `idx_books_uuid` and `UNIQUE(uuid)`.
    #[test]
    fn exact_hydration_sql_keeps_uuid_in_list_bare() {
        let (page_sql, _) = statements_for(BOOK_PAGE_COLUMNS, UuidMatch::Exact, 2, false, false);
        let desugared = bookclerk_plugin_abi::desugar_canonical_sql(&page_sql);
        let env = crate::migrations::host_sql_type_env();
        let req = ExecuteRequest {
            operation_id: "exact-plan".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![select_stmt(
                &desugared,
                vec![
                    DbValue::Text("u-one".into()),
                    DbValue::Text("u-two".into()),
                    DbValue::Int64(2),
                    DbValue::Int64(0),
                ],
                2,
            )],
        };
        let proofs = bookclerk_plugin_abi::typecheck_execute_request_proofs(&req, &env)
            .unwrap_or_else(|err| panic!("exact sql typecheck: {err}"));
        let lowered = bookclerk_db_exec::lower_canonical_sql_typed(
            sea_orm::DatabaseBackend::Postgres,
            &desugared,
            Some(&proofs[0]),
        )
        .unwrap_or_else(|err| panic!("lower exact sql: {err}"));
        let predicate = lowered
            .split_once(" WHERE ")
            .and_then(|(_, rest)| rest.split_once(" ORDER BY "))
            .map(|(pred, _)| pred)
            .unwrap_or("");
        assert!(
            predicate.contains("uuid IN ($1, $2)") && !predicate.contains("COLLATE"),
            "exact uuid IN must stay bare so idx_books_uuid matches: {predicate}"
        );
    }

    async fn postgres_page_plan(store: &LibraryStore) -> String {
        let lowered = lowered_account_page_sql();
        let mut sql = lowered;
        for (marker, literal) in [("$3", "2"), ("$2", "2"), ("$1", "'case'")] {
            sql = sql.replace(marker, literal);
        }
        let sql = format!("EXPLAIN {sql}");
        let rows = ConnectionTrait::query_all_raw(
            &store.db,
            Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql),
        )
        .await
        .expect("explain");
        let mut lines = Vec::new();
        for row in rows {
            if let Ok(text) = row.try_get_by_index::<String>(0) {
                lines.push(text);
            }
        }
        lines.join("\n")
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
            assert!(
                !page_sql.contains("SELECT *"),
                "page sql names its columns: {page_sql}"
            );
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

        let key_sql = "SELECT uuid, title FROM books WHERE lower(uuid) IN (lower(?)) LIMIT 4";
        let key_plan = explain(
            &store,
            key_sql,
            vec![Value::String(Some("uuid-00001".into()))],
        )
        .await;
        assert!(
            key_plan.contains("idx_books_uuid_lower"),
            "folded uuid lookup did not use the expression index:\n{key_plan}"
        );

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

    /// Search hydration sends every hit uuid. A 500-wide `IN` must still return
    /// the SQL page instead of rejecting the batch.
    #[tokio::test]
    async fn uuid_page_accepts_five_hundred_search_hits() {
        let store = memory_store().await;
        store
            .upsert_account("envelope-a", "us", None, false, "audible")
            .await
            .unwrap();
        let mut uuids = Vec::with_capacity(500);
        for i in 0..40u32 {
            let saved = store
                .upsert_book(&NewBook::minimal(
                    format!("B{i:05}"),
                    "envelope-a",
                    "us",
                    format!("Title {i:05}"),
                ))
                .await
                .unwrap();
            uuids.push(saved.uuid);
        }
        while uuids.len() < 500 {
            uuids.push(format!("missing-{:05}", uuids.len()));
        }
        let page = store
            .list_books_by_uuid_page(&uuids, None, None, 40, 0)
            .await
            .unwrap_or_else(|err| panic!("uuid page: {err}"));
        assert_eq!(page.books.len(), 40);
        assert!(page.total >= 40);
        let small = store
            .list_books_by_uuid_page(&uuids, None, None, 8, 0)
            .await
            .unwrap_or_else(|err| panic!("uuid page limit 8: {err}"));
        assert_eq!(small.books.len(), 8);
        assert!(small.total > 0);
    }

    /// Guest `maxResultBytes` is enforced. Every search hit exists, and both
    /// envelope page widths must still return a page.
    #[tokio::test]
    async fn uuid_page_under_sqlite_result_cap() {
        struct CapsGuest {
            db: sea_orm::DatabaseConnection,
        }

        #[async_trait::async_trait]
        impl crate::TypedAtomicExec for CapsGuest {
            async fn execute_typed(
                &self,
                envelope: bookclerk_db_exec::AdapterExecuteRequest,
            ) -> std::result::Result<
                bookclerk_plugin_abi::ExecuteReply,
                bookclerk_plugin_abi::PluginError,
            > {
                let caps = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
                bookclerk_db_exec::execute_typed_envelope_on_connection(
                    &self.db,
                    &envelope,
                    bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
                    bookclerk_db_exec::AtomicSession::default()
                        .with_type_env(crate::migrations::host_sql_type_env()),
                )
                .await
                .map_err(|err| bookclerk_plugin_abi::PluginError::internal(err.to_string()))
            }
        }

        let store = memory_store().await;
        let db = store.db.clone();
        let store = store.with_typed_exec(std::sync::Arc::new(CapsGuest { db }));
        store
            .upsert_account("envelope-a", "us", None, false, "audible")
            .await
            .unwrap();
        let mut uuids = Vec::with_capacity(500);
        for i in 0..500u32 {
            let saved = store
                .upsert_book(&NewBook::minimal(
                    format!("B{i:05}"),
                    "envelope-a",
                    "us",
                    format!("Title {i:05}"),
                ))
                .await
                .unwrap();
            uuids.push(saved.uuid);
        }
        let page = store
            .list_books_by_uuid_page(&uuids, None, None, 40, 0)
            .await
            .unwrap_or_else(|err| panic!("capped uuid page: {err}"));
        assert_eq!(page.books.len(), 40);
        assert!(page.total >= 500);
        assert_eq!(page.books[0].title, "Title 00000");
        assert_eq!(page.books[39].title, "Title 00039");
        let small = store
            .list_books_by_uuid_page(&uuids, None, None, 8, 0)
            .await
            .unwrap_or_else(|err| panic!("capped uuid page limit 8: {err}"));
        assert_eq!(small.books.len(), 8);
        assert!(small.total >= 500);
        assert_eq!(small.books[0].title, "Title 00000");
        assert_eq!(small.books[7].title, "Title 00007");
        let shifted = store
            .list_books_by_uuid_page(&uuids, None, None, 10, 100)
            .await
            .unwrap_or_else(|err| panic!("capped uuid page offset 100: {err}"));
        assert_eq!(shifted.books.len(), 10);
        assert!(shifted.total >= 500);
        assert_eq!(shifted.books[0].title, "Title 00100");
    }

    /// A non-search page of 256 full rows exceeds `maxResultBytes` in one
    /// guest result. The same limit must still return the requested page.
    #[tokio::test]
    async fn filtered_page_limit_256_stays_under_sqlite_result_cap() {
        struct CapsGuest {
            db: sea_orm::DatabaseConnection,
        }

        #[async_trait::async_trait]
        impl crate::TypedAtomicExec for CapsGuest {
            async fn execute_typed(
                &self,
                envelope: bookclerk_db_exec::AdapterExecuteRequest,
            ) -> std::result::Result<
                bookclerk_plugin_abi::ExecuteReply,
                bookclerk_plugin_abi::PluginError,
            > {
                let caps = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
                bookclerk_db_exec::execute_typed_envelope_on_connection(
                    &self.db,
                    &envelope,
                    bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
                    bookclerk_db_exec::AtomicSession::default()
                        .with_type_env(crate::migrations::host_sql_type_env()),
                )
                .await
                .map_err(|err| bookclerk_plugin_abi::PluginError::internal(err.to_string()))
            }
        }

        let store = memory_store().await;
        let db = store.db.clone();
        let store = store.with_typed_exec(std::sync::Arc::new(CapsGuest { db }));
        store
            .upsert_account("envelope-a", "us", None, false, "audible")
            .await
            .unwrap();
        seed_envelope_books(&store, 256).await;
        let page = store
            .list_books_filtered_page(None, None, 256, 0)
            .await
            .unwrap_or_else(|err| panic!("limit 256: {err}"));
        assert_eq!(page.books.len(), 256);
        assert_eq!(page.total, 256);
        assert_eq!(page.books[0].title, "Title 00000");
        assert_eq!(page.books[255].title, "Title 00255");
        let shifted = store
            .list_books_filtered_page(None, None, 256, 10)
            .await
            .unwrap_or_else(|err| panic!("limit 256 offset 10: {err}"));
        assert_eq!(shifted.books.len(), 246);
        assert_eq!(shifted.total, 256);
        assert_eq!(shifted.books[0].title, "Title 00010");
    }

    /// Store whose guest enforces sqlite `maxResultBytes`.
    async fn capped_store() -> LibraryStore {
        struct CapsGuest {
            db: sea_orm::DatabaseConnection,
        }

        #[async_trait::async_trait]
        impl crate::TypedAtomicExec for CapsGuest {
            async fn execute_typed(
                &self,
                envelope: bookclerk_db_exec::AdapterExecuteRequest,
            ) -> std::result::Result<
                bookclerk_plugin_abi::ExecuteReply,
                bookclerk_plugin_abi::PluginError,
            > {
                let caps = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
                bookclerk_db_exec::execute_typed_envelope_on_connection(
                    &self.db,
                    &envelope,
                    bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
                    bookclerk_db_exec::AtomicSession::default()
                        .with_type_env(crate::migrations::host_sql_type_env()),
                )
                .await
                .map_err(|err| bookclerk_plugin_abi::PluginError::internal(err.to_string()))
            }
        }

        let store = memory_store().await;
        let db = store.db.clone();
        store.with_typed_exec(std::sync::Arc::new(CapsGuest { db }))
    }

    /// One enriched `books` wire row: long description and the other text
    /// fields a catalog page actually returns.
    fn enriched_books_wire_row() -> (Vec<bookclerk_plugin_abi::DbColumn>, DbRow) {
        let long = "x".repeat(8_000);
        let columns = [
            ("id", bookclerk_plugin_abi::DbType::Int64),
            ("uuid", bookclerk_plugin_abi::DbType::Text),
            ("source", bookclerk_plugin_abi::DbType::Text),
            ("account_id", bookclerk_plugin_abi::DbType::Text),
            ("product_id", bookclerk_plugin_abi::DbType::Text),
            ("asin", bookclerk_plugin_abi::DbType::Text),
            ("isbn", bookclerk_plugin_abi::DbType::Text),
            ("marketplace", bookclerk_plugin_abi::DbType::Text),
            ("title", bookclerk_plugin_abi::DbType::Text),
            ("authors", bookclerk_plugin_abi::DbType::Text),
            ("narrators", bookclerk_plugin_abi::DbType::Text),
            ("series", bookclerk_plugin_abi::DbType::Text),
            ("series_index", bookclerk_plugin_abi::DbType::Text),
            ("series_asin", bookclerk_plugin_abi::DbType::Text),
            ("acquire_status", bookclerk_plugin_abi::DbType::Text),
            ("storage_key", bookclerk_plugin_abi::DbType::Text),
            ("error_message", bookclerk_plugin_abi::DbType::Text),
            ("purchased_at", bookclerk_plugin_abi::DbType::Text),
            ("tags", bookclerk_plugin_abi::DbType::Text),
            ("rating_overall", bookclerk_plugin_abi::DbType::Float64),
            ("rating_performance", bookclerk_plugin_abi::DbType::Float64),
            ("rating_story", bookclerk_plugin_abi::DbType::Float64),
            ("is_finished", bookclerk_plugin_abi::DbType::Int64),
            ("pdf_status", bookclerk_plugin_abi::DbType::Text),
            ("pdf_storage_key", bookclerk_plugin_abi::DbType::Text),
            ("publisher", bookclerk_plugin_abi::DbType::Text),
            ("length_minutes", bookclerk_plugin_abi::DbType::Int64),
            ("is_abridged", bookclerk_plugin_abi::DbType::Int64),
            ("content_kind", bookclerk_plugin_abi::DbType::Text),
            ("categories", bookclerk_plugin_abi::DbType::Text),
            ("subtitle", bookclerk_plugin_abi::DbType::Text),
            ("published_at", bookclerk_plugin_abi::DbType::Text),
            ("description", bookclerk_plugin_abi::DbType::Text),
            ("language", bookclerk_plugin_abi::DbType::Text),
            ("cover_url", bookclerk_plugin_abi::DbType::Text),
            ("subjects", bookclerk_plugin_abi::DbType::Text),
            ("enrich_source", bookclerk_plugin_abi::DbType::Text),
            ("enrich_confidence", bookclerk_plugin_abi::DbType::Float64),
            ("enrich_updated_at", bookclerk_plugin_abi::DbType::Text),
            ("created_at", bookclerk_plugin_abi::DbType::Text),
            ("updated_at", bookclerk_plugin_abi::DbType::Text),
        ];
        let cols = columns
            .iter()
            .map(|(name, db_type)| bookclerk_plugin_abi::DbColumn {
                name: (*name).to_string(),
                db_type: *db_type,
            })
            .collect();
        let text = |value: &str| DbValue::Text(value.to_string());
        let row = DbRow {
            values: vec![
                DbValue::Int64(10_000),
                text("01234567-89ab-cdef-0123-456789abcdef"),
                text("audible"),
                text("envelope-a"),
                text("B09999"),
                text("B09999"),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                text("us"),
                text("Title 09999"),
                text(&long),
                text(&long),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                text("not_acquired"),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Float64),
                DbValue::null(bookclerk_plugin_abi::DbType::Float64),
                DbValue::null(bookclerk_plugin_abi::DbType::Float64),
                DbValue::Int64(0),
                text("not_acquired"),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Int64),
                DbValue::Int64(0),
                text("book"),
                text(&long),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                text(&long),
                text("en"),
                text(&long),
                text(&long),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                DbValue::null(bookclerk_plugin_abi::DbType::Float64),
                DbValue::null(bookclerk_plugin_abi::DbType::Text),
                text("2026-10-01T10:57:53.123456789+00:00"),
                text("2026-10-01T10:57:53.123456789+00:00"),
            ],
        };
        (cols, row)
    }

    /// Enriched `books` rows push a 64-row page over `maxResultBytes`. One row
    /// still fits, so the reader can halve until the page returns.
    #[test]
    fn enriched_64_row_page_exceeds_sqlite_result_cap() {
        let (cols, row) = enriched_books_wire_row();
        let cap = usize::try_from(bookclerk_plugin_abi::FIRST_PARTY_MAX_RESULT_BYTES).unwrap();
        let one = bookclerk_plugin_abi::StatementResult::from_rows(cols.clone(), vec![row.clone()])
            .unwrap();
        let one_bytes = bookclerk_plugin_abi::encoded_statement_result_bytes(&one)
            .unwrap()
            .len();
        assert!(
            one_bytes <= cap,
            "one enriched row is {one_bytes} bytes; maxResultBytes is {cap}"
        );
        let page = bookclerk_plugin_abi::StatementResult::from_rows(cols, vec![row; 64]).unwrap();
        let page_bytes = bookclerk_plugin_abi::encoded_statement_result_bytes(&page)
            .unwrap()
            .len();
        assert!(
            page_bytes > cap,
            "64 enriched rows are {page_bytes} bytes and were expected to exceed {cap}"
        );
    }

    /// A page of enriched rows must still return under the guest byte cap.
    #[tokio::test]
    async fn filtered_page_of_enriched_rows_shrinks_to_the_result_cap() {
        let store = capped_store().await;
        store
            .upsert_account("envelope-a", "us", None, false, "audible")
            .await
            .unwrap();
        seed_envelope_books(&store, 64).await;
        let blob = "x".repeat(8_000);
        let backend = store.db.get_database_backend();
        ConnectionTrait::execute_raw(
            &store.db,
            Statement::from_sql_and_values(
                backend,
                "UPDATE books SET description = ?, authors = ?, narrators = ?, \
                 categories = ?, subjects = ?, cover_url = ?",
                vec![Value::String(Some(blob)); 6],
            ),
        )
        .await
        .unwrap();
        let page = store
            .list_books_filtered_page(None, None, 64, 0)
            .await
            .unwrap_or_else(|err| panic!("enriched page: {err}"));
        assert_eq!(page.books.len(), 64);
        assert_eq!(page.total, 64);
        assert_eq!(page.books[0].title, "Title 00000");
        assert_eq!(page.books[63].title, "Title 00063");
        assert!(page.books.iter().all(|book| {
            book.description
                .as_deref()
                .is_some_and(|text| text.len() == 8_000)
        }));
        let catalog = store
            .list_books_page(None, None, 64)
            .await
            .unwrap_or_else(|err| panic!("enriched catalog page failed: {err}"));
        assert_eq!(catalog.len(), 64);
        assert_eq!(catalog[0].title, "Title 00000");
        assert_eq!(catalog[63].title, "Title 00063");
    }

    /// A lowercased search hit must hydrate the stored uuid, including the
    /// mixed-case ids used by the page-order tests.
    #[tokio::test]
    async fn uuid_page_hydrates_mixed_case_stored_ids() {
        let store = memory_store().await;
        store
            .upsert_account("case", "us", None, false, "audible")
            .await
            .unwrap();
        for (uuid, title) in [
            ("u-gamma", "Gamma"),
            ("u-beta", "beta"),
            ("u-BETA", "BETA"),
            ("u-alpha", "alpha"),
            ("u-Alpha", "Alpha"),
        ] {
            let mut book = NewBook::minimal(uuid, "case", "us", title);
            book.uuid = Some(uuid.to_string());
            store.upsert_book(&book).await.unwrap();
        }
        let hits = ["u-gamma", "u-beta", "u-BETA", "u-alpha", "u-Alpha"]
            .into_iter()
            .map(|uuid| uuid.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let page = store
            .list_books_by_uuid_page(&hits, None, None, 10, 0)
            .await
            .unwrap_or_else(|err| panic!("mixed-case uuid page: {err}"));
        assert_eq!(page.total, 5);
        assert_eq!(page.books.len(), 5);
        let titles = page
            .books
            .iter()
            .map(|book| book.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(titles, ["Alpha", "alpha", "BETA", "beta", "Gamma"]);
        assert!(page.books.iter().any(|book| book.uuid == "u-Alpha"));
        assert!(page.books.iter().any(|book| book.uuid == "u-BETA"));
    }
}
