//! Unreleased host schema ops; this ordered list is the source of truth.

use super::MigrationOp;

/// Live unreleased host schema (one BookclerkSQL statement per op).
pub(super) const UNRELEASED_OPS: &[MigrationOp] = &[
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS accounts (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        plugin_key TEXT NOT NULL,
        account_id TEXT NOT NULL,
        marketplace TEXT NOT NULL,
        label TEXT,
        scan_enabled INTEGER NOT NULL DEFAULT 1,
        source TEXT NOT NULL,
        connection_status TEXT NOT NULL DEFAULT 'active',
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(plugin_key, account_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_accounts_account_id ON accounts(account_id)",
    ),
    // Where each account's storefront scan stands (`store::scan_state`): the
    // opaque sync token the plugin returned at the end of its last pass, the
    // cursor and token of a pass that stopped part way (so the scan job
    // resumes there), when the last complete pass ended (a full pass is
    // forced once that is older than the full-pass interval), and the id of
    // the pass in progress, which each page stamps on the holdings it lists
    // (`holdings.seen_pass`).
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS account_scan_state (
        plugin_key TEXT NOT NULL,
        account_id TEXT NOT NULL,
        sync_token TEXT NOT NULL DEFAULT '',
        resume_cursor TEXT NOT NULL DEFAULT '',
        resume_token TEXT NOT NULL DEFAULT '',
        last_complete_at TEXT,
        pass_id TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL,
        PRIMARY KEY (plugin_key, account_id),
        FOREIGN KEY(plugin_key, account_id) REFERENCES accounts(plugin_key, account_id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS books (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        uuid TEXT NOT NULL UNIQUE,
        plugin_key TEXT NOT NULL,
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
        UNIQUE(plugin_key, account_id, product_id),
        FOREIGN KEY(plugin_key, account_id) REFERENCES accounts(plugin_key, account_id) ON DELETE CASCADE
    )",
    ),
    // Search hydration matches `lower(uuid)`. A btree on the expression keeps
    // that predicate off a table scan. Stored uuids that differ only by ASCII
    // case stay distinct rows. Postgres lowering wraps the argument as
    // `lower((uuid COLLATE "C"))` so the fold matches SQLite. `IF NOT EXISTS`
    // leaves an index created from the older expression in place.
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_uuid_lower ON books(lower(uuid))"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_status ON books(acquire_status)"),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_books_account ON books(plugin_key, account_id)",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_title ON books(title)"),
    // Title order for library pages is ASCII case-fold, then code points, then
    // `uuid` in code-point order. SQLite serves the fold with `COLLATE NOCASE`
    // and the tie with BINARY. Postgres lowering rewrites the fold to
    // `lower(title COLLATE "C")`, the tie to `(uuid COLLATE "C")`, and marks
    // the index `NULLS FIRST`. Binary `idx_books_title` cannot serve the plan.
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_books_page_title ON books(title COLLATE NOCASE, uuid)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_books_page_status ON books(acquire_status, title COLLATE NOCASE, uuid)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_books_page_account ON books(account_id, title COLLATE NOCASE, uuid)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_books_page_account_status ON books(account_id, acquire_status, title COLLATE NOCASE, uuid)",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_pdf_status ON books(pdf_status)"),
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
        plugin_key TEXT NOT NULL,
        account_id TEXT NOT NULL,
        product_id TEXT NOT NULL,
        reason TEXT,
        created_at TEXT NOT NULL,
        PRIMARY KEY (plugin_key, account_id, product_id),
        FOREIGN KEY(plugin_key, account_id) REFERENCES accounts(plugin_key, account_id) ON DELETE CASCADE
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
        plugin_key TEXT NOT NULL,
        account_id TEXT NOT NULL,
        source TEXT NOT NULL,
        created_at TEXT NOT NULL,
        UNIQUE(identity_id, plugin_key, account_id),
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE CASCADE,
        FOREIGN KEY(plugin_key, account_id) REFERENCES accounts(plugin_key, account_id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_account_links_account ON account_links(plugin_key, account_id)",
    ),
    MigrationOp::Schema(
        r"CREATE UNIQUE INDEX IF NOT EXISTS idx_account_links_account_exclusive ON account_links(plugin_key, account_id)",
    ),
    // The operator's per-member rule "may connect store accounts here". No
    // row means allowed (the default); `may_connect = 0` refuses a connect
    // on this instance and any account that arrives for the member from
    // another library (`control_plane::carried_accounts`).
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS member_account_rules (
        user_id INTEGER PRIMARY KEY,
        may_connect INTEGER NOT NULL,
        updated_at TEXT NOT NULL,
        FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
    )",
    ),
    // Where a store account travelled with its person: `sent` to a branch
    // (`peer = node:<node_id>`) or `received` from a library this install
    // joined (`peer = link:<cluster_id>`). Read to disconnect an account
    // everywhere and to keep a single-keeper login in step.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS account_shares (
        plugin_key TEXT NOT NULL,
        account_id TEXT NOT NULL,
        peer TEXT NOT NULL,
        direction TEXT NOT NULL CHECK (direction IN ('sent', 'received')),
        shared_at TEXT NOT NULL,
        PRIMARY KEY (plugin_key, account_id, peer, direction),
        FOREIGN KEY(plugin_key, account_id) REFERENCES accounts(plugin_key, account_id) ON DELETE CASCADE
    )",
    ),
    // The library data model (docs/adr/library-data-model.md): FRBR works,
    // expressions and editions (manifestations), the store listings that
    // sell editions and the holdings that own listings. The CHECK lists are
    // the stored spellings of `bookclerk_resolve::kinds`
    // (`library_model_checks_match_the_resolver_enums`).
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS works (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK (kind IN ('book', 'podcast_show', 'podcast_episode', 'drama')),
        title TEXT NOT NULL,
        match_key TEXT NOT NULL DEFAULT '',
        parent_work_id TEXT REFERENCES works(id) ON DELETE SET NULL,
        description TEXT,
        subjects TEXT,
        categories TEXT,
        cover_url TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_works_match ON works(match_key)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_works_parent ON works(parent_work_id)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_works_title ON works(title)"),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS work_identifiers (
        scheme TEXT NOT NULL CHECK (scheme IN ('isbn', 'asin', 'openlibrary_edition', 'openlibrary_work')),
        value TEXT NOT NULL,
        work_id TEXT NOT NULL REFERENCES works(id) ON DELETE CASCADE,
        created_at TEXT NOT NULL,
        PRIMARY KEY (scheme, value)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_work_identifiers_work ON work_identifiers(work_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS expressions (
        id TEXT PRIMARY KEY,
        work_id TEXT NOT NULL REFERENCES works(id) ON DELETE CASCADE,
        kind TEXT NOT NULL CHECK (kind IN ('narration', 'text', 'translation', 'dramatisation', 'podcast_audio')),
        language TEXT,
        abridgement TEXT NOT NULL CHECK (abridgement IN ('unknown', 'unabridged', 'abridged')),
        credit_key TEXT NOT NULL DEFAULT '',
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_expressions_work ON expressions(work_id)"),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS editions (
        id TEXT PRIMARY KEY,
        expression_id TEXT NOT NULL REFERENCES expressions(id) ON DELETE CASCADE,
        media_kind TEXT NOT NULL CHECK (media_kind IN ('audiobook', 'ebook', 'print', 'podcast_audio')),
        title TEXT NOT NULL,
        subtitle TEXT,
        published_at TEXT,
        duration_minutes INTEGER,
        cover_url TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_editions_expression ON editions(expression_id)",
    ),
    // `strong_key` is set for strong schemes only; NULLs never conflict, so
    // a strong identifier names one edition while a weak one (an ISBN) can
    // sit on several.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS edition_identifiers (
        edition_id TEXT NOT NULL REFERENCES editions(id) ON DELETE CASCADE,
        scheme TEXT NOT NULL CHECK (scheme IN ('isbn', 'asin', 'openlibrary_edition', 'openlibrary_work')),
        value TEXT NOT NULL,
        marketplace TEXT NOT NULL DEFAULT '',
        strength TEXT NOT NULL CHECK (strength IN ('strong', 'weak')),
        strong_key TEXT UNIQUE,
        created_at TEXT NOT NULL,
        PRIMARY KEY (edition_id, scheme, value, marketplace)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_edition_identifiers_value ON edition_identifiers(scheme, value, marketplace)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS contributors (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        sort_name TEXT NOT NULL,
        name_key TEXT NOT NULL UNIQUE,
        created_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS credits (
        target_kind TEXT NOT NULL CHECK (target_kind IN ('work', 'expression', 'edition')),
        target_id TEXT NOT NULL,
        contributor_id TEXT NOT NULL REFERENCES contributors(id) ON DELETE CASCADE,
        role TEXT NOT NULL CHECK (role IN ('author', 'narrator', 'translator', 'editor', 'illustrator', 'publisher', 'host', 'cast', 'adaptor')),
        position INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        PRIMARY KEY (target_kind, target_id, contributor_id, role)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_credits_contributor ON credits(contributor_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS series (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        name_key TEXT NOT NULL UNIQUE,
        created_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS series_entries (
        series_id TEXT NOT NULL REFERENCES series(id) ON DELETE CASCADE,
        work_id TEXT NOT NULL REFERENCES works(id) ON DELETE CASCADE,
        position TEXT,
        sort_position REAL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (series_id, work_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_series_entries_work ON series_entries(work_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS store_listings (
        id TEXT PRIMARY KEY,
        plugin_key TEXT NOT NULL,
        marketplace TEXT NOT NULL,
        product_id TEXT NOT NULL,
        edition_id TEXT NOT NULL REFERENCES editions(id) ON DELETE CASCADE,
        title TEXT NOT NULL,
        categories TEXT,
        subtitle TEXT,
        authors TEXT,
        narrators TEXT,
        series TEXT,
        series_index TEXT,
        description TEXT,
        publisher TEXT,
        published_at TEXT,
        length_minutes INTEGER,
        language TEXT,
        cover_url TEXT,
        url TEXT,
        rating_overall REAL,
        rating_count INTEGER,
        is_abridged INTEGER,
        observed_at TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(plugin_key, marketplace, product_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_store_listings_edition ON store_listings(edition_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_store_listings_observed ON store_listings(observed_at)",
    ),
    // What a listing costs, per audience: `non_member` is what someone
    // without an account at the store pays, `member` what a member or
    // subscriber pays. One row per listing and audience, refreshed in place.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS listing_offers (
        listing_id TEXT NOT NULL REFERENCES store_listings(id) ON DELETE CASCADE,
        audience TEXT NOT NULL CHECK (audience IN ('non_member', 'member')),
        price_cents INTEGER,
        price_label TEXT,
        currency TEXT,
        observed_at TEXT NOT NULL,
        PRIMARY KEY (listing_id, audience)
    )",
    ),
    // An Administrator's decision the resolver obeys: pin a listing to an
    // edition, or keep the work matcher away from a work it split.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS resolution_overrides (
        subject_kind TEXT NOT NULL CHECK (subject_kind IN ('listing', 'work')),
        subject_id TEXT NOT NULL,
        action TEXT NOT NULL CHECK (action IN ('pin_edition', 'isolate')),
        target_id TEXT,
        created_by_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (subject_kind, subject_id)
    )",
    ),
    // What the resolver could not decide, for an Administrator: the
    // conflict's kind and the JSON array of edition or work ids involved.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS resolution_conflicts (
        listing_id TEXT NOT NULL REFERENCES store_listings(id) ON DELETE CASCADE,
        kind TEXT NOT NULL CHECK (kind IN ('strong_identifiers', 'work_identifiers', 'ambiguous_work', 'work_kind_mismatch')),
        subject_ids TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (listing_id, kind, subject_ids)
    )",
    ),
    // One user's own state for a work (docs/adr/library-data-model.md):
    // finished flag, ratings and tags. Members who share a storefront
    // account each keep their own.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS user_title_state (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        work_id TEXT NOT NULL REFERENCES works(id) ON DELETE CASCADE,
        is_finished INTEGER NOT NULL DEFAULT 0,
        rating_overall REAL,
        rating_performance REAL,
        rating_story REAL,
        tags TEXT,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (user_id, work_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_user_title_state_work ON user_title_state(work_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS listening_progress (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        identity_id INTEGER,
        provider TEXT NOT NULL,
        external_user_id TEXT NOT NULL,
        work_id TEXT NOT NULL REFERENCES works(id) ON DELETE CASCADE,
        edition_id TEXT REFERENCES editions(id) ON DELETE SET NULL,
        external_item_id TEXT NOT NULL,
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
        r"CREATE INDEX IF NOT EXISTS idx_listening_work ON listening_progress(work_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_listening_identity ON listening_progress(identity_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_listening_user ON listening_progress(provider, external_user_id)",
    ),
    // A wish names the work it is for; the title, authors and identifiers
    // live on the work and on the wish's storefront sources, never copied
    // here. `edition_id` is set when the wish named an edition the library
    // knows.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS title_requests (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        uuid TEXT NOT NULL UNIQUE,
        identity_id INTEGER,
        work_id TEXT NOT NULL REFERENCES works(id),
        edition_id TEXT REFERENCES editions(id) ON DELETE SET NULL,
        notes TEXT,
        status TEXT NOT NULL DEFAULT 'open',
        preferred_source TEXT,
        resolved_book_uuid TEXT,
        cover_url TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        FOREIGN KEY(identity_id) REFERENCES portal_identities(id) ON DELETE SET NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_title_requests_status_created ON title_requests(status, created_at)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_title_requests_identity ON title_requests(identity_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_title_requests_work ON title_requests(work_id, status)",
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
        listing_id TEXT REFERENCES store_listings(id) ON DELETE SET NULL,
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
        r"CREATE INDEX IF NOT EXISTS idx_trs_listing ON title_request_sources(listing_id)",
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
        r"CREATE TABLE IF NOT EXISTS discover_candidates (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        subject_key TEXT NOT NULL,
        source TEXT NOT NULL,
        listing_id TEXT NOT NULL REFERENCES store_listings(id) ON DELETE CASCADE,
        seed_scope TEXT NOT NULL,
        origin TEXT NOT NULL DEFAULT '',
        seed_title TEXT,
        seed_categories TEXT,
        starter_genre TEXT,
        fetched_at TEXT NOT NULL,
        UNIQUE(subject_key, listing_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_discover_candidates_subject ON discover_candidates(subject_key, source)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_discover_candidates_listing ON discover_candidates(listing_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS discover_store_calls (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        call_key TEXT NOT NULL UNIQUE,
        source TEXT NOT NULL,
        fetched_at TEXT NOT NULL,
        writer TEXT NOT NULL DEFAULT ''
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_discover_store_calls_fetched ON discover_store_calls(fetched_at)",
    ),
    // The listings one cached storefront call answered, in the store's order.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS discover_store_call_hits (
        call_key TEXT NOT NULL REFERENCES discover_store_calls(call_key) ON DELETE CASCADE,
        position INTEGER NOT NULL,
        listing_id TEXT NOT NULL REFERENCES store_listings(id) ON DELETE CASCADE,
        origin TEXT NOT NULL DEFAULT '',
        rank_hint INTEGER,
        PRIMARY KEY (call_key, position)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_discover_store_call_hits_listing ON discover_store_call_hits(listing_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS discover_feeds (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        subject_key TEXT NOT NULL UNIQUE,
        feed_json TEXT NOT NULL,
        seed_scope TEXT NOT NULL,
        candidates_fetched_at TEXT,
        computed_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS discover_fetches (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        subject_key TEXT NOT NULL UNIQUE,
        fetched_at TEXT NOT NULL
    )",
    ),
    // Stored taste ranking of the shared request queue: one row per queue
    // entry, refreshed by the `discover_request_queue` job. Entry metadata
    // and wish counts stay on `title_requests` and are joined at read time.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS discover_request_queue (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        entry_key TEXT NOT NULL UNIQUE,
        work_id TEXT,
        owned INTEGER NOT NULL DEFAULT 0,
        taste_score REAL NOT NULL DEFAULT 0,
        reasons_json TEXT NOT NULL DEFAULT '[]',
        ranked_at TEXT NOT NULL
    )",
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
        r"CREATE TABLE IF NOT EXISTS bookclerk_receipts (
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
        r"CREATE INDEX IF NOT EXISTS idx_bookclerk_receipts_expires ON bookclerk_receipts(expires_at)",
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
        r"CREATE TABLE IF NOT EXISTS job_secret_handoffs (
        job_id TEXT NOT NULL,
        lease_generation INTEGER NOT NULL,
        kind TEXT NOT NULL,
        provider TEXT NOT NULL,
        account_id TEXT NOT NULL,
        name TEXT NOT NULL,
        node_id TEXT NOT NULL,
        handed_at TEXT NOT NULL,
        PRIMARY KEY (job_id, lease_generation, kind, provider, account_id, name),
        FOREIGN KEY(job_id) REFERENCES jobs(id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE UNIQUE INDEX IF NOT EXISTS idx_job_temp_paths_job_path
        ON job_temp_paths(job_id, path)",
    ),
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
        queue_name TEXT NOT NULL DEFAULT 'core.events',
        content_type TEXT NOT NULL DEFAULT 'json',
        deliver_after TEXT NOT NULL DEFAULT '',
        UNIQUE(queue_name, account_id, source, event_type, dedup_key)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_domain_events_queue ON domain_events(queue_name, created_at, id)",
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
        queue_name TEXT NOT NULL DEFAULT 'core.events',
        UNIQUE(event_id, plugin_id),
        FOREIGN KEY(event_id) REFERENCES domain_events(id) ON DELETE CASCADE
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_queue_state ON event_deliveries(queue_name, state)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_claim ON event_deliveries(state, run_after, created_at)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_plugin_order ON event_deliveries(plugin_id, ordering_key, created_at)",
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
        r"CREATE TABLE IF NOT EXISTS bookclerk_slots (
        slot_key TEXT PRIMARY KEY NOT NULL,
        bump INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_databases (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        plugin_id TEXT NOT NULL,
        binding TEXT NOT NULL,
        adapter_plugin_key TEXT NOT NULL,
        backend_kind TEXT NOT NULL,
        unit_ref TEXT NOT NULL,
        created_at TEXT NOT NULL,
        UNIQUE(plugin_id, binding)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_plugin_databases_plugin ON plugin_databases(plugin_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS cluster_identity (
        id INTEGER PRIMARY KEY NOT NULL,
        cluster_id TEXT NOT NULL,
        secret_fingerprint TEXT NOT NULL,
        schema_state TEXT NOT NULL,
        created_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS nodes (
        node_id TEXT PRIMARY KEY NOT NULL,
        cluster_id TEXT NOT NULL,
        created_at TEXT NOT NULL,
        incarnation TEXT NOT NULL,
        heartbeat_at TEXT NOT NULL,
        software_version TEXT NOT NULL,
        schema_state TEXT NOT NULL,
        compatible INTEGER NOT NULL,
        logical_cpus INTEGER,
        cpu_max_quota_us INTEGER,
        cpu_max_period_us INTEGER,
        memory_max_bytes INTEGER,
        memory_current_bytes INTEGER,
        memory_anon_bytes INTEGER,
        files_dir_free_bytes INTEGER,
        scratch_bytes INTEGER,
        target TEXT NOT NULL DEFAULT '',
        labels_json TEXT NOT NULL DEFAULT '[]',
        kind TEXT NOT NULL DEFAULT 'owner' CHECK (kind IN ('owner', 'host')),
        owner_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
        share_books INTEGER CHECK (share_books IN (0, 1)),
        include_branches INTEGER CHECK (include_branches IN (0, 1)),
        catalog_fence INTEGER NOT NULL DEFAULT 0,
        catalog_cursor TEXT NOT NULL DEFAULT '',
        catalog_pass_started_at TEXT,
        catalog_synced_at TEXT,
        catalog_full_pass_at TEXT
    )",
    ),
    // Who holds a store listing (docs/adr/library-data-model.md): a store
    // account of the listing's plugin, or a branch library node. The typed
    // columns carry the foreign keys, so deleting the account or node drops
    // its holdings; `holder` is the stored key (`account:<id>` /
    // `node:<id>`, `bookclerk_resolve::Holder`) the uniqueness and the
    // resolver read. `id` is new for each holding, so a title removed and
    // held again announces itself again (`holding_added:<id>`). `seen_pass`
    // is the scan pass that last listed it (`account_scan_state.pass_id`);
    // a complete full pass removes the account's holdings it did not list.
    // A node holding is a title that branch shares with this library
    // (`control_plane::branch_catalog`): `share_ref` is `store:product_id` as
    // the branch named it, `share_title` the title as received (JSON
    // `CatalogTitle`), `share_digest` its manifest digest and
    // `delivered_digest` the digest whose files reached this library; its
    // `seen_pass` is the branch's catalog pass. Account holdings leave them
    // empty.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS holdings (
        id TEXT PRIMARY KEY NOT NULL,
        listing_id TEXT NOT NULL REFERENCES store_listings(id) ON DELETE CASCADE,
        holder TEXT NOT NULL,
        holder_kind TEXT NOT NULL CHECK (holder_kind IN ('account', 'node')),
        plugin_key TEXT,
        account_id TEXT,
        node_id TEXT REFERENCES nodes(node_id) ON DELETE CASCADE,
        entitlement TEXT NOT NULL CHECK (entitlement IN ('purchase', 'plus', 'loan', 'free')),
        purchased_at TEXT,
        seen_pass TEXT,
        share_ref TEXT,
        share_title TEXT,
        share_digest TEXT,
        delivered_digest TEXT,
        delivered_at TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE (listing_id, holder),
        FOREIGN KEY (plugin_key, account_id) REFERENCES accounts(plugin_key, account_id) ON DELETE CASCADE,
        CHECK ((holder_kind = 'account' AND plugin_key IS NOT NULL AND account_id IS NOT NULL AND node_id IS NULL)
            OR (holder_kind = 'node' AND node_id IS NOT NULL AND plugin_key IS NULL AND account_id IS NULL)),
        CHECK (holder_kind = 'node' OR (share_ref IS NULL AND share_title IS NULL
            AND share_digest IS NULL AND delivered_digest IS NULL AND delivered_at IS NULL))
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_holdings_account ON holdings(plugin_key, account_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_holdings_node ON holdings(node_id, share_ref)",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_holdings_holder ON holdings(holder)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_nodes_heartbeat ON nodes(heartbeat_at)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_nodes_owner ON nodes(owner_user_id)"),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS configuration_documents (
        scope_type TEXT NOT NULL,
        scope_id TEXT NOT NULL,
        namespace TEXT NOT NULL,
        schema_version INTEGER NOT NULL,
        revision INTEGER NOT NULL,
        document_json TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        updated_by TEXT NOT NULL,
        write_operation_id TEXT NOT NULL,
        PRIMARY KEY (scope_type, scope_id, namespace)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS configuration_audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        scope_type TEXT NOT NULL,
        scope_id TEXT NOT NULL,
        namespace TEXT NOT NULL,
        schema_version INTEGER NOT NULL,
        revision INTEGER NOT NULL,
        actor TEXT NOT NULL,
        recorded_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_configuration_audit_doc
        ON configuration_audit(scope_type, scope_id, namespace, revision)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS configuration_changes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        scope_type TEXT NOT NULL,
        scope_id TEXT NOT NULL,
        namespace TEXT NOT NULL,
        revision INTEGER NOT NULL,
        committed_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_configuration_changes_doc
        ON configuration_changes(scope_type, scope_id, namespace, id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS storage_scan_rows (
        scan_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        identity TEXT NOT NULL,
        object_key TEXT NOT NULL,
        size INTEGER NOT NULL DEFAULT 0,
        media_rank INTEGER NOT NULL DEFAULT 0,
        is_audio INTEGER NOT NULL DEFAULT 0,
        claimed INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (scan_id, kind, identity, object_key)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_storage_scan_identity ON storage_scan_rows(scan_id, kind, identity)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS storage_scan_generations (
        scan_id TEXT PRIMARY KEY NOT NULL,
        instance_id TEXT NOT NULL,
        job_id TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL,
        completed INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_instances (
        plugin_instance_id TEXT PRIMARY KEY NOT NULL,
        plugin_key TEXT NOT NULL,
        display_name TEXT NOT NULL DEFAULT '',
        created_at TEXT NOT NULL,
        created_by TEXT NOT NULL,
        owner_node_id TEXT
    )",
    ),
    // Settings schema a host pinned from the installed manifest. Writes from
    // any API host validate against this row; a host with a newer package
    // version (or a different schema at the same version) replaces it by
    // compare-and-swap, and an older package refuses to spawn against it.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_schemas (
        plugin_key TEXT PRIMARY KEY NOT NULL,
        manifest_id TEXT NOT NULL,
        schema_sha256 TEXT NOT NULL,
        package_version TEXT NOT NULL,
        schema_version INTEGER NOT NULL,
        settings_json TEXT NOT NULL,
        recorded_at TEXT NOT NULL,
        recorded_by_node TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_plugin_instances_plugin_key
        ON plugin_instances(plugin_key)",
    ),
    // Default enrollment for one plugin key. Explicit create_plugin_instance
    // does not write this row, so two instances may still share a key.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_instance_defaults (
        plugin_key TEXT PRIMARY KEY NOT NULL,
        plugin_instance_id TEXT NOT NULL,
        FOREIGN KEY (plugin_instance_id) REFERENCES plugin_instances(plugin_instance_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_deployments (
        deployment_id TEXT PRIMARY KEY NOT NULL,
        plugin_instance_id TEXT NOT NULL,
        node_id TEXT NOT NULL,
        desired TEXT NOT NULL CHECK (desired IN ('present', 'absent', 'placed', 'unplaced')),
        revision INTEGER NOT NULL,
        updated_at TEXT NOT NULL,
        updated_by TEXT NOT NULL,
        UNIQUE (plugin_instance_id, node_id),
        FOREIGN KEY (plugin_instance_id) REFERENCES plugin_instances(plugin_instance_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_plugin_deployments_host_id
        ON plugin_deployments(node_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_deployment_observations (
        deployment_id TEXT NOT NULL,
        node_id TEXT NOT NULL,
        incarnation TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('materialized', 'running', 'healthy', 'error', 'retired')),
        detail TEXT NOT NULL,
        applied_config_revision INTEGER,
        observed_at TEXT NOT NULL,
        reserved_memory_bytes INTEGER,
        reserved_cpu_millicores INTEGER,
        PRIMARY KEY (deployment_id, node_id),
        FOREIGN KEY (deployment_id) REFERENCES plugin_deployments(deployment_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_plugin_deployment_observations_host_id
        ON plugin_deployment_observations(node_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_kv (
        plugin_key TEXT NOT NULL,
        namespace TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        metadata TEXT NOT NULL DEFAULT '',
        expires_at TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL,
        PRIMARY KEY (plugin_key, namespace, key)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_plugin_kv_expires
        ON plugin_kv(expires_at)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_versions (
        plugin_key TEXT NOT NULL,
        version TEXT NOT NULL,
        manifest_id TEXT NOT NULL,
        manifest_toml TEXT NOT NULL,
        settings_json TEXT NOT NULL,
        artifacts_json TEXT NOT NULL,
        publisher_verified INTEGER NOT NULL,
        grant_json TEXT NOT NULL DEFAULT '',
        coordinate TEXT NOT NULL DEFAULT '',
        installed_at TEXT NOT NULL,
        installed_by TEXT NOT NULL,
        PRIMARY KEY (plugin_key, version)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_installs (
        plugin_key TEXT PRIMARY KEY NOT NULL,
        version TEXT NOT NULL,
        revision INTEGER NOT NULL,
        updated_at TEXT NOT NULL,
        updated_by TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_placements (
        plugin_instance_id TEXT PRIMARY KEY NOT NULL,
        replicas INTEGER CHECK (replicas IS NULL OR replicas >= 0),
        constraints_json TEXT NOT NULL DEFAULT '[]',
        revision INTEGER NOT NULL,
        updated_at TEXT NOT NULL,
        updated_by TEXT NOT NULL,
        placed INTEGER NOT NULL DEFAULT 0,
        detail TEXT NOT NULL DEFAULT '',
        scheduled_at TEXT NOT NULL DEFAULT '',
        FOREIGN KEY (plugin_instance_id) REFERENCES plugin_instances(plugin_instance_id)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS node_status (
        node_id TEXT PRIMARY KEY NOT NULL,
        incarnation TEXT NOT NULL,
        started_at TEXT NOT NULL,
        heartbeat_at TEXT NOT NULL,
        heartbeat_ms INTEGER NOT NULL,
        interval_ms INTEGER NOT NULL,
        cpu_millicores INTEGER NOT NULL,
        memory_bytes INTEGER,
        cpu_budget_millicores INTEGER NOT NULL,
        memory_budget_bytes INTEGER,
        schedulable INTEGER NOT NULL,
        cpu_used_millicores INTEGER,
        memory_available_bytes INTEGER,
        files_dir_free_bytes INTEGER,
        reserved_cpu_millicores INTEGER NOT NULL,
        reserved_memory_bytes INTEGER NOT NULL,
        running_guests INTEGER NOT NULL,
        availability_ppm INTEGER NOT NULL,
        online_since_ms INTEGER NOT NULL,
        cache_bytes INTEGER,
        cache_budget_bytes INTEGER,
        cache_swept_ms INTEGER
    )",
    ),
    MigrationOp::Schema(super::CLUSTER_LEASES_DDL),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_artifacts (
        sha256 TEXT PRIMARY KEY NOT NULL,
        size INTEGER NOT NULL,
        chunk_bytes INTEGER NOT NULL,
        created_at TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_artifact_chunks (
        sha256 TEXT NOT NULL,
        chunk_bytes INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (sha256, chunk_bytes, seq)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS node_enrollment_links (
        link_id TEXT PRIMARY KEY NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        for_user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        label TEXT NOT NULL DEFAULT '',
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        redeemed_at TEXT,
        redeemed_node_id TEXT,
        revoked_at TEXT,
        bring_accounts INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_node_enrollment_links_user ON node_enrollment_links(for_user_id)",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS node_credentials (
        node_id TEXT PRIMARY KEY NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
        sign_public_key BLOB NOT NULL,
        box_public_key BLOB NOT NULL,
        link_id TEXT NOT NULL UNIQUE,
        enrolled_at TEXT NOT NULL,
        revoked_at TEXT,
        last_connected_at TEXT
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS cluster_links (
        owner_cluster_id TEXT PRIMARY KEY NOT NULL,
        owner_origin TEXT NOT NULL,
        remote_node_id TEXT NOT NULL,
        spki_pin TEXT,
        state TEXT NOT NULL CHECK (state IN ('active', 'revoked', 'left')),
        enrolled_at TEXT NOT NULL,
        share_books INTEGER NOT NULL DEFAULT 1 CHECK (share_books IN (0, 1)),
        include_branches INTEGER NOT NULL DEFAULT 1 CHECK (include_branches IN (0, 1)),
        ancestry_json TEXT NOT NULL DEFAULT '[]'
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS remote_plugin_consents (
        owner_cluster_id TEXT NOT NULL,
        plugin_key TEXT NOT NULL,
        requested_version TEXT NOT NULL,
        requested_grant_json TEXT NOT NULL,
        requested_artifact_url TEXT NOT NULL,
        requested_artifact_sha256 TEXT NOT NULL,
        requested_artifact_payload_sha256 TEXT NOT NULL,
        version TEXT,
        approved_request_json TEXT,
        approved_artifact_url TEXT,
        approved_artifact_sha256 TEXT,
        approved_artifact_payload_sha256 TEXT,
        grant_json TEXT,
        approved_at TEXT,
        approved_by TEXT,
        revoked_at TEXT,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (owner_cluster_id, plugin_key)
    )",
    ),
    // H-4 step 9: one title's streamed upload from a host node. Only the
    // token's hash is stored; nothing of the files is.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS node_upload_sessions (
        upload_id TEXT PRIMARY KEY NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        node_id TEXT NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
        job_id TEXT NOT NULL,
        lease_generation INTEGER NOT NULL,
        receiving_node_id TEXT NOT NULL,
        book_uuid TEXT NOT NULL,
        primary_key TEXT NOT NULL,
        manifest_json TEXT NOT NULL,
        received_json TEXT NOT NULL DEFAULT '[]',
        state TEXT NOT NULL CHECK (state IN ('open', 'committed', 'aborted')),
        created_at TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        committed_at TEXT
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_node_upload_sessions_job ON node_upload_sessions(job_id)",
    ),
    // Every file key an acquire or a host upload wrote, per output
    // destination, and the edition it belongs to (`owner_kind = 'edition'`),
    // so an upload is never granted another edition's file in a folder
    // titles share. A title a branch shares here is a node holding, so its
    // keys are its edition's too. `writer` is the host upload id that
    // claimed the key last (a branch's catalog upload included), or
    // `acquire` for an acquire run on the node itself. `role` says what the file is; audio quality
    // (`lossless`, `bitrate_kbps`) decides whether a later copy of the same
    // edition replaces it.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS edition_files (
        destination TEXT NOT NULL,
        storage_key TEXT NOT NULL,
        owner_kind TEXT NOT NULL CHECK (owner_kind IN ('edition')),
        owner_id TEXT NOT NULL,
        role TEXT NOT NULL CHECK (role IN ('audio', 'part', 'chapter', 'pdf', 'cover', 'sidecar', 'transcript', 'epub')),
        lossless INTEGER NOT NULL DEFAULT 0,
        bitrate_kbps INTEGER,
        sha256 TEXT,
        writer TEXT NOT NULL,
        written_at TEXT NOT NULL,
        PRIMARY KEY (destination, storage_key)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_edition_files_owner ON edition_files(owner_kind, owner_id)",
    ),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_edition_files_writer ON edition_files(writer)",
    ),
    // The live socket's revision reads take each table's newest change
    // stamp every couple of seconds; an index makes that a lookup.
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_books_updated ON books(updated_at)"),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_jobs_updated ON jobs(updated_at)"),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_jobs_state_finished ON jobs(state, finished_at)",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at)"),
    MigrationOp::Schema(
        r"CREATE INDEX IF NOT EXISTS idx_event_deliveries_updated ON event_deliveries(updated_at)",
    ),
    MigrationOp::Schema(r"CREATE INDEX IF NOT EXISTS idx_users_updated ON users(updated_at)"),
    // Join a library: on a joining host, one sign-in in progress. The PKCE
    // verifier and node keys are sealed in `encrypted_secrets` (`kind =
    // cluster_join`); only the hash of `state` is stored here. Rows live
    // ten minutes. `allow_clerk` records that the operator offered to join
    // as a clerk (the install was empty when the sign-in started).
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS pending_joins (
        state_hash TEXT PRIMARY KEY NOT NULL,
        owner_origin TEXT NOT NULL,
        spki_pin TEXT,
        owner_cluster_id TEXT NOT NULL,
        library_name TEXT NOT NULL,
        branch_name TEXT NOT NULL,
        redirect_uri TEXT NOT NULL,
        allow_clerk INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        expires_at TEXT NOT NULL
    )",
    ),
    // Last claimed fire per plugin cron schedule (`control_plane::cron_fires`).
    // `claim` is the token of the batch that last moved the cursor; the job
    // insert in that batch selects only while the token is still there.
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS plugin_cron_cursors (
        plugin_instance_id TEXT NOT NULL,
        cron TEXT NOT NULL,
        last_fire_ms INTEGER NOT NULL,
        claim TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL,
        PRIMARY KEY (plugin_instance_id, cron),
        FOREIGN KEY (plugin_instance_id) REFERENCES plugin_instances(plugin_instance_id)
    )",
    ),
    // One change counter per live-socket topic, bumped in the same batch as
    // every write to the topic's table (`control_plane::live`).
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS live_topic_revisions (
        topic TEXT PRIMARY KEY NOT NULL,
        revision INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Data(
        r"INSERT OR IGNORE INTO live_topic_revisions (topic, revision) VALUES
        ('library', 0), ('jobs', 0), ('deliveries', 0), ('nodes', 0), ('config', 0),
        ('consents', 0), ('clusters', 0), ('users', 0), ('discover', 0)",
    ),
];

#[cfg(test)]
mod tests {
    use super::{MigrationOp, UNRELEASED_OPS};
    use bookclerk_resolve::kinds::sql_check_list;
    use bookclerk_resolve::{
        Abridgement, CreditRole, Entitlement, ExpressionKind, IdentifierScheme, Level, MediaKind,
        Strength, WorkKind,
    };

    /// The `CREATE TABLE` statement for `table`.
    fn table_ddl(table: &str) -> &'static str {
        let head = format!("CREATE TABLE IF NOT EXISTS {table} (");
        UNRELEASED_OPS
            .iter()
            .find_map(|op| match op {
                MigrationOp::Schema(sql) if sql.starts_with(&head) => Some(*sql),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no DDL for {table}"))
    }

    /// Asserts `table.column` checks exactly `values`.
    fn assert_check(table: &str, column: &str, values: impl IntoIterator<Item = &'static str>) {
        let expected = format!("{column} IN ({})", sql_check_list(values));
        assert!(
            table_ddl(table).contains(&expected),
            "{table}.{column} does not check `{expected}`"
        );
    }

    #[test]
    fn library_model_checks_match_the_resolver_kinds() {
        assert_check("works", "kind", WorkKind::ALL.iter().map(|k| k.as_str()));
        assert_check(
            "expressions",
            "kind",
            ExpressionKind::ALL.iter().map(|k| k.as_str()),
        );
        assert_check(
            "expressions",
            "abridgement",
            Abridgement::ALL.iter().map(|k| k.as_str()),
        );
        assert_check(
            "editions",
            "media_kind",
            MediaKind::ALL.iter().map(|k| k.as_str()),
        );
        for table in ["work_identifiers", "edition_identifiers"] {
            assert_check(
                table,
                "scheme",
                IdentifierScheme::ALL.iter().map(|k| k.as_str()),
            );
        }
        assert_check(
            "edition_identifiers",
            "strength",
            Strength::ALL.iter().map(|k| k.as_str()),
        );
        assert_check(
            "credits",
            "target_kind",
            Level::ALL.iter().map(|k| k.as_str()),
        );
        assert_check(
            "credits",
            "role",
            CreditRole::ALL.iter().map(|k| k.as_str()),
        );
        assert_check(
            "holdings",
            "entitlement",
            Entitlement::ALL.iter().map(|k| k.as_str()),
        );
        assert_check(
            "edition_files",
            "role",
            crate::FileRole::ALL.iter().map(|k| k.as_str()),
        );
    }
}
