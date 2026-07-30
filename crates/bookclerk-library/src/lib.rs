//! Canonical Bookclerk library database.
//!
//! [`LibraryStore`] is SeaORM-backed: every backend is a
//! [`sea_orm::DatabaseConnection`] (local `sqlite`/rusqlite proxy by default,
//! Cloudflare `d1` over HTTP, or native `postgres`). The store API is `async`
//! and issues all CRUD through typed SeaORM [`entities`]
//! (`Entity::find` / `QueryFilter` / `ActiveModel`); `ON CONFLICT … COALESCE`
//! upserts become load-then-merge in Rust. Raw SQL survives only in
//! [`migrations`] bootstrap for D1/Postgres. The schema is a single greenfield
//! definition in [`migrations`]. See `docs/database.md`.

mod db;
pub mod entities;
mod error;
pub mod master_key;
mod migrations;
mod models;
pub mod scope;
pub mod secrets;
mod store;

pub use db::{
    apply_pending_migrations, connect_d1, connect_from_config, connect_postgres, connect_sqlite,
    connect_sqlite_memory, resolve_d1_api_token, resolve_postgres_url, D1Proxy, SqliteProxy,
};
pub use error::{LibraryError, Result};
pub use master_key::{
    configure_master_key, configure_master_key_with, inspect_master_key, master_key_path,
    require_master_key, resolve_master_key, resolve_master_key_with, seal_with_dek,
    unseal_with_dek, wrap_master_key, MasterKey, MasterKeyFormat,
    AUTH_PASSWORD_ENV as MASTER_KEY_AUTH_PASSWORD_ENV, MASTER_KEY_FILE_NAME,
};
pub use models::{
    content_kind_from_classic, content_kind_to_classic, is_downloadable, is_episode,
    is_podcast_parent, portal_prefs_key, AccountLinkRecord, AccountRecord, AcquireStatus,
    BookRecord, ClaimTicketRecord, EmbeddingRecord, GlobalQueueEntry, ListeningProgressRecord,
    PortalIdentity, RequestStatus, TitleRequestRecord, UserPreferences, WorkRecord,
    OPERATOR_PREFS_KEY,
};
pub use scope::SourceScope;
pub use secrets::{
    b64_string_to_bytes, build_sealed_record, bytes_to_b64_string, clear_unseal_cache,
    decrypt_secret, delete_secret, delete_secrets_for_account, encrypt_secret, get_secret,
    list_secrets, secret_account_type, secret_kind, unseal_secret, upsert_secret, EncryptedBlob,
    EncryptedSecretRecord, SecretStore, CIPHER_ALGORITHM, FORMAT_SEALED_V1, KDF_ALGORITHM,
    KDF_M_COST, KDF_P_COST, KDF_T_COST,
};
pub use store::{
    fallback_work_key, prefer_enrichment_source, wishlist_identities_match,
    CatalogEnrichmentFields, LibraryStore, NewBook, NewListeningProgress, NewTitleRequest, NewWork,
    SavedFilterRecord, UserBookFields, WishlistIdentity,
};
