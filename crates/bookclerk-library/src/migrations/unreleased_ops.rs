//! Unreleased host schema ops; this ordered list is the source of truth.

use super::MigrationOp;

/// Live unreleased host schema (one BookclerkSQL statement per op).
pub(super) const UNRELEASED_OPS: &[MigrationOp] = &[
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS accounts (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        account_id TEXT NOT NULL UNIQUE,
        marketplace TEXT NOT NULL,
        label TEXT,
        scan_enabled INTEGER NOT NULL DEFAULT 1,
        source TEXT NOT NULL DEFAULT 'audible',
        connection_status TEXT NOT NULL DEFAULT 'active',
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS books (
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
    )",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_uuid ON books(uuid)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_status ON books(acquire_status)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_account ON books(account_id)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_title ON books(title)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_pdf_status ON books(pdf_status)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_tags ON books(tags)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_series_asin ON books(series_asin)"),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_books_content_kind ON books(content_kind)",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_isbn ON books(isbn)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_source ON books(source)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_asin ON books(asin)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_product_id ON books(product_id)"),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS ignored_titles (
        source TEXT NOT NULL,
        account_id TEXT NOT NULL,
        product_id TEXT NOT NULL,
        reason TEXT,
        created_at TEXT NOT NULL,
        PRIMARY KEY (source, account_id, product_id),
        FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS saved_filters (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL UNIQUE,
        query TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        role TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active',
        display_name TEXT,
        login_name TEXT,
        email TEXT,
        password_hash TEXT,
        security_version INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        last_seen_at TEXT,
        avatar_source TEXT,
        totp_enabled INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_users_role ON users(role)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_users_status ON users(status)"),
    MigrationOp::Schema(
        r"CREATE UNIQUE INDEX IF NOT EXISTS idx_users_login_name ON users(login_name)",
    ),
    MigrationOp::Schema(r"CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email ON users(email)"),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS portal_identities (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        provider TEXT NOT NULL,
        external_user_id TEXT NOT NULL,
        label TEXT,
        user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
        created_at TEXT NOT NULL,
        picture_url TEXT,
        UNIQUE(provider, external_user_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_portal_identities_user ON portal_identities(user_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS claim_tickets (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        identity_id INTEGER,
        expires_at TEXT NOT NULL,
        redeemed_at TEXT,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE SET NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_claim_tickets_hash ON claim_tickets(token_hash)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_claim_tickets_identity ON claim_tickets(identity_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS portal_sessions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        identity_id INTEGER NOT NULL,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL,
        user_agent TEXT,
        device_type TEXT,
        client_label TEXT,
        last_used_at TEXT,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_portal_sessions_hash ON portal_sessions(token_hash)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS operator_sessions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_used_at TEXT,
        elevated_from_user_id INTEGER,
        impersonating_user_id INTEGER,
        user_agent TEXT,
        device_type TEXT,
        client_label TEXT
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_operator_sessions_hash ON operator_sessions(token_hash)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS security_audit_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at TEXT NOT NULL,
        actor TEXT NOT NULL,
        action TEXT NOT NULL,
        detail_json TEXT
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_security_audit_at ON security_audit_events(at)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS account_links (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        identity_id INTEGER NOT NULL,
        account_id TEXT NOT NULL,
        source TEXT NOT NULL,
        created_at TEXT NOT NULL,
        UNIQUE(identity_id, account_id),
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE,
        FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_account_links_account ON account_links(account_id)",
    ),
    MigrationOp::Schema(
        r"CREATE UNIQUE INDEX IF NOT EXISTS idx_account_links_account_exclusive ON account_links(account_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS works (
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
    )",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_works_asin ON works(canonical_asin)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_works_isbn ON works(canonical_isbn)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_works_title ON works(title)"),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS work_editions (
        work_id TEXT NOT NULL,
        book_uuid TEXT NOT NULL UNIQUE,
        created_at TEXT NOT NULL,
        PRIMARY KEY (work_id, book_uuid),
        FOREIGN KEY(work_id) REFERENCES works(id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_work_editions_book ON work_editions(book_uuid)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS listening_progress (
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
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_listening_book ON listening_progress(book_uuid)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_listening_work ON listening_progress(work_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_listening_user ON listening_progress(provider, external_user_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS title_requests (
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
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_title_requests_status ON title_requests(status)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_title_requests_identity ON title_requests(identity_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_title_requests_work_key ON title_requests(work_key)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_title_requests_identity_status ON title_requests(identity_id, status)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS title_request_sources (
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
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_trs_request ON title_request_sources(title_request_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS embeddings (
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
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_embeddings_target ON embeddings(target_kind, target_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS user_preferences (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        subject_key TEXT NOT NULL UNIQUE,
        identity_id INTEGER,
        default_view TEXT NOT NULL DEFAULT 'discover',
        disabled_shelves_json TEXT NOT NULL DEFAULT '[]',
        discover_sort TEXT NOT NULL DEFAULT 'relevance',
        discover_sort_dir TEXT NOT NULL DEFAULT 'desc',
        discover_language TEXT,
        discover_excluded_sources_json TEXT NOT NULL DEFAULT '[]',
        theme TEXT NOT NULL DEFAULT 'system',
        updated_at TEXT NOT NULL,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_user_preferences_identity ON user_preferences(identity_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS encrypted_secrets (
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
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_kind ON encrypted_secrets(kind)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_account ON encrypted_secrets(account_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_encrypted_secrets_account_type ON encrypted_secrets(account_type)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS user_invites (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        role TEXT NOT NULL,
        login_name TEXT,
        display_name TEXT,
        expires_at TEXT NOT NULL,
        redeemed_at TEXT,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_user_invites_hash ON user_invites(token_hash)",
    ),
    MigrationOp::Schema(
        r#"CREATE TABLE IF NOT EXISTS oidc_clients (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        client_id TEXT NOT NULL UNIQUE,
        client_secret_hash TEXT,
        redirect_uris_json TEXT NOT NULL,
        name TEXT,
        created_at TEXT NOT NULL,
        issue_refresh_token INTEGER NOT NULL DEFAULT 1,
        allowed_scopes_json TEXT NOT NULL DEFAULT '["openid","profile","email"]',
        enabled INTEGER NOT NULL DEFAULT 1,
        plugin_id TEXT
    )"#,
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS oidc_auth_codes (
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
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_oidc_auth_codes_hash ON oidc_auth_codes(code_hash)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS oidc_refresh_tokens (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        token_hash TEXT NOT NULL UNIQUE,
        client_id TEXT NOT NULL,
        user_id INTEGER NOT NULL,
        scope TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        revoked_at TEXT,
        created_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_oidc_refresh_hash ON oidc_refresh_tokens(token_hash)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS oidc_rp_states (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        state_hash TEXT NOT NULL UNIQUE,
        provider_id TEXT NOT NULL,
        pkce_verifier TEXT NOT NULL,
        nonce TEXT NOT NULL,
        purpose TEXT NOT NULL,
        user_id INTEGER,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS webauthn_credentials (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL,
        credential_id TEXT NOT NULL UNIQUE,
        passkey_json TEXT NOT NULL,
        name TEXT,
        created_at TEXT NOT NULL,
        last_used_at TEXT
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_webauthn_credentials_user ON webauthn_credentials(user_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS webauthn_challenges (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        challenge_id TEXT NOT NULL UNIQUE,
        user_id INTEGER,
        kind TEXT NOT NULL,
        state_json TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        created_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_oidc_rp_states_expires ON oidc_rp_states(expires_at)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_webauthn_challenges_expires ON webauthn_challenges(expires_at)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS db_atomic_receipts (
        operation_id TEXT PRIMARY KEY NOT NULL,
        operation_kind TEXT NOT NULL,
        request_hash TEXT NOT NULL,
        status TEXT NOT NULL,
        payload TEXT,
        created_at TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        consume_key TEXT UNIQUE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_db_atomic_receipts_expires ON db_atomic_receipts(expires_at)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS jobs (
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
        finished_at TEXT,
        lease_generation INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_jobs_claim ON jobs(resource_class, state, run_after, priority)",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_jobs_dedup ON jobs(dedup_key, state)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state)"),
    MigrationOp::Schema(
        r"CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_dedup_active
        ON jobs(dedup_key) WHERE state IN ('pending', 'running')",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS job_temp_paths (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id TEXT NOT NULL,
        path TEXT NOT NULL,
        created_at TEXT NOT NULL,
        reserved_bytes INTEGER NOT NULL DEFAULT 0,
        FOREIGN KEY(job_id) REFERENCES jobs(id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_job_temp_paths_job ON job_temp_paths(job_id)",
    ),
    MigrationOp::Schema(
        r"CREATE UNIQUE INDEX IF NOT EXISTS idx_job_temp_paths_job_path
        ON job_temp_paths(job_id, path)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS job_queue_control (
        id INTEGER PRIMARY KEY CHECK (id = 1)
    )",
    ),
    MigrationOp::Data(r"INSERT OR IGNORE INTO job_queue_control (id) VALUES (1)"),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS domain_events (
        id TEXT PRIMARY KEY NOT NULL,
        event_type TEXT NOT NULL,
        schema_version INTEGER NOT NULL,
        occurred_at TEXT NOT NULL,
        account_id TEXT NOT NULL DEFAULT '',
        source TEXT NOT NULL DEFAULT '',
        correlation_id TEXT NOT NULL DEFAULT '',
        causation_id TEXT NOT NULL DEFAULT '',
        dedup_key TEXT NOT NULL,
        payload TEXT NOT NULL,
        ordering_key TEXT NOT NULL DEFAULT '',
        dispatch_state TEXT NOT NULL,
        created_at TEXT NOT NULL,
        wake_pending INTEGER NOT NULL DEFAULT 1,
        wake_lease_owner TEXT,
        wake_lease_expires_at TEXT,
        wake_cursor_at TEXT NOT NULL DEFAULT '',
        wake_cursor_id TEXT NOT NULL DEFAULT '',
        dispatch_snapshot_json TEXT NOT NULL DEFAULT '',
        UNIQUE(account_id, source, event_type, dedup_key)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_domain_events_dispatch ON domain_events(dispatch_state, created_at)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_domain_events_dispatch_created ON domain_events(dispatch_state, created_at, id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_domain_events_wake_pending ON domain_events(created_at, id) WHERE wake_pending = 1",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS event_deliveries (
        id TEXT PRIMARY KEY NOT NULL,
        event_id TEXT NOT NULL,
        plugin_id TEXT NOT NULL,
        idempotency_key TEXT NOT NULL UNIQUE,
        state TEXT NOT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        max_attempts INTEGER NOT NULL DEFAULT 8,
        lease_owner TEXT,
        lease_expires_at TEXT,
        lease_generation INTEGER NOT NULL DEFAULT 0,
        run_after TEXT NOT NULL,
        invocation_sequence INTEGER NOT NULL DEFAULT 0,
        resume_pending INTEGER NOT NULL DEFAULT 0,
        checkpoint_json TEXT,
        checkpoint_schema_version INTEGER NOT NULL DEFAULT 0,
        ordering_key TEXT NOT NULL DEFAULT '',
        outcome TEXT,
        error_message TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        cancel_requested INTEGER NOT NULL DEFAULT 0,
        resource_class TEXT NOT NULL DEFAULT 'network',
        wake_event_type TEXT NOT NULL DEFAULT '',
        wake_filter_json TEXT NOT NULL DEFAULT '',
        wake_grants_json TEXT NOT NULL DEFAULT '',
        UNIQUE(event_id, plugin_id),
        FOREIGN KEY(event_id) REFERENCES domain_events(id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_claim ON event_deliveries(state, run_after, created_at)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_plugin_order ON event_deliveries(plugin_id, ordering_key, created_at)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_state ON event_deliveries(state)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_wake ON event_deliveries(state, wake_event_type)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_plugin_running ON event_deliveries(plugin_id, state)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS event_subscriber_nodes (
        node_id TEXT NOT NULL,
        plugin_id TEXT NOT NULL,
        subscriptions_json TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1,
        heartbeat_at TEXT NOT NULL,
        PRIMARY KEY (node_id, plugin_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_subscriber_nodes_heartbeat
        ON event_subscriber_nodes(heartbeat_at)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS event_outbox_stats (
        id INTEGER PRIMARY KEY NOT NULL,
        retries_total INTEGER NOT NULL DEFAULT 0,
        suspensions_total INTEGER NOT NULL DEFAULT 0,
        dead_letters_total INTEGER NOT NULL DEFAULT 0,
        dispatch_latency_ms_sum INTEGER NOT NULL DEFAULT 0,
        dispatch_count INTEGER NOT NULL DEFAULT 0,
        handler_latency_ms_sum INTEGER NOT NULL DEFAULT 0,
        handler_count INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Data(
        r"INSERT OR IGNORE INTO event_outbox_stats (
        id, retries_total, suspensions_total, dead_letters_total,
        dispatch_latency_ms_sum, dispatch_count, handler_latency_ms_sum, handler_count
    ) VALUES (1, 0, 0, 0, 0, 0, 0, 0)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS db_serialization_slots (
        slot_key TEXT PRIMARY KEY NOT NULL,
        bump INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_databases (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        plugin_id TEXT NOT NULL,
        binding TEXT NOT NULL,
        backend_kind TEXT NOT NULL,
        unit_ref TEXT NOT NULL,
        created_at TEXT NOT NULL,
        UNIQUE(plugin_id, binding)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_plugin_databases_plugin ON plugin_databases(plugin_id)",
    ),
];
