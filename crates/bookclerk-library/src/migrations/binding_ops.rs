//! Binding bootstrap ops; this ordered list is the source of truth.

use super::MigrationOp;

/// Host-owned bootstrap applied inside every isolated plugin binding.
pub(super) const BINDING_BOOTSTRAP_OPS: &[MigrationOp] = &[
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
        r"CREATE TABLE IF NOT EXISTS bookclerk_sql_catalog (
        table_name TEXT NOT NULL,
        column_name TEXT NOT NULL,
        sql_type TEXT NOT NULL,
        ordinal INTEGER NOT NULL,
        is_identity INTEGER NOT NULL,
        default_sql TEXT NOT NULL,
        PRIMARY KEY (table_name, column_name)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS bookclerk_sql_schema (
        table_name TEXT PRIMARY KEY NOT NULL,
        fingerprint TEXT NOT NULL,
        identity_column TEXT NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS bookclerk_identity (
        table_name TEXT PRIMARY KEY NOT NULL,
        last INTEGER NOT NULL
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS bookclerk_sql_ddl (
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        table_name TEXT NOT NULL,
        canonical_sql TEXT NOT NULL,
        PRIMARY KEY (kind, name)
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS bookclerk_slots (
        slot_key TEXT PRIMARY KEY NOT NULL,
        bump INTEGER NOT NULL DEFAULT 0
    )",
    ),
    MigrationOp::Schema(
        r"CREATE TABLE IF NOT EXISTS bookclerk_plugin_migrations (
        ordinal INTEGER PRIMARY KEY NOT NULL,
        migration_id TEXT NOT NULL UNIQUE,
        checksum TEXT NOT NULL,
        applied_at TEXT NOT NULL
    )",
    ),
];
