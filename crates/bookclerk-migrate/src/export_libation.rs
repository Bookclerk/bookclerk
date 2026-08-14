//! Export Bookclerk state to a classic Libation Files directory layout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bookclerk_config::Config;
use bookclerk_library::content_kind_to_classic;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{MigrateError, Result};
use crate::settings::config_to_settings_json;

/// Options for Libation-compatible export.
#[derive(Debug, Clone)]
pub struct LibationExportOptions {
    /// Bookclerk or Libation files directory root for this operation.
    pub files_dir: PathBuf,
    /// Destination path for the export archive or directory.
    pub dest: PathBuf,
    /// When true, overwrite existing data instead of failing on conflict.
    pub force: bool,
    /// When true, report what would change without writing files.
    pub dry_run: bool,
}

/// Summary of a Libation export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibationExportSummary {
    /// Count of settings keys imported or exported.
    pub settings: bool,
    /// Count of accounts imported or exported.
    pub accounts: usize,
    /// Count of book rows imported or exported.
    pub books: usize,
    /// Non-fatal warnings collected during the run (operator-facing).
    pub warnings: Vec<String>,
}

/// Write Settings.json, AccountsSettings.json, and LibationContext.db.
///
/// # Arguments
///
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `LibationExportSummary` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn export_libation(opts: LibationExportOptions) -> Result<LibationExportSummary> {
    let mut summary = LibationExportSummary::default();
    if !opts.dry_run {
        if opts.dest.exists() {
            if !opts.force {
                return Err(MigrateError::Source(format!(
                    "destination {} already exists (pass --force)",
                    opts.dest.display()
                )));
            }
        } else {
            std::fs::create_dir_all(&opts.dest)?;
        }
    }

    let config_path = opts.files_dir.join("config.toml");
    let config = if config_path.exists() {
        Config::from_toml_file(&config_path)?
    } else {
        summary
            .warnings
            .push("config.toml missing — exporting default settings".into());
        Config::default()
    };

    let settings = config_to_settings_json(&config);
    summary.settings = true;
    if !opts.dry_run {
        let path = opts.dest.join("Settings.json");
        let bytes = serde_json::to_vec_pretty(&settings)
            .map_err(|e| MigrateError::Settings(e.to_string()))?;
        std::fs::write(&path, bytes)?;
    }

    let library_db = opts.files_dir.join("library.db");
    let store = if library_db.exists() {
        bookclerk_plugin_database_sqlite::open_store(&library_db).await?
    } else {
        summary
            .warnings
            .push("library.db missing — accounts/books empty".into());
        bookclerk_plugin_database_sqlite::open_store_memory().await?
    };

    let accounts = store.list_accounts().await?;
    summary.accounts = accounts.len();
    let accounts_json = accounts_to_libation_json(&accounts);
    if !opts.dry_run {
        let path = opts.dest.join("AccountsSettings.json");
        let bytes = serde_json::to_vec_pretty(&accounts_json)
            .map_err(|e| MigrateError::Accounts(e.to_string()))?;
        std::fs::write(&path, bytes)?;
    }

    let books = store.list_books(None).await?;
    summary.books = books.len();
    if !opts.dry_run {
        let db_path = opts.dest.join("LibationContext.db");
        if db_path.exists() {
            std::fs::remove_file(&db_path)?;
        }
        write_libation_context_db(&db_path, &books)?;
    }

    Ok(summary)
}

/// Projects Bookclerk account rows into classic `AccountsSettings.json` shape.
fn accounts_to_libation_json(accounts: &[bookclerk_library::AccountRecord]) -> Value {
    let list: Vec<Value> = accounts
        .iter()
        .map(|a| {
            json!({
                "AccountId": a.account_id,
                "AccountName": a.label.clone().unwrap_or_else(|| a.account_id.clone()),
                "IdentityTokens": {
                    "Locale": a.marketplace,
                },
                "LibraryScan": a.scan_enabled,
            })
        })
        .collect();
    json!({ "Accounts": list })
}

/// Creates a classic `LibationContext.db` and inserts books, contributors, and series.
fn write_libation_context_db(path: &Path, books: &[bookclerk_library::BookRecord]) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(CLASSIC_SQLITE_DDL)?;
    conn.execute(
        r#"INSERT INTO "Contributors" ("ContributorId", "Name", "AudibleContributorId")
           VALUES (-1, '', NULL)"#,
        [],
    )?;

    let mut contributor_ids: HashMap<String, i32> = HashMap::new();
    let mut series_ids: HashMap<String, i32> = HashMap::new();
    let mut next_contributor = 1i32;
    let mut next_series = 1i32;

    for book in books {
        let book_id = book.id as i32;
        let content_type = content_kind_to_classic(&book.content_kind);
        let title = book.title.clone();
        let subtitle = book.subtitle.clone().unwrap_or_default();
        let length = book.length_minutes.unwrap_or(0) as i32;
        let product_id = book.asin_or_isbn().to_string();
        let date_published = book
            .published_at
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string());

        conn.execute(
            r#"INSERT INTO "Books" (
                "BookId", "AudibleProductId", "Title", "Subtitle", "Description",
                "LengthInMinutes", "ContentType", "Locale", "PictureId", "PictureLarge",
                "IsAbridged", "IsSpatial", "DatePublished", "Language",
                "Rating_OverallRating", "Rating_PerformanceRating", "Rating_StoryRating"
            ) VALUES (?1,?2,?3,?4,'',?5,?6,?7,NULL,NULL,?8,0,?9,NULL,0,0,0)"#,
            params![
                book_id,
                product_id,
                title,
                subtitle,
                length,
                content_type,
                book.marketplace,
                book.is_abridged as i32,
                date_published,
            ],
        )?;

        let date_added = book
            .purchased_at
            .unwrap_or(book.created_at)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        conn.execute(
            r#"INSERT INTO "LibraryBooks" (
                "BookId", "DateAdded", "Account", "IsDeleted", "AbsentFromLastScan",
                "IncludedUntil", "IsAudiblePlus"
            ) VALUES (?1,?2,?3,0,0,NULL,0)"#,
            params![book_id, date_added, book.account_id],
        )?;

        let tags = book.tags.clone().unwrap_or_default();
        conn.execute(
            r#"INSERT INTO "UserDefinedItem" (
                "BookId", "LastDownloaded", "LastDownloadedVersion", "LastDownloadedFormat",
                "LastDownloadedFileVersion", "Tags",
                "Rating_OverallRating", "Rating_PerformanceRating", "Rating_StoryRating",
                "BookStatus", "PdfStatus", "IsFinished"
            ) VALUES (?1,NULL,NULL,NULL,NULL,?2,?3,?4,?5,?6,?7,?8)"#,
            params![
                book_id,
                tags,
                book.rating_overall.unwrap_or(0.0),
                book.rating_performance.unwrap_or(0.0),
                book.rating_story.unwrap_or(0.0),
                book.acquire_status.to_classic(),
                book.pdf_status.to_classic(),
                book.is_finished as i32,
            ],
        )?;

        for (role, names) in [
            (1i32, book.authors.as_deref()),
            (2i32, book.narrators.as_deref()),
        ] {
            let Some(name) = names.map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            let cid = if let Some(id) = contributor_ids.get(name) {
                *id
            } else {
                let id = next_contributor;
                next_contributor += 1;
                conn.execute(
                    r#"INSERT INTO "Contributors" ("ContributorId", "Name", "AudibleContributorId")
                       VALUES (?1,?2,NULL)"#,
                    params![id, name],
                )?;
                contributor_ids.insert(name.to_string(), id);
                id
            };
            conn.execute(
                r#"INSERT OR IGNORE INTO "BookContributor" ("BookId", "ContributorId", "Role", "Order")
                   VALUES (?1,?2,?3,0)"#,
                params![book_id, cid, role],
            )?;
        }

        if let Some(series_name) = book.series.as_deref().filter(|s| !s.is_empty()) {
            let series_key = book
                .series_asin
                .clone()
                .unwrap_or_else(|| format!("name:{series_name}"));
            let sid = if let Some(id) = series_ids.get(&series_key) {
                *id
            } else {
                let id = next_series;
                next_series += 1;
                let audible_id = book
                    .series_asin
                    .clone()
                    .unwrap_or_else(|| format!("GEN-{id}"));
                conn.execute(
                    r#"INSERT INTO "Series" ("SeriesId", "AudibleSeriesId", "Name")
                       VALUES (?1,?2,?3)"#,
                    params![id, audible_id, series_name],
                )?;
                series_ids.insert(series_key, id);
                id
            };
            let order: Option<String> = if book.content_kind == "podcast" {
                None
            } else {
                book.series_index.clone()
            };
            conn.execute(
                r#"INSERT OR IGNORE INTO "SeriesBook" ("SeriesId", "BookId", "Order")
                   VALUES (?1,?2,?3)"#,
                params![sid, book_id, order],
            )?;
        }
    }

    Ok(())
}

/// Minimal classic Libation SQLite schema used for export (Books, Contributors, Series, …).
const CLASSIC_SQLITE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS "Books" (
    "BookId" INTEGER PRIMARY KEY,
    "AudibleProductId" TEXT NOT NULL,
    "Title" TEXT NOT NULL,
    "Subtitle" TEXT NOT NULL DEFAULT '',
    "Description" TEXT NOT NULL DEFAULT '',
    "LengthInMinutes" INTEGER NOT NULL DEFAULT 0,
    "ContentType" INTEGER NOT NULL DEFAULT 1,
    "Locale" TEXT NOT NULL,
    "PictureId" TEXT,
    "PictureLarge" TEXT,
    "IsAbridged" INTEGER NOT NULL DEFAULT 0,
    "IsSpatial" INTEGER NOT NULL DEFAULT 0,
    "DatePublished" TEXT,
    "Language" TEXT,
    "Rating_OverallRating" REAL NOT NULL DEFAULT 0,
    "Rating_PerformanceRating" REAL NOT NULL DEFAULT 0,
    "Rating_StoryRating" REAL NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS "IX_Books_AudibleProductId" ON "Books" ("AudibleProductId");

CREATE TABLE IF NOT EXISTS "Contributors" (
    "ContributorId" INTEGER PRIMARY KEY,
    "Name" TEXT NOT NULL,
    "AudibleContributorId" TEXT
);

CREATE TABLE IF NOT EXISTS "Series" (
    "SeriesId" INTEGER PRIMARY KEY,
    "AudibleSeriesId" TEXT NOT NULL,
    "Name" TEXT
);

CREATE TABLE IF NOT EXISTS "Categories" (
    "CategoryId" INTEGER PRIMARY KEY,
    "AudibleCategoryId" TEXT NOT NULL,
    "Name" TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS "CategoryLadders" (
    "CategoryLadderId" INTEGER PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS "LibraryBooks" (
    "BookId" INTEGER PRIMARY KEY REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "DateAdded" TEXT NOT NULL,
    "Account" TEXT NOT NULL,
    "IsDeleted" INTEGER NOT NULL DEFAULT 0,
    "AbsentFromLastScan" INTEGER NOT NULL DEFAULT 0,
    "IncludedUntil" TEXT,
    "IsAudiblePlus" INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS "UserDefinedItem" (
    "BookId" INTEGER PRIMARY KEY REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "LastDownloaded" TEXT,
    "LastDownloadedVersion" TEXT,
    "LastDownloadedFormat" INTEGER,
    "LastDownloadedFileVersion" TEXT,
    "Tags" TEXT NOT NULL DEFAULT '',
    "Rating_OverallRating" REAL NOT NULL DEFAULT 0,
    "Rating_PerformanceRating" REAL NOT NULL DEFAULT 0,
    "Rating_StoryRating" REAL NOT NULL DEFAULT 0,
    "BookStatus" INTEGER NOT NULL DEFAULT 0,
    "PdfStatus" INTEGER,
    "IsFinished" INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS "Supplement" (
    "SupplementId" INTEGER PRIMARY KEY,
    "BookId" INTEGER NOT NULL REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "Url" TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS "BookContributor" (
    "BookId" INTEGER NOT NULL REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "ContributorId" INTEGER NOT NULL REFERENCES "Contributors"("ContributorId") ON DELETE CASCADE,
    "Role" INTEGER NOT NULL,
    "Order" INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY ("BookId", "ContributorId", "Role")
);

CREATE TABLE IF NOT EXISTS "SeriesBook" (
    "SeriesId" INTEGER NOT NULL REFERENCES "Series"("SeriesId") ON DELETE CASCADE,
    "BookId" INTEGER NOT NULL REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "Order" TEXT,
    PRIMARY KEY ("SeriesId", "BookId")
);

CREATE TABLE IF NOT EXISTS "BookCategory" (
    "BookId" INTEGER NOT NULL REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "CategoryLadderId" INTEGER NOT NULL REFERENCES "CategoryLadders"("CategoryLadderId") ON DELETE CASCADE,
    PRIMARY KEY ("BookId", "CategoryLadderId")
);

CREATE TABLE IF NOT EXISTS "CategoryCategoryLadder" (
    "_categoriesCategoryId" INTEGER NOT NULL REFERENCES "Categories"("CategoryId") ON DELETE CASCADE,
    "_categoryLaddersCategoryLadderId" INTEGER NOT NULL REFERENCES "CategoryLadders"("CategoryLadderId") ON DELETE CASCADE,
    PRIMARY KEY ("_categoriesCategoryId", "_categoryLaddersCategoryLadderId")
);
"#;
