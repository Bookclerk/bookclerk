//! Canonical Bookclerk library database.
//!
//! [`LibraryStore`] is SeaORM-backed: every backend is a
//! [`sea_orm::DatabaseConnection`]. Engine connect/migrate/proxy quirks live in
//! the database plugin (`bookclerk-plugin-database-sqlite` (and optional d1/postgres guests)); hosts open the store via
//! the external guest RPC. This crate owns the greenfield schema
//! ([`migrations`]), typed [`entities`], and the store API. See
//! `docs/database.md`.

#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

mod atomic_ops;
mod atomic_txn;
mod backend_migrate;
mod backup;
mod binding_schema;
mod db_atomic;
pub mod email;
pub mod entities;
mod error;
mod host_schema;
mod in_process_atomic;
pub mod master_key;
pub mod migrations;
/// Public library DTOs and status enums (`BookRecord`, `AcquireStatus`, prefs) re-exported from this crate.
mod models;
pub mod operator_token;
pub mod password;
pub mod proxy_txn;
mod schema_state;
mod schema_walk;
pub mod scope;
pub mod secrets;
mod session_client;
/// Host-owned generic SQL atomic plans for database adapters.
pub mod sql_plan;
mod store;
mod text;
mod token_hash;
mod wishlist_merge;

pub use atomic_ops::{atomic_status, DbAtomicParams, DbAtomicResult};
pub use atomic_txn::{AtomicTxnBackend, TypedAtomicExec};
pub use backend_migrate::{migrate_library_backend, BackendMigrateOptions, BackendMigrateSummary};
pub use backup::encode::{CanonicalObject, CHUNK_TARGET_UNCOMPRESSED_BYTES};
pub use backup::repository::BackupRepository;
pub use backup::restore::restore_backup_unit;
pub use backup::schema::{library_ddl_for_schema_state, sort_tables_by_foreign_keys};
pub use backup::verify::verify_recovery_point;
pub use backup::{
    admit_canonical_schema, admit_canonical_statements, apply_admitted_sql, archive_backup,
    backup_library, extract_backup_archive, filter_library_pack_ddl, library_canonical_schema,
    library_canonical_schema_for_state, list_backups, plugin_canonical_schema_from_ddl_catalog,
    prune_automatic_backups, resolve_backup_spec, restore_backup, restore_backup_in_repo,
    BackupListEntry, BackupManifest, BackupOutcome, BackupReason, BackupRequest, BackupResolve,
    BackupUnit, CanonicalDatabaseSchema, CanonicalExportOpts, CanonicalRestoreKind,
    CanonicalRestoreOpts, CanonicalTableSchema, DatabaseUnitKind, IdentityHighWater,
    PreparedPluginUnit, RestorePlan, SchemaBackupOpts, ValidatedBackup, BACKUPS_DIR,
    BACKUP_FORMAT_VERSION, BACKUP_RETENTION, LIBRARY_SKIP_TABLES,
};
pub use binding_schema::{apply_binding_bootstrap, binding_bootstrap_plan};
pub use bookclerk_plugin_abi::{AdapterBackupOps, GuestSqlPolicy, SharedAdapterBackupOps};
pub use db_atomic::{
    db_atomic_operation_id, db_atomic_request_hash, execute_db_atomic, execute_named_atomic,
};
pub use email::{gravatar_hash, is_valid_user_email, normalize_user_email};
pub use error::{LibraryError, Result};
pub use host_schema::{
    apply_host_schema, apply_host_schema_with_batch, apply_host_schema_with_batch_opts,
    apply_host_schema_with_options, current_schema_state, current_schema_state_in,
    current_schema_version, ensure_restore_target_is_replaceable, migrate_host_schema_to,
    migrate_host_schema_to_with_batch, HostSchemaKind, SchemaApplyOptions, SchemaBatch,
};
pub use in_process_atomic::InProcessSqliteAtomic;
pub use master_key::{
    configure_master_key, configure_master_key_with, inspect_master_key, master_key_path,
    require_master_key, resolve_master_key, resolve_master_key_with, seal_with_dek,
    unseal_with_dek, wrap_master_key, MasterKey, MasterKeyFormat,
    AUTH_PASSWORD_ENV as MASTER_KEY_AUTH_PASSWORD_ENV, MASTER_KEY_FILE_NAME,
};
pub use migrations::{
    apply_migration_plan, apply_plugin_migrations, binding_bootstrap_sql,
    binding_bootstrap_statements, binding_bootstrap_type_env, current_canonical_schema,
    current_canonical_table_names, downgrade_migration_plan, history_from_execute_reply,
    host_migration_plan, latest_schema_sqlite, load_plugin_migration_history,
    min_supported_schema_version, next_pending_plugin_migration, pending_plugin_suffix,
    plugin_apply_statements, plugin_binding_type_env, plugin_history_digest,
    plugin_history_session_matches, plugin_journal_has_entry, plugin_journal_select_request,
    plugin_migration_checksum, prove_plugin_migration_sequence, remaining_plugin_suffix_batches,
    remaining_upgrade_batches, require_history_prefix, schema_session_matches, schema_slot_key,
    sql_string_literal, unreleased_checksum, unreleased_ops, unreleased_sql, HostMigrationStep,
    MigrationOp, MigrationPlan, MigrationStep, PlanOp, PluginJournalEntry, PluginMigrationHistory,
    PluginMigrationSequence, ProvenPluginMigration, BOOKCLERK_SCHEMA_NAMESPACE,
    MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS, PLUGIN_MIGRATION_SLOT_KEY, SCHEMA_MIGRATIONS_DDL,
    SCHEMA_VERSION,
};
pub use models::{
    catalog_subscribers_for_event, collapse_live_subscriber_nodes, content_kind_from_classic,
    content_kind_to_classic, event_filter_matches, event_matches_wake_grants,
    intersect_event_filters, is_downloadable, is_episode, is_podcast_parent, job_backoff_run_after,
    normalize_theme, portal_prefs_key, push_queue_wisher, subscription_matches_event,
    user_prefs_key, wake_grants_from_subscriptions, AccountLinkRecord, AccountRecord,
    AcquireStatus, BookRecord, ClaimTicketRecord, DomainEventRecord, EmbeddingRecord,
    EnqueueJobSpec, EnqueueOutcome, EventCatalogSubscription, EventDeliveryFence,
    EventDeliveryMetrics, EventDeliveryRecord, EventSubscriber, EventSubscriberCatalogRecord,
    EventSubscriberNodeRecord, EventWakeGrant, GlobalQueueEntry, JobFence, JobKind, JobPayload,
    JobRecord, JobResourceClass, JobState, JobTempPath, JobTrigger, ListeningProgressRecord,
    OidcClientRecord, OperatorSessionRecord, PendingWakeProgress, PortalIdentity,
    PortalSessionRecord, PublishDomainEventOutcome, PublishDomainEventSpec, QueueWisher,
    RequestStatus, SecurityAuditEvent, StoredPasskey, TitleRequestRecord, TitleRequestSourceRecord,
    UserIntegrationHint, UserInviteRecord, UserListeningHint, UserPreferences, UserPresenceExtras,
    UserRecord, UserRole, UserSsoPicture, UserStatus, WishlistPurchaseHint, WishlistStoreEdition,
    WorkRecord, EVENT_DELIVERY_MAX_ATTEMPTS, EVENT_RESOURCE_CLASS_NETWORK,
    EVENT_SUBSCRIBER_HEARTBEAT_TTL_SECS, JOB_PAYLOAD_VERSION, MAX_QUEUE_WISHERS,
    OPERATOR_PREFS_KEY,
};
pub use operator_token::{
    env_operator_token, legacy_operator_token_file, load_operator_token,
    read_or_create_operator_token, resolve_operator_token, rotate_operator_token,
    save_operator_token, ResolveOperatorToken, OPERATOR_TOKEN_ACCOUNT_ID,
    OPERATOR_TOKEN_SECRET_NAME,
};
pub use password::{hash_password, verify_password};
pub use proxy_txn::{
    arm_exec_budget, clear_exec_budget, consume_atomic_interrupt, consume_begin_injection,
    consume_commit_injection, current_exec_budget, exec_deadline_expired,
    exec_deadline_remaining_ms, inject_atomic_interrupt, inject_atomic_interrupt_after,
    inject_begin_failures, inject_commit_failures, inject_savepoint_release_failures,
    inject_savepoint_rollback_failures, is_txn_broken, note_begin_failed, note_commit_failed,
    note_query_row, query_row_cap, query_rows_seen, take_txn_fault, txn_broken_err,
    with_exec_budget, AtomicInterruptKind, AtomicInterruptPhase, ExecBudget,
};
pub use schema_state::{SchemaState, SCHEMA_STATE_FROZEN, SCHEMA_STATE_UNRELEASED};
pub use schema_walk::{
    plan_downgrade_to_binary, plan_schema_walk, plan_schema_walk_from_state, SchemaWalk,
};
pub use scope::SourceScope;
pub use secrets::{
    b64_string_to_bytes, build_sealed_record, bytes_to_b64_string, clear_unseal_cache,
    decrypt_secret, delete_secret, delete_secrets_for_account, encrypt_secret, get_secret,
    list_secrets, secret_account_type, secret_kind, unseal_secret, upsert_secret, EncryptedBlob,
    EncryptedSecretRecord, SecretStore, CIPHER_ALGORITHM, FORMAT_SEALED_V1, KDF_ALGORITHM,
    KDF_M_COST, KDF_P_COST, KDF_T_COST,
};
pub use session_client::{classify_session_client, SessionClientInfo};
pub use sql_plan::{
    authorize_guest_typed_request, authorize_typed_request, compile_claim_event_delivery,
    compile_named_request, execute_compiled_on, execute_compiled_on_capped,
    execute_guest_atomic_with, execute_typed_on, execute_typed_on_session, interpret_typed_exec,
    proxy_read_kind, proxy_write_kind, validate_execute_reply, validate_execute_request,
    validate_plan, validate_proven_execute_request, wake_page_for_max_binds, AtomicSelection,
    AtomicSession, CompiledAtomic,
};
pub use store::{
    event_outbox::prepare_publish_domain_event, fallback_work_key, inject_dispatch_page_failures,
    plugin_databases::PluginDatabaseRecord, prefer_enrichment_source, set_dispatch_chunk_for_test,
    wishlist_identities_match, CatalogEnrichmentFields, LibraryStore, NewBook,
    NewListeningProgress, NewTitleRequest, NewTitleRequestSource, NewWork, SavedFilterRecord,
    UserBookFields, WishlistIdentity,
};
pub use text::{
    decode_html_entities, decode_html_entities_cow, decode_html_entities_in_place,
    decode_html_entities_opt, decode_html_entities_opt_in_place, str_maybe_html_entity,
};
pub use token_hash::{
    derive_claim_password_fingerprint, derive_claim_session_token, hash_token,
    parse_claim_redeem_nonce, CLAIM_REDEEM_NONCE_HEX_LEN,
};
pub use wishlist_merge::{apply_merged_sources, pick_better_description};
