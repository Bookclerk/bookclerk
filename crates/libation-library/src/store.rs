//! SQLite-backed library store (rusqlite + bundled libsqlite3).

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{LibraryError, Result};
use crate::migrations;
use crate::models::{AccountRecord, BookRecord, LiberateStatus};

const BOOK_SELECT: &str = r#"
    SELECT id, asin, account_id, marketplace, title, authors, narrators, series, series_index,
           series_asin, liberate_status, storage_key, error_message, purchased_at,
           tags, rating_overall, rating_performance, rating_story, is_finished,
           pdf_status, pdf_storage_key, publisher, length_minutes, is_abridged,
           content_kind, categories, subtitle, published_at,
           created_at, updated_at
    FROM books
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

    /// Upsert an account (updates `scan_enabled` on conflict).
    pub fn upsert_account(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
    ) -> Result<AccountRecord> {
        self.upsert_account_inner(account_id, marketplace, label, scan_enabled, true)
    }

    /// Ensure an account row exists for a scan without overwriting `scan_enabled`.
    pub fn ensure_account(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
    ) -> Result<AccountRecord> {
        self.upsert_account_inner(account_id, marketplace, label, true, false)
    }

    fn upsert_account_inner(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        update_scan_enabled: bool,
    ) -> Result<AccountRecord> {
        let now = Utc::now().to_rfc3339();
        self.with_conn(|conn| {
            if update_scan_enabled {
                conn.execute(
                    r#"
                    INSERT INTO accounts (account_id, marketplace, label, scan_enabled, created_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
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
                        now,
                        now
                    ],
                )?;
            } else {
                conn.execute(
                    r#"
                    INSERT INTO accounts (account_id, marketplace, label, scan_enabled, created_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
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
                    INSERT INTO accounts (account_id, marketplace, label, scan_enabled, created_at, updated_at)
                    SELECT ?1, marketplace, label, scan_enabled, created_at, ?2
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

            // Move books that do not already exist under `to`.
            conn.execute(
                r#"
                UPDATE books SET account_id = ?1, updated_at = ?2
                WHERE account_id = ?3
                  AND asin NOT IN (SELECT asin FROM books WHERE account_id = ?1)
                "#,
                params![to, Utc::now().to_rfc3339(), from],
            )?;
            // Drop duplicate ASIN rows left on the old id.
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
                r#"
                SELECT id, account_id, marketplace, label, scan_enabled, created_at, updated_at
                FROM accounts WHERE account_id = ?1
                "#,
                params![account_id],
                map_account_row,
            )
            .optional()
            .map_err(LibraryError::from)
        })
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT id, account_id, marketplace, label, scan_enabled, created_at, updated_at
                FROM accounts ORDER BY account_id
                "#,
            )?;
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
        self.with_conn(|conn| {
            conn.execute(
                r#"
                INSERT INTO books (
                    asin, account_id, marketplace, title, authors, narrators, series, series_index,
                    series_asin, liberate_status, purchased_at, publisher, length_minutes, is_abridged,
                    content_kind, categories, subtitle, published_at, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
                ON CONFLICT(asin, account_id) DO UPDATE SET
                    marketplace = excluded.marketplace,
                    title = excluded.title,
                    authors = excluded.authors,
                    narrators = excluded.narrators,
                    series = excluded.series,
                    series_index = excluded.series_index,
                    series_asin = COALESCE(excluded.series_asin, books.series_asin),
                    purchased_at = excluded.purchased_at,
                    publisher = COALESCE(excluded.publisher, books.publisher),
                    length_minutes = COALESCE(excluded.length_minutes, books.length_minutes),
                    is_abridged = excluded.is_abridged,
                    content_kind = excluded.content_kind,
                    categories = COALESCE(excluded.categories, books.categories),
                    subtitle = COALESCE(excluded.subtitle, books.subtitle),
                    published_at = COALESCE(excluded.published_at, books.published_at),
                    updated_at = excluded.updated_at
                "#,
                params![
                    book.asin,
                    book.account_id,
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
        self.get_book(&book.asin, &book.account_id)?
            .ok_or_else(|| LibraryError::NotFound(book.asin.clone()))
    }

    pub fn get_book(&self, asin: &str, account_id: &str) -> Result<Option<BookRecord>> {
        self.with_conn(|conn| {
            conn.query_row(
                &format!("{BOOK_SELECT} WHERE asin = ?1 AND account_id = ?2"),
                params![asin, account_id],
                map_book_row,
            )
            .optional()
            .map_err(LibraryError::from)
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

    /// Update user-defined fields (tags, ratings, finished) without touching scan metadata.
    pub fn update_user_fields(
        &self,
        asin: &str,
        account_id: &str,
        fields: &UserBookFields,
    ) -> Result<()> {
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
                WHERE asin = ?7 AND account_id = ?8
                "#,
                params![
                    fields.tags,
                    fields.rating_overall,
                    fields.rating_performance,
                    fields.rating_story,
                    fields.is_finished.map(|b| if b { 1 } else { 0 }),
                    now,
                    asin,
                    account_id,
                ],
            )?;
            Ok(n)
        })?;
        if rows == 0 {
            return Err(LibraryError::NotFound(asin.into()));
        }
        Ok(())
    }

    pub fn set_pdf_status(
        &self,
        asin: &str,
        account_id: &str,
        status: LiberateStatus,
        pdf_storage_key: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let rows = self.with_conn(|conn| {
            let n = conn.execute(
                r#"
                UPDATE books SET pdf_status = ?1, pdf_storage_key = ?2, updated_at = ?3
                WHERE asin = ?4 AND account_id = ?5
                "#,
                params![status.as_str(), pdf_storage_key, now, asin, account_id],
            )?;
            Ok(n)
        })?;
        if rows == 0 {
            return Err(LibraryError::NotFound(asin.into()));
        }
        Ok(())
    }

    pub fn is_ignored(&self, asin: &str, account_id: &str) -> Result<bool> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT 1 FROM ignored_asins WHERE asin = ?1 AND account_id = ?2",
                params![asin, account_id],
                |_| Ok(true),
            )
            .optional()
            .map(|opt| opt.is_some())
            .map_err(LibraryError::from)
        })
    }

    pub fn set_ignored(
        &self,
        asin: &str,
        account_id: &str,
        ignored: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.with_conn(|conn| {
            if ignored {
                conn.execute(
                    r#"
                    INSERT INTO ignored_asins (asin, account_id, reason, created_at)
                    VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT(asin, account_id) DO UPDATE SET reason = excluded.reason
                    "#,
                    params![asin, account_id, reason, now],
                )?;
            } else {
                conn.execute(
                    "DELETE FROM ignored_asins WHERE asin = ?1 AND account_id = ?2",
                    params![asin, account_id],
                )?;
            }
            Ok(())
        })
    }

    pub fn set_liberate_status(
        &self,
        asin: &str,
        account_id: &str,
        status: LiberateStatus,
        storage_key: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let rows = self.with_conn(|conn| {
            let n = conn.execute(
                r#"
                UPDATE books SET
                    liberate_status = ?1,
                    storage_key = ?2,
                    error_message = ?3,
                    updated_at = ?4
                WHERE asin = ?5 AND account_id = ?6
                "#,
                params![
                    status.as_str(),
                    storage_key,
                    error_message,
                    now,
                    asin,
                    account_id
                ],
            )?;
            Ok(n)
        })?;
        if rows == 0 {
            return Err(LibraryError::NotFound(asin.into()));
        }
        Ok(())
    }

    /// Bulk-update liberate status (classic `set-status --force`).
    pub fn bulk_set_liberate_status(
        &self,
        account: Option<&str>,
        asins: &[String],
        status: LiberateStatus,
    ) -> Result<u32> {
        let books = self.list_books(account)?;
        let mut updated = 0u32;
        for book in books {
            if !asins.is_empty() && !asins.iter().any(|a| a.eq_ignore_ascii_case(&book.asin)) {
                continue;
            }
            self.set_liberate_status(
                &book.asin,
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

/// Input for inserting / updating a book from sync.
#[derive(Debug, Clone)]
pub struct NewBook {
    pub asin: String,
    pub account_id: String,
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
    /// Minimal book row with scan metadata defaults.
    #[must_use]
    pub fn minimal(
        asin: impl Into<String>,
        account_id: impl Into<String>,
        marketplace: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        Self {
            asin: asin.into(),
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
        asin: r.get("asin")?,
        account_id: r.get("account_id")?,
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

        let mut book = NewBook::minimal("B00TEST", "user-1", "us", "Test Book");
        book.authors = Some("Author".into());
        let book = store.upsert_book(&book).unwrap();
        assert_eq!(book.title, "Test Book");
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
}
