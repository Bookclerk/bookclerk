//! `bookclerk export postgres` (hidden alias: `copydb`) — SQLite → PostgreSQL.
//!
//! Default `--format flat` writes the native bookclerk schema (`accounts` / `books`).
//! `--format classic` writes the Libation EF schema (`Books`, `LibraryBooks`, …).

use std::collections::HashMap;
use std::path::PathBuf;

use bookclerk_config::Config;
use bookclerk_library::{content_kind_to_classic, AcquireStatus};
use clap::{Args, ValueEnum};
use rusqlite::Connection;

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum CopyDbFormat {
    /// Classic Libation EF / Postgres schema.
    Classic,
    /// Native bookclerk flat schema.
    #[default]
    Flat,
}

#[derive(Debug, Args)]
pub struct CopyDbArgs {
    /// PostgreSQL connection string.
    #[arg(short = 'c', long)]
    connection: String,
    /// Source SQLite path (default: `{files_dir}/library.db`).
    #[arg(long)]
    source: Option<PathBuf>,
    /// Output schema format.
    #[arg(long, value_enum, default_value_t = CopyDbFormat::Classic)]
    format: CopyDbFormat,
}

pub async fn run(args: CopyDbArgs, config: &Config) -> anyhow::Result<()> {
    let paths = config.paths();
    let source = args.source.unwrap_or_else(|| paths.library_db.clone());
    // Ensure schema migrations (e.g. series_asin) are applied before export.
    let _ = bookclerk_plugin_database::sqlite::open_store(&source).await?;
    let conn = Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (mut client, connection) =
        tokio_postgres::connect(&args.connection, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            tracing::error!(error = %err, "postgres connection error");
        }
    });

    match args.format {
        CopyDbFormat::Classic => export_classic(&conn, &mut client, &source).await,
        CopyDbFormat::Flat => export_flat(&conn, &mut client, &source).await,
    }
}

async fn export_classic(
    conn: &Connection,
    client: &mut tokio_postgres::Client,
    source: &std::path::Path,
) -> anyhow::Result<()> {
    client.batch_execute(CLASSIC_DDL).await?;

    let tx = client.transaction().await?;
    // FK-safe wipe order.
    for table in [
        "BookCategory",
        "CategoryCategoryLadder",
        "SeriesBook",
        "BookContributor",
        "Supplement",
        "UserDefinedItem",
        "LibraryBooks",
        "Books",
        "CategoryLadders",
        "Categories",
        "Series",
        "Contributors",
    ] {
        tx.execute(&format!("DELETE FROM \"{table}\""), &[]).await?;
    }

    // Seed empty contributor (classic EF HasData ContributorId = -1).
    tx.execute(
        r#"INSERT INTO "Contributors" ("ContributorId", "Name", "AudibleContributorId")
           VALUES (-1, '', NULL)
           ON CONFLICT ("ContributorId") DO NOTHING"#,
        &[],
    )
    .await?;

    let books = load_flat_books(conn)?;
    let mut contributor_ids: HashMap<String, i32> = HashMap::new();
    let mut series_ids: HashMap<String, i32> = HashMap::new();
    let mut next_contributor = 1i32;
    let mut next_series = 1i32;
    let mut book_count = 0usize;

    for book in &books {
        let book_id = book.id as i32;
        let content_type = content_kind_to_classic(&book.content_kind);
        let (title, subtitle) = split_title_subtitle(&book.title, book.subtitle.as_deref());
        let date_published = book.published_at.map(|d| d.naive_utc());
        let length = book.length_minutes.unwrap_or(0) as i32;

        tx.execute(
            r#"INSERT INTO "Books" (
                "BookId", "AudibleProductId", "Title", "Subtitle", "Description",
                "LengthInMinutes", "ContentType", "Locale", "PictureId", "PictureLarge",
                "IsAbridged", "IsSpatial", "DatePublished", "Language",
                "Rating_OverallRating", "Rating_PerformanceRating", "Rating_StoryRating"
            ) VALUES ($1,$2,$3,$4,'',$5,$6,$7,NULL,NULL,$8,FALSE,$9,NULL,0,0,0)"#,
            &[
                &book_id,
                &book.product_id,
                &title,
                &subtitle,
                &length,
                &content_type,
                &book.marketplace,
                &book.is_abridged,
                &date_published,
            ],
        )
        .await?;

        let date_added = book.purchased_at.unwrap_or(book.created_at).naive_utc();
        tx.execute(
            r#"INSERT INTO "LibraryBooks" (
                "BookId", "DateAdded", "Account", "IsDeleted", "AbsentFromLastScan",
                "IncludedUntil", "IsAudiblePlus"
            ) VALUES ($1,$2,$3,FALSE,FALSE,NULL,FALSE)"#,
            &[&book_id, &date_added, &book.account_id],
        )
        .await?;

        let book_status = AcquireStatus::parse(&book.acquire_status)
            .unwrap_or(AcquireStatus::NotAcquired)
            .to_classic();
        let pdf_status = AcquireStatus::parse(&book.pdf_status)
            .unwrap_or(AcquireStatus::NotAcquired)
            .to_classic();
        let tags = book.tags.clone().unwrap_or_default();
        let rating_o = book.rating_overall.unwrap_or(0.0);
        let rating_p = book.rating_performance.unwrap_or(0.0);
        let rating_s = book.rating_story.unwrap_or(0.0);
        tx.execute(
            r#"INSERT INTO "UserDefinedItem" (
                "BookId", "LastDownloaded", "LastDownloadedVersion", "LastDownloadedFormat",
                "LastDownloadedFileVersion", "Tags",
                "Rating_OverallRating", "Rating_PerformanceRating", "Rating_StoryRating",
                "BookStatus", "PdfStatus", "IsFinished"
            ) VALUES ($1,NULL,NULL,NULL,NULL,$2,$3,$4,$5,$6,$7,$8)"#,
            &[
                &book_id,
                &tags,
                &rating_o,
                &rating_p,
                &rating_s,
                &book_status,
                &pdf_status,
                &book.is_finished,
            ],
        )
        .await?;

        // Authors / narrators: keep the SQLite display string as one Contributor
        // name. Sync joins people with ", ", so splitting on commas would corrupt
        // legitimate names ("Last, First", "Name, PhD"). Classic AuthorNames is
        // also a ", "-joined display string, so a single contributor preserves
        // round-trip text; Audible re-scan recreates per-person rows when needed.
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
                tx.execute(
                    r#"INSERT INTO "Contributors" ("ContributorId", "Name", "AudibleContributorId")
                       VALUES ($1,$2,NULL)"#,
                    &[&id, &name],
                )
                .await?;
                contributor_ids.insert(name.to_string(), id);
                id
            };
            tx.execute(
                r#"INSERT INTO "BookContributor" ("BookId", "ContributorId", "Role", "Order")
                   VALUES ($1,$2,$3,0)
                   ON CONFLICT DO NOTHING"#,
                &[&book_id, &cid, &role],
            )
            .await?;
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
                tx.execute(
                    r#"INSERT INTO "Series" ("SeriesId", "AudibleSeriesId", "Name")
                       VALUES ($1,$2,$3)"#,
                    &[&id, &audible_id, &series_name],
                )
                .await?;
                series_ids.insert(series_key, id);
                id
            };
            // Classic clears series order on podcast parents.
            let order: Option<String> = if book.content_kind == "podcast" {
                None
            } else {
                book.series_index.clone()
            };
            tx.execute(
                r#"INSERT INTO "SeriesBook" ("SeriesId", "BookId", "Order")
                   VALUES ($1,$2,$3)
                   ON CONFLICT DO NOTHING"#,
                &[&sid, &book_id, &order],
            )
            .await?;
        }

        book_count += 1;
    }

    // Explicit BookId / SeriesId inserts leave SERIAL sequences at 1; advance
    // them so later Classic/EF inserts do not collide on primary keys.
    reset_serial_sequence(&tx, "Books", "BookId", true).await?;
    reset_serial_sequence(&tx, "Series", "SeriesId", true).await?;
    reset_serial_sequence(&tx, "Categories", "CategoryId", true).await?;
    reset_serial_sequence(&tx, "CategoryLadders", "CategoryLadderId", true).await?;
    reset_serial_sequence(&tx, "Supplement", "SupplementId", true).await?;

    tx.commit().await?;
    println!(
        "copied {book_count} book(s) from {} to postgres (classic Libation schema)",
        source.display()
    );
    Ok(())
}

async fn export_flat(
    conn: &Connection,
    client: &mut tokio_postgres::Client,
    source: &std::path::Path,
) -> anyhow::Result<()> {
    client
        .batch_execute(
            r#"
        CREATE TABLE IF NOT EXISTS accounts (
            id SERIAL PRIMARY KEY,
            account_id TEXT NOT NULL UNIQUE,
            source TEXT NOT NULL DEFAULT 'audible',
            marketplace TEXT NOT NULL,
            label TEXT,
            scan_enabled BOOLEAN NOT NULL DEFAULT TRUE,
            created_at TIMESTAMPTZ NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL
        );
        CREATE TABLE IF NOT EXISTS books (
            id SERIAL PRIMARY KEY,
            uuid TEXT NOT NULL UNIQUE,
            source TEXT NOT NULL DEFAULT 'audible',
            account_id TEXT NOT NULL,
            product_id TEXT NOT NULL,
            asin TEXT,
            isbn TEXT,
            marketplace TEXT NOT NULL,
            title TEXT NOT NULL,
            authors TEXT,
            narrators TEXT,
            series TEXT,
            series_index TEXT,
            series_asin TEXT,
            acquire_status TEXT NOT NULL,
            storage_key TEXT,
            error_message TEXT,
            purchased_at TIMESTAMPTZ,
            tags TEXT,
            rating_overall REAL,
            rating_performance REAL,
            rating_story REAL,
            is_finished BOOLEAN NOT NULL DEFAULT FALSE,
            pdf_status TEXT NOT NULL,
            pdf_storage_key TEXT,
            publisher TEXT,
            length_minutes INTEGER,
            is_abridged BOOLEAN NOT NULL DEFAULT FALSE,
            content_kind TEXT NOT NULL DEFAULT 'book',
            categories TEXT,
            subtitle TEXT,
            published_at TIMESTAMPTZ,
            created_at TIMESTAMPTZ NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL,
            UNIQUE (source, account_id, product_id)
        );
        CREATE TABLE IF NOT EXISTS saved_filters (
            id SERIAL PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            query TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL
        );
        "#,
        )
        .await?;

    let tx = client.transaction().await?;
    tx.execute("DELETE FROM books", &[]).await?;
    tx.execute("DELETE FROM accounts", &[]).await?;
    tx.execute("DELETE FROM saved_filters", &[]).await?;

    let mut stmt = conn.prepare(
        "SELECT account_id, marketplace, label, scan_enabled, created_at, updated_at,
                COALESCE(source, 'audible')
         FROM accounts",
    )?;
    let accounts = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)? != 0,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut acct_count = 0usize;
    for row in accounts {
        let (account_id, marketplace, label, scan_enabled, created_at, updated_at, source) = row?;
        tx.execute(
            "INSERT INTO accounts (account_id, source, marketplace, label, scan_enabled, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7::timestamptz)",
            &[
                &account_id,
                &source,
                &marketplace,
                &label,
                &scan_enabled,
                &created_at,
                &updated_at,
            ],
        )
        .await?;
        acct_count += 1;
    }

    let books = load_flat_books(conn)?;
    let mut book_count = 0usize;
    for book in books {
        tx.execute(
            "INSERT INTO books (
                uuid, source, account_id, product_id, asin, isbn, marketplace, title, authors,
                narrators, series, series_index, series_asin, acquire_status, storage_key,
                error_message, purchased_at, tags, rating_overall, rating_performance,
                rating_story, is_finished, pdf_status, pdf_storage_key, publisher,
                length_minutes, is_abridged, content_kind, categories, subtitle, published_at,
                created_at, updated_at
            ) VALUES (
                $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17::timestamptz,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30,$31::timestamptz,$32::timestamptz,$33::timestamptz
            )",
            &[
                &book.uuid,
                &book.source,
                &book.account_id,
                &book.product_id,
                &book.asin,
                &book.isbn,
                &book.marketplace,
                &book.title,
                &book.authors,
                &book.narrators,
                &book.series,
                &book.series_index,
                &book.series_asin,
                &book.acquire_status,
                &book.storage_key,
                &book.error_message,
                &book.purchased_at.map(|d| d.to_rfc3339()),
                &book.tags,
                &book.rating_overall.map(f64::from),
                &book.rating_performance.map(f64::from),
                &book.rating_story.map(f64::from),
                &book.is_finished,
                &book.pdf_status,
                &book.pdf_storage_key,
                &book.publisher,
                &book.length_minutes,
                &book.is_abridged,
                &book.content_kind,
                &book.categories,
                &book.subtitle,
                &book.published_at.map(|d| d.to_rfc3339()),
                &book.created_at.to_rfc3339(),
                &book.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        book_count += 1;
    }

    // Copy saved filters when present.
    if let Ok(mut stmt) =
        conn.prepare("SELECT name, query, created_at, updated_at FROM saved_filters")
    {
        let filters = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in filters {
            let (name, query, created_at, updated_at) = row?;
            tx.execute(
                "INSERT INTO saved_filters (name, query, created_at, updated_at)
                 VALUES ($1,$2,$3::timestamptz,$4::timestamptz)",
                &[&name, &query, &created_at, &updated_at],
            )
            .await?;
        }
    }

    reset_serial_sequence(&tx, "accounts", "id", false).await?;
    reset_serial_sequence(&tx, "books", "id", false).await?;
    reset_serial_sequence(&tx, "saved_filters", "id", false).await?;

    tx.commit().await?;
    println!(
        "copied {acct_count} account(s) and {book_count} book(s) from {} to postgres (flat schema)",
        source.display()
    );
    Ok(())
}

#[derive(Debug)]
struct FlatBook {
    id: i64,
    uuid: String,
    source: String,
    account_id: String,
    product_id: String,
    asin: Option<String>,
    isbn: Option<String>,
    marketplace: String,
    title: String,
    authors: Option<String>,
    narrators: Option<String>,
    series: Option<String>,
    series_index: Option<String>,
    series_asin: Option<String>,
    acquire_status: String,
    storage_key: Option<String>,
    error_message: Option<String>,
    purchased_at: Option<chrono::DateTime<chrono::Utc>>,
    tags: Option<String>,
    rating_overall: Option<f32>,
    rating_performance: Option<f32>,
    rating_story: Option<f32>,
    is_finished: bool,
    pdf_status: String,
    pdf_storage_key: Option<String>,
    publisher: Option<String>,
    length_minutes: Option<i64>,
    is_abridged: bool,
    content_kind: String,
    categories: Option<String>,
    subtitle: Option<String>,
    published_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

fn load_flat_books(conn: &Connection) -> anyhow::Result<Vec<FlatBook>> {
    let mut stmt = conn.prepare(
        "SELECT id, uuid, source, account_id, product_id, asin, isbn, marketplace, title, authors,
                narrators, series, series_index, series_asin, acquire_status, storage_key,
                error_message, purchased_at, tags, rating_overall, rating_performance, rating_story,
                is_finished, pdf_status, pdf_storage_key, publisher, length_minutes, is_abridged,
                content_kind, categories, subtitle, published_at, created_at, updated_at
         FROM books",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(FlatBook {
            id: row.get(0)?,
            uuid: row.get(1)?,
            source: row
                .get::<_, String>(2)
                .unwrap_or_else(|_| String::from("audible")),
            account_id: row.get(3)?,
            product_id: row.get(4)?,
            asin: row.get(5)?,
            isbn: row.get(6)?,
            marketplace: row.get(7)?,
            title: row.get(8)?,
            authors: row.get(9)?,
            narrators: row.get(10)?,
            series: row.get(11)?,
            series_index: row.get(12)?,
            series_asin: row.get(13).ok().flatten(),
            acquire_status: row.get(14)?,
            storage_key: row.get(15)?,
            error_message: row.get(16)?,
            purchased_at: row
                .get::<_, Option<String>>(17)?
                .as_deref()
                .and_then(parse_dt),
            tags: row.get(18)?,
            rating_overall: row.get(19)?,
            rating_performance: row.get(20)?,
            rating_story: row.get(21)?,
            is_finished: row.get::<_, i64>(22)? != 0,
            pdf_status: row.get(23)?,
            pdf_storage_key: row.get(24)?,
            publisher: row.get(25)?,
            length_minutes: row.get(26)?,
            is_abridged: row.get::<_, i64>(27)? != 0,
            content_kind: row.get(28).unwrap_or_else(|_| "book".into()),
            categories: row.get(29).ok().flatten(),
            subtitle: row.get(30).ok().flatten(),
            published_at: row
                .get::<_, Option<String>>(31)
                .ok()
                .flatten()
                .as_deref()
                .and_then(parse_dt),
            created_at: parse_dt(&row.get::<_, String>(32)?).unwrap_or_else(chrono::Utc::now),
            updated_at: parse_dt(&row.get::<_, String>(33)?).unwrap_or_else(chrono::Utc::now),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn split_title_subtitle(title: &str, subtitle: Option<&str>) -> (String, String) {
    if let Some(sub) = subtitle.filter(|s| !s.trim().is_empty()) {
        return (title.to_string(), sub.trim().to_string());
    }
    if let Some((t, s)) = title.split_once(": ") {
        (t.to_string(), s.to_string())
    } else {
        (title.to_string(), String::new())
    }
}

fn parse_dt(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Some(dt.and_utc());
    }
    None
}

/// Advance a SERIAL sequence to at least `MAX(column)` after bulk inserts that
/// supplied explicit primary keys (otherwise the next DEFAULT insert collides).
///
/// `table` / `column` must be trusted identifiers from this module.
async fn reset_serial_sequence(
    tx: &tokio_postgres::Transaction<'_>,
    table: &str,
    column: &str,
    quoted: bool,
) -> anyhow::Result<()> {
    let (seq_table, max_expr, from_table) = if quoted {
        (
            format!("\"{table}\""),
            format!("MAX(\"{column}\")"),
            format!("\"{table}\""),
        )
    } else {
        (
            table.to_string(),
            format!("MAX({column})"),
            table.to_string(),
        )
    };
    let sql = format!(
        "SELECT setval(\
            pg_get_serial_sequence('{seq_table}', '{column}'), \
            COALESCE((SELECT {max_expr} FROM {from_table}), 1), \
            true\
        )"
    );
    match tx.execute(&sql, &[]).await {
        Ok(_) => Ok(()),
        // No sequence yet (table empty / not SERIAL) — nothing to fix.
        Err(err) if err.to_string().contains("null value") => Ok(()),
        Err(err) => Err(err.into()),
    }
}

const CLASSIC_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS "Books" (
    "BookId" SERIAL PRIMARY KEY,
    "AudibleProductId" TEXT NOT NULL,
    "Title" TEXT NOT NULL,
    "Subtitle" TEXT NOT NULL DEFAULT '',
    "Description" TEXT NOT NULL DEFAULT '',
    "LengthInMinutes" INTEGER NOT NULL DEFAULT 0,
    "ContentType" INTEGER NOT NULL DEFAULT 1,
    "Locale" TEXT NOT NULL,
    "PictureId" TEXT,
    "PictureLarge" TEXT,
    "IsAbridged" BOOLEAN NOT NULL DEFAULT FALSE,
    "IsSpatial" BOOLEAN NOT NULL DEFAULT FALSE,
    "DatePublished" TIMESTAMP WITHOUT TIME ZONE,
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
    "SeriesId" SERIAL PRIMARY KEY,
    "AudibleSeriesId" TEXT NOT NULL,
    "Name" TEXT
);
CREATE INDEX IF NOT EXISTS "IX_Series_AudibleSeriesId" ON "Series" ("AudibleSeriesId");

CREATE TABLE IF NOT EXISTS "Categories" (
    "CategoryId" SERIAL PRIMARY KEY,
    "AudibleCategoryId" TEXT NOT NULL,
    "Name" TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS "CategoryLadders" (
    "CategoryLadderId" SERIAL PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS "LibraryBooks" (
    "BookId" INTEGER PRIMARY KEY REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "DateAdded" TIMESTAMP WITHOUT TIME ZONE NOT NULL,
    "Account" TEXT NOT NULL,
    "IsDeleted" BOOLEAN NOT NULL DEFAULT FALSE,
    "AbsentFromLastScan" BOOLEAN NOT NULL DEFAULT FALSE,
    "IncludedUntil" TIMESTAMP WITHOUT TIME ZONE,
    "IsAudiblePlus" BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS "UserDefinedItem" (
    "BookId" INTEGER PRIMARY KEY REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "LastDownloaded" TIMESTAMP WITHOUT TIME ZONE,
    "LastDownloadedVersion" TEXT,
    "LastDownloadedFormat" BIGINT,
    "LastDownloadedFileVersion" TEXT,
    "Tags" TEXT NOT NULL DEFAULT '',
    "Rating_OverallRating" REAL NOT NULL DEFAULT 0,
    "Rating_PerformanceRating" REAL NOT NULL DEFAULT 0,
    "Rating_StoryRating" REAL NOT NULL DEFAULT 0,
    "BookStatus" INTEGER NOT NULL DEFAULT 0,
    "PdfStatus" INTEGER,
    "IsFinished" BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS "Supplement" (
    "SupplementId" SERIAL PRIMARY KEY,
    "BookId" INTEGER NOT NULL REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "Url" TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS "BookContributor" (
    "BookId" INTEGER NOT NULL REFERENCES "Books"("BookId") ON DELETE CASCADE,
    "ContributorId" INTEGER NOT NULL REFERENCES "Contributors"("ContributorId") ON DELETE CASCADE,
    "Role" INTEGER NOT NULL,
    "Order" SMALLINT NOT NULL DEFAULT 0,
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
