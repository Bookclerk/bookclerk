//! Schema migrations via `rusqlite_migration` (`PRAGMA user_version`).

use rusqlite_migration::{Migrations, M};

/// Ordered schema migrations for the Libation library DB.
///
/// Add new `M::up(...)` entries at the end only — never reorder or edit applied ones.
#[must_use]
pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(
            r#"
        CREATE TABLE accounts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id TEXT NOT NULL UNIQUE,
            marketplace TEXT NOT NULL,
            label TEXT,
            scan_enabled INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE books (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            asin TEXT NOT NULL,
            account_id TEXT NOT NULL,
            marketplace TEXT NOT NULL,
            title TEXT NOT NULL,
            authors TEXT,
            narrators TEXT,
            series TEXT,
            series_index TEXT,
            liberate_status TEXT NOT NULL DEFAULT 'not_liberated',
            storage_key TEXT,
            error_message TEXT,
            purchased_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(asin, account_id),
            FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
        );

        CREATE INDEX idx_books_status ON books(liberate_status);
        CREATE INDEX idx_books_account ON books(account_id);
        CREATE INDEX idx_books_title ON books(title);
        "#,
        ),
        M::up(
            r#"
        ALTER TABLE books ADD COLUMN tags TEXT;
        ALTER TABLE books ADD COLUMN rating_overall REAL;
        ALTER TABLE books ADD COLUMN rating_performance REAL;
        ALTER TABLE books ADD COLUMN rating_story REAL;
        ALTER TABLE books ADD COLUMN is_finished INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE books ADD COLUMN pdf_status TEXT NOT NULL DEFAULT 'not_liberated';
        ALTER TABLE books ADD COLUMN pdf_storage_key TEXT;
        ALTER TABLE books ADD COLUMN publisher TEXT;
        ALTER TABLE books ADD COLUMN length_minutes INTEGER;
        ALTER TABLE books ADD COLUMN is_abridged INTEGER NOT NULL DEFAULT 0;

        CREATE TABLE ignored_asins (
            asin TEXT NOT NULL,
            account_id TEXT NOT NULL,
            reason TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (asin, account_id),
            FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
        );

        CREATE INDEX idx_books_pdf_status ON books(pdf_status);
        CREATE INDEX idx_books_tags ON books(tags);
        "#,
        ),
        M::up(
            r#"
        ALTER TABLE books ADD COLUMN content_kind TEXT NOT NULL DEFAULT 'book';
        ALTER TABLE books ADD COLUMN categories TEXT;
        ALTER TABLE books ADD COLUMN subtitle TEXT;
        ALTER TABLE books ADD COLUMN published_at TEXT;

        CREATE TABLE saved_filters (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            query TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        "#,
        ),
        M::up(
            r#"
        ALTER TABLE books ADD COLUMN series_asin TEXT;
        CREATE INDEX idx_books_series_asin ON books(series_asin);
        CREATE INDEX idx_books_content_kind ON books(content_kind);
        "#,
        ),
        // Multi-source ownership rows: UUID public id; ISBN/ASIN as attributes.
        M::up(
            r#"
        ALTER TABLE accounts ADD COLUMN source TEXT NOT NULL DEFAULT 'audible';

        CREATE TABLE books_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            uuid TEXT NOT NULL UNIQUE,
            source TEXT NOT NULL,
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
            liberate_status TEXT NOT NULL DEFAULT 'not_liberated',
            storage_key TEXT,
            error_message TEXT,
            purchased_at TEXT,
            tags TEXT,
            rating_overall REAL,
            rating_performance REAL,
            rating_story REAL,
            is_finished INTEGER NOT NULL DEFAULT 0,
            pdf_status TEXT NOT NULL DEFAULT 'not_liberated',
            pdf_storage_key TEXT,
            publisher TEXT,
            length_minutes INTEGER,
            is_abridged INTEGER NOT NULL DEFAULT 0,
            content_kind TEXT NOT NULL DEFAULT 'book',
            categories TEXT,
            subtitle TEXT,
            published_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(source, account_id, product_id),
            FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
        );

        INSERT INTO books_new (
            id, uuid, source, account_id, product_id, asin, isbn, marketplace, title,
            authors, narrators, series, series_index, series_asin, liberate_status,
            storage_key, error_message, purchased_at, tags, rating_overall,
            rating_performance, rating_story, is_finished, pdf_status, pdf_storage_key,
            publisher, length_minutes, is_abridged, content_kind, categories, subtitle,
            published_at, created_at, updated_at
        )
        SELECT
            id,
            lower(hex(randomblob(16))),
            'audible',
            account_id,
            asin,
            asin,
            NULL,
            marketplace,
            title,
            authors,
            narrators,
            series,
            series_index,
            series_asin,
            liberate_status,
            storage_key,
            error_message,
            purchased_at,
            tags,
            rating_overall,
            rating_performance,
            rating_story,
            is_finished,
            pdf_status,
            pdf_storage_key,
            publisher,
            length_minutes,
            is_abridged,
            content_kind,
            categories,
            subtitle,
            published_at,
            created_at,
            updated_at
        FROM books;

        DROP TABLE books;
        ALTER TABLE books_new RENAME TO books;

        CREATE INDEX idx_books_uuid ON books(uuid);
        CREATE INDEX idx_books_status ON books(liberate_status);
        CREATE INDEX idx_books_account ON books(account_id);
        CREATE INDEX idx_books_title ON books(title);
        CREATE INDEX idx_books_pdf_status ON books(pdf_status);
        CREATE INDEX idx_books_tags ON books(tags);
        CREATE INDEX idx_books_series_asin ON books(series_asin);
        CREATE INDEX idx_books_content_kind ON books(content_kind);
        CREATE INDEX idx_books_isbn ON books(isbn);
        CREATE INDEX idx_books_source ON books(source);
        CREATE INDEX idx_books_asin ON books(asin);
        CREATE INDEX idx_books_product_id ON books(product_id);

        CREATE TABLE ignored_titles (
            source TEXT NOT NULL,
            account_id TEXT NOT NULL,
            product_id TEXT NOT NULL,
            reason TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (source, account_id, product_id),
            FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
        );

        INSERT INTO ignored_titles (source, account_id, product_id, reason, created_at)
        SELECT 'audible', account_id, asin, reason, created_at FROM ignored_asins;

        DROP TABLE ignored_asins;
        "#,
        ),
    ])
}
