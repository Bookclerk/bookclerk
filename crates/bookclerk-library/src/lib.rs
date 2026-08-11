//! Canonical Bookclerk library database.
//!
//! [`LibraryStore`] is SeaORM-backed: every backend is a
//! [`sea_orm::DatabaseConnection`]. Engine connect/migrate/proxy quirks live in
//! the database plugin (`bookclerk-plugin-database-sqlite` (and optional d1/postgres guests)); hosts open the store via
//! the external guest RPC. This crate owns the greenfield schema
//! ([`migrations`]), typed [`entities`], and the store API. See
//! `docs/database.md`.

mod backend_migrate;
pub mod entities;
mod error;
pub mod master_key;
pub mod migrations;
mod models;
pub mod operator_token;
pub mod password;
pub mod scope;
pub mod secrets;
mod store;
mod text;
mod token_hash;
mod wishlist_merge;

pub use backend_migrate::{migrate_library_backend, BackendMigrateOptions, BackendMigrateSummary};
pub use error::{LibraryError, Result};
pub use master_key::{
    configure_master_key, configure_master_key_with, inspect_master_key, master_key_path,
    require_master_key, resolve_master_key, resolve_master_key_with, seal_with_dek,
    unseal_with_dek, wrap_master_key, MasterKey, MasterKeyFormat,
    AUTH_PASSWORD_ENV as MASTER_KEY_AUTH_PASSWORD_ENV, MASTER_KEY_FILE_NAME,
};
pub use models::{
    content_kind_from_classic, content_kind_to_classic, is_downloadable, is_episode,
    is_podcast_parent, portal_prefs_key, user_prefs_key, AccountLinkRecord, AccountRecord,
    AcquireStatus, BookRecord, ClaimTicketRecord, EmbeddingRecord, GlobalQueueEntry,
    ListeningProgressRecord, OperatorSessionRecord, PortalIdentity, RequestStatus,
    SecurityAuditEvent, TitleRequestRecord, TitleRequestSourceRecord, UserInviteRecord,
    UserPreferences, UserRecord, UserRole, UserStatus, WishlistPurchaseHint, WishlistStoreEdition,
    WorkRecord, OPERATOR_PREFS_KEY,
};
pub use operator_token::{
    env_operator_token, legacy_operator_token_file, load_operator_token,
    read_or_create_operator_token, resolve_operator_token, rotate_operator_token,
    save_operator_token, ResolveOperatorToken, OPERATOR_TOKEN_ACCOUNT_ID,
    OPERATOR_TOKEN_SECRET_NAME,
};
pub use password::{hash_password, verify_password};
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
    CatalogEnrichmentFields, LibraryStore, NewBook, NewListeningProgress, NewTitleRequest,
    NewTitleRequestSource, NewWork, SavedFilterRecord, UserBookFields, WishlistIdentity,
};
pub use text::{
    decode_html_entities, decode_html_entities_cow, decode_html_entities_in_place,
    decode_html_entities_opt, decode_html_entities_opt_in_place, str_maybe_html_entity,
};
pub use token_hash::hash_token;
pub use wishlist_merge::{apply_merged_sources, pick_better_description};
