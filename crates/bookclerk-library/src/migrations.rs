//! Greenfield schema for the Bookclerk library DB.
//!
//! Fresh databases apply a single version-1 DDL via [`migration_sql`] (SQLite
//! `PRAGMA user_version`) / [`migration_sql_postgres`] (D1/Postgres
//! `schema_migrations`). [`latest_schema_sqlite`] / [`latest_schema_postgres`]
//! expose that same DDL for introspection. Base DDL uses
//! `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`.

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

/// Greenfield SQLite DDL for a fresh library database (`PRAGMA user_version` v1).
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

    CREATE TABLE IF NOT EXISTS users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        role TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active',
        display_name TEXT,
        password_hash TEXT,
        security_version INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_users_role ON users(role);
    CREATE INDEX IF NOT EXISTS idx_users_status ON users(status);

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

    CREATE TABLE IF NOT EXISTS operator_sessions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_used_at TEXT
    );

    CREATE INDEX IF NOT EXISTS idx_operator_sessions_hash ON operator_sessions(token_hash);

    CREATE TABLE IF NOT EXISTS security_audit_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at TEXT NOT NULL,
        actor TEXT NOT NULL,
        action TEXT NOT NULL,
        detail_json TEXT
    );

    CREATE INDEX IF NOT EXISTS idx_security_audit_at ON security_audit_events(at);

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
        cover_url TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE SET NULL
    );

    CREATE INDEX IF NOT EXISTS idx_title_requests_status ON title_requests(status);
    CREATE INDEX IF NOT EXISTS idx_title_requests_identity ON title_requests(identity_id);
    CREATE INDEX IF NOT EXISTS idx_title_requests_work_key ON title_requests(work_key);
    CREATE INDEX IF NOT EXISTS idx_title_requests_identity_status ON title_requests(identity_id, status);

    CREATE TABLE IF NOT EXISTS title_request_sources (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        title_request_id INTEGER NOT NULL,
        source TEXT NOT NULL,
        product_id TEXT NOT NULL,
        title TEXT,
        subtitle TEXT,
        authors TEXT,
        narrators TEXT,
        series TEXT,
        series_index TEXT,
        asin TEXT,
        isbn TEXT,
        description TEXT,
        publisher TEXT,
        length_minutes INTEGER,
        published_at TEXT,
        categories TEXT,
        language TEXT,
        cover_url TEXT,
        url TEXT,
        price_cents INTEGER,
        currency TEXT,
        price_label TEXT,
        list_price_cents INTEGER,
        list_price_label TEXT,
        member_price_cents INTEGER,
        member_price_label TEXT,
        observed_at TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(title_request_id, source, product_id),
        FOREIGN KEY(title_request_id) REFERENCES title_requests(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_trs_request ON title_request_sources(title_request_id);

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
        discover_sort TEXT NOT NULL DEFAULT 'relevance',
        discover_sort_dir TEXT NOT NULL DEFAULT 'desc',
        discover_language TEXT,
        discover_excluded_sources_json TEXT NOT NULL DEFAULT '[]',
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

/// Additive migration: durable hashed operator sessions (#117).
///
/// Existing installs already applied greenfield v1; this version creates the
/// table when missing. Fresh databases also create it via [`SQLITE_SCHEMA`] /
/// [`POSTGRES_SCHEMA`] (`IF NOT EXISTS` keeps the double apply safe).
const MIGRATION_V2_OPERATOR_SESSIONS_SQLITE: &str = r#"
    CREATE TABLE IF NOT EXISTS operator_sessions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_used_at TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_operator_sessions_hash ON operator_sessions(token_hash);
"#;

/// Postgres DDL adding hashed operator sessions (mirrors SQLite v2).
const MIGRATION_V2_OPERATOR_SESSIONS_POSTGRES: &str = r#"
    CREATE TABLE IF NOT EXISTS operator_sessions (
        id BIGSERIAL PRIMARY KEY,
        token_hash TEXT NOT NULL UNIQUE,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_used_at TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_operator_sessions_hash ON operator_sessions(token_hash);
"#;

/// Additive migration: first-party `users` + bridge column on portal identities (#117).
///
/// Data backfill (portal row → member user, prefs subject remap) runs in
/// [`crate::LibraryStore::ensure_users_bridged`] after DDL apply.
const MIGRATION_V3_USERS_SQLITE: &str = r#"
    CREATE TABLE IF NOT EXISTS users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        role TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active',
        display_name TEXT,
        password_hash TEXT,
        security_version INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_users_role ON users(role);
    CREATE INDEX IF NOT EXISTS idx_users_status ON users(status);
"#;

/// Postgres DDL creating first-party `users` (mirrors SQLite v3).
const MIGRATION_V3_USERS_POSTGRES: &str = r#"
    CREATE TABLE IF NOT EXISTS users (
        id BIGSERIAL PRIMARY KEY,
        role TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active',
        display_name TEXT,
        password_hash TEXT,
        security_version BIGINT NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_users_role ON users(role);
    CREATE INDEX IF NOT EXISTS idx_users_status ON users(status);
"#;

/// SQLite cannot add a FK column in one statement portably; add nullable `user_id`
/// then index. Existing rows are bridged in Rust.
const MIGRATION_V3_PORTAL_USER_ID_SQLITE: &str = r#"
    ALTER TABLE portal_identities ADD COLUMN user_id INTEGER REFERENCES users(id) ON DELETE SET NULL;
    CREATE INDEX IF NOT EXISTS idx_portal_identities_user ON portal_identities(user_id);
"#;

/// Postgres DDL adding nullable `portal_identities.user_id` with `IF NOT EXISTS`.
const MIGRATION_V3_PORTAL_USER_ID_POSTGRES: &str = r#"
    ALTER TABLE portal_identities ADD COLUMN IF NOT EXISTS user_id BIGINT REFERENCES users(id) ON DELETE SET NULL;
    CREATE INDEX IF NOT EXISTS idx_portal_identities_user ON portal_identities(user_id);
"#;

/// Additive migration: elevate / impersonate metadata + security audit (#117 Phase 2).
const MIGRATION_V4_ELEVATE_AUDIT_SQLITE: &str = r#"
    ALTER TABLE operator_sessions ADD COLUMN elevated_from_user_id INTEGER;
    ALTER TABLE operator_sessions ADD COLUMN impersonating_user_id INTEGER;
    CREATE TABLE IF NOT EXISTS security_audit_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at TEXT NOT NULL,
        actor TEXT NOT NULL,
        action TEXT NOT NULL,
        detail_json TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_security_audit_at ON security_audit_events(at);
"#;

/// Postgres DDL for elevate/impersonate session columns and `security_audit_events`.
const MIGRATION_V4_ELEVATE_AUDIT_POSTGRES: &str = r#"
    ALTER TABLE operator_sessions ADD COLUMN IF NOT EXISTS elevated_from_user_id BIGINT;
    ALTER TABLE operator_sessions ADD COLUMN IF NOT EXISTS impersonating_user_id BIGINT;
    CREATE TABLE IF NOT EXISTS security_audit_events (
        id BIGSERIAL PRIMARY KEY,
        at TEXT NOT NULL,
        actor TEXT NOT NULL,
        action TEXT NOT NULL,
        detail_json TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_security_audit_at ON security_audit_events(at);
"#;

/// Additive migration: local login_name + user invites (#117 Phase 3).
const MIGRATION_V5_PROVISIONING_SQLITE: &str = r#"
    ALTER TABLE users ADD COLUMN login_name TEXT;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_users_login_name ON users(login_name);
    CREATE TABLE IF NOT EXISTS user_invites (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        role TEXT NOT NULL,
        login_name TEXT,
        display_name TEXT,
        expires_at TEXT NOT NULL,
        redeemed_at TEXT,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_user_invites_hash ON user_invites(token_hash);
"#;

/// Postgres DDL adding `users.login_name` and `user_invites`.
const MIGRATION_V5_PROVISIONING_POSTGRES: &str = r#"
    ALTER TABLE users ADD COLUMN IF NOT EXISTS login_name TEXT;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_users_login_name ON users(login_name);
    CREATE TABLE IF NOT EXISTS user_invites (
        id BIGSERIAL PRIMARY KEY,
        token_hash TEXT NOT NULL UNIQUE,
        role TEXT NOT NULL,
        login_name TEXT,
        display_name TEXT,
        expires_at TEXT NOT NULL,
        redeemed_at TEXT,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_user_invites_hash ON user_invites(token_hash);
"#;

/// Additive migration: OIDC authorization server tables (#117 Phase 4).
const MIGRATION_V6_OIDC_SQLITE: &str = r#"
    CREATE TABLE IF NOT EXISTS oidc_clients (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        client_id TEXT NOT NULL UNIQUE,
        client_secret_hash TEXT,
        redirect_uris_json TEXT NOT NULL,
        name TEXT,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS oidc_auth_codes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        code_hash TEXT NOT NULL UNIQUE,
        client_id TEXT NOT NULL,
        user_id INTEGER NOT NULL,
        redirect_uri TEXT NOT NULL,
        code_challenge TEXT NOT NULL,
        code_challenge_method TEXT NOT NULL,
        scope TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        consumed_at TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_oidc_auth_codes_hash ON oidc_auth_codes(code_hash);
    CREATE TABLE IF NOT EXISTS oidc_refresh_tokens (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        client_id TEXT NOT NULL,
        user_id INTEGER NOT NULL,
        scope TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        revoked_at TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_oidc_refresh_hash ON oidc_refresh_tokens(token_hash);
"#;

/// Postgres DDL for OIDC clients, auth codes, and refresh tokens.
const MIGRATION_V6_OIDC_POSTGRES: &str = r#"
    CREATE TABLE IF NOT EXISTS oidc_clients (
        id BIGSERIAL PRIMARY KEY,
        client_id TEXT NOT NULL UNIQUE,
        client_secret_hash TEXT,
        redirect_uris_json TEXT NOT NULL,
        name TEXT,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS oidc_auth_codes (
        id BIGSERIAL PRIMARY KEY,
        code_hash TEXT NOT NULL UNIQUE,
        client_id TEXT NOT NULL,
        user_id BIGINT NOT NULL,
        redirect_uri TEXT NOT NULL,
        code_challenge TEXT NOT NULL,
        code_challenge_method TEXT NOT NULL,
        scope TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        consumed_at TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_oidc_auth_codes_hash ON oidc_auth_codes(code_hash);
    CREATE TABLE IF NOT EXISTS oidc_refresh_tokens (
        id BIGSERIAL PRIMARY KEY,
        token_hash TEXT NOT NULL UNIQUE,
        client_id TEXT NOT NULL,
        user_id BIGINT NOT NULL,
        scope TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        revoked_at TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_oidc_refresh_hash ON oidc_refresh_tokens(token_hash);
"#;

/// Exclusive account_links: one portal identity per store account (#117 Phase 5).
const MIGRATION_V7_EXCLUSIVE_LINKS_SQLITE: &str = r#"
    CREATE UNIQUE INDEX IF NOT EXISTS idx_account_links_account_exclusive ON account_links(account_id);
"#;

/// Postgres unique index so one portal identity owns each store account.
const MIGRATION_V7_EXCLUSIVE_LINKS_POSTGRES: &str = r#"
    CREATE UNIQUE INDEX IF NOT EXISTS idx_account_links_account_exclusive ON account_links(account_id);
"#;

/// Additive migration: session client metadata for operator/portal session lists.
///
/// Plain `ADD COLUMN` (no `IF NOT EXISTS`): the SQLite build bundled with
/// `rusqlite` rejects `ADD COLUMN IF NOT EXISTS` (`near "EXISTS": syntax error`).
const MIGRATION_V8_SESSION_CLIENT_SQLITE: &str = r#"
    ALTER TABLE operator_sessions ADD COLUMN user_agent TEXT;
    ALTER TABLE operator_sessions ADD COLUMN device_type TEXT;
    ALTER TABLE operator_sessions ADD COLUMN client_label TEXT;
    ALTER TABLE portal_sessions ADD COLUMN user_agent TEXT;
    ALTER TABLE portal_sessions ADD COLUMN device_type TEXT;
    ALTER TABLE portal_sessions ADD COLUMN client_label TEXT;
    ALTER TABLE portal_sessions ADD COLUMN last_used_at TEXT;
"#;

/// Postgres session client-metadata columns (`IF NOT EXISTS` is valid here).
const MIGRATION_V8_SESSION_CLIENT_POSTGRES: &str = r#"
    ALTER TABLE operator_sessions ADD COLUMN IF NOT EXISTS user_agent TEXT;
    ALTER TABLE operator_sessions ADD COLUMN IF NOT EXISTS device_type TEXT;
    ALTER TABLE operator_sessions ADD COLUMN IF NOT EXISTS client_label TEXT;
    ALTER TABLE portal_sessions ADD COLUMN IF NOT EXISTS user_agent TEXT;
    ALTER TABLE portal_sessions ADD COLUMN IF NOT EXISTS device_type TEXT;
    ALTER TABLE portal_sessions ADD COLUMN IF NOT EXISTS client_label TEXT;
    ALTER TABLE portal_sessions ADD COLUMN IF NOT EXISTS last_used_at TEXT;
"#;

/// Optional contact email on first-party users (invites / notifications).
const MIGRATION_V9_USER_EMAIL_SQLITE: &str = r#"
    ALTER TABLE users ADD COLUMN email TEXT;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email ON users(email);
"#;

/// Postgres optional unique `users.email` for invites and notifications.
const MIGRATION_V9_USER_EMAIL_POSTGRES: &str = r#"
    ALTER TABLE users ADD COLUMN IF NOT EXISTS email TEXT;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email ON users(email);
"#;

/// OIDC RP login state + WebAuthn credentials / challenges.
const MIGRATION_V10_SSO_WEBAUTHN_SQLITE: &str = r#"
    CREATE TABLE IF NOT EXISTS oidc_rp_states (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        state_hash TEXT NOT NULL UNIQUE,
        provider_id TEXT NOT NULL,
        pkce_verifier TEXT NOT NULL,
        nonce TEXT NOT NULL,
        purpose TEXT NOT NULL,
        user_id INTEGER,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS webauthn_credentials (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL,
        credential_id TEXT NOT NULL UNIQUE,
        passkey_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_used_at TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_webauthn_credentials_user ON webauthn_credentials(user_id);
    CREATE TABLE IF NOT EXISTS webauthn_challenges (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        challenge_id TEXT NOT NULL UNIQUE,
        user_id INTEGER,
        kind TEXT NOT NULL,
        state_json TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_oidc_rp_states_expires ON oidc_rp_states(expires_at);
    CREATE INDEX IF NOT EXISTS idx_webauthn_challenges_expires ON webauthn_challenges(expires_at);
"#;

/// Durable `dbAtomic` receipts (idempotent replay after a lost response).
const MIGRATION_V11_ATOMIC_RECEIPTS_SQLITE: &str = r#"
    CREATE TABLE IF NOT EXISTS db_atomic_receipts (
        operation_id TEXT PRIMARY KEY NOT NULL,
        operation_kind TEXT NOT NULL,
        request_hash TEXT NOT NULL,
        status TEXT NOT NULL,
        payload TEXT,
        created_at TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        consume_key TEXT UNIQUE
    );
    CREATE INDEX IF NOT EXISTS idx_db_atomic_receipts_expires ON db_atomic_receipts(expires_at);
"#;

/// Postgres DDL for OIDC RP login state and WebAuthn credentials/challenges.
const MIGRATION_V10_SSO_WEBAUTHN_POSTGRES: &str = r#"
    CREATE TABLE IF NOT EXISTS oidc_rp_states (
        id BIGSERIAL PRIMARY KEY,
        state_hash TEXT NOT NULL UNIQUE,
        provider_id TEXT NOT NULL,
        pkce_verifier TEXT NOT NULL,
        nonce TEXT NOT NULL,
        purpose TEXT NOT NULL,
        user_id BIGINT,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS webauthn_credentials (
        id BIGSERIAL PRIMARY KEY,
        user_id BIGINT NOT NULL,
        credential_id TEXT NOT NULL UNIQUE,
        passkey_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_used_at TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_webauthn_credentials_user ON webauthn_credentials(user_id);
    CREATE TABLE IF NOT EXISTS webauthn_challenges (
        id BIGSERIAL PRIMARY KEY,
        challenge_id TEXT NOT NULL UNIQUE,
        user_id BIGINT,
        kind TEXT NOT NULL,
        state_json TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_oidc_rp_states_expires ON oidc_rp_states(expires_at);
    CREATE INDEX IF NOT EXISTS idx_webauthn_challenges_expires ON webauthn_challenges(expires_at);
"#;

/// Postgres durable `dbAtomic` receipts for idempotent replay after a lost response.
const MIGRATION_V11_ATOMIC_RECEIPTS_POSTGRES: &str = r#"
    CREATE TABLE IF NOT EXISTS db_atomic_receipts (
        operation_id TEXT PRIMARY KEY NOT NULL,
        operation_kind TEXT NOT NULL,
        request_hash TEXT NOT NULL,
        status TEXT NOT NULL,
        payload TEXT,
        created_at TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        consume_key TEXT UNIQUE
    );
    CREATE INDEX IF NOT EXISTS idx_db_atomic_receipts_expires ON db_atomic_receipts(expires_at);
"#;

/// Durable daemon job queue and associated scratch-path ledger (SQLite).
const MIGRATION_V12_JOBS_SQLITE: &str = r#"
    CREATE TABLE IF NOT EXISTS jobs (
        id TEXT PRIMARY KEY NOT NULL,
        kind TEXT NOT NULL,
        state TEXT NOT NULL,
        priority INTEGER NOT NULL DEFAULT 0,
        resource_class TEXT NOT NULL,
        payload TEXT NOT NULL,
        progress TEXT,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        max_attempts INTEGER NOT NULL DEFAULT 3,
        run_after TEXT NOT NULL,
        lease_owner TEXT,
        lease_expires_at TEXT,
        dedup_key TEXT NOT NULL,
        error_kind TEXT,
        error_message TEXT,
        cancel_requested INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        started_at TEXT,
        finished_at TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_jobs_claim ON jobs(resource_class, state, run_after, priority);
    CREATE INDEX IF NOT EXISTS idx_jobs_dedup ON jobs(dedup_key, state);
    CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state);
    CREATE TABLE IF NOT EXISTS job_temp_paths (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id TEXT NOT NULL,
        path TEXT NOT NULL,
        created_at TEXT NOT NULL,
        FOREIGN KEY(job_id) REFERENCES jobs(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_job_temp_paths_job ON job_temp_paths(job_id);
"#;

/// Durable daemon job queue and associated scratch-path ledger (Postgres / D1).
const MIGRATION_V12_JOBS_POSTGRES: &str = r#"
    CREATE TABLE IF NOT EXISTS jobs (
        id TEXT PRIMARY KEY NOT NULL,
        kind TEXT NOT NULL,
        state TEXT NOT NULL,
        priority BIGINT NOT NULL DEFAULT 0,
        resource_class TEXT NOT NULL,
        payload TEXT NOT NULL,
        progress TEXT,
        attempt_count BIGINT NOT NULL DEFAULT 0,
        max_attempts BIGINT NOT NULL DEFAULT 3,
        run_after TEXT NOT NULL,
        lease_owner TEXT,
        lease_expires_at TEXT,
        dedup_key TEXT NOT NULL,
        error_kind TEXT,
        error_message TEXT,
        cancel_requested BIGINT NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        started_at TEXT,
        finished_at TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_jobs_claim ON jobs(resource_class, state, run_after, priority);
    CREATE INDEX IF NOT EXISTS idx_jobs_dedup ON jobs(dedup_key, state);
    CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state);
    CREATE TABLE IF NOT EXISTS job_temp_paths (
        id BIGSERIAL PRIMARY KEY,
        job_id TEXT NOT NULL,
        path TEXT NOT NULL,
        created_at TEXT NOT NULL,
        FOREIGN KEY(job_id) REFERENCES jobs(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_job_temp_paths_job ON job_temp_paths(job_id);
"#;

/// Lease generation, active-key uniqueness, and reserved scratch bytes (SQLite).
const MIGRATION_V13_JOB_FENCE_SQLITE: &str = r#"
    ALTER TABLE jobs ADD COLUMN lease_generation INTEGER NOT NULL DEFAULT 0;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_dedup_active
        ON jobs(dedup_key) WHERE state IN ('pending', 'running');
    ALTER TABLE job_temp_paths ADD COLUMN reserved_bytes INTEGER NOT NULL DEFAULT 0;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_job_temp_paths_job_path
        ON job_temp_paths(job_id, path);
"#;

/// Lease generation, active-key uniqueness, and reserved scratch bytes (Postgres / D1).
const MIGRATION_V13_JOB_FENCE_POSTGRES: &str = r#"
    ALTER TABLE jobs ADD COLUMN lease_generation BIGINT NOT NULL DEFAULT 0;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_dedup_active
        ON jobs(dedup_key) WHERE state IN ('pending', 'running');
    ALTER TABLE job_temp_paths ADD COLUMN reserved_bytes BIGINT NOT NULL DEFAULT 0;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_job_temp_paths_job_path
        ON job_temp_paths(job_id, path);
"#;

/// Singleton row that serializes admission and quota updates (SQLite).
const MIGRATION_V14_JOB_QUEUE_CONTROL_SQLITE: &str = r#"
    CREATE TABLE IF NOT EXISTS job_queue_control (
        id INTEGER PRIMARY KEY CHECK (id = 1)
    );
    INSERT OR IGNORE INTO job_queue_control (id) VALUES (1);
"#;

/// Singleton row that serializes admission and quota updates (Postgres / D1).
const MIGRATION_V14_JOB_QUEUE_CONTROL_POSTGRES: &str = r#"
    CREATE TABLE IF NOT EXISTS job_queue_control (
        id BIGINT PRIMARY KEY CHECK (id = 1)
    );
    INSERT INTO job_queue_control (id) VALUES (1) ON CONFLICT (id) DO NOTHING;
"#;

/// Durable `users.last_seen_at` for presence after logout (SQLite).
///
/// Backfills from any portal session (including expired) so existing accounts
/// that have signed in are not treated as never-seen.
const MIGRATION_V15_USER_LAST_SEEN_SQLITE: &str = r#"
    ALTER TABLE users ADD COLUMN last_seen_at TEXT;
    UPDATE users SET last_seen_at = (
        SELECT MAX(COALESCE(ps.last_used_at, ps.created_at))
        FROM portal_sessions ps
        INNER JOIN portal_identities pi ON pi.id = ps.identity_id
        WHERE pi.user_id = users.id
    )
    WHERE last_seen_at IS NULL;
"#;

/// Durable `users.last_seen_at` for presence after logout (Postgres / D1).
const MIGRATION_V15_USER_LAST_SEEN_POSTGRES: &str = r#"
    ALTER TABLE users ADD COLUMN IF NOT EXISTS last_seen_at TEXT;
    UPDATE users SET last_seen_at = (
        SELECT MAX(COALESCE(ps.last_used_at, ps.created_at))
        FROM portal_sessions ps
        INNER JOIN portal_identities pi ON pi.id = ps.identity_id
        WHERE pi.user_id = users.id
    )
    WHERE last_seen_at IS NULL;
"#;

/// Profile picture choice plus IdP-supplied avatar URLs (SQLite).
///
/// `users.avatar_source` is `NULL`/`auto`, `monogram`, `gravatar`, `upload`, or
/// `sso:{portal_identities.id}`. `portal_identities.picture_url` stores the last
/// HTTPS picture from the identity provider.
const MIGRATION_V16_AVATAR_SOURCE_SQLITE: &str = r#"
    ALTER TABLE users ADD COLUMN avatar_source TEXT;
    ALTER TABLE portal_identities ADD COLUMN picture_url TEXT;
"#;

/// Profile picture choice plus IdP-supplied avatar URLs (Postgres / D1).
const MIGRATION_V16_AVATAR_SOURCE_POSTGRES: &str = r#"
    ALTER TABLE users ADD COLUMN IF NOT EXISTS avatar_source TEXT;
    ALTER TABLE portal_identities ADD COLUMN IF NOT EXISTS picture_url TEXT;
"#;

/// Passkey display names plus a durable TOTP-enrolled flag on users.
const MIGRATION_V17_PASSKEY_NAME_TOTP_SQLITE: &str = r#"
    ALTER TABLE webauthn_credentials ADD COLUMN name TEXT;
    ALTER TABLE users ADD COLUMN totp_enabled INTEGER NOT NULL DEFAULT 0;
"#;

/// Postgres passkey names and TOTP enrolled flag.
const MIGRATION_V17_PASSKEY_NAME_TOTP_POSTGRES: &str = r#"
    ALTER TABLE webauthn_credentials ADD COLUMN IF NOT EXISTS name TEXT;
    ALTER TABLE users ADD COLUMN IF NOT EXISTS totp_enabled BIGINT NOT NULL DEFAULT 0;
"#;

/// OIDC AS client token policy (refresh + allowed scopes).
const MIGRATION_V18_OIDC_CLIENT_POLICY_SQLITE: &str = r#"
    ALTER TABLE oidc_clients ADD COLUMN issue_refresh_token INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE oidc_clients ADD COLUMN allowed_scopes_json TEXT NOT NULL DEFAULT '["openid","profile","email"]';
"#;

/// Postgres OIDC AS client token policy.
const MIGRATION_V18_OIDC_CLIENT_POLICY_POSTGRES: &str = r#"
    ALTER TABLE oidc_clients ADD COLUMN IF NOT EXISTS issue_refresh_token BIGINT NOT NULL DEFAULT 1;
    ALTER TABLE oidc_clients ADD COLUMN IF NOT EXISTS allowed_scopes_json TEXT NOT NULL DEFAULT '["openid","profile","email"]';
"#;

/// OIDC client enable flag + plugin ownership (see docs/adr/plugin-oidc-clients.md).
const MIGRATION_V19_OIDC_CLIENT_PLUGIN_SQLITE: &str = r#"
    ALTER TABLE oidc_clients ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE oidc_clients ADD COLUMN plugin_id TEXT;
    UPDATE oidc_clients SET plugin_id = 'audiobookshelf' WHERE client_id = 'audiobookshelf';
"#;

/// Postgres OIDC client enable flag + plugin ownership.
const MIGRATION_V19_OIDC_CLIENT_PLUGIN_POSTGRES: &str = r#"
    ALTER TABLE oidc_clients ADD COLUMN IF NOT EXISTS enabled BIGINT NOT NULL DEFAULT 1;
    ALTER TABLE oidc_clients ADD COLUMN IF NOT EXISTS plugin_id TEXT;
    UPDATE oidc_clients SET plugin_id = 'audiobookshelf' WHERE client_id = 'audiobookshelf' AND plugin_id IS NULL;
"#;

/// Appearance preference (`system` / `light` / `dark`) on user_preferences.
const MIGRATION_V20_THEME_SQLITE: &str = r#"
    ALTER TABLE user_preferences ADD COLUMN theme TEXT NOT NULL DEFAULT 'system';
"#;

/// Postgres appearance preference on user_preferences.
const MIGRATION_V20_THEME_POSTGRES: &str = r#"
    ALTER TABLE user_preferences ADD COLUMN IF NOT EXISTS theme TEXT NOT NULL DEFAULT 'system';
"#;

/// Ordered migration list for local SQLite files (`PRAGMA user_version`).
#[must_use]
pub fn migration_sql() -> &'static [&'static str] {
    &[
        SQLITE_SCHEMA,
        MIGRATION_V2_OPERATOR_SESSIONS_SQLITE,
        MIGRATION_V3_USERS_SQLITE,
        MIGRATION_V3_PORTAL_USER_ID_SQLITE,
        MIGRATION_V4_ELEVATE_AUDIT_SQLITE,
        MIGRATION_V5_PROVISIONING_SQLITE,
        MIGRATION_V6_OIDC_SQLITE,
        MIGRATION_V7_EXCLUSIVE_LINKS_SQLITE,
        MIGRATION_V8_SESSION_CLIENT_SQLITE,
        MIGRATION_V9_USER_EMAIL_SQLITE,
        MIGRATION_V10_SSO_WEBAUTHN_SQLITE,
        MIGRATION_V11_ATOMIC_RECEIPTS_SQLITE,
        MIGRATION_V12_JOBS_SQLITE,
        MIGRATION_V13_JOB_FENCE_SQLITE,
        MIGRATION_V14_JOB_QUEUE_CONTROL_SQLITE,
        MIGRATION_V15_USER_LAST_SEEN_SQLITE,
        MIGRATION_V16_AVATAR_SOURCE_SQLITE,
        MIGRATION_V17_PASSKEY_NAME_TOTP_SQLITE,
        MIGRATION_V18_OIDC_CLIENT_POLICY_SQLITE,
        MIGRATION_V19_OIDC_CLIENT_PLUGIN_SQLITE,
        MIGRATION_V20_THEME_SQLITE,
    ]
}

/// Ordered DDL for D1 / Postgres `schema_migrations` versioning.
#[must_use]
pub fn migration_sql_postgres() -> &'static [&'static str] {
    &[
        POSTGRES_SCHEMA,
        MIGRATION_V2_OPERATOR_SESSIONS_POSTGRES,
        MIGRATION_V3_USERS_POSTGRES,
        MIGRATION_V3_PORTAL_USER_ID_POSTGRES,
        MIGRATION_V4_ELEVATE_AUDIT_POSTGRES,
        MIGRATION_V5_PROVISIONING_POSTGRES,
        MIGRATION_V6_OIDC_POSTGRES,
        MIGRATION_V7_EXCLUSIVE_LINKS_POSTGRES,
        MIGRATION_V8_SESSION_CLIENT_POSTGRES,
        MIGRATION_V9_USER_EMAIL_POSTGRES,
        MIGRATION_V10_SSO_WEBAUTHN_POSTGRES,
        MIGRATION_V11_ATOMIC_RECEIPTS_POSTGRES,
        MIGRATION_V12_JOBS_POSTGRES,
        MIGRATION_V13_JOB_FENCE_POSTGRES,
        MIGRATION_V14_JOB_QUEUE_CONTROL_POSTGRES,
        MIGRATION_V15_USER_LAST_SEEN_POSTGRES,
        MIGRATION_V16_AVATAR_SOURCE_POSTGRES,
        MIGRATION_V17_PASSKEY_NAME_TOTP_POSTGRES,
        MIGRATION_V18_OIDC_CLIENT_POLICY_POSTGRES,
        MIGRATION_V19_OIDC_CLIENT_PLUGIN_POSTGRES,
        MIGRATION_V20_THEME_POSTGRES,
    ]
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

/// Greenfield Postgres/D1 DDL mirroring [`SQLITE_SCHEMA`] with `BIGINT` / `BIGSERIAL`.
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

        CREATE TABLE IF NOT EXISTS users (
            id BIGSERIAL PRIMARY KEY,
            role TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            display_name TEXT,
            password_hash TEXT,
            security_version BIGINT NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_users_role ON users(role);
        CREATE INDEX IF NOT EXISTS idx_users_status ON users(status);

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

        CREATE TABLE IF NOT EXISTS operator_sessions (
            id BIGSERIAL PRIMARY KEY,
            token_hash TEXT NOT NULL UNIQUE,
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_used_at TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_operator_sessions_hash ON operator_sessions(token_hash);

        CREATE TABLE IF NOT EXISTS security_audit_events (
            id BIGSERIAL PRIMARY KEY,
            at TEXT NOT NULL,
            actor TEXT NOT NULL,
            action TEXT NOT NULL,
            detail_json TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_security_audit_at ON security_audit_events(at);

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
            cover_url TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_title_requests_status ON title_requests(status);
        CREATE INDEX IF NOT EXISTS idx_title_requests_identity ON title_requests(identity_id);
        CREATE INDEX IF NOT EXISTS idx_title_requests_work_key ON title_requests(work_key);
        CREATE INDEX IF NOT EXISTS idx_title_requests_identity_status ON title_requests(identity_id, status);

        CREATE TABLE IF NOT EXISTS title_request_sources (
            id BIGSERIAL PRIMARY KEY,
            title_request_id BIGINT NOT NULL REFERENCES title_requests(id) ON DELETE CASCADE,
            source TEXT NOT NULL,
            product_id TEXT NOT NULL,
            title TEXT,
            subtitle TEXT,
            authors TEXT,
            narrators TEXT,
            series TEXT,
            series_index TEXT,
            asin TEXT,
            isbn TEXT,
            description TEXT,
            publisher TEXT,
            length_minutes BIGINT,
            published_at TEXT,
            categories TEXT,
            language TEXT,
            cover_url TEXT,
            url TEXT,
            price_cents BIGINT,
            currency TEXT,
            price_label TEXT,
            list_price_cents BIGINT,
            list_price_label TEXT,
            member_price_cents BIGINT,
            member_price_label TEXT,
            observed_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(title_request_id, source, product_id)
        );

        CREATE INDEX IF NOT EXISTS idx_trs_request ON title_request_sources(title_request_id);

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
            discover_sort TEXT NOT NULL DEFAULT 'relevance',
            discover_sort_dir TEXT NOT NULL DEFAULT 'desc',
            discover_language TEXT,
            discover_excluded_sources_json TEXT NOT NULL DEFAULT '[]',
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
