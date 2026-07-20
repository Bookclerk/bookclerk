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
    ])
}
