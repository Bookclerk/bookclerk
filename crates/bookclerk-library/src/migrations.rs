//! Schema migrations via `rusqlite_migration` (`PRAGMA user_version`).

use rusqlite_migration::{Migrations, M};

/// Ordered schema migrations for the Bookclerk library DB.
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
            acquire_status TEXT NOT NULL DEFAULT 'not_acquired',
            storage_key TEXT,
            error_message TEXT,
            purchased_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(asin, account_id),
            FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
        );

        CREATE INDEX idx_books_status ON books(acquire_status);
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
        ALTER TABLE books ADD COLUMN pdf_status TEXT NOT NULL DEFAULT 'not_acquired';
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
            acquire_status TEXT NOT NULL DEFAULT 'not_acquired',
            storage_key TEXT,
            error_message TEXT,
            purchased_at TEXT,
            tags TEXT,
            rating_overall REAL,
            rating_performance REAL,
            rating_story REAL,
            is_finished INTEGER NOT NULL DEFAULT 0,
            pdf_status TEXT NOT NULL DEFAULT 'not_acquired',
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
            authors, narrators, series, series_index, series_asin, acquire_status,
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
            acquire_status,
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
        CREATE INDEX idx_books_status ON books(acquire_status);
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
        // Portal identities, claim tickets, sessions, account links; connection_status.
        M::up(
            r#"
        ALTER TABLE accounts ADD COLUMN connection_status TEXT NOT NULL DEFAULT 'active';

        CREATE TABLE portal_identities (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider TEXT NOT NULL,
            external_user_id TEXT NOT NULL,
            label TEXT,
            created_at TEXT NOT NULL,
            UNIQUE(provider, external_user_id)
        );

        CREATE TABLE claim_tickets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            token_hash TEXT NOT NULL UNIQUE,
            identity_id INTEGER,
            expires_at TEXT NOT NULL,
            redeemed_at TEXT,
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE SET NULL
        );

        CREATE INDEX idx_claim_tickets_hash ON claim_tickets(token_hash);
        CREATE INDEX idx_claim_tickets_identity ON claim_tickets(identity_id);

        CREATE TABLE portal_sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            token_hash TEXT NOT NULL UNIQUE,
            identity_id INTEGER NOT NULL,
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE
        );

        CREATE INDEX idx_portal_sessions_hash ON portal_sessions(token_hash);

        CREATE TABLE account_links (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            identity_id INTEGER NOT NULL,
            account_id TEXT NOT NULL,
            source TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(identity_id, account_id),
            FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE,
            FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
        );

        CREATE INDEX idx_account_links_account ON account_links(account_id);
        "#,
        ),
        // Discovery: durable enrichment fields, works graph, listening, requests, embeddings.
        M::up(
            r#"
        ALTER TABLE books ADD COLUMN description TEXT;
        ALTER TABLE books ADD COLUMN language TEXT;
        ALTER TABLE books ADD COLUMN cover_url TEXT;
        ALTER TABLE books ADD COLUMN subjects TEXT;
        ALTER TABLE books ADD COLUMN enrich_source TEXT;
        ALTER TABLE books ADD COLUMN enrich_confidence REAL;
        ALTER TABLE books ADD COLUMN enrich_updated_at TEXT;

        CREATE TABLE works (
            id TEXT PRIMARY KEY,
            canonical_asin TEXT,
            canonical_isbn TEXT,
            title TEXT NOT NULL,
            authors TEXT,
            narrators TEXT,
            description TEXT,
            subjects TEXT,
            categories TEXT,
            language TEXT,
            series TEXT,
            series_index TEXT,
            cover_url TEXT,
            openlibrary_id TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX idx_works_asin ON works(canonical_asin);
        CREATE INDEX idx_works_isbn ON works(canonical_isbn);
        CREATE INDEX idx_works_title ON works(title);

        CREATE TABLE work_editions (
            work_id TEXT NOT NULL,
            book_uuid TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL,
            PRIMARY KEY (work_id, book_uuid),
            FOREIGN KEY(work_id) REFERENCES works(id) ON DELETE CASCADE
        );

        CREATE INDEX idx_work_editions_book ON work_editions(book_uuid);

        CREATE TABLE listening_progress (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            identity_id INTEGER,
            provider TEXT NOT NULL,
            external_user_id TEXT NOT NULL,
            book_uuid TEXT,
            work_id TEXT,
            external_item_id TEXT NOT NULL,
            title TEXT,
            authors TEXT,
            asin TEXT,
            isbn TEXT,
            progress REAL,
            current_time_seconds REAL,
            duration_seconds REAL,
            is_finished INTEGER NOT NULL DEFAULT 0,
            last_listened_at TEXT,
            updated_at TEXT NOT NULL,
            UNIQUE(provider, external_user_id, external_item_id),
            FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE SET NULL
        );

        CREATE INDEX idx_listening_book ON listening_progress(book_uuid);
        CREATE INDEX idx_listening_work ON listening_progress(work_id);
        CREATE INDEX idx_listening_user ON listening_progress(provider, external_user_id);

        CREATE TABLE title_requests (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            uuid TEXT NOT NULL UNIQUE,
            identity_id INTEGER,
            title TEXT NOT NULL,
            authors TEXT,
            asin TEXT,
            isbn TEXT,
            notes TEXT,
            status TEXT NOT NULL DEFAULT 'open',
            preferred_source TEXT,
            work_id TEXT,
            resolved_book_uuid TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE SET NULL
        );

        CREATE INDEX idx_title_requests_status ON title_requests(status);
        CREATE INDEX idx_title_requests_identity ON title_requests(identity_id);

        CREATE TABLE embeddings (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            target_kind TEXT NOT NULL,
            target_id TEXT NOT NULL,
            model TEXT NOT NULL,
            dims INTEGER NOT NULL,
            vector BLOB NOT NULL,
            text_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(target_kind, target_id, model)
        );

        CREATE INDEX idx_embeddings_target ON embeddings(target_kind, target_id);
        "#,
        ),
        // Per-user GUI / Discover preferences (not config.toml).
        M::up(
            r#"
        CREATE TABLE user_preferences (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            subject_key TEXT NOT NULL UNIQUE,
            identity_id INTEGER,
            default_view TEXT NOT NULL DEFAULT 'discover',
            disabled_shelves_json TEXT NOT NULL DEFAULT '[]',
            updated_at TEXT NOT NULL,
            FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE
        );

        CREATE INDEX idx_user_preferences_identity ON user_preferences(identity_id);
        "#,
        ),
        // Stable bibliographic key for wishlist aggregation (multi-user boost).
        M::up(
            r#"
        ALTER TABLE title_requests ADD COLUMN work_key TEXT NOT NULL DEFAULT '';
        CREATE INDEX idx_title_requests_work_key ON title_requests(work_key);
        CREATE INDEX idx_title_requests_identity_status ON title_requests(identity_id, status);
        "#,
        ),
    ])
}
