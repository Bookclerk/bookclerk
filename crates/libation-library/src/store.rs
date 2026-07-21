//! SQLite-backed library store (rusqlite + bundled libsqlite3).

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::error::{LibraryError, Result};
use crate::migrations;
use crate::models::{AccountRecord, BookRecord, LiberateStatus};

const BOOK_SELECT: &str = r#"
    SELECT id, uuid, source, account_id, product_id, asin, isbn, marketplace, title, authors,
           narrators, series, series_index, series_asin, liberate_status, storage_key,
           error_message, purchased_at, tags, rating_overall, rating_performance, rating_story,
           is_finished, pdf_status, pdf_storage_key, publisher, length_minutes, is_abridged,
           content_kind, categories, subtitle, published_at, created_at, updated_at
    FROM books
"#;

const ACCOUNT_SELECT: &str = r#"
    SELECT id, account_id, marketplace, label, scan_enabled, source, created_at, updated_at
    FROM accounts
"#;

/// Handle to the Libation library database.
///
/// Sync API (SQLite is local and fast). Callers in async contexts may invoke
/// these methods directly; wrap in `spawn_blocking` only for long batches.
#[derive(Clone)]
pub struct LibraryStore {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for LibraryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryStore").finish_non_exhaustive()
    }
}

impl LibraryStore {
    /// Open (or create) the SQLite database at `path` and run migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        migrations::migrations().to_latest(&mut conn)?;
        tracing::debug!(path = %path.display(), "opened library database");
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory database (tests).
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        migrations::migrations().to_latest(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| LibraryError::Other(anyhow::anyhow!("library database mutex poisoned")))?;
        f(&conn)
    }

    /// Upsert an account (updates `scan_enabled` on conflict). Defaults `source` to `"audible"`.
    pub fn upsert_account(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
    ) -> Result<AccountRecord> {
        self.upsert_account_with_source(account_id, marketplace, label, scan_enabled, "audible")
    }

    /// Upsert an account with an explicit catalog `source` (`audible`, `libro`, …).
    pub fn upsert_account_with_source(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        source: &str,
    ) -> Result<AccountRecord> {
        self.upsert_account_inner(account_id, marketplace, label, scan_enabled, source, true)
    }

    /// Ensure an account row exists for a scan without overwriting `scan_enabled`.
    /// Defaults `source` to `"audible"`.
    pub fn ensure_account(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
    ) -> Result<AccountRecord> {
        self.ensure_account_with_source(account_id, marketplace, label, "audible")
    }

    /// Ensure an account row exists with an explicit `source`, without overwriting `scan_enabled`.
    pub fn ensure_account_with_source(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        source: &str,
    ) -> Result<AccountRecord> {
        self.upsert_account_inner(account_id, marketplace, label, true, source, false)
    }

    fn upsert_account_inner(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        source: &str,
        update_scan_enabled: bool,
    ) -> Result<AccountRecord> {
        let now = Utc::now().to_rfc3339();
        self.with_conn(|conn| {
            // Reject collisions when the same account_id is claimed by another source.
            let existing_source: Option<String> = conn
                .query_row(
                    "SELECT source FROM accounts WHERE account_id = ?1",
                    params![account_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(existing) = existing_source.as_deref() {
                if existing != source {
                    return Err(LibraryError::Other(anyhow::anyhow!(
                        "account_id `{account_id}` already exists for source `{existing}`; \
                         cannot claim it for source `{source}`"
                    )));
                }
            }

            if update_scan_enabled {
                conn.execute(
                    r#"
                    INSERT INTO accounts (account_id, marketplace, label, scan_enabled, source, created_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                    ON CONFLICT(account_id) DO UPDATE SET
                        marketplace = excluded.marketplace,
                        label = COALESCE(excluded.label, accounts.label),
                        scan_enabled = excluded.scan_enabled,
                        updated_at = excluded.updated_at
                    "#,
                    params![
                        account_id,
                        marketplace,
                        label,
                        i64::from(scan_enabled),
                        source,
                        now,
                        now
                    ],
                )?;
            } else {
                conn.execute(
                    r#"
                    INSERT INTO accounts (account_id, marketplace, label, scan_enabled, source, created_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                    ON CONFLICT(account_id) DO UPDATE SET
                        marketplace = excluded.marketplace,
                        label = COALESCE(excluded.label, accounts.label),
                        updated_at = excluded.updated_at
                    "#,
                    params![
                        account_id,
                        marketplace,
                        label,
                        i64::from(scan_enabled),
                        source,
                        now,
                        now
                    ],
                )?;
            }
            Ok(())
        })?;
        self.get_account(account_id)?
            .ok_or_else(|| LibraryError::NotFound(account_id.into()))
    }

    /// Remap `from` → `to` account ids (books + account row).
    ///
    /// Used when classic Libation stored an email `AccountId` and a later
    /// login/scan discovers Audible `customer_id`.
    pub fn remap_account_id(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        self.with_conn(|conn| {
            let from_exists: bool = conn
                .query_row(
                    "SELECT 1 FROM accounts WHERE account_id = ?1",
                    params![from],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !from_exists {
                return Ok(());
            }

            let to_exists: bool = conn
                .query_row(
                    "SELECT 1 FROM accounts WHERE account_id = ?1",
                    params![to],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);

            if !to_exists {
                // Copy account row under the new id, then move books, then drop old.
                conn.execute(
                    r#"
                    INSERT INTO accounts (account_id, marketplace, label, scan_enabled, source, created_at, updated_at)
                    SELECT ?1, marketplace, label, scan_enabled, source, created_at, ?2
                    FROM accounts WHERE account_id = ?3
                    "#,
                    params![to, Utc::now().to_rfc3339(), from],
                )?;
            } else {
                // Prefer label from the old row when the canonical row has none.
                conn.execute(
                    r#"
                    UPDATE accounts SET
                        label = COALESCE(accounts.label, (SELECT label FROM accounts WHERE account_id = ?1)),
                        updated_at = ?2
                    WHERE account_id = ?3
                    "#,
                    params![from, Utc::now().to_rfc3339(), to],
                )?;
            }

            // Move books that do not already exist under `to` (same source + product_id).
            conn.execute(
                r#"
                UPDATE books SET account_id = ?1, updated_at = ?2
                WHERE account_id = ?3
                  AND NOT EXISTS (
                      SELECT 1 FROM books b2
                      WHERE b2.account_id = ?1
                        AND b2.source = books.source
                        AND b2.product_id = books.product_id
                  )
                "#,
                params![to, Utc::now().to_rfc3339(), from],
            )?;
            // Drop duplicate product rows left on the old id.
            conn.execute("DELETE FROM books WHERE account_id = ?1", params![from])?;
            conn.execute("DELETE FROM accounts WHERE account_id = ?1", params![from])?;
            Ok(())
        })
    }

    /// Remap any existing alias account ids onto `canonical_id`, then upsert.
    pub fn reconcile_account_id(
        &self,
        canonical_id: &str,
        aliases: &[&str],
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
    ) -> Result<AccountRecord> {
        for alias in aliases {
            if *alias != canonical_id {
                self.remap_account_id(alias, canonical_id)?;
            }
        }
        self.upsert_account(canonical_id, marketplace, label, scan_enabled)
    }

    pub fn get_account(&self, account_id: &str) -> Result<Option<AccountRecord>> {
        self.with_conn(|conn| {
            conn.query_row(
                &format!("{ACCOUNT_SELECT} WHERE account_id = ?1"),
                params![account_id],
                map_account_row,
            )
            .optional()
            .map_err(LibraryError::from)
        })
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!("{ACCOUNT_SELECT} ORDER BY account_id"))?;
            let rows = stmt
                .query_map([], map_account_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Resolve an account row by id or nickname (`label`), case-insensitive.
    pub fn find_account(&self, identifier: &str) -> Result<Option<AccountRecord>> {
        let needle = identifier.to_ascii_lowercase();
        Ok(self.list_accounts()?.into_iter().find(|a| {
            a.account_id.eq_ignore_ascii_case(identifier)
                || a.label
                    .as_ref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(identifier))
                || a.account_id.to_ascii_lowercase() == needle
        }))
    }

    /// Toggle whether an account is included in automatic library scans.
    pub fn set_scan_enabled(&self, account_id: &str, scan_enabled: bool) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let updated = self.with_conn(|conn| {
            conn.execute(
                "UPDATE accounts SET scan_enabled = ?1, updated_at = ?2 WHERE account_id = ?3",
                params![i64::from(scan_enabled), now, account_id],
            )
            .map_err(LibraryError::from)
        })?;
        if updated == 0 {
            return Err(LibraryError::NotFound(account_id.into()));
        }
        Ok(())
    }

    /// Upsert a book from a library sync.
    pub fn upsert_book(&self, book: &NewBook) -> Result<BookRecord> {
        let now = Utc::now().to_rfc3339();
        let uuid = book
            .uuid
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.with_conn(|conn| {
            conn.execute(
                r#"
                INSERT INTO books (
                    uuid, source, account_id, product_id, asin, isbn, marketplace, title,
                    authors, narrators, series, series_index, series_asin, liberate_status,
                    purchased_at, publisher, length_minutes, is_abridged, content_kind,
                    categories, subtitle, published_at, created_at, updated_at
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                    ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
                )
                ON CONFLICT(source, account_id, product_id) DO UPDATE SET
                    asin = COALESCE(excluded.asin, books.asin),
                    isbn = COALESCE(excluded.isbn, books.isbn),
                    marketplace = excluded.marketplace,
                    -- When the existing row already has an Audible ASIN (native or
                    -- enriched) and the incoming row does not, keep catalog fields
                    -- so a Libro rescan does not wipe Audible enrichment.
                    title = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN excluded.title ELSE books.title END,
                    authors = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN excluded.authors ELSE books.authors END,
                    narrators = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN excluded.narrators ELSE books.narrators END,
                    series = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN excluded.series ELSE books.series END,
                    series_index = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN excluded.series_index ELSE books.series_index END,
                    series_asin = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN COALESCE(excluded.series_asin, books.series_asin)
                        ELSE books.series_asin END,
                    purchased_at = excluded.purchased_at,
                    publisher = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN COALESCE(excluded.publisher, books.publisher)
                        ELSE books.publisher END,
                    length_minutes = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN COALESCE(excluded.length_minutes, books.length_minutes)
                        ELSE books.length_minutes END,
                    is_abridged = excluded.is_abridged,
                    content_kind = excluded.content_kind,
                    categories = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN COALESCE(excluded.categories, books.categories)
                        ELSE books.categories END,
                    subtitle = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN COALESCE(excluded.subtitle, books.subtitle)
                        ELSE books.subtitle END,
                    published_at = CASE
                        WHEN excluded.asin IS NOT NULL OR books.asin IS NULL
                        THEN COALESCE(excluded.published_at, books.published_at)
                        ELSE books.published_at END,
                    updated_at = excluded.updated_at
                "#,
                params![
                    uuid,
                    book.source,
                    book.account_id,
                    book.product_id,
                    book.asin,
                    book.isbn,
                    book.marketplace,
                    book.title,
                    book.authors,
                    book.narrators,
                    book.series,
                    book.series_index,
                    book.series_asin,
                    LiberateStatus::NotLiberated.as_str(),
                    book.purchased_at.map(|d| d.to_rfc3339()),
                    book.publisher,
                    book.length_minutes,
                    i64::from(book.is_abridged),
                    book.content_kind,
                    book.categories,
                    book.subtitle,
                    book.published_at.map(|d| d.to_rfc3339()),
                    now,
                    now,
                ],
            )?;
            Ok(())
        })?;
        self.get_book(&book.product_id, &book.account_id)?
            .ok_or_else(|| LibraryError::NotFound(book.product_id.clone()))
    }

    /// Look up a book by its public `uuid`.
    pub fn get_book_by_uuid(&self, uuid: &str) -> Result<Option<BookRecord>> {
        self.with_conn(|conn| {
            conn.query_row(
                &format!("{BOOK_SELECT} WHERE uuid = ?1"),
                params![uuid],
                map_book_row,
            )
            .optional()
            .map_err(LibraryError::from)
        })
    }

    /// Look up a book by uuid, product_id, asin, or isbn for the given account.
    ///
    /// Prefer exact uuid / product_id matches. When only ISBN matches multiple
    /// sources under one account, prefer product_id/uuid exactness, then
    /// deterministic `ORDER BY source`.
    pub fn get_book(&self, title_id: &str, account_id: &str) -> Result<Option<BookRecord>> {
        self.with_conn(|conn| {
            let sql = format!(
                "{BOOK_SELECT}
                WHERE account_id = ?2
                  AND (uuid = ?1 OR product_id = ?1 OR asin = ?1 OR isbn = ?1)
                ORDER BY
                  CASE
                    WHEN uuid = ?1 THEN 0
                    WHEN product_id = ?1 THEN 1
                    WHEN asin = ?1 THEN 2
                    ELSE 3
                  END,
                  source
                LIMIT 1"
            );
            conn.query_row(&sql, params![title_id, account_id], map_book_row)
                .optional()
                .map_err(LibraryError::from)
        })
    }

    /// All ownership rows sharing an ISBN (cross-account / cross-store enrichment).
    pub fn find_books_by_isbn(&self, isbn: &str) -> Result<Vec<BookRecord>> {
        self.with_conn(|conn| {
            let sql = format!("{BOOK_SELECT} WHERE isbn = ?1 ORDER BY source, account_id");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![isbn], map_book_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn list_books(&self, account_id: Option<&str>) -> Result<Vec<BookRecord>> {
        self.with_conn(|conn| {
            if let Some(account_id) = account_id {
                let sql = format!("{BOOK_SELECT} WHERE account_id = ?1 ORDER BY title");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(params![account_id], map_book_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            } else {
                let sql = format!("{BOOK_SELECT} ORDER BY title");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt
                    .query_map([], map_book_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            }
        })
    }

    /// Resolve `title_id` to the stored public uuid, or error if missing.
    fn resolve_uuid(&self, title_id: &str, account_id: &str) -> Result<String> {
        self.get_book(title_id, account_id)?
            .map(|b| b.uuid)
            .ok_or_else(|| LibraryError::NotFound(title_id.into()))
    }

    /// Update user-defined fields (tags, ratings, finished) without touching scan metadata.
    pub fn update_user_fields(
        &self,
        title_id: &str,
        account_id: &str,
        fields: &UserBookFields,
    ) -> Result<()> {
        let uuid = self.resolve_uuid(title_id, account_id)?;
        let now = Utc::now().to_rfc3339();
        let rows = self.with_conn(|conn| {
            let n = conn.execute(
                r#"
                UPDATE books SET
                    tags = COALESCE(?1, tags),
                    rating_overall = COALESCE(?2, rating_overall),
                    rating_performance = COALESCE(?3, rating_performance),
                    rating_story = COALESCE(?4, rating_story),
                    is_finished = COALESCE(?5, is_finished),
                    updated_at = ?6
                WHERE uuid = ?7
                "#,
                params![
                    fields.tags,
                    fields.rating_overall,
                    fields.rating_performance,
                    fields.rating_story,
                    fields.is_finished.map(|b| if b { 1 } else { 0 }),
                    now,
                    uuid,
                ],
            )?;
            Ok(n)
        })?;
        if rows == 0 {
            return Err(LibraryError::NotFound(title_id.into()));
        }
        Ok(())
    }

    pub fn set_pdf_status(
        &self,
        title_id: &str,
        account_id: &str,
        status: LiberateStatus,
        pdf_storage_key: Option<&str>,
    ) -> Result<()> {
        let uuid = self.resolve_uuid(title_id, account_id)?;
        let now = Utc::now().to_rfc3339();
        let rows = self.with_conn(|conn| {
            let n = conn.execute(
                r#"
                UPDATE books SET pdf_status = ?1, pdf_storage_key = ?2, updated_at = ?3
                WHERE uuid = ?4
                "#,
                params![status.as_str(), pdf_storage_key, now, uuid],
            )?;
            Ok(n)
        })?;
        if rows == 0 {
            return Err(LibraryError::NotFound(title_id.into()));
        }
        Ok(())
    }

    pub fn is_ignored(&self, title_id: &str, account_id: &str) -> Result<bool> {
        let (source, product_id) = match self.get_book(title_id, account_id)? {
            Some(b) => (b.source, b.product_id),
            None => (String::from("audible"), title_id.to_string()),
        };
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT 1 FROM ignored_titles WHERE source = ?1 AND account_id = ?2 AND product_id = ?3",
                params![source, account_id, product_id],
                |_| Ok(true),
            )
            .optional()
            .map(|opt| opt.is_some())
            .map_err(LibraryError::from)
        })
    }

    pub fn set_ignored(
        &self,
        title_id: &str,
        account_id: &str,
        ignored: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        let (source, product_id) = match self.get_book(title_id, account_id)? {
            Some(b) => (b.source, b.product_id),
            None => (String::from("audible"), title_id.to_string()),
        };
        let now = Utc::now().to_rfc3339();
        self.with_conn(|conn| {
            if ignored {
                conn.execute(
                    r#"
                    INSERT INTO ignored_titles (source, account_id, product_id, reason, created_at)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    ON CONFLICT(source, account_id, product_id) DO UPDATE SET reason = excluded.reason
                    "#,
                    params![source, account_id, product_id, reason, now],
                )?;
            } else {
                conn.execute(
                    "DELETE FROM ignored_titles WHERE source = ?1 AND account_id = ?2 AND product_id = ?3",
                    params![source, account_id, product_id],
                )?;
            }
            Ok(())
        })
    }

    pub fn set_liberate_status(
        &self,
        title_id: &str,
        account_id: &str,
        status: LiberateStatus,
        storage_key: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let uuid = self.resolve_uuid(title_id, account_id)?;
        let now = Utc::now().to_rfc3339();
        let rows = self.with_conn(|conn| {
            let n = conn.execute(
                r#"
                UPDATE books SET
                    liberate_status = ?1,
                    storage_key = ?2,
                    error_message = ?3,
                    updated_at = ?4
                WHERE uuid = ?5
                "#,
                params![status.as_str(), storage_key, error_message, now, uuid],
            )?;
            Ok(n)
        })?;
        if rows == 0 {
            return Err(LibraryError::NotFound(title_id.into()));
        }
        Ok(())
    }

    /// Bulk-update liberate status (classic `set-status --force`).
    ///
    /// When `asins` is non-empty, matches against uuid, product_id, isbn, or asin.
    pub fn bulk_set_liberate_status(
        &self,
        account: Option<&str>,
        asins: &[String],
        status: LiberateStatus,
    ) -> Result<u32> {
        let books = self.list_books(account)?;
        let mut updated = 0u32;
        for book in books {
            if !asins.is_empty()
                && !asins.iter().any(|a| {
                    a.eq_ignore_ascii_case(&book.uuid)
                        || a.eq_ignore_ascii_case(&book.product_id)
                        || book
                            .isbn
                            .as_ref()
                            .is_some_and(|isbn| a.eq_ignore_ascii_case(isbn))
                        || book
                            .asin
                            .as_ref()
                            .is_some_and(|asin| a.eq_ignore_ascii_case(asin))
                })
            {
                continue;
            }
            self.set_liberate_status(
                &book.uuid,
                &book.account_id,
                status,
                book.storage_key.as_deref(),
                None,
            )?;
            updated += 1;
        }
        Ok(updated)
    }

    /// List saved quick-filter expressions.
    pub fn list_saved_filters(&self) -> Result<Vec<SavedFilterRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, query, created_at, updated_at FROM saved_filters ORDER BY name",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(SavedFilterRecord {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        query: row.get(2)?,
                        created_at: parse_dt(&row.get::<_, String>(3)?),
                        updated_at: parse_dt(&row.get::<_, String>(4)?),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn upsert_saved_filter(&self, name: &str, query: &str) -> Result<SavedFilterRecord> {
        let now = Utc::now().to_rfc3339();
        self.with_conn(|conn| {
            conn.execute(
                r#"
                INSERT INTO saved_filters (name, query, created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(name) DO UPDATE SET
                    query = excluded.query,
                    updated_at = excluded.updated_at
                "#,
                params![name, query, now, now],
            )?;
            Ok(())
        })?;
        self.get_saved_filter(name)?
            .ok_or_else(|| LibraryError::NotFound(name.into()))
    }

    pub fn get_saved_filter(&self, name: &str) -> Result<Option<SavedFilterRecord>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, name, query, created_at, updated_at FROM saved_filters WHERE name = ?1",
                params![name],
                |row| {
                    Ok(SavedFilterRecord {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        query: row.get(2)?,
                        created_at: parse_dt(&row.get::<_, String>(3)?),
                        updated_at: parse_dt(&row.get::<_, String>(4)?),
                    })
                },
            )
            .optional()
            .map_err(LibraryError::from)
        })
    }

    pub fn delete_saved_filter(&self, name: &str) -> Result<()> {
        let rows = self.with_conn(|conn| {
            let n = conn.execute("DELETE FROM saved_filters WHERE name = ?1", params![name])?;
            Ok(n)
        })?;
        if rows == 0 {
            return Err(LibraryError::NotFound(name.into()));
        }
        Ok(())
    }

    pub fn count_by_status(&self, status: LiberateStatus) -> Result<i64> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM books WHERE liberate_status = ?1",
                params![status.as_str()],
                |row| row.get(0),
            )
            .map_err(LibraryError::from)
        })
    }
}

/// Prefer an Audible ownership row with the richest metadata for enrichment.
#[must_use]
pub fn prefer_enrichment_source(books: &[BookRecord]) -> Option<&BookRecord> {
    books.iter().max_by_key(|b| {
        let mut score = 0u32;
        if b.source.eq_ignore_ascii_case("audible") {
            score += 100;
        }
        if b.asin.is_some() {
            score += 10;
        }
        if b.isbn.is_some() {
            score += 10;
        }
        if b.authors.is_some() {
            score += 2;
        }
        if b.narrators.is_some() {
            score += 2;
        }
        if b.series.is_some() {
            score += 2;
        }
        if b.publisher.is_some() {
            score += 1;
        }
        if b.subtitle.is_some() {
            score += 1;
        }
        if b.length_minutes.is_some() {
            score += 1;
        }
        if b.categories.is_some() {
            score += 1;
        }
        if !b.title.is_empty() {
            score += 1;
        }
        score
    })
}

/// Input for inserting / updating a book from sync.
#[derive(Debug, Clone)]
pub struct NewBook {
    /// Public id; generated on insert when `None`.
    pub uuid: Option<String>,
    pub product_id: String,
    pub source: String,
    pub account_id: String,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub marketplace: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub series_asin: Option<String>,
    pub purchased_at: Option<chrono::DateTime<Utc>>,
    pub publisher: Option<String>,
    pub length_minutes: Option<i64>,
    pub is_abridged: bool,
    pub content_kind: String,
    pub categories: Option<String>,
    pub subtitle: Option<String>,
    pub published_at: Option<chrono::DateTime<Utc>>,
}

impl NewBook {
    /// Minimal Audible book row with scan metadata defaults.
    ///
    /// Sets `product_id` from the first argument, `asin = Some(product_id)`,
    /// `isbn = None`, `uuid = None` (auto), and `source = "audible"`.
    #[must_use]
    pub fn minimal(
        product_id: impl Into<String>,
        account_id: impl Into<String>,
        marketplace: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        let product_id = product_id.into();
        Self {
            uuid: None,
            asin: Some(product_id.clone()),
            isbn: None,
            product_id,
            source: String::from("audible"),
            account_id: account_id.into(),
            marketplace: marketplace.into(),
            title: title.into(),
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: String::from("book"),
            categories: None,
            subtitle: None,
            published_at: None,
        }
    }
}

fn map_account_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRecord> {
    let created_at: String = r.get("created_at")?;
    let updated_at: String = r.get("updated_at")?;
    Ok(AccountRecord {
        id: r.get("id")?,
        account_id: r.get("account_id")?,
        source: r
            .get::<_, String>("source")
            .unwrap_or_else(|_| String::from("audible")),
        marketplace: r.get("marketplace")?,
        label: r.get("label")?,
        scan_enabled: r.get::<_, i64>("scan_enabled")? != 0,
        created_at: parse_dt(&created_at),
        updated_at: parse_dt(&updated_at),
    })
}

/// Saved Lucene-style quick filter.
#[derive(Debug, Clone)]
pub struct SavedFilterRecord {
    pub id: i64,
    pub name: String,
    pub query: String,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

/// Partial update for user-defined book fields.
#[derive(Debug, Clone, Default)]
pub struct UserBookFields {
    pub tags: Option<String>,
    pub rating_overall: Option<f32>,
    pub rating_performance: Option<f32>,
    pub rating_story: Option<f32>,
    pub is_finished: Option<bool>,
}

fn map_book_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BookRecord> {
    let status_raw: String = r.get("liberate_status")?;
    let pdf_raw: String = r
        .get("pdf_status")
        .unwrap_or_else(|_| "not_liberated".into());
    let created_at: String = r.get("created_at")?;
    let updated_at: String = r.get("updated_at")?;
    let purchased_at: Option<String> = r.get("purchased_at")?;
    let published_at: Option<String> = r.get("published_at").ok().flatten();
    Ok(BookRecord {
        id: r.get("id")?,
        uuid: r.get("uuid")?,
        source: r
            .get::<_, String>("source")
            .unwrap_or_else(|_| String::from("audible")),
        account_id: r.get("account_id")?,
        product_id: r.get("product_id")?,
        asin: r.get("asin")?,
        isbn: r.get("isbn")?,
        marketplace: r.get("marketplace")?,
        title: r.get("title")?,
        authors: r.get("authors")?,
        narrators: r.get("narrators")?,
        series: r.get("series")?,
        series_index: r.get("series_index")?,
        series_asin: r.get("series_asin").ok().flatten(),
        liberate_status: LiberateStatus::parse(&status_raw).unwrap_or_default(),
        storage_key: r.get("storage_key")?,
        error_message: r.get("error_message")?,
        purchased_at: purchased_at.as_deref().map(parse_dt),
        tags: r.get("tags").ok(),
        rating_overall: r.get("rating_overall").ok(),
        rating_performance: r.get("rating_performance").ok(),
        rating_story: r.get("rating_story").ok(),
        is_finished: r.get::<_, i64>("is_finished").unwrap_or(0) != 0,
        pdf_status: LiberateStatus::parse(&pdf_raw).unwrap_or_default(),
        pdf_storage_key: r.get("pdf_storage_key").ok(),
        publisher: r.get("publisher").ok(),
        length_minutes: r.get("length_minutes").ok(),
        is_abridged: r.get::<_, i64>("is_abridged").unwrap_or(0) != 0,
        content_kind: r
            .get::<_, String>("content_kind")
            .unwrap_or_else(|_| "book".into()),
        categories: r.get("categories").ok(),
        subtitle: r.get("subtitle").ok(),
        published_at: published_at.as_deref().map(parse_dt),
        created_at: parse_dt(&created_at),
        updated_at: parse_dt(&updated_at),
    })
}

fn parse_dt(value: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_and_book_roundtrip() {
        let store = LibraryStore::open_in_memory().unwrap();
        let acct = store
            .upsert_account("user-1", "us", Some("Main"), true)
            .unwrap();
        assert_eq!(acct.account_id, "user-1");
        assert_eq!(acct.source, "audible");

        let mut book = NewBook::minimal("B00TEST", "user-1", "us", "Test Book");
        book.authors = Some("Author".into());
        let book = store.upsert_book(&book).unwrap();
        assert_eq!(book.title, "Test Book");
        assert!(!book.uuid.is_empty());
        assert_eq!(book.product_id, "B00TEST");
        assert_eq!(book.asin.as_deref(), Some("B00TEST"));
        assert!(book.isbn.is_none());
        assert_eq!(book.source, "audible");
        assert_eq!(book.title_id(), book.uuid.as_str());
        assert_eq!(book.asin_or_isbn(), "B00TEST");
        assert_eq!(book.liberate_status, LiberateStatus::NotLiberated);

        store
            .set_liberate_status(
                "B00TEST",
                "user-1",
                LiberateStatus::Liberated,
                Some("Author/Test Book/book.m4b"),
                None,
            )
            .unwrap();

        let updated = store.get_book("B00TEST", "user-1").unwrap().unwrap();
        assert_eq!(updated.liberate_status, LiberateStatus::Liberated);
        assert_eq!(
            updated.storage_key.as_deref(),
            Some("Author/Test Book/book.m4b")
        );
        assert_eq!(store.count_by_status(LiberateStatus::Liberated).unwrap(), 1);

        let by_uuid = store.get_book_by_uuid(&updated.uuid).unwrap().unwrap();
        assert_eq!(by_uuid.product_id, "B00TEST");
    }

    #[test]
    fn same_isbn_multi_account_and_source() {
        let store = LibraryStore::open_in_memory().unwrap();
        store.upsert_account("user-1", "us", None, true).unwrap();
        store.upsert_account("user-2", "us", None, true).unwrap();
        store
            .upsert_account_with_source("libro-1", "us", None, true, "libro")
            .unwrap();

        let mut a1 = NewBook::minimal("B00SAME", "user-1", "us", "Same Book");
        a1.isbn = Some("9781234567890".into());
        store.upsert_book(&a1).unwrap();

        let mut a2 = NewBook::minimal("B00SAME", "user-2", "us", "Same Book");
        a2.isbn = Some("9781234567890".into());
        store.upsert_book(&a2).unwrap();

        let libro = NewBook {
            uuid: None,
            product_id: "9781234567890".into(),
            source: "libro".into(),
            account_id: "libro-1".into(),
            asin: None,
            isbn: Some("9781234567890".into()),
            marketplace: "us".into(),
            title: "Same Book".into(),
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: "book".into(),
            categories: None,
            subtitle: None,
            published_at: None,
        };
        store.upsert_book(&libro).unwrap();

        let by_isbn = store.find_books_by_isbn("9781234567890").unwrap();
        assert_eq!(by_isbn.len(), 3);
        let uuids: std::collections::HashSet<_> = by_isbn.iter().map(|b| b.uuid.as_str()).collect();
        assert_eq!(uuids.len(), 3);

        let preferred = prefer_enrichment_source(&by_isbn).unwrap();
        assert_eq!(preferred.source, "audible");
    }

    #[test]
    fn libro_rescan_preserves_audible_enrichment() {
        let store = LibraryStore::open_in_memory().unwrap();
        store
            .upsert_account_with_source("libro-1", "us", None, true, "libro")
            .unwrap();

        let isbn = "9781234567890";
        let initial = NewBook {
            uuid: None,
            product_id: isbn.into(),
            source: "libro".into(),
            account_id: "libro-1".into(),
            asin: None,
            isbn: Some(isbn.into()),
            marketplace: "us".into(),
            title: "Sparse Libro Title".into(),
            authors: Some("Libro Author".into()),
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: "book".into(),
            categories: None,
            subtitle: None,
            published_at: None,
        };
        let row = store.upsert_book(&initial).unwrap();

        // Simulate Audible catalog enrichment.
        let enriched = NewBook {
            uuid: Some(row.uuid.clone()),
            asin: Some("B00ENRICHED".into()),
            title: "Rich Audible Title".into(),
            authors: Some("Audible Author".into()),
            narrators: Some("Audible Narrator".into()),
            series: Some("Audible Series".into()),
            publisher: Some("Publisher".into()),
            length_minutes: Some(420),
            subtitle: Some("A Subtitle".into()),
            ..initial.clone()
        };
        let after_enrich = store.upsert_book(&enriched).unwrap();
        assert_eq!(after_enrich.asin.as_deref(), Some("B00ENRICHED"));
        assert_eq!(after_enrich.title, "Rich Audible Title");
        assert_eq!(after_enrich.narrators.as_deref(), Some("Audible Narrator"));

        // Libro rescan without asin must not wipe enrichment.
        let rescan = NewBook {
            title: "Sparse Libro Title Again".into(),
            authors: Some("Libro Author".into()),
            narrators: None,
            asin: None,
            series: None,
            publisher: None,
            length_minutes: Some(400),
            subtitle: None,
            ..initial
        };
        let after_rescan = store.upsert_book(&rescan).unwrap();
        assert_eq!(after_rescan.uuid, row.uuid);
        assert_eq!(after_rescan.asin.as_deref(), Some("B00ENRICHED"));
        assert_eq!(after_rescan.title, "Rich Audible Title");
        assert_eq!(after_rescan.authors.as_deref(), Some("Audible Author"));
        assert_eq!(after_rescan.narrators.as_deref(), Some("Audible Narrator"));
        assert_eq!(after_rescan.series.as_deref(), Some("Audible Series"));
        assert_eq!(after_rescan.publisher.as_deref(), Some("Publisher"));
        assert_eq!(after_rescan.length_minutes, Some(420));
        assert_eq!(after_rescan.subtitle.as_deref(), Some("A Subtitle"));
        assert_eq!(after_rescan.download_product_id(), isbn);
    }

    #[test]
    fn download_product_id_is_source_native() {
        let store = LibraryStore::open_in_memory().unwrap();
        store
            .upsert_account_with_source("libro-1", "us", None, true, "libro")
            .unwrap();
        let book = store
            .upsert_book(&NewBook {
                uuid: None,
                product_id: "9789999999999".into(),
                source: "libro".into(),
                account_id: "libro-1".into(),
                asin: Some("B00FROMAD".into()),
                isbn: Some("9789999999999".into()),
                marketplace: "us".into(),
                title: "Enriched".into(),
                authors: None,
                narrators: None,
                series: None,
                series_index: None,
                series_asin: None,
                purchased_at: None,
                publisher: None,
                length_minutes: None,
                is_abridged: false,
                content_kind: "book".into(),
                categories: None,
                subtitle: None,
                published_at: None,
            })
            .unwrap();
        assert_eq!(book.download_product_id(), "9789999999999");
        assert_eq!(book.audible_asin(), Some("B00FROMAD"));
    }

    #[test]
    fn ensure_account_preserves_scan_enabled() {
        let store = LibraryStore::open_in_memory().unwrap();
        store
            .upsert_account("user-1", "us", Some("Main"), false)
            .unwrap();
        store.ensure_account("user-1", "us", Some("Main")).unwrap();
        let acct = store.get_account("user-1").unwrap().unwrap();
        assert!(!acct.scan_enabled);
    }

    #[test]
    fn upsert_account_with_source_persists() {
        let store = LibraryStore::open_in_memory().unwrap();
        let acct = store
            .upsert_account_with_source("libro-1", "us", Some("Libro"), true, "libro")
            .unwrap();
        assert_eq!(acct.source, "libro");
        let again = store.get_account("libro-1").unwrap().unwrap();
        assert_eq!(again.source, "libro");
    }

    #[test]
    fn remap_account_moves_books() {
        let store = LibraryStore::open_in_memory().unwrap();
        store
            .upsert_account("email@example.com", "us", Some("Main"), true)
            .unwrap();
        store
            .upsert_book(&NewBook::minimal(
                "B00TEST",
                "email@example.com",
                "us",
                "Test Book",
            ))
            .unwrap();

        store
            .remap_account_id("email@example.com", "amzn1.account.CID")
            .unwrap();

        assert!(store.get_account("email@example.com").unwrap().is_none());
        assert!(store.get_account("amzn1.account.CID").unwrap().is_some());
        assert!(store
            .get_book("B00TEST", "amzn1.account.CID")
            .unwrap()
            .is_some());
    }

    #[test]
    fn ignored_titles_roundtrip() {
        let store = LibraryStore::open_in_memory().unwrap();
        store.upsert_account("user-1", "us", None, true).unwrap();
        store
            .upsert_book(&NewBook::minimal("B00TEST", "user-1", "us", "Test"))
            .unwrap();
        assert!(!store.is_ignored("B00TEST", "user-1").unwrap());
        store
            .set_ignored("B00TEST", "user-1", true, Some("skip"))
            .unwrap();
        assert!(store.is_ignored("B00TEST", "user-1").unwrap());
        store.set_ignored("B00TEST", "user-1", false, None).unwrap();
        assert!(!store.is_ignored("B00TEST", "user-1").unwrap());
    }
}
