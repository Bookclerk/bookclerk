//! Import classic `LibationContext.db` into libation-rs library.db.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Utc};
use libation_library::{
    content_kind_from_classic, LiberateStatus, LibraryStore, NewBook, UserBookFields,
};
use rusqlite::Connection;

use crate::error::{MigrateError, Result};
use crate::files::{storage_key_for, AudioPathMap};

#[derive(Debug, Default)]
pub struct LibraryImportSummary {
    pub books: usize,
    pub liberated: usize,
    pub storage_keys: usize,
    pub warnings: Vec<String>,
}

/// Import books from classic EF Core SQLite `LibationContext.db`.
pub fn import_library_db(
    classic_db: &Path,
    store: &LibraryStore,
    audio_paths: &AudioPathMap,
    books_root: &Path,
    account_id_map: &HashMap<(String, String), String>,
    _dry_run: bool,
) -> Result<LibraryImportSummary> {
    let conn = Connection::open_with_flags(
        classic_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| {
        MigrateError::Library(format!("failed to open {}: {err}", classic_db.display()))
    })?;

    // Ensure expected tables exist (older DBs may differ slightly).
    let has_books: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='Books'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_books {
        return Err(MigrateError::Library(
            "LibationContext.db has no Books table".into(),
        ));
    }

    let sql = r#"
        SELECT
            b.AudibleProductId AS asin,
            b.Title AS title,
            b.Subtitle AS subtitle,
            b.Locale AS locale,
            b.ContentType AS content_type,
            b.LengthInMinutes AS length_minutes,
            b.IsAbridged AS is_abridged,
            b.DatePublished AS date_published,
            lb.Account AS account,
            lb.DateAdded AS date_added,
            lb.IsDeleted AS is_deleted,
            COALESCE(udi.BookStatus, 0) AS book_status,
            udi.Tags AS tags,
            udi.Rating_OverallRating AS rating_overall,
            udi.Rating_PerformanceRating AS rating_performance,
            udi.Rating_StoryRating AS rating_story,
            udi.IsFinished AS is_finished,
            (
                SELECT GROUP_CONCAT(c.Name, ', ')
                FROM BookContributor bc
                JOIN Contributors c ON c.ContributorId = bc.ContributorId
                WHERE bc.BookId = b.BookId AND bc.Role = 1
            ) AS authors,
            (
                SELECT GROUP_CONCAT(c.Name, ', ')
                FROM BookContributor bc
                JOIN Contributors c ON c.ContributorId = bc.ContributorId
                WHERE bc.BookId = b.BookId AND bc.Role = 2
            ) AS narrators,
            (
                SELECT s.Name
                FROM SeriesBook sb
                JOIN Series s ON s.SeriesId = sb.SeriesId
                WHERE sb.BookId = b.BookId
                LIMIT 1
            ) AS series_name,
            (
                SELECT sb."Order"
                FROM SeriesBook sb
                WHERE sb.BookId = b.BookId
                LIMIT 1
            ) AS series_order,
            (
                SELECT s.AudibleSeriesId
                FROM SeriesBook sb
                JOIN Series s ON s.SeriesId = sb.SeriesId
                WHERE sb.BookId = b.BookId
                LIMIT 1
            ) AS series_asin
        FROM LibraryBooks lb
        JOIN Books b ON b.BookId = lb.BookId
        LEFT JOIN UserDefinedItem udi ON udi.BookId = b.BookId
        WHERE lb.IsDeleted = 0
    "#;

    let mut stmt = conn
        .prepare(sql)
        .map_err(|err| MigrateError::Library(format!("query prepare failed: {err}")))?;

    let mut summary = LibraryImportSummary::default();
    let rows = stmt
        .query_map([], |row| {
            Ok(ClassicBookRow {
                asin: row.get::<_, String>(0)?,
                title: row.get::<_, String>(1)?,
                subtitle: row.get::<_, String>(2).unwrap_or_default(),
                locale: row.get::<_, String>(3).unwrap_or_else(|_| "us".into()),
                content_type: row.get::<_, i64>(4).unwrap_or(1),
                length_minutes: row.get::<_, Option<i64>>(5)?,
                is_abridged: row.get::<_, i64>(6).unwrap_or(0) != 0,
                date_published: row.get::<_, Option<String>>(7)?,
                account: row.get::<_, String>(8)?,
                date_added: row.get::<_, Option<String>>(9)?,
                book_status: row.get::<_, i64>(11).unwrap_or(0),
                tags: row.get::<_, Option<String>>(12)?,
                rating_overall: row.get::<_, Option<f64>>(13)?.map(|v| v as f32),
                rating_performance: row.get::<_, Option<f64>>(14)?.map(|v| v as f32),
                rating_story: row.get::<_, Option<f64>>(15)?.map(|v| v as f32),
                is_finished: row.get::<_, i64>(16).unwrap_or(0) != 0,
                authors: row.get::<_, Option<String>>(17)?,
                narrators: row.get::<_, Option<String>>(18)?,
                series_name: row.get::<_, Option<String>>(19)?,
                series_order: row.get::<_, Option<String>>(20)?,
                series_asin: row.get::<_, Option<String>>(21)?,
            })
        })
        .map_err(|err| MigrateError::Library(format!("query failed: {err}")))?;

    for row in rows {
        let row = row.map_err(|err| MigrateError::Library(format!("row read failed: {err}")))?;
        if row.asin.is_empty() {
            continue;
        }

        let account_id = account_id_map
            .get(&(row.account.clone(), row.locale.clone()))
            .cloned()
            .or_else(|| {
                // Locale mismatch fallback: any mapping for this AccountId.
                account_id_map
                    .iter()
                    .find(|((id, _), _)| id == &row.account)
                    .map(|(_, v)| v.clone())
            })
            .unwrap_or_else(|| row.account.clone());

        // Ensure account row exists even if AccountsSettings was missing.
        let _ = store.upsert_account(&account_id, &row.locale, None, true);

        let title = if row.subtitle.trim().is_empty() {
            row.title.clone()
        } else {
            format!("{}: {}", row.title, row.subtitle.trim())
        };

        let content_kind = content_kind_from_classic(row.content_type);
        // Classic clears series order on podcast parents.
        let series_index = if content_kind == "podcast" {
            None
        } else {
            row.series_order.filter(|s| !s.is_empty())
        };

        store.upsert_book(&NewBook {
            uuid: None,
            product_id: row.asin.clone(),
            asin: Some(row.asin.clone()),
            isbn: None,
            source: String::from("audible"),
            account_id: account_id.clone(),
            marketplace: row.locale.clone(),
            title,
            authors: row.authors.filter(|s| !s.is_empty()),
            narrators: row.narrators.filter(|s| !s.is_empty()),
            series: row.series_name.filter(|s| !s.is_empty()),
            series_index,
            series_asin: row.series_asin.filter(|s| !s.is_empty()),
            purchased_at: row.date_added.as_deref().and_then(parse_dt),
            publisher: None,
            length_minutes: row.length_minutes,
            is_abridged: row.is_abridged,
            content_kind,
            categories: None,
            subtitle: {
                let s = row.subtitle.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            },
            published_at: row.date_published.as_deref().and_then(parse_dt),
        })?;

        let status = LiberateStatus::from_classic(row.book_status);
        let storage_key = audio_paths
            .get(&row.asin)
            .map(|p| storage_key_for(p, books_root));
        if storage_key.is_some() {
            summary.storage_keys += 1;
        }
        if status == LiberateStatus::Liberated {
            summary.liberated += 1;
        }

        store.set_liberate_status(&row.asin, &account_id, status, storage_key.as_deref(), None)?;

        let has_user_fields = row.tags.as_ref().is_some_and(|s| !s.is_empty())
            || row.rating_overall.is_some()
            || row.rating_performance.is_some()
            || row.rating_story.is_some()
            || row.is_finished;
        if has_user_fields {
            store.update_user_fields(
                &row.asin,
                &account_id,
                &UserBookFields {
                    tags: row.tags.clone(),
                    rating_overall: row.rating_overall,
                    rating_performance: row.rating_performance,
                    rating_story: row.rating_story,
                    is_finished: Some(row.is_finished),
                },
            )?;
        }

        summary.books += 1;
    }

    Ok(summary)
}

struct ClassicBookRow {
    asin: String,
    title: String,
    subtitle: String,
    locale: String,
    content_type: i64,
    length_minutes: Option<i64>,
    is_abridged: bool,
    date_published: Option<String>,
    account: String,
    date_added: Option<String>,
    book_status: i64,
    tags: Option<String>,
    rating_overall: Option<f32>,
    rating_performance: Option<f32>,
    rating_story: Option<f32>,
    is_finished: bool,
    authors: Option<String>,
    narrators: Option<String>,
    series_name: Option<String>,
    series_order: Option<String>,
    series_asin: Option<String>,
}

fn parse_dt(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(dt.and_utc());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Some(dt.and_utc());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn imports_minimal_classic_db() {
        let dir = tempdir().unwrap();
        let classic = dir.path().join("LibationContext.db");
        {
            let conn = Connection::open(&classic).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE Books (
                    BookId INTEGER PRIMARY KEY,
                    AudibleProductId TEXT NOT NULL,
                    Title TEXT NOT NULL,
                    Subtitle TEXT NOT NULL DEFAULT '',
                    Description TEXT NOT NULL DEFAULT '',
                    LengthInMinutes INTEGER NOT NULL DEFAULT 0,
                    ContentType INTEGER NOT NULL DEFAULT 0,
                    Locale TEXT NOT NULL,
                    IsAbridged INTEGER NOT NULL DEFAULT 0,
                    IsSpatial INTEGER NOT NULL DEFAULT 0,
                    DatePublished TEXT,
                    Rating_OverallRating REAL NOT NULL DEFAULT 0,
                    Rating_PerformanceRating REAL NOT NULL DEFAULT 0,
                    Rating_StoryRating REAL NOT NULL DEFAULT 0
                );
                CREATE TABLE LibraryBooks (
                    BookId INTEGER PRIMARY KEY,
                    DateAdded TEXT NOT NULL,
                    Account TEXT NOT NULL,
                    IsDeleted INTEGER NOT NULL DEFAULT 0,
                    AbsentFromLastScan INTEGER NOT NULL DEFAULT 0,
                    IsAudiblePlus INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE UserDefinedItem (
                    BookId INTEGER PRIMARY KEY,
                    Tags TEXT NOT NULL DEFAULT '',
                    Rating_OverallRating REAL NOT NULL DEFAULT 0,
                    Rating_PerformanceRating REAL NOT NULL DEFAULT 0,
                    Rating_StoryRating REAL NOT NULL DEFAULT 0,
                    BookStatus INTEGER NOT NULL DEFAULT 0,
                    IsFinished INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE Contributors (
                    ContributorId INTEGER PRIMARY KEY,
                    Name TEXT NOT NULL
                );
                CREATE TABLE BookContributor (
                    BookId INTEGER NOT NULL,
                    ContributorId INTEGER NOT NULL,
                    Role INTEGER NOT NULL,
                    "Order" INTEGER NOT NULL,
                    PRIMARY KEY (BookId, ContributorId, Role)
                );
                CREATE TABLE Series (
                    SeriesId INTEGER PRIMARY KEY,
                    AudibleSeriesId TEXT NOT NULL,
                    Name TEXT
                );
                CREATE TABLE SeriesBook (
                    SeriesId INTEGER NOT NULL,
                    BookId INTEGER NOT NULL,
                    "Order" TEXT,
                    PRIMARY KEY (SeriesId, BookId)
                );

                INSERT INTO Books (BookId, AudibleProductId, Title, Subtitle, Locale)
                VALUES (1, 'B00TEST', 'Hello', 'World', 'us');
                INSERT INTO LibraryBooks (BookId, DateAdded, Account, IsDeleted)
                VALUES (1, '2024-01-02 03:04:05', 'user@example.com', 0);
                INSERT INTO UserDefinedItem (BookId, BookStatus) VALUES (1, 1);
                INSERT INTO Contributors (ContributorId, Name) VALUES (1, 'Ann Author');
                INSERT INTO BookContributor (BookId, ContributorId, Role, "Order")
                VALUES (1, 1, 1, 0);
                "#,
            )
            .unwrap();
        }

        let store = LibraryStore::open_in_memory().unwrap();
        let mut paths = AudioPathMap::new();
        paths.insert(
            "B00TEST".into(),
            Path::new("/books/Ann Author/Hello/B00TEST.m4b").to_path_buf(),
        );
        let map = HashMap::from([(("user@example.com".into(), "us".into()), "cust-1".into())]);
        let summary =
            import_library_db(&classic, &store, &paths, Path::new("/books"), &map, false).unwrap();
        assert_eq!(summary.books, 1);
        assert_eq!(summary.liberated, 1);
        assert_eq!(summary.storage_keys, 1);
        let book = store.get_book("B00TEST", "cust-1").unwrap().unwrap();
        assert_eq!(book.title, "Hello: World");
        assert_eq!(book.authors.as_deref(), Some("Ann Author"));
        assert_eq!(book.liberate_status, LiberateStatus::Liberated);
        assert_eq!(
            book.storage_key.as_deref(),
            Some("Ann Author/Hello/B00TEST.m4b")
        );
    }
}
