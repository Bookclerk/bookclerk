//! Greenfield schema for the Bookclerk library DB.
//!
//! Bookclerk is greenfield: there is a **single** current schema rather than an
//! ordered historical migration chain. Local SQLite files apply
//! [`latest_schema_sqlite`] via [`rusqlite_migration`] (`PRAGMA user_version`);
//! D1 applies the same SQLite DDL through SeaORM, and Postgres applies
//! [`latest_schema_postgres`] — see [`crate::db`]. Every statement uses
//! `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`, so re-applying
//! the schema is idempotent.

use rusqlite_migration::{Migrations, M};

/// Final SQLite DDL for a fresh Bookclerk library database.
///
/// SeaORM entities in [`crate::entities`] mirror these columns exactly. All
/// integer columns map to `i64`, reals to `f64`, blobs to `Vec<u8>`, and text
/// (including RFC 3339 timestamps) to `String`.
#[must_use]
pub fn latest_schema_sqlite() -> &'static str {
    SQLITE_SCHEMA
}

const SQLITE_SCHEMA: &str = r#"
    CREATE TABLE IF NOT EXISTS accounts (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        account_id TEXT NOT NULL UNIQUE,
        marketplace TEXT NOT NULL,
        label TEXT,
        scan_enabled INTEGER NOT NULL DEFAULT 1,
        source TEXT NOT NULL DEFAULT 'audible',
        connection_status TEXT NOT NULL DEFAULT 'active',
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS books (
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
        description TEXT,
        language TEXT,
        cover_url TEXT,
        subjects TEXT,
        enrich_source TEXT,
        enrich_confidence REAL,
        enrich_updated_at TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(source, account_id, product_id),
        FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_books_uuid ON books(uuid);
    CREATE INDEX IF NOT EXISTS idx_books_status ON books(acquire_status);
    CREATE INDEX IF NOT EXISTS idx_books_account ON books(account_id);
    CREATE INDEX IF NOT EXISTS idx_books_title ON books(title);
    CREATE INDEX IF NOT EXISTS idx_books_pdf_status ON books(pdf_status);
    CREATE INDEX IF NOT EXISTS idx_books_tags ON books(tags);
    CREATE INDEX IF NOT EXISTS idx_books_series_asin ON books(series_asin);
    CREATE INDEX IF NOT EXISTS idx_books_content_kind ON books(content_kind);
    CREATE INDEX IF NOT EXISTS idx_books_isbn ON books(isbn);
    CREATE INDEX IF NOT EXISTS idx_books_source ON books(source);
    CREATE INDEX IF NOT EXISTS idx_books_asin ON books(asin);
    CREATE INDEX IF NOT EXISTS idx_books_product_id ON books(product_id);

    CREATE TABLE IF NOT EXISTS ignored_titles (
        source TEXT NOT NULL,
        account_id TEXT NOT NULL,
        product_id TEXT NOT NULL,
        reason TEXT,
        created_at TEXT NOT NULL,
        PRIMARY KEY (source, account_id, product_id),
        FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS saved_filters (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL UNIQUE,
        query TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS portal_identities (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        provider TEXT NOT NULL,
        external_user_id TEXT NOT NULL,
        label TEXT,
        created_at TEXT NOT NULL,
        UNIQUE(provider, external_user_id)
    );

    CREATE TABLE IF NOT EXISTS claim_tickets (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        identity_id INTEGER,
        expires_at TEXT NOT NULL,
        redeemed_at TEXT,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE SET NULL
    );

    CREATE INDEX IF NOT EXISTS idx_claim_tickets_hash ON claim_tickets(token_hash);
    CREATE INDEX IF NOT EXISTS idx_claim_tickets_identity ON claim_tickets(identity_id);

    CREATE TABLE IF NOT EXISTS portal_sessions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        identity_id INTEGER NOT NULL,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_portal_sessions_hash ON portal_sessions(token_hash);

    CREATE TABLE IF NOT EXISTS account_links (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        identity_id INTEGER NOT NULL,
        account_id TEXT NOT NULL,
        source TEXT NOT NULL,
        created_at TEXT NOT NULL,
        UNIQUE(identity_id, account_id),
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE,
        FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_account_links_account ON account_links(account_id);

    CREATE TABLE IF NOT EXISTS works (
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

    CREATE INDEX IF NOT EXISTS idx_works_asin ON works(canonical_asin);
    CREATE INDEX IF NOT EXISTS idx_works_isbn ON works(canonical_isbn);
    CREATE INDEX IF NOT EXISTS idx_works_title ON works(title);

    CREATE TABLE IF NOT EXISTS work_editions (
        work_id TEXT NOT NULL,
        book_uuid TEXT NOT NULL UNIQUE,
        created_at TEXT NOT NULL,
        PRIMARY KEY (work_id, book_uuid),
        FOREIGN KEY(work_id) REFERENCES works(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_work_editions_book ON work_editions(book_uuid);

    CREATE TABLE IF NOT EXISTS listening_progress (
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

    CREATE INDEX IF NOT EXISTS idx_listening_book ON listening_progress(book_uuid);
    CREATE INDEX IF NOT EXISTS idx_listening_work ON listening_progress(work_id);
    CREATE INDEX IF NOT EXISTS idx_listening_user ON listening_progress(provider, external_user_id);

    CREATE TABLE IF NOT EXISTS title_requests (
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
        work_key TEXT NOT NULL DEFAULT '',
        resolved_book_uuid TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE SET NULL
    );

    CREATE INDEX IF NOT EXISTS idx_title_requests_status ON title_requests(status);
    CREATE INDEX IF NOT EXISTS idx_title_requests_identity ON title_requests(identity_id);
    CREATE INDEX IF NOT EXISTS idx_title_requests_work_key ON title_requests(work_key);
    CREATE INDEX IF NOT EXISTS idx_title_requests_identity_status ON title_requests(identity_id, status);

    CREATE TABLE IF NOT EXISTS embeddings (
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

    CREATE INDEX IF NOT EXISTS idx_embeddings_target ON embeddings(target_kind, target_id);

    CREATE TABLE IF NOT EXISTS user_preferences (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        subject_key TEXT NOT NULL UNIQUE,
        identity_id INTEGER,
        default_view TEXT NOT NULL DEFAULT 'discover',
        disabled_shelves_json TEXT NOT NULL DEFAULT '[]',
        updated_at TEXT NOT NULL,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_user_preferences_identity ON user_preferences(identity_id);

    CREATE TABLE IF NOT EXISTS encrypted_secrets (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        kind TEXT NOT NULL,
        provider TEXT,
        account_type TEXT NOT NULL DEFAULT 'integration',
        account_id TEXT,
        name TEXT NOT NULL,
        format TEXT NOT NULL DEFAULT 'json',
        ciphertext BLOB NOT NULL,
        kdf_algorithm TEXT,
        kdf_salt BLOB,
        kdf_m_cost INTEGER,
        kdf_t_cost INTEGER,
        kdf_p_cost INTEGER,
        cipher_algorithm TEXT,
        cipher_nonce BLOB,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(kind, provider, account_type, account_id, name)
    );

    CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_kind ON encrypted_secrets(kind);
    CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_account ON encrypted_secrets(account_id);
    CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_account_type ON encrypted_secrets(account_type);
    "#;

/// Single-entry migration list for local SQLite files (`PRAGMA user_version`).
///
/// Greenfield: the only entry is [`latest_schema_sqlite`]. Its 1-based position
/// (version 1) is recorded in `PRAGMA user_version` after `to_latest`.
#[must_use]
pub fn migration_sql() -> &'static [&'static str] {
    &[SQLITE_SCHEMA]
}

/// SQLite schema migrations (single greenfield schema).
///
/// Built from [`migration_sql`].
#[must_use]
pub fn migrations() -> Migrations<'static> {
    Migrations::new(migration_sql().iter().map(|sql| M::up(sql)).collect())
}

/// Final PostgreSQL DDL for a fresh Bookclerk library database.
///
/// Mirrors [`latest_schema_sqlite`] with Postgres-native types. Integer columns
/// use `BIGINT` / `BIGSERIAL` so the shared SeaORM entities (all `i64`) load on
/// both the SQLite proxy and native Postgres backends.
#[must_use]
pub fn latest_schema_postgres() -> &'static str {
    POSTGRES_SCHEMA
}

const POSTGRES_SCHEMA: &str = r#"
        CREATE TABLE IF NOT EXISTS accounts (
            id BIGSERIAL PRIMARY KEY,
            account_id TEXT NOT NULL UNIQUE,
            marketplace TEXT NOT NULL,
            label TEXT,
            scan_enabled BIGINT NOT NULL DEFAULT 1,
            source TEXT NOT NULL DEFAULT 'audible',
            connection_status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS books (
            id BIGSERIAL PRIMARY KEY,
            uuid TEXT NOT NULL UNIQUE,
            source TEXT NOT NULL,
            account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
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
            rating_overall DOUBLE PRECISION,
            rating_performance DOUBLE PRECISION,
            rating_story DOUBLE PRECISION,
            is_finished BIGINT NOT NULL DEFAULT 0,
            pdf_status TEXT NOT NULL DEFAULT 'not_acquired',
            pdf_storage_key TEXT,
            publisher TEXT,
            length_minutes BIGINT,
            is_abridged BIGINT NOT NULL DEFAULT 0,
            content_kind TEXT NOT NULL DEFAULT 'book',
            categories TEXT,
            subtitle TEXT,
            published_at TEXT,
            description TEXT,
            language TEXT,
            cover_url TEXT,
            subjects TEXT,
            enrich_source TEXT,
            enrich_confidence DOUBLE PRECISION,
            enrich_updated_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(source, account_id, product_id)
        );

        CREATE INDEX IF NOT EXISTS idx_books_uuid ON books(uuid);
        CREATE INDEX IF NOT EXISTS idx_books_status ON books(acquire_status);
        CREATE INDEX IF NOT EXISTS idx_books_account ON books(account_id);
        CREATE INDEX IF NOT EXISTS idx_books_title ON books(title);
        CREATE INDEX IF NOT EXISTS idx_books_pdf_status ON books(pdf_status);
        CREATE INDEX IF NOT EXISTS idx_books_tags ON books(tags);
        CREATE INDEX IF NOT EXISTS idx_books_series_asin ON books(series_asin);
        CREATE INDEX IF NOT EXISTS idx_books_content_kind ON books(content_kind);
        CREATE INDEX IF NOT EXISTS idx_books_isbn ON books(isbn);
        CREATE INDEX IF NOT EXISTS idx_books_source ON books(source);
        CREATE INDEX IF NOT EXISTS idx_books_asin ON books(asin);
        CREATE INDEX IF NOT EXISTS idx_books_product_id ON books(product_id);

        CREATE TABLE IF NOT EXISTS ignored_titles (
            source TEXT NOT NULL,
            account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
            product_id TEXT NOT NULL,
            reason TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (source, account_id, product_id)
        );

        CREATE TABLE IF NOT EXISTS saved_filters (
            id BIGSERIAL PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            query TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS portal_identities (
            id BIGSERIAL PRIMARY KEY,
            provider TEXT NOT NULL,
            external_user_id TEXT NOT NULL,
            label TEXT,
            created_at TEXT NOT NULL,
            UNIQUE(provider, external_user_id)
        );

        CREATE TABLE IF NOT EXISTS claim_tickets (
            id BIGSERIAL PRIMARY KEY,
            token_hash TEXT NOT NULL UNIQUE,
            identity_id BIGINT REFERENCES portal_identities(id) ON DELETE SET NULL,
            expires_at TEXT NOT NULL,
            redeemed_at TEXT,
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_claim_tickets_hash ON claim_tickets(token_hash);
        CREATE INDEX IF NOT EXISTS idx_claim_tickets_identity ON claim_tickets(identity_id);

        CREATE TABLE IF NOT EXISTS portal_sessions (
            id BIGSERIAL PRIMARY KEY,
            token_hash TEXT NOT NULL UNIQUE,
            identity_id BIGINT NOT NULL REFERENCES portal_identities(id) ON DELETE CASCADE,
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_portal_sessions_hash ON portal_sessions(token_hash);

        CREATE TABLE IF NOT EXISTS account_links (
            id BIGSERIAL PRIMARY KEY,
            identity_id BIGINT NOT NULL REFERENCES portal_identities(id) ON DELETE CASCADE,
            account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
            source TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(identity_id, account_id)
        );

        CREATE INDEX IF NOT EXISTS idx_account_links_account ON account_links(account_id);

        CREATE TABLE IF NOT EXISTS works (
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

        CREATE INDEX IF NOT EXISTS idx_works_asin ON works(canonical_asin);
        CREATE INDEX IF NOT EXISTS idx_works_isbn ON works(canonical_isbn);
        CREATE INDEX IF NOT EXISTS idx_works_title ON works(title);

        CREATE TABLE IF NOT EXISTS work_editions (
            work_id TEXT NOT NULL REFERENCES works(id) ON DELETE CASCADE,
            book_uuid TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL,
            PRIMARY KEY (work_id, book_uuid)
        );

        CREATE INDEX IF NOT EXISTS idx_work_editions_book ON work_editions(book_uuid);

        CREATE TABLE IF NOT EXISTS listening_progress (
            id BIGSERIAL PRIMARY KEY,
            identity_id BIGINT REFERENCES portal_identities(id) ON DELETE SET NULL,
            provider TEXT NOT NULL,
            external_user_id TEXT NOT NULL,
            book_uuid TEXT,
            work_id TEXT,
            external_item_id TEXT NOT NULL,
            title TEXT,
            authors TEXT,
            asin TEXT,
            isbn TEXT,
            progress DOUBLE PRECISION,
            current_time_seconds DOUBLE PRECISION,
            duration_seconds DOUBLE PRECISION,
            is_finished BIGINT NOT NULL DEFAULT 0,
            last_listened_at TEXT,
            updated_at TEXT NOT NULL,
            UNIQUE(provider, external_user_id, external_item_id)
        );

        CREATE INDEX IF NOT EXISTS idx_listening_book ON listening_progress(book_uuid);
        CREATE INDEX IF NOT EXISTS idx_listening_work ON listening_progress(work_id);
        CREATE INDEX IF NOT EXISTS idx_listening_user ON listening_progress(provider, external_user_id);

        CREATE TABLE IF NOT EXISTS title_requests (
            id BIGSERIAL PRIMARY KEY,
            uuid TEXT NOT NULL UNIQUE,
            identity_id BIGINT REFERENCES portal_identities(id) ON DELETE SET NULL,
            title TEXT NOT NULL,
            authors TEXT,
            asin TEXT,
            isbn TEXT,
            notes TEXT,
            status TEXT NOT NULL DEFAULT 'open',
            preferred_source TEXT,
            work_id TEXT,
            work_key TEXT NOT NULL DEFAULT '',
            resolved_book_uuid TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_title_requests_status ON title_requests(status);
        CREATE INDEX IF NOT EXISTS idx_title_requests_identity ON title_requests(identity_id);
        CREATE INDEX IF NOT EXISTS idx_title_requests_work_key ON title_requests(work_key);
        CREATE INDEX IF NOT EXISTS idx_title_requests_identity_status ON title_requests(identity_id, status);

        CREATE TABLE IF NOT EXISTS embeddings (
            id BIGSERIAL PRIMARY KEY,
            target_kind TEXT NOT NULL,
            target_id TEXT NOT NULL,
            model TEXT NOT NULL,
            dims BIGINT NOT NULL,
            vector BYTEA NOT NULL,
            text_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(target_kind, target_id, model)
        );

        CREATE INDEX IF NOT EXISTS idx_embeddings_target ON embeddings(target_kind, target_id);

        CREATE TABLE IF NOT EXISTS user_preferences (
            id BIGSERIAL PRIMARY KEY,
            subject_key TEXT NOT NULL UNIQUE,
            identity_id BIGINT REFERENCES portal_identities(id) ON DELETE CASCADE,
            default_view TEXT NOT NULL DEFAULT 'discover',
            disabled_shelves_json TEXT NOT NULL DEFAULT '[]',
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_user_preferences_identity ON user_preferences(identity_id);

        CREATE TABLE IF NOT EXISTS encrypted_secrets (
            id BIGSERIAL PRIMARY KEY,
            kind TEXT NOT NULL,
            provider TEXT,
            account_type TEXT NOT NULL DEFAULT 'integration',
            account_id TEXT,
            name TEXT NOT NULL,
            format TEXT NOT NULL DEFAULT 'json',
            ciphertext BYTEA NOT NULL,
            kdf_algorithm TEXT,
            kdf_salt BYTEA,
            kdf_m_cost BIGINT,
            kdf_t_cost BIGINT,
            kdf_p_cost BIGINT,
            cipher_algorithm TEXT,
            cipher_nonce BYTEA,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(kind, provider, account_type, account_id, name)
        );

        CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_kind ON encrypted_secrets(kind);
        CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_account ON encrypted_secrets(account_id);
        CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_account_type ON encrypted_secrets(account_type);
        "#;
