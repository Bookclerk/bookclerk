//! Export Bookclerk state to a classic Libation Files directory layout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bookclerk_config::Config;
use bookclerk_library::{content_kind_to_classic, AcquireTitle, LibraryStore, UserTitleState};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

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
    /// Whose tags, ratings and finished flags to export (login name or
    /// user id); `None` exports every title with them empty, since they
    /// belong to each person.
    pub user: Option<String>,
}

/// Summary of a Libation export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibationExportSummary {
    /// Count of settings keys imported or exported.
    pub settings: bool,
    /// Count of storefront identities exported (JSON key remains `accounts`).
    #[serde(rename = "accounts")]
    pub storefronts: usize,
    /// Count of book rows imported or exported.
    pub books: usize,
    /// Non-fatal warnings collected during the run (operator-facing).
    pub warnings: Vec<String>,
}

/// Write Settings.json and LibationContext.db.
///
/// `AccountsSettings.json` is written separately by
/// [`crate::write_classic_storefront_file`] so the operator-facing summary
/// never carries `list_accounts()` payloads.
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
    if !opts.dry_run && opts.dest.exists() && !opts.force {
        return Err(MigrateError::Source(format!(
            "destination {} already exists (pass --force)",
            opts.dest.display()
        )));
    }

    let config_path = opts.files_dir.join("config.toml");
    let mut config = if config_path.exists() {
        Config::from_toml_file(&config_path)?
    } else {
        summary
            .warnings
            .push("config.toml missing — exporting default settings".into());
        Config::default()
    };

    let library_db = opts.files_dir.join("library.db");
    let dest = if library_db.exists() {
        Some(crate::store::DestStore::open(&opts.files_dir, false).await?)
    } else {
        summary
            .warnings
            .push("library.db missing — accounts/books empty".into());
        None
    };

    // The person is checked before anything is written, so a mistyped
    // `--user` leaves the destination as it was.
    let user = match (opts.user.as_deref(), dest.as_ref()) {
        (Some(name), Some(dest)) => Some(user_named(&dest.store, name).await?),
        (Some(_), None) => {
            return Err(MigrateError::Source(
                "--user needs a library.db to read the person from".into(),
            ));
        }
        (None, _) => None,
    };
    if !opts.dry_run && !opts.dest.exists() {
        std::fs::create_dir_all(&opts.dest)?;
    }

    // Packaging settings are the `core.output` document once a host has
    // enrolled; `[output]` in config.toml is only the first-run seed.
    if let Some(dest) = dest.as_ref() {
        match bookclerk_library::control_plane::load_output(&dest.store).await {
            Ok(doc) => doc.body.apply_to(&mut config.output, None),
            Err(err) => summary.warnings.push(format!(
                "core.output unavailable — exporting config.toml [output]: {err}"
            )),
        }
    }

    // The books directory and Audible bitrate live on plugin instances;
    // export writes Libation defaults for them.
    let settings = config_to_settings_json(&config, &crate::ClassicPluginSettings::default());
    summary.settings = true;
    if !opts.dry_run {
        let path = opts.dest.join("Settings.json");
        let bytes = serde_json::to_vec_pretty(&settings)
            .map_err(|e| MigrateError::Settings(e.to_string()))?;
        std::fs::write(&path, bytes)?;
    }
    if let Some(dest) = dest.as_ref() {
        summary.storefronts = usize::try_from(dest.store.count_identities().await?).unwrap_or(0);
    }

    let books = match dest.as_ref() {
        Some(dest) => export_books(&dest.store, user).await?,
        None => Vec::new(),
    };
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

/// The user `name` names: a login name, else a numeric user id.
///
/// # Errors
///
/// Returns [`MigrateError::Source`] when no user matches, or the store's
/// error when a read fails.
async fn user_named(store: &LibraryStore, name: &str) -> Result<i64> {
    let name = name.trim();
    if let Some(user) = store.get_user_by_login_name(name).await? {
        return Ok(user.id);
    }
    if let Ok(id) = name.parse::<i64>() {
        if let Some(user) = store.get_user(id).await? {
            return Ok(user.id);
        }
    }
    Err(MigrateError::Source(format!(
        "no user with login name or id `{name}`"
    )))
}

/// One title of the export: an account's title and, when a person was
/// named, their state for its work.
struct ExportBook {
    /// The account title.
    title: AcquireTitle,
    /// The person's state for the title's work, when they have one.
    state: Option<UserTitleState>,
}

/// Every account title in the library, with `user`'s state for each
/// title's work when a user is given.
///
/// # Errors
///
/// Returns the store's error when a read fails.
async fn export_books(store: &LibraryStore, user: Option<i64>) -> Result<Vec<ExportBook>> {
    let mut titles = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store
            .acquire_titles_page(None, after.as_deref(), 64)
            .await?;
        titles.extend(page.titles);
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let mut states: HashMap<String, UserTitleState> = HashMap::new();
    let mut works = std::collections::BTreeMap::new();
    if let Some(user) = user {
        works = store
            .works_of_editions(titles.iter().map(|t| t.edition_id.clone()).collect())
            .await?;
        let work_ids: Vec<String> = works.values().cloned().collect();
        states = store
            .user_title_states_of_works(user, &work_ids)
            .await?
            .into_iter()
            .map(|state| (state.work_id.clone(), state))
            .collect();
    }
    Ok(titles
        .into_iter()
        .map(|title| {
            let state = works
                .get(&title.edition_id)
                .and_then(|work| states.get(work))
                .cloned();
            ExportBook { title, state }
        })
        .collect())
}

/// Creates a classic `LibationContext.db` and inserts books, contributors, and series.
fn write_libation_context_db(path: &Path, books: &[ExportBook]) -> Result<()> {
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

    let exported_at = chrono::Utc::now();
    for (book_id, ExportBook { title: book, state }) in (1i64..).zip(books) {
        let content_type = content_kind_to_classic(book.content_kind);
        let title = book.title.clone();
        let subtitle = book.subtitle.clone().unwrap_or_default();
        let length = book.length_minutes.unwrap_or(0);
        let product_id = book
            .asin
            .as_deref()
            .or(book.isbn.as_deref())
            .unwrap_or(book.product_id.as_str())
            .to_string();
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
                i32::from(book.is_abridged),
                date_published,
            ],
        )?;

        // The purchase time, else when this library first held the title;
        // the export time only for a title with neither.
        let date_added = book
            .purchased_at
            .or(book.added_at)
            .unwrap_or(exported_at)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        conn.execute(
            r#"INSERT INTO "LibraryBooks" (
                "BookId", "DateAdded", "Account", "IsDeleted", "AbsentFromLastScan",
                "IncludedUntil", "IsAudiblePlus"
            ) VALUES (?1,?2,?3,0,0,NULL,0)"#,
            params![book_id, date_added, book.account_id],
        )?;

        let tags = state
            .as_ref()
            .and_then(|state| state.tags.clone())
            .unwrap_or_default();
        let rating =
            |pick: fn(&UserTitleState) -> Option<f64>| state.as_ref().and_then(pick).unwrap_or(0.0);
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
                rating(|state| state.rating_overall),
                rating(|state| state.rating_performance),
                rating(|state| state.rating_story),
                book.status.to_classic(),
                book.pdf_status.to_classic(),
                i32::from(state.as_ref().is_some_and(|state| state.is_finished)),
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
            // A series id is the store's own, so two stores' series that
            // share an id stay apart.
            let series_key = match book.series_product_id.as_deref() {
                Some(id) => format!("id:{}\u{1f}{}\u{1f}{id}", book.plugin_key, book.marketplace),
                None => format!("name:{series_name}"),
            };
            let sid = if let Some(id) = series_ids.get(&series_key) {
                *id
            } else {
                let id = next_series;
                next_series += 1;
                let audible_id = book
                    .series_product_id
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn libation_export_summary_is_cardinality_only() {
        let src = tempdir().unwrap();
        let dest = tempdir().unwrap();
        let store = crate::store::DestStore::open(src.path(), false)
            .await
            .unwrap();
        store
            .store
            .upsert_account(
                "audible",
                "reader@example.com",
                "us",
                Some("Reader"),
                true,
                "audible",
            )
            .await
            .unwrap();
        drop(store);

        let summary = export_libation(LibationExportOptions {
            files_dir: src.path().to_path_buf(),
            dest: dest.path().to_path_buf(),
            force: true,
            dry_run: false,
            user: None,
        })
        .await
        .unwrap();

        assert_eq!(summary.storefronts, 1);
        assert!(summary
            .warnings
            .iter()
            .all(|w| !w.contains("reader@example.com")));
        crate::write_classic_storefront_file(src.path(), dest.path())
            .await
            .unwrap();
        let json = std::fs::read_to_string(dest.path().join("AccountsSettings.json")).unwrap();
        assert!(json.contains("reader@example.com"));
        assert!(json.contains("Reader"));
        let encoded = serde_json::to_value(&summary).unwrap();
        assert_eq!(encoded["accounts"], 1);
        assert!(encoded.get("storefronts").is_none());
    }

    #[tokio::test]
    async fn libation_export_dry_run_counts_without_writing_accounts() {
        let src = tempdir().unwrap();
        let dest = tempdir().unwrap();
        let store = crate::store::DestStore::open(src.path(), false)
            .await
            .unwrap();
        store
            .store
            .upsert_account("audible", "reader@example.com", "us", None, true, "audible")
            .await
            .unwrap();
        drop(store);

        let summary = export_libation(LibationExportOptions {
            files_dir: src.path().to_path_buf(),
            dest: dest.path().to_path_buf(),
            force: true,
            dry_run: true,
            user: None,
        })
        .await
        .unwrap();

        assert_eq!(summary.storefronts, 1);
        assert!(!dest.path().join("AccountsSettings.json").exists());
    }

    /// Without a person every title goes out with empty tags, zero
    /// ratings and unfinished; with one, their own state for the work.
    #[tokio::test]
    async fn libation_export_writes_one_persons_state_only_when_named() {
        let src = tempdir().unwrap();
        let store = crate::store::DestStore::open(src.path(), false)
            .await
            .unwrap();
        let library = &store.store;
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        library
            .upsert_book(&bookclerk_library::NewBook {
                series: Some("Saga".into()),
                series_index: Some("2".into()),
                authors: Some("Ann Author".into()),
                narrators: Some("Ned Narrator".into()),
                ..bookclerk_library::NewBook::minimal("B0LIBATION", "acct", "us", "Exported")
            })
            .await
            .unwrap();
        library
            .set_acquire_status(
                "B0LIBATION",
                "acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some("k/Exported.m4b"),
                None,
            )
            .await
            .unwrap();
        let riley = library
            .create_user_with_login(
                bookclerk_library::UserRole::Member,
                Some("Riley"),
                Some("riley"),
                None,
            )
            .await
            .unwrap();
        let books = export_books(library, None).await.unwrap();
        let edition = books[0].title.edition_id.clone();
        let work = library
            .works_of_editions(std::iter::once(edition.clone()).collect())
            .await
            .unwrap()
            .remove(&edition)
            .unwrap();
        library
            .set_user_title_state(
                riley.id,
                &work,
                &bookclerk_library::UserBookFields {
                    tags: Some("favourite".into()),
                    rating_overall: Some(4.5),
                    is_finished: Some(true),
                    ..bookclerk_library::UserBookFields::default()
                },
            )
            .await
            .unwrap();
        drop(store);

        let row = |dest: &Path| -> (String, f64, i64, i64, String) {
            let conn = Connection::open(dest.join("LibationContext.db")).unwrap();
            conn.query_row(
                r#"SELECT u."Tags", u."Rating_OverallRating", u."IsFinished", u."BookStatus",
                          b."AudibleProductId"
                   FROM "UserDefinedItem" u JOIN "Books" b ON b."BookId" = u."BookId""#,
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
        };

        for (user, expect_tags, expect_rating, expect_finished) in [
            (None, "", 0.0, 0),
            (Some("riley".to_string()), "favourite", 4.5, 1),
            (Some(riley.id.to_string()), "favourite", 4.5, 1),
        ] {
            let dest = tempdir().unwrap();
            let summary = export_libation(LibationExportOptions {
                files_dir: src.path().to_path_buf(),
                dest: dest.path().to_path_buf(),
                force: true,
                dry_run: false,
                user,
            })
            .await
            .unwrap();
            assert_eq!(summary.books, 1);
            let (tags, rating, finished, status, product) = row(dest.path());
            assert_eq!(tags, expect_tags);
            assert!((rating - expect_rating).abs() < f64::EPSILON);
            assert_eq!(finished, expect_finished);
            assert_eq!(
                status,
                i64::from(bookclerk_library::AcquireStatus::Acquired.to_classic())
            );
            assert_eq!(product, "B0LIBATION");
            let conn = Connection::open(dest.path().join("LibationContext.db")).unwrap();
            let series: (String, String) = conn
                .query_row(
                    r#"SELECT s."Name", sb."Order" FROM "SeriesBook" sb
                       JOIN "Series" s ON s."SeriesId" = sb."SeriesId""#,
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(series, ("Saga".to_string(), "2".to_string()));
            let contributors: i64 = conn
                .query_row(r#"SELECT COUNT(*) FROM "BookContributor""#, [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(contributors, 2);
        }

        let parent = tempdir().unwrap();
        let out = parent.path().join("out");
        let err = export_libation(LibationExportOptions {
            files_dir: src.path().to_path_buf(),
            dest: out.clone(),
            force: true,
            dry_run: false,
            user: Some("nobody".into()),
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("nobody"), "{err}");
        assert!(
            !out.exists(),
            "an unknown person must leave the destination unwritten"
        );

        // Two stores may use the same series id for different series; the
        // same store's titles of one series still share a row.
        let store = crate::store::DestStore::open(src.path(), false)
            .await
            .unwrap();
        let mine = export_books(&store.store, None).await.unwrap().remove(0);
        let mut first = mine.title.clone();
        first.series_product_id = Some("42".into());
        let mut sibling = first.clone();
        sibling.product_id = "B0LIBATION2".into();
        let mut other_store = first.clone();
        other_store.plugin_key = "libro".into();
        other_store.product_id = "9780000000002".into();
        other_store.series = Some("Another".into());
        let books: Vec<ExportBook> = [first, sibling, other_store]
            .into_iter()
            .map(|title| ExportBook { title, state: None })
            .collect();
        let dest = tempdir().unwrap();
        let db = dest.path().join("LibationContext.db");
        write_libation_context_db(&db, &books).unwrap();
        let conn = Connection::open(&db).unwrap();
        let series: i64 = conn
            .query_row(r#"SELECT COUNT(*) FROM "Series""#, [], |r| r.get(0))
            .unwrap();
        assert_eq!(series, 2);
    }
}
