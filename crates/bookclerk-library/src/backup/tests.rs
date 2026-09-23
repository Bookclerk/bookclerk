//! Backup repository, streaming capture, restore, and schema-state tests.

#![allow(clippy::missing_docs_in_private_items)]

use super::*;
use crate::backup::encode::{
    decode_canonical_object, encode_canonical_object, sha256_hex, unwrap_stored_object,
    wrap_stored_object, CanonicalObject, CHUNK_TARGET_UNCOMPRESSED_BYTES, CODEC_GZIP,
    MAX_OBJECT_UNCOMPRESSED_BYTES, OBJECT_HEADER_LEN, OBJECT_MAGIC, OBJECT_STORE_VERSION,
};
use crate::backup::repository::BackupRepository;
use crate::backup::restore::{apply_admitted_sql, restore_backup_unit};
use crate::backup::schema::{
    admit_canonical_schema, admit_canonical_statements, canonical_order_by_sql,
    filter_library_pack_ddl, library_ddl_for_schema_state_with, order_key_columns,
    sort_tables_by_foreign_keys,
};
use crate::backup::util::validate_cell;
use crate::backup::verify::{verify_recovery_point, verify_unit};
use crate::host_schema::{
    apply_fresh_schema_sqlite, apply_host_schema, current_schema_state,
    ensure_restore_target_is_replaceable,
};
use crate::migrations::{
    override_host_migration_plan, HostMigrationStep, MigrationOp, SCHEMA_MIGRATIONS_DDL,
    SCHEMA_VERSION,
};
use crate::LibraryStore;
use bookclerk_plugin_abi::{
    encoded_statement_result_bytes, parse_create_table_schema, DbCapabilities, DbColumn, DbRow,
    DbType, DbValue, SqlType, StatementResult, FIRST_PARTY_MAX_RESULT_BYTES,
    FIRST_PARTY_MAX_RESULT_ROWS, SQL_CONTRACT_VERSION,
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn backup_req(files: &Path, state: SchemaState, reason: BackupReason) -> BackupRequest {
    BackupRequest {
        files_dir: files.to_path_buf(),
        schema_state: state,
        reason,
        to_version: 0,
        include_plugin_databases: false,
        consistent_backup_read: true,
        backend_at_capture: "sqlite".into(),
        max_result_rows: FIRST_PARTY_MAX_RESULT_ROWS,
        max_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
        max_atomic_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
        plugin_units: Vec::new(),
        adapter: None,
        in_process: false,
    }
}

fn restore_ok() -> CanonicalRestoreOpts {
    CanonicalRestoreOpts::default()
}

fn restore_without_atomic() -> CanonicalRestoreOpts {
    CanonicalRestoreOpts {
        atomic_unit_restore: false,
        ..CanonicalRestoreOpts::default()
    }
}

fn restore_postgres() -> CanonicalRestoreOpts {
    CanonicalRestoreOpts {
        in_process: true,
        ..CanonicalRestoreOpts::default()
    }
}

fn restore_postgres_plugin() -> CanonicalRestoreOpts {
    CanonicalRestoreOpts {
        in_process: true,
        ..restore_ok()
    }
}

fn export_postgres() -> CanonicalExportOpts {
    CanonicalExportOpts {
        in_process: true,
        ..CanonicalExportOpts::default()
    }
}

/// Third-party adapter: `schemaMigrations` without extra flags.
fn third_party_sqlite_row_marker_caps() -> DbCapabilities {
    let mut caps = DbCapabilities::advertised_sqlite();
    caps.schema_migrations = true;
    caps
}

async fn seed_sqlite_with_leftover_pragma(db: &sea_orm::DatabaseConnection) {
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        SCHEMA_MIGRATIONS_DDL.to_string(),
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "PRAGMA user_version = 5",
    ))
    .await
    .unwrap();
    assert_eq!(db.get_database_backend(), DbBackend::Sqlite);
}

fn prepared_plugin(
    plugin_id: &str,
    binding: &str,
    db: sea_orm::DatabaseConnection,
) -> PreparedPluginUnit {
    PreparedPluginUnit {
        plugin_id: plugin_id.into(),
        binding: binding.into(),
        backend_at_capture: "sqlite".into(),
        db,
        max_result_rows: FIRST_PARTY_MAX_RESULT_ROWS,
        max_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
        max_atomic_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
        adapter: None,
    }
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_all_raw(Statement::from_string(DbBackend::Sqlite, sql.to_string()))
        .await
        .unwrap()
        .first()
        .and_then(|row| {
            row.try_get::<i64>("", "count")
                .ok()
                .or_else(|| row.try_get_by_index::<i64>(0).ok())
        })
        .unwrap_or(0)
}

async fn apply_bootstrap(db: &DatabaseConnection) {
    crate::apply_binding_bootstrap(db).await.unwrap();
}

#[test]
fn identical_canonical_chunks_hash_identically() {
    let a = CanonicalObject::TableChunk {
        table: "t".into(),
        columns: vec!["id".into()],
        rows: vec![vec![DbValue::Int64(1)]],
    };
    let b = a.clone();
    assert_eq!(
        sha256_hex(&encode_canonical_object(&a).unwrap()),
        sha256_hex(&encode_canonical_object(&b).unwrap())
    );
}

#[test]
fn gzip_envelope_preserves_uncompressed_digest() {
    let raw = encode_canonical_object(&CanonicalObject::Identity {
        entries: BTreeMap::new(),
    })
    .unwrap();
    let stored = wrap_stored_object(&raw).unwrap();
    assert_eq!(unwrap_stored_object(&stored).unwrap(), raw);
    assert_eq!(
        decode_canonical_object(&raw).unwrap(),
        CanonicalObject::Identity {
            entries: BTreeMap::new()
        }
    );
    const {
        assert!(CHUNK_TARGET_UNCOMPRESSED_BYTES >= 128 * 1024);
        assert!(CHUNK_TARGET_UNCOMPRESSED_BYTES <= 512 * 1024);
    }
}

#[test]
fn gzip_envelope_rejects_declared_length_over_cap() {
    let mut stored = vec![0u8; OBJECT_HEADER_LEN];
    stored[..4].copy_from_slice(OBJECT_MAGIC);
    stored[4] = OBJECT_STORE_VERSION;
    stored[5] = CODEC_GZIP;
    let too_big = u64::try_from(MAX_OBJECT_UNCOMPRESSED_BYTES)
        .unwrap()
        .saturating_add(1);
    stored[40..OBJECT_HEADER_LEN].copy_from_slice(&too_big.to_be_bytes());
    let err = unwrap_stored_object(&stored).unwrap_err();
    assert!(
        err.to_string().contains("exceeds") && err.to_string().contains("uncompressed"),
        "{err}"
    );
}

#[test]
fn gzip_envelope_rejects_expansion_past_declared_length() {
    let uncompressed = vec![b'x'; 64];
    let mut stored = wrap_stored_object(&uncompressed).unwrap();
    stored[40..OBJECT_HEADER_LEN].copy_from_slice(&8u64.to_be_bytes());
    let err = unwrap_stored_object(&stored).unwrap_err();
    assert!(
        err.to_string().contains("exceeds declared")
            || err.to_string().contains("does not match declared"),
        "{err}"
    );
}

#[test]
fn admit_rejects_unparseable_schema() {
    let err =
        admit_canonical_schema(SQL_CONTRACT_VERSION, "CREATE VIEW v AS SELECT 1").unwrap_err();
    assert!(err.to_string().contains("not fully admitted"), "{err}");
}

#[test]
fn fk_order_parents_before_reverse_alphabetical_children() {
    let sql = r#"
        CREATE TABLE a_child (
            id INTEGER PRIMARY KEY,
            parent_id INTEGER NOT NULL REFERENCES z_parent(id)
        );
        CREATE TABLE z_parent (id INTEGER PRIMARY KEY);
    "#;
    let schema = admit_canonical_schema(SQL_CONTRACT_VERSION, sql).unwrap();
    let ordered = sort_tables_by_foreign_keys(schema.tables).unwrap();
    assert_eq!(ordered[0].parsed.table, "z_parent");
    assert_eq!(ordered[1].parsed.table, "a_child");
}

#[test]
fn admit_covers_pk_unique_fk_index_default_and_check() {
    let sql = r#"
        CREATE TABLE z_parent (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL DEFAULT 'x',
            flag BOOLEAN NOT NULL CHECK (flag = TRUE OR flag = FALSE)
        );
        CREATE TABLE a_child (
            id INTEGER PRIMARY KEY,
            parent_id INTEGER NOT NULL,
            UNIQUE (parent_id),
            FOREIGN KEY (parent_id) REFERENCES z_parent (id)
        );
        CREATE INDEX idx_child_parent ON a_child (parent_id);
    "#;
    let schema = admit_canonical_schema(SQL_CONTRACT_VERSION, sql).unwrap();
    let ordered = sort_tables_by_foreign_keys(schema.tables.clone()).unwrap();
    assert_eq!(ordered[0].parsed.table, "z_parent");
    assert_eq!(ordered[1].parsed.table, "a_child");
    assert_eq!(schema.indexes.len(), 1);
    assert_eq!(schema.indexes[0].name, "idx_child_parent");
    assert_eq!(schema.indexes[0].columns, vec!["parent_id".to_string()]);
}

#[test]
fn fk_cycle_fails_closed() {
    let sql = r#"
        CREATE TABLE a (id INTEGER PRIMARY KEY, b_id INTEGER REFERENCES b(id));
        CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a(id));
    "#;
    let schema = admit_canonical_schema(SQL_CONTRACT_VERSION, sql).unwrap();
    let err = sort_tables_by_foreign_keys(schema.tables).unwrap_err();
    assert!(err.to_string().contains("cyclic"), "{err}");
}

#[test]
fn references_inside_default_string_is_not_a_foreign_key() {
    let sql = r#"
        CREATE TABLE parent (id INTEGER PRIMARY KEY);
        CREATE TABLE child (
            id INTEGER PRIMARY KEY,
            note TEXT NOT NULL DEFAULT 'REFERENCES parent'
        );
    "#;
    let schema = admit_canonical_schema(SQL_CONTRACT_VERSION, sql).unwrap();
    let ordered = sort_tables_by_foreign_keys(schema.tables).unwrap();
    assert_eq!(ordered[0].parsed.table, "parent");
    assert_eq!(ordered[1].parsed.table, "child");
    assert!(ordered[1].parsed.referenced_tables().is_empty());
}

#[test]
fn admit_rejects_create_index_with_trailing_tokens() {
    let err = admit_canonical_schema(
        SQL_CONTRACT_VERSION,
        "CREATE TABLE t (id INTEGER PRIMARY KEY);\nCREATE INDEX idx ON t THIS IS NOT SQL",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("not fully admitted") || err.to_string().contains("INDEX"),
        "{err}"
    );
}

#[test]
fn restore_opts_require_schema_migrations_from_capabilities_not_identity() {
    CanonicalRestoreOpts::from_caps(&DbCapabilities::advertised_sqlite()).unwrap();
    CanonicalRestoreOpts::from_caps(&DbCapabilities::advertised_postgres()).unwrap();
    CanonicalRestoreOpts::from_caps(&third_party_sqlite_row_marker_caps()).unwrap();

    let mut none = DbCapabilities::advertised_sqlite();
    none.schema_migrations = false;
    let err = CanonicalRestoreOpts::from_caps(&none).unwrap_err();
    assert!(
        err.to_string().contains("not a known versioning contract"),
        "{err}"
    );
}

#[test]
fn admit_rejects_create_index_with_malformed_where() {
    let err = admit_canonical_schema(
        SQL_CONTRACT_VERSION,
        "CREATE TABLE t (id INTEGER PRIMARY KEY);\nCREATE INDEX idx ON t (id) WHERE THIS IS NOT SQL",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("not fully admitted") || err.to_string().contains("WHERE"),
        "{err}"
    );
    admit_canonical_schema(
        SQL_CONTRACT_VERSION,
        "CREATE TABLE t (id INTEGER PRIMARY KEY);\nCREATE INDEX idx ON t (id) WHERE id IS NOT NULL",
    )
    .expect("valid partial index");
}

#[test]
fn admit_rejects_create_index_with_unknown_column() {
    let err = admit_canonical_schema(
        SQL_CONTRACT_VERSION,
        "CREATE TABLE t (id INTEGER);\nCREATE INDEX idx ON t (no_such_column)",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("not fully admitted")
            || err.to_string().contains("unknown column"),
        "{err}"
    );
    admit_canonical_schema(
        SQL_CONTRACT_VERSION,
        "CREATE TABLE t (id INTEGER);\nCREATE INDEX idx ON t (id)",
    )
    .expect("indexed column exists");
}

#[test]
fn order_key_prefers_pk_then_unique_then_full_row() {
    let pk =
        parse_create_table_schema("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
            .unwrap();
    assert_eq!(
        order_key_columns(&pk),
        vec!["id".to_string(), "name".to_string()]
    );
    assert_eq!(
        canonical_order_by_sql(&pk),
        "id ASC NULLS FIRST, name ASC NULLS FIRST"
    );
    let uniq = parse_create_table_schema("CREATE TABLE t (a TEXT, b TEXT, UNIQUE (b, a))").unwrap();
    assert_eq!(
        order_key_columns(&uniq),
        vec!["b".to_string(), "a".to_string()]
    );
    let one_unique =
        parse_create_table_schema("CREATE TABLE t (k TEXT UNIQUE, extra TEXT)").unwrap();
    assert_eq!(
        order_key_columns(&one_unique),
        vec!["k".to_string(), "extra".to_string()]
    );
    let keyless = parse_create_table_schema("CREATE TABLE t (a TEXT, b TEXT)").unwrap();
    assert_eq!(
        order_key_columns(&keyless),
        vec!["a".to_string(), "b".to_string()]
    );
}

#[test]
fn put_object_replaces_corrupt_existing_digest() {
    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let object = CanonicalObject::Identity {
        entries: BTreeMap::new(),
    };
    let digest = repo.put_object(&object).unwrap();
    let path = files
        .path()
        .join(BACKUPS_DIR)
        .join("objects")
        .join(&digest[..2])
        .join(&digest[2..]);
    std::fs::write(&path, b"truncated").unwrap();
    assert!(repo.get_object(&digest).is_err());
    assert_eq!(repo.put_object(&object).unwrap(), digest);
    match repo.get_object(&digest).unwrap() {
        CanonicalObject::Identity { entries } => assert!(entries.is_empty()),
        other => panic!("expected Identity, got {other:?}"),
    }
}

#[test]
fn put_object_concurrent_same_digest_is_stable() {
    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let object = CanonicalObject::Identity {
        entries: BTreeMap::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let repo = repo.clone();
            let object = object.clone();
            scope.spawn(move || repo.put_object(&object).unwrap());
        }
    });
    let digest = repo.put_object(&object).unwrap();
    match repo.get_object(&digest).unwrap() {
        CanonicalObject::Identity { entries } => assert!(entries.is_empty()),
        other => panic!("expected Identity, got {other:?}"),
    }
}

#[test]
fn gc_waits_for_publication_lock_and_keeps_live_objects() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let object = CanonicalObject::Identity {
        entries: BTreeMap::new(),
    };
    let guard = repo.lock_exclusive().unwrap();
    let digest = repo.put_object(&object).unwrap();
    let started = std::sync::Arc::new(AtomicBool::new(false));
    let finished = std::sync::Arc::new(AtomicBool::new(false));
    let started_gc = started.clone();
    let finished_gc = finished.clone();
    let repo_gc = repo.clone();
    let handle = std::thread::spawn(move || {
        started_gc.store(true, Ordering::SeqCst);
        repo_gc.gc_unreferenced_objects().unwrap();
        finished_gc.store(true, Ordering::SeqCst);
    });
    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(80));
    assert!(
        !finished.load(Ordering::SeqCst),
        "gc must not run while the publication lock is held"
    );
    assert!(repo.object_exists(&digest));
    let unit = BackupUnit {
        kind: DatabaseUnitKind::Library,
        plugin_id: None,
        binding: None,
        backend_at_capture: "sqlite".into(),
        sql_contract_version: SQL_CONTRACT_VERSION,
        schema_object: digest.clone(),
        identity_object: digest.clone(),
        tables: Vec::new(),
        plugin_schema_namespace: None,
        plugin_schema_state: None,
        plugin_schema_checksum: None,
    };
    let manifest = BackupManifest {
        format_version: BACKUP_FORMAT_VERSION,
        id: "lock-test".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        schema_state: "unreleased".into(),
        schema_checksum: None,
        frozen_version: None,
        sql_contract_version: SQL_CONTRACT_VERSION,
        app_version: "test".into(),
        reason: BackupReason::Manual,
        migrate_to: 0,
        include_plugin_databases: false,
        units: vec![unit],
    };
    repo.publish_manifest(&manifest).unwrap();
    drop(guard);
    handle.join().unwrap();
    assert!(repo.object_exists(&digest));
    assert!(finished.load(Ordering::SeqCst));
}

#[test]
fn validate_cell_enforces_types_and_nullability() {
    validate_cell(
        "t",
        "n",
        &DbValue::Null(DbType::Int64),
        SqlType::Integer,
        false,
    )
    .unwrap();
    let err = validate_cell(
        "t",
        "n",
        &DbValue::Null(DbType::Int64),
        SqlType::Integer,
        true,
    )
    .unwrap_err();
    assert!(err.to_string().contains("NOT NULL"), "{err}");
    validate_cell("t", "b", &DbValue::Boolean(true), SqlType::Boolean, true).unwrap();
    validate_cell("t", "i", &DbValue::Int64(i64::MIN), SqlType::Integer, true).unwrap();
    validate_cell("t", "i", &DbValue::Int64(i64::MAX), SqlType::Integer, true).unwrap();
    validate_cell("t", "f", &DbValue::Float64(1.25), SqlType::Real, true).unwrap();
    let err =
        validate_cell("t", "f", &DbValue::Float64(f64::NAN), SqlType::Real, true).unwrap_err();
    assert!(err.to_string().contains("non-finite"), "{err}");
    let err = validate_cell(
        "t",
        "f",
        &DbValue::Float64(f64::INFINITY),
        SqlType::Real,
        true,
    )
    .unwrap_err();
    assert!(err.to_string().contains("non-finite"), "{err}");
    validate_cell("t", "s", &DbValue::Text("🦀".into()), SqlType::Text, true).unwrap();
    validate_cell(
        "t",
        "blob",
        &DbValue::Bytes(vec![0, 255]),
        SqlType::Blob,
        true,
    )
    .unwrap();
    let err = validate_cell("t", "s", &DbValue::Int64(1), SqlType::Text, true).unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");
}

#[test]
fn schema_state_frozen_uses_that_version_not_latest() {
    const V1_OPS: &[MigrationOp] = &[MigrationOp::Schema(
        "CREATE TABLE frozen_v1 (id INTEGER PRIMARY KEY)",
    )];
    const V2_OPS: &[MigrationOp] = &[MigrationOp::Schema(
        "CREATE TABLE frozen_v2 (id INTEGER PRIMARY KEY)",
    )];
    let v1 = HostMigrationStep {
        version: 1,
        steps: V1_OPS,
        down: None,
        introduced_in: "0.1.0",
    };
    let v2 = HostMigrationStep {
        version: 2,
        steps: V2_OPS,
        down: None,
        introduced_in: "0.2.0",
    };
    let plan = [v1, v2];
    const UNRELEASED_ONLY: &[MigrationOp] = &[MigrationOp::Schema(
        "CREATE TABLE unreleased_only (id INTEGER PRIMARY KEY)",
    )];
    let checksum = plan[0].checksum();
    let sql = library_ddl_for_schema_state_with(
        &plan,
        UNRELEASED_ONLY,
        "deadbeef",
        SCHEMA_MIGRATIONS_DDL,
        2,
        &SchemaState::Frozen {
            version: 1,
            checksum,
        },
    )
    .unwrap();
    assert!(sql.contains("frozen_v1"));
    assert!(!sql.contains("frozen_v2"));
    assert!(!sql.contains("unreleased_only"));
}

#[test]
fn library_current_schema_admits_after_filtering_seed_dml() {
    let schema = library_canonical_schema().expect("current library pack must admit");
    assert!(
        schema.tables.iter().any(|t| t.parsed.table == "accounts"),
        "expected accounts table"
    );
    let joined = schema.schema_sql().join("\n").to_ascii_uppercase();
    assert!(
        !joined.contains("INSERT"),
        "seed DML must not live in the schema object"
    );
    let filtered = filter_library_pack_ddl(
        "INSERT OR IGNORE INTO bookclerk_slots (slot_key, bump) VALUES ('x', 0);\n\
         CREATE TABLE t (id INTEGER PRIMARY KEY)",
    )
    .unwrap();
    assert!(filtered.to_ascii_uppercase().contains("CREATE TABLE"));
    assert!(!filtered.to_ascii_uppercase().contains("INSERT"));
}

#[test]
fn backup_schema_admits_statement_list_without_join_reparse() {
    // Semicolons inside a DEFAULT literal must stay in the same statement.
    // Joining then splitting on ';' would mis-parse this list.
    let statements = [
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT DEFAULT ';not-a-split')",
        "CREATE INDEX idx_notes_body ON notes (body)",
    ];
    let admitted = admit_canonical_statements(SQL_CONTRACT_VERSION, &statements).unwrap();
    assert_eq!(admitted.tables.len(), 1);
    assert_eq!(admitted.indexes.len(), 1);
    assert!(admitted.tables[0]
        .create_sql
        .contains("DEFAULT ';not-a-split'"));
}

#[test]
fn schema_state_unreleased_includes_matching_pack() {
    const EXTRA_OPS: &[MigrationOp] = &[MigrationOp::Schema(
        "CREATE TABLE extra (id INTEGER PRIMARY KEY)",
    )];
    let checksum = crate::migrations::migration_ops_checksum(EXTRA_OPS, None);
    let sql = library_ddl_for_schema_state_with(
        &[],
        EXTRA_OPS,
        &checksum,
        SCHEMA_MIGRATIONS_DDL,
        SCHEMA_VERSION,
        &SchemaState::Unreleased {
            base_version: SCHEMA_VERSION,
            checksum: checksum.clone(),
        },
    )
    .unwrap();
    assert!(sql.contains("extra"), "{sql}");
    assert!(
        sql.to_ascii_lowercase()
            .contains("bookclerk_schema_migrations"),
        "{sql}"
    );
}

#[tokio::test]
async fn uninitialized_skips_even_with_include_plugin_databases() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    let skip = backup_library(
        &db,
        &BackupRequest {
            files_dir: files.path().to_path_buf(),
            schema_state: SchemaState::Uninitialized,
            reason: BackupReason::Manual,
            to_version: 0,
            include_plugin_databases: true,
            consistent_backup_read: true,
            backend_at_capture: "sqlite".into(),
            max_result_rows: 100,
            max_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
            max_atomic_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
            plugin_units: Vec::new(),
            adapter: None,
            in_process: false,
        },
    )
    .await
    .unwrap();
    assert!(skip.is_none());
}

#[tokio::test]
async fn sqlite_library_round_trip_replaces_not_merges() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let outcome = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .expect("unreleased backup");
    assert!(outcome.manifest.schema_state.starts_with("unreleased@"));
    assert_eq!(outcome.manifest.reason, BackupReason::Manual);
    assert_eq!(outcome.manifest.format_version, BACKUP_FORMAT_VERSION);

    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('extra', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    restore_backup(&db, files.path(), &outcome.manifest.id, &restore_ok())
        .await
        .unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM accounts").await, 1);
}

#[tokio::test]
async fn second_recovery_point_reuses_unchanged_objects() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let first = backup_library(
        &db,
        &backup_req(files.path(), state.clone(), BackupReason::Manual),
    )
    .await
    .unwrap()
    .unwrap();
    let second = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let shared: BTreeSet<_> = first
        .manifest
        .referenced_objects()
        .intersection(&second.manifest.referenced_objects())
        .cloned()
        .collect();
    assert!(
        !shared.is_empty(),
        "unchanged canonical content must reuse objects"
    );
    let repo = BackupRepository::open(files.path()).unwrap();
    repo.delete_manifest(&first.manifest.id).unwrap();
    verify_recovery_point(&repo, &second.manifest.id).unwrap();
    restore_backup(&db, files.path(), &second.manifest.id, &restore_ok())
        .await
        .unwrap();
}

#[tokio::test]
async fn gc_retains_live_objects_and_drops_orphans() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let first = backup_library(
        &db,
        &backup_req(files.path(), state.clone(), BackupReason::PreMigrate),
    )
    .await
    .unwrap()
    .unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('later', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let second = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    repo.delete_manifest(&first.manifest.id).unwrap();
    let deleted = repo.gc_unreferenced_objects().unwrap();
    assert!(
        deleted > 0
            || first
                .manifest
                .referenced_objects()
                .is_subset(&second.manifest.referenced_objects())
    );
    verify_recovery_point(&repo, &second.manifest.id).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn gc_refuses_symlinked_object_prefix_and_leaves_outside_intact() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let _ = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let objects = files.path().join(BACKUPS_DIR).join("objects");
    let outside = files.path().join("outside-victim");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), b"outside-bytes").unwrap();
    std::fs::write(outside.join(".dot"), b"dot-bytes").unwrap();
    // Plant a malicious prefix that points outside the objects tree.
    let link = objects.join("aa");
    let _ = std::fs::remove_dir_all(&link);
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let err = repo.gc_unreferenced_objects().unwrap_err();
    assert!(
        err.to_string().contains("symlinked backup object prefix"),
        "unexpected error: {err}"
    );
    assert_eq!(
        std::fs::read(outside.join("keep.txt")).unwrap(),
        b"outside-bytes"
    );
    assert_eq!(std::fs::read(outside.join(".dot")).unwrap(), b"dot-bytes");
}

#[cfg(unix)]
#[tokio::test]
async fn gc_unlinks_leaf_symlink_without_following_target() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let outcome = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let objects = files.path().join(BACKUPS_DIR).join("objects");
    let outside = files.path().join("leaf-victim.bin");
    std::fs::write(&outside, b"do-not-delete").unwrap();
    // Orphan leaf under a real prefix, plus a symlink leaf pointing outside.
    let prefix = objects.join("ab");
    std::fs::create_dir_all(&prefix).unwrap();
    let orphan = prefix.join("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
    std::fs::write(&orphan, b"orphan").unwrap();
    let link = prefix.join("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    // Also plant a leaf symlink to a live in-repo object and ensure peer survives GC
    // when that digest is still live (symlink to peer is not a live digest name).
    let live: String = outcome
        .manifest
        .referenced_objects()
        .into_iter()
        .next()
        .unwrap();
    let peer = objects.join(&live[..2]).join(&live[2..]);
    assert!(peer.is_file(), "live object must exist");
    let peer_link = prefix.join("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
    std::os::unix::fs::symlink(&peer, &peer_link).unwrap();
    let peer_bytes = std::fs::read(&peer).unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let deleted = repo.gc_unreferenced_objects().unwrap();
    assert!(
        deleted >= 2,
        "expected orphan + symlink leaves removed, got {deleted}"
    );
    assert!(!orphan.exists());
    assert!(
        link.symlink_metadata().is_err(),
        "outside symlink leaf removed"
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"do-not-delete");
    assert!(
        peer_link.symlink_metadata().is_err(),
        "peer symlink leaf removed"
    );
    assert_eq!(
        std::fs::read(&peer).unwrap(),
        peer_bytes,
        "live peer bytes survive"
    );
    verify_recovery_point(&repo, &outcome.manifest.id).unwrap();
}

#[tokio::test]
async fn gc_fails_closed_when_published_manifest_is_unreadable() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let first = backup_library(
        &db,
        &backup_req(files.path(), state.clone(), BackupReason::PreMigrate),
    )
    .await
    .unwrap()
    .unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('later', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let second = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let manifest_path = files
        .path()
        .join(BACKUPS_DIR)
        .join("manifests")
        .join(format!("{}.json", first.manifest.id));
    std::fs::write(&manifest_path, b"{not-a-manifest").unwrap();
    let listed = repo.list_manifests();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, second.manifest.id);
    let err = repo.gc_unreferenced_objects().unwrap_err();
    assert!(
        err.to_string().contains("digest")
            || err.to_string().contains("json")
            || err.to_string().contains("manifest"),
        "{err}"
    );
    let mut retained = first.manifest.referenced_objects();
    retained.extend(second.manifest.referenced_objects());
    for digest in retained {
        let path = {
            let path = files
                .path()
                .join(BACKUPS_DIR)
                .join("objects")
                .join(&digest[..2])
                .join(&digest[2..]);
            let s = path.to_string_lossy().into_owned();
            assert!(!s.contains("..") && !s.contains('\0'));
            std::path::PathBuf::from(s)
        };
        assert!(
            path.is_file(),
            "GC must retain object {digest} when a published manifest is unreadable"
        );
    }
    verify_recovery_point(&repo, &second.manifest.id).unwrap();
}

#[tokio::test]
async fn capture_orders_nulls_first_with_declared_tiebreakers() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&db).await;
    apply_admitted_sql(
        &db,
        &["CREATE TABLE items (k TEXT UNIQUE, extra TEXT)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    for (k, extra) in [(Some("m"), Some("b")), (None, Some("z")), (None, Some("a"))] {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO items (k, extra) VALUES (?, ?)",
            [
                bookclerk_db_exec::db_value_to_sea(&match k {
                    Some(s) => DbValue::Text(s.into()),
                    None => DbValue::Null(DbType::Text),
                }),
                bookclerk_db_exec::db_value_to_sea(&match extra {
                    Some(s) => DbValue::Text(s.into()),
                    None => DbValue::Null(DbType::Text),
                }),
            ],
        ))
        .await
        .unwrap();
    }
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit = capture::capture_plugin_unit(
        &db,
        &repo,
        &CanonicalExportOpts::default(),
        "demo",
        "items",
        "sqlite",
    )
    .await
    .unwrap();
    let meta = unit.tables.iter().find(|t| t.name == "items").unwrap();
    let mut rows = Vec::new();
    for digest in &meta.chunks {
        let CanonicalObject::TableChunk { rows: chunk, .. } = repo.get_object(digest).unwrap()
        else {
            panic!("expected table chunk");
        };
        rows.extend(chunk);
    }
    assert_eq!(
        rows,
        vec![
            vec![DbValue::Null(DbType::Text), DbValue::Text("a".into())],
            vec![DbValue::Null(DbType::Text), DbValue::Text("z".into())],
            vec![DbValue::Text("m".into()), DbValue::Text("b".into())],
        ]
    );
}

#[tokio::test]
async fn missing_and_corrupt_objects_fail_verify() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let outcome = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let digest = outcome.manifest.units[0].schema_object.clone();
    let path = files
        .path()
        .join(BACKUPS_DIR)
        .join("objects")
        .join(&digest[..2])
        .join(&digest[2..]);
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"not-a-bcko-object").unwrap();
    let err = verify_recovery_point(&repo, &outcome.manifest.id).unwrap_err();
    assert!(
        err.to_string().contains("corrupt")
            || err.to_string().contains("magic")
            || err.to_string().contains("digest"),
        "{err}"
    );
    std::fs::write(&path, original).unwrap();
    std::fs::remove_file(&path).unwrap();
    let err = verify_recovery_point(&repo, &outcome.manifest.id).unwrap_err();
    assert!(err.to_string().contains("missing"), "{err}");
}

#[tokio::test]
async fn omitted_backup_table_fails_verify_before_restore_mutates() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let before = count(
        &db,
        "SELECT COUNT(*) AS count FROM accounts WHERE account_id = 'keep'",
    )
    .await;
    assert_eq!(before, 1);
    let state = current_schema_state(&db).await.unwrap();
    let outcome = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let mut manifest = repo.read_manifest(&outcome.manifest.id).unwrap();
    manifest.units[0].tables.retain(|t| t.name != "books");
    repo.publish_manifest(&manifest).unwrap();
    let err = verify_recovery_point(&repo, &outcome.manifest.id).unwrap_err();
    assert!(
        err.to_string().contains("books") && err.to_string().contains("missing"),
        "{err}"
    );
    let err = restore_backup_in_repo(&db, &repo, &outcome.manifest.id, &restore_ok())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("books") || err.to_string().contains("table set"),
        "{err}"
    );
    let after = count(
        &db,
        "SELECT COUNT(*) AS count FROM accounts WHERE account_id = 'keep'",
    )
    .await;
    assert_eq!(after, 1, "restore must fail before DROP");
}

#[tokio::test]
async fn identity_object_must_cover_every_identity_table() {
    let src = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&src).await;
    apply_admitted_sql(
        &src,
        &["CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO notes (body) VALUES ('a')",
    ))
    .await
    .unwrap();
    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let mut unit = capture::capture_plugin_unit(
        &src,
        &repo,
        &CanonicalExportOpts::default(),
        "demo",
        "notes",
        "sqlite",
    )
    .await
    .unwrap();
    let CanonicalObject::Identity { mut entries } = repo.get_object(&unit.identity_object).unwrap()
    else {
        panic!("expected identity");
    };
    entries.remove("notes");
    unit.identity_object = repo
        .put_object(&CanonicalObject::Identity { entries })
        .unwrap();
    let err = verify_unit(&repo, &unit).unwrap_err();
    assert!(
        err.to_string().contains("identity") && err.to_string().contains("notes"),
        "{err}"
    );
}

#[tokio::test]
async fn restore_fails_closed_when_target_frozen_history_is_newer() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let outcome = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "DELETE FROM bookclerk_schema_migrations",
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO bookclerk_schema_migrations (namespace, version, state, checksum, app_version, applied_at) \
         VALUES ('bookclerk', 1, 'frozen', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', '0', 't')",
    ))
    .await
    .unwrap();
    let err = ensure_restore_target_is_replaceable(&db).await.unwrap_err();
    assert!(
        err.to_string().contains("newer") || err.to_string().contains("frozen@1"),
        "{err}"
    );
    let repo = BackupRepository::open(files.path()).unwrap();
    let err = restore_backup_in_repo(&db, &repo, &outcome.manifest.id, &restore_ok())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("newer") || err.to_string().contains("frozen@1"),
        "{err}"
    );
    let after = count(
        &db,
        "SELECT COUNT(*) AS count FROM accounts WHERE account_id = 'keep'",
    )
    .await;
    assert_eq!(after, 1, "restore must fail before DROP");
}

const SYNTHETIC_V1_SQL: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)";
const SYNTHETIC_V1_OPS: &[MigrationOp] = &[MigrationOp::Schema(SYNTHETIC_V1_SQL)];
const SYNTHETIC_V1_PLAN: &[HostMigrationStep] = &[HostMigrationStep {
    version: 1,
    steps: SYNTHETIC_V1_OPS,
    down: None,
    introduced_in: "0.0.0",
}];

#[tokio::test]
async fn backup_fails_closed_when_frozen_checksum_is_tampered() {
    let _guard = override_host_migration_plan(SYNTHETIC_V1_PLAN);
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_fresh_schema_sqlite(&db, SYNTHETIC_V1_PLAN, &[], 1)
        .await
        .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    match &state {
        SchemaState::Frozen { version, .. } => assert_eq!(*version, 1),
        other => panic!("expected Frozen@1, got {other}"),
    }
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "UPDATE bookclerk_schema_migrations SET checksum = 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef' \
         WHERE state = 'frozen'",
    ))
    .await
    .unwrap();
    let err = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("checksum mismatch") || err.to_string().contains("frozen"),
        "{err}"
    );
}

#[tokio::test]
async fn restore_uses_capability_kind_not_seaorm_backend() {
    let files = tempfile::tempdir().unwrap();
    let src = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&src).await.unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&src).await.unwrap();
    let outcome = backup_library(&src, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();

    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    seed_sqlite_with_leftover_pragma(&dest).await;
    let opts = CanonicalRestoreOpts::from_caps(&third_party_sqlite_row_marker_caps()).unwrap();
    restore_backup_in_repo(&dest, &repo, &outcome.manifest.id, &opts)
        .await
        .unwrap();
    assert_eq!(
        count(
            &dest,
            "SELECT COUNT(*) AS count FROM accounts WHERE account_id = 'keep'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn plugin_restore_fails_closed_when_stale_binding_lacks_catalog() {
    let src = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&src).await;
    apply_admitted_sql(
        &src,
        &["CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO notes (id, body) VALUES (1, 'src')",
    ))
    .await
    .unwrap();
    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit = capture::capture_plugin_unit(
        &src,
        &repo,
        &CanonicalExportOpts::default(),
        "demo",
        "notes",
        "sqlite",
    )
    .await
    .unwrap();
    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    dest.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "CREATE TABLE leftover (id INTEGER PRIMARY KEY)",
    ))
    .await
    .unwrap();
    dest.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO leftover (id) VALUES (7)",
    ))
    .await
    .unwrap();
    let err = restore_backup_unit(
        &dest,
        &repo,
        &unit,
        CanonicalRestoreKind::PluginBinding,
        &restore_ok(),
        false,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("reset or recreate")
            || err.to_string().contains("no durable canonical DDL catalog"),
        "{err}"
    );
    assert_eq!(
        count(&dest, "SELECT COUNT(*) AS count FROM leftover").await,
        1
    );
}

#[tokio::test]
async fn plugin_backup_round_trips_user_table_named_pg_notes() {
    let src = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&src).await;
    apply_admitted_sql(
        &src,
        &["CREATE TABLE pg_notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO pg_notes (id, body) VALUES (1, 'keep')",
    ))
    .await
    .unwrap();
    let names = super::util::backup_list_user_relations(&src, None)
        .await
        .unwrap();
    assert!(
        names.iter().any(|n| n == "pg_notes"),
        "user table pg_notes must not be treated as an engine catalog: {names:?}"
    );

    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit = capture::capture_plugin_unit(
        &src,
        &repo,
        &CanonicalExportOpts::default(),
        "demo",
        "pg_notes",
        "sqlite",
    )
    .await
    .unwrap();
    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    restore_backup_unit(
        &dest,
        &repo,
        &unit,
        CanonicalRestoreKind::PluginBinding,
        &restore_ok(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        count(
            &dest,
            "SELECT COUNT(*) AS count FROM pg_notes WHERE body = 'keep'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn plugin_restore_fails_closed_when_pg_notes_leftover_lacks_catalog() {
    let src = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&src).await;
    apply_admitted_sql(
        &src,
        &["CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO notes (id, body) VALUES (1, 'src')",
    ))
    .await
    .unwrap();
    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit = capture::capture_plugin_unit(
        &src,
        &repo,
        &CanonicalExportOpts::default(),
        "demo",
        "notes",
        "sqlite",
    )
    .await
    .unwrap();
    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    dest.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "CREATE TABLE pg_notes (id INTEGER PRIMARY KEY)",
    ))
    .await
    .unwrap();
    dest.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO pg_notes (id) VALUES (7)",
    ))
    .await
    .unwrap();
    let err = restore_backup_unit(
        &dest,
        &repo,
        &unit,
        CanonicalRestoreKind::PluginBinding,
        &restore_ok(),
        false,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("reset or recreate")
            || err.to_string().contains("no durable canonical DDL catalog"),
        "{err}"
    );
    assert_eq!(
        count(&dest, "SELECT COUNT(*) AS count FROM pg_notes").await,
        1
    );
}

#[test]
fn capture_text_order_by_desugars_and_proves_collate_sites() {
    let parsed =
        parse_create_table_schema("CREATE TABLE items (k TEXT PRIMARY KEY, extra TEXT)").unwrap();
    let order = canonical_order_by_sql(&parsed);
    let sql = format!("SELECT k, extra FROM items ORDER BY {order} LIMIT 10 OFFSET 0");
    let mut env = bookclerk_plugin_abi::SqlTypeEnv::new();
    env.insert_table("items", parsed.columns.clone());
    let mut req = bookclerk_plugin_abi::ExecuteRequest {
        operation_id: "backup-capture".into(),
        request_hash: String::new(),
        statements: vec![bookclerk_plugin_abi::TypedDbStatement {
            sql: sql.clone(),
            parameters: Vec::new(),
            kind: bookclerk_plugin_abi::DbPlanStatementKind::Select,
            max_rows: 10,
            result_selection: bookclerk_plugin_abi::DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    };
    bookclerk_plugin_abi::desugar_execute_request(&mut req);
    let proofs = bookclerk_plugin_abi::typecheck_execute_request_proofs(&req, &env).unwrap();
    let sql = req.statements[0].sql.as_str();
    assert!(
        sql.contains("NULLS"),
        "host desugar must make ORDER BY NULLS explicit: {sql}"
    );
    assert!(
        proofs.iter().any(|p| !p.text_collate_sites.is_empty()),
        "TEXT ORDER BY must prove collate sites for adapter COLLATE realization"
    );
}

#[tokio::test]
async fn retention_never_deletes_manual_backups() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    backup_library(
        &db,
        &backup_req(files.path(), state.clone(), BackupReason::Manual),
    )
    .await
    .unwrap();
    for _ in 0..BACKUP_RETENTION + 2 {
        backup_library(
            &db,
            &backup_req(files.path(), state.clone(), BackupReason::PreMigrate),
        )
        .await
        .unwrap();
    }
    let listed = list_backups(files.path()).unwrap();
    let manuals = listed
        .iter()
        .filter(|e| e.manifest.reason == BackupReason::Manual)
        .count();
    assert_eq!(manuals, 1);
    let autos = listed
        .iter()
        .filter(|e| e.manifest.reason == BackupReason::PreMigrate)
        .count();
    assert!(autos <= BACKUP_RETENTION, "{autos}");
}

#[tokio::test]
async fn missing_consistent_backup_read_fails_closed() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let mut req = backup_req(files.path(), state, BackupReason::Manual);
    req.consistent_backup_read = false;
    let err = backup_library(&db, &req).await.unwrap_err();
    assert!(err.to_string().contains("consistentBackupRead"), "{err}");
}

#[tokio::test]
async fn restore_without_atomic_unit_restore_fails_before_drop() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let outcome = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let err = restore_backup(
        &db,
        files.path(),
        &outcome.manifest.id,
        &restore_without_atomic(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("atomicUnitRestore"), "{err}");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM accounts").await, 1);
}

#[tokio::test]
async fn paged_table_larger_than_max_result_rows_round_trips() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&db).await;
    apply_admitted_sql(
        &db,
        &["CREATE TABLE items (id INTEGER PRIMARY KEY, body TEXT NOT NULL)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    for i in 0..25 {
        db.execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            format!("INSERT INTO items (id, body) VALUES ({i}, 'row-{i}')"),
        ))
        .await
        .unwrap();
    }
    let repo = BackupRepository::open(files.path()).unwrap();
    let opts = CanonicalExportOpts {
        max_result_rows: 7,
        chunk_target_bytes: 80,
        ..Default::default()
    };
    let unit = capture::capture_plugin_unit(&db, &repo, &opts, "demo", "items", "sqlite")
        .await
        .unwrap();
    let chunks: usize = unit.tables.iter().map(|t| t.chunks.len()).sum();
    assert!(chunks > 1, "expected multiple chunks, got {chunks}");
    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&dest).await;
    restore_backup_unit(
        &dest,
        &repo,
        &unit,
        CanonicalRestoreKind::PluginBinding,
        &restore_ok(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(count(&dest, "SELECT COUNT(*) FROM items").await, 25);
}

fn encoded_blob_page_bytes(n: usize, payload: &[u8]) -> usize {
    let cols = vec![
        DbColumn {
            name: "id".into(),
            db_type: DbType::Int64,
        },
        DbColumn {
            name: "blob".into(),
            db_type: DbType::Bytes,
        },
    ];
    let rows = (0..n)
        .map(|i| DbRow {
            values: vec![
                DbValue::Int64(i64::try_from(i).unwrap()),
                DbValue::Bytes(payload.to_vec()),
            ],
        })
        .collect();
    let stmt = StatementResult::from_rows(cols, rows).unwrap();
    encoded_statement_result_bytes(&stmt).unwrap().len()
}

#[tokio::test]
async fn capture_pages_when_row_count_fits_but_byte_budget_does_not() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&db).await;
    apply_admitted_sql(
        &db,
        &["CREATE TABLE blobs (id INTEGER PRIMARY KEY, blob BLOB NOT NULL)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    let payload = vec![0xABu8; 96];
    for i in 0..8 {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO blobs (id, blob) VALUES (?, ?)",
            [
                bookclerk_db_exec::db_value_to_sea(&DbValue::Int64(i)),
                bookclerk_db_exec::db_value_to_sea(&DbValue::Bytes(payload.clone())),
            ],
        ))
        .await
        .unwrap();
    }
    let one = encoded_blob_page_bytes(1, &payload);
    let many = encoded_blob_page_bytes(8, &payload);
    assert!(
        many > one,
        "multi-row encoded page ({many}) must exceed one row ({one})"
    );
    let budget = u32::try_from(one + (many - one) / 2).unwrap();
    assert!(
        usize::try_from(budget).unwrap() > one,
        "budget {budget} must still fit a single row ({one})"
    );
    let repo = BackupRepository::open(files.path()).unwrap();
    let opts = CanonicalExportOpts {
        max_result_rows: 100,
        max_result_bytes: budget,
        max_atomic_result_bytes: budget,
        ..Default::default()
    };
    let unit = capture::capture_plugin_unit(&db, &repo, &opts, "demo", "blobs", "sqlite")
        .await
        .unwrap();
    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&dest).await;
    restore_backup_unit(
        &dest,
        &repo,
        &unit,
        CanonicalRestoreKind::PluginBinding,
        &restore_ok(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(count(&dest, "SELECT COUNT(*) FROM blobs").await, 8);
}

#[tokio::test]
async fn capture_fails_closed_when_schema_state_changes_inside_txn() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "UPDATE bookclerk_schema_migrations SET checksum = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
         WHERE state = 'unreleased'",
    ))
    .await
    .unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let err = capture::capture_library_unit(
        &db,
        &repo,
        &state,
        &CanonicalExportOpts::default(),
        "sqlite",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("schema state changed"), "{err}");
}

#[tokio::test]
async fn plugin_round_trip_preserves_version_and_does_not_migrate() {
    let src = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&src).await;
    apply_admitted_sql(
        &src,
        &[
            "CREATE TABLE z_parent (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE)",
            "CREATE TABLE a_child (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                parent_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                blob BLOB,
                flag BOOLEAN,
                rating REAL,
                note TEXT,
                FOREIGN KEY (parent_id) REFERENCES z_parent (id)
            )",
            "CREATE TABLE plugin_meta (schema_version INTEGER NOT NULL)",
            "CREATE UNIQUE INDEX idx_child_title ON a_child (title)",
        ],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO z_parent (id, name) VALUES (42, 'ann')",
    ))
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO a_child (id, parent_id, title, blob, flag, rating, note) VALUES \
         (7, 42, '🦀', X'0001ff', 1, 1.5, NULL)",
    ))
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO plugin_meta (schema_version) VALUES (7)",
    ))
    .await
    .unwrap();

    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit = capture::capture_plugin_unit(
        &src,
        &repo,
        &CanonicalExportOpts::default(),
        "demo",
        "notes",
        "sqlite",
    )
    .await
    .unwrap();
    assert!(unit.tables.iter().all(|t| t.name != "accounts"));

    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&dest).await;
    restore_backup_unit(
        &dest,
        &repo,
        &unit,
        CanonicalRestoreKind::PluginBinding,
        &restore_ok(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        count(&dest, "SELECT schema_version FROM plugin_meta").await,
        7
    );
    dest.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO z_parent (name) VALUES ('next')",
    ))
    .await
    .unwrap();
    let next = count(&dest, "SELECT id FROM z_parent WHERE name = 'next'").await;
    assert!(next > 42, "next id {next} must be beyond 42");
    dest.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "UPDATE plugin_meta SET schema_version = 8",
    ))
    .await
    .unwrap();
    assert_eq!(
        count(&dest, "SELECT schema_version FROM plugin_meta").await,
        8
    );
}

#[tokio::test]
async fn library_only_restore_preserves_plugin_registry() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    LibraryStore::from_connection(db.clone())
        .record_plugin_database(
            "demoplug",
            "notes",
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "sqlite",
            "/tmp/live.db",
        )
        .await
        .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let outcome = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    assert!(!outcome.manifest.include_plugin_databases);
    restore_backup(&db, files.path(), &outcome.manifest.id, &restore_ok())
        .await
        .unwrap();
    let rows = LibraryStore::from_connection(db.clone())
        .list_plugin_databases(None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].unit_ref, "/tmp/live.db");
}

#[tokio::test]
async fn empty_included_plugin_backup_preserves_live_registry() {
    // `--include-plugin-databases` with an empty registry still sets the
    // manifest flag, but captures no units. Restore must not wipe bindings
    // registered after the backup was taken.
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let mut req = backup_req(files.path(), state, BackupReason::Manual);
    req.include_plugin_databases = true;
    let outcome = backup_library(&db, &req).await.unwrap().unwrap();
    assert!(outcome.manifest.include_plugin_databases);
    assert_eq!(outcome.manifest.units.len(), 1, "library unit only");

    LibraryStore::from_connection(db.clone())
        .record_plugin_database(
            "demoplug",
            "notes",
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "sqlite",
            "/tmp/after-backup.db",
        )
        .await
        .unwrap();
    restore_backup(&db, files.path(), &outcome.manifest.id, &restore_ok())
        .await
        .unwrap();
    let rows = LibraryStore::from_connection(db.clone())
        .list_plugin_databases(None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].unit_ref, "/tmp/after-backup.db");
}

#[tokio::test]
async fn restore_sqlite_keeps_foreign_keys_enforced() {
    let files = tempfile::tempdir().unwrap();
    let src = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&src).await.unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&src).await.unwrap();
    let outcome = backup_library(&src, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&dest).await.unwrap();
    restore_backup(&dest, files.path(), &outcome.manifest.id, &restore_ok())
        .await
        .unwrap();
    let fk = dest
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_keys",
        ))
        .await
        .unwrap();
    let fk_on = fk
        .first()
        .and_then(|row| row.try_get_by_index::<i64>(0).ok())
        .unwrap_or(0);
    assert_eq!(fk_on, 1, "restore must not leave PRAGMA foreign_keys off");
    let err = dest
        .execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "INSERT INTO books (uuid, source, account_id, product_id, marketplace, title, created_at, updated_at) \
             VALUES ('u', 'audible', 'missing', 'p', 'us', 't', 't', 't')",
        ))
        .await
        .expect_err("orphan book insert must fail");
    let s = err.to_string().to_lowercase();
    assert!(
        s.contains("foreign") || s.contains("constraint") || s.contains("787"),
        "{err}"
    );
}

#[tokio::test]
async fn include_plugin_databases_fails_closed_without_prepared_unit() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    LibraryStore::from_connection(db.clone())
        .record_plugin_database(
            "demoplug",
            "notes",
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "sqlite",
            "/tmp/unused.db",
        )
        .await
        .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let mut req = backup_req(files.path(), state, BackupReason::Manual);
    req.include_plugin_databases = true;
    let err = backup_library(&db, &req).await.unwrap_err();
    assert!(
        err.to_string().contains("demoplug/notes") && err.to_string().contains("fails closed"),
        "{err}"
    );
}

#[tokio::test]
async fn include_plugin_databases_captures_and_restores_plugin_unit() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let plugin = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&plugin).await;
    apply_admitted_sql(
        &plugin,
        &["CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    plugin
        .execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "INSERT INTO notes (id, body) VALUES (3, 'hi')",
        ))
        .await
        .unwrap();
    LibraryStore::from_connection(db.clone())
        .record_plugin_database(
            "demoplug",
            "notes",
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "sqlite",
            "memory",
        )
        .await
        .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let mut req = backup_req(files.path(), state, BackupReason::Manual);
    req.include_plugin_databases = true;
    req.plugin_units = vec![prepared_plugin("demoplug", "notes", plugin.clone())];
    let outcome = backup_library(&db, &req).await.unwrap().unwrap();
    assert_eq!(outcome.manifest.units.len(), 2);

    plugin
        .execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "INSERT INTO notes (id, body) VALUES (9, 'extra')",
        ))
        .await
        .unwrap();
    let plan = restore_backup(&db, files.path(), &outcome.manifest.id, &restore_ok())
        .await
        .unwrap();
    assert_eq!(plan.plugin_units.len(), 1);
    let after_library = LibraryStore::from_connection(db.clone())
        .list_plugin_databases(None)
        .await
        .unwrap();
    assert!(
        after_library.is_empty(),
        "included plugin DBs rebuild the registry; leftover source unit_ref must not remain"
    );
    LibraryStore::from_connection(db.clone())
        .rebind_plugin_database(
            "demoplug",
            "notes",
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "sqlite",
            "/tmp/target.db",
        )
        .await
        .unwrap();
    let rebound = LibraryStore::from_connection(db.clone())
        .get_plugin_database("demoplug", "notes")
        .await
        .unwrap()
        .expect("rebound row");
    assert_eq!(rebound.unit_ref, "/tmp/target.db");
    assert_eq!(rebound.backend_kind, "sqlite");
    let repo = BackupRepository::open(files.path()).unwrap();
    restore_backup_unit(
        &plugin,
        &repo,
        &plan.plugin_units[0],
        CanonicalRestoreKind::PluginBinding,
        &restore_ok(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(count(&plugin, "SELECT COUNT(*) FROM notes").await, 1);
}

#[tokio::test]
async fn include_plugin_databases_captures_two_bindings() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    let notes = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&notes).await;
    apply_admitted_sql(
        &notes,
        &["CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    let cache = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&cache).await;
    apply_admitted_sql(
        &cache,
        &["CREATE TABLE cache (id INTEGER PRIMARY KEY, body TEXT)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    let store = LibraryStore::from_connection(db.clone());
    store
        .record_plugin_database(
            "demoplug",
            "notes",
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "sqlite",
            "memory-notes",
        )
        .await
        .unwrap();
    store
        .record_plugin_database(
            "demoplug",
            "cache",
            "platform:bookclerk/bookclerk-plugin-database-sqlite",
            "sqlite",
            "memory-cache",
        )
        .await
        .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let mut req = backup_req(files.path(), state, BackupReason::Manual);
    req.include_plugin_databases = true;
    req.plugin_units = vec![
        prepared_plugin("demoplug", "notes", notes),
        prepared_plugin("demoplug", "cache", cache),
    ];
    let outcome = backup_library(&db, &req).await.unwrap().unwrap();
    assert_eq!(outcome.manifest.plugin_units().len(), 2);
    let repo = BackupRepository::open(files.path()).unwrap();
    let validated = verify_recovery_point(&repo, &outcome.manifest.id).unwrap();
    assert_eq!(validated.plugin_units.len(), 2);
}

#[tokio::test]
async fn quoted_unicode_text_round_trips() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO accounts (account_id, marketplace, source, label, created_at, updated_at) \
         VALUES ('a1', 'us', 'audible', 'it''s \"quoted\" and 🦀', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let outcome = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap()
        .unwrap();
    restore_backup(&db, files.path(), &outcome.manifest.id, &restore_ok())
        .await
        .unwrap();
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT label FROM accounts",
        ))
        .await
        .unwrap();
    let label = rows[0]
        .try_get::<Option<String>>("", "label")
        .ok()
        .flatten();
    assert_eq!(label.as_deref(), Some("it's \"quoted\" and 🦀"));
}

#[tokio::test]
async fn semicolon_default_check_and_unicode_round_trip_as_canonical_bundle() {
    let src = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&src).await;
    apply_admitted_sql(
        &src,
        &["CREATE TABLE notes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL DEFAULT 'a;b',
                flag TEXT CHECK (flag <> ';'),
                body TEXT
            )"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO notes (flag, body) VALUES ('ok', 'café 🎵 שׁ')",
    ))
    .await
    .unwrap();
    src.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO notes (title, flag, body) VALUES ('x;y', 'z', 'it''s \"quoted\"')",
    ))
    .await
    .unwrap();

    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit = capture::capture_plugin_unit(
        &src,
        &repo,
        &CanonicalExportOpts::default(),
        "demo",
        "notes",
        "d1",
    )
    .await
    .unwrap();

    let dest = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&dest).await;
    restore_backup_unit(
        &dest,
        &repo,
        &unit,
        CanonicalRestoreKind::PluginBinding,
        &restore_ok(),
        false,
    )
    .await
    .unwrap();

    let rows = dest
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT title, flag, body FROM notes ORDER BY id",
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let title0 = rows[0]
        .try_get::<Option<String>>("", "title")
        .ok()
        .flatten();
    assert_eq!(title0.as_deref(), Some("a;b"));
    let body0 = rows[0].try_get::<Option<String>>("", "body").ok().flatten();
    assert_eq!(body0.as_deref(), Some("café 🎵 שׁ"));
    dest.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT INTO notes (flag) VALUES (';')",
    ))
    .await
    .expect_err("CHECK (flag <> ';') must survive restore");
}

#[tokio::test]
async fn missing_host_table_aborts_backup() {
    let files = tempfile::tempdir().unwrap();
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Sqlite,
        "DROP TABLE books",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let err = backup_library(&db, &backup_req(files.path(), state, BackupReason::Manual))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot query") || err.to_string().contains("books"),
        "{err}"
    );
}

#[test]
fn extract_rejects_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("escape.tar.gz");
    {
        let file = std::fs::File::create(&archive).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut header = tar::Header::new_old();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path("placeholder.txt").unwrap();
        header.set_size(4);
        {
            let bytes = header.as_mut_bytes();
            let name = b"../escape.txt\0";
            bytes[..name.len()].copy_from_slice(name);
            for b in &mut bytes[name.len()..100] {
                *b = 0;
            }
        }
        header.set_cksum();
        builder.append(&header, b"evil".as_slice()).unwrap();
        builder.finish().unwrap();
    }
    let out = dir.path().join("out");
    let err = extract_backup_archive(&archive, &out).unwrap_err();
    assert!(err.to_string().contains("escapes"), "{err}");
}

#[test]
fn extract_rejects_non_regular_entries() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("link.tar.gz");
    {
        let file = std::fs::File::create(&archive).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_path("ok.txt").unwrap();
        header.set_size(0);
        header.set_cksum();
        builder.append(&header, &[] as &[u8]).unwrap();
        builder.finish().unwrap();
    }
    let out = dir.path().join("out");
    let err = extract_backup_archive(&archive, &out).unwrap_err();
    assert!(err.to_string().contains("not allowed"), "{err}");
}

fn write_targz(path: &Path, files: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    for (name, body) in files {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path(name).unwrap();
        header.set_size(u64::try_from(body.len()).unwrap());
        header.set_cksum();
        builder.append(&header, *body).unwrap();
    }
    builder.finish().unwrap();
}

#[test]
fn extract_rejects_oversized_entry() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("big.tar.gz");
    write_targz(&archive, &[("objects/aa/bb", b"hello world")]);
    let out = dir.path().join("out");
    let err = extract_backup_archive_limited(
        &archive,
        &out,
        ArchiveExtractLimits {
            max_entries: MAX_BACKUP_ARCHIVE_ENTRIES,
            max_entry_bytes: 4,
            max_total_bytes: MAX_BACKUP_ARCHIVE_TOTAL_BYTES,
            max_stream_bytes: MAX_BACKUP_ARCHIVE_STREAM_BYTES,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("entry") && err.to_string().contains("bytes"),
        "{err}"
    );
}

#[test]
fn extract_rejects_too_many_entries() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("many.tar.gz");
    write_targz(
        &archive,
        &[("a.txt", b"one"), ("b.txt", b"two"), ("c.txt", b"three")],
    );
    let out = dir.path().join("out");
    let err = extract_backup_archive_limited(
        &archive,
        &out,
        ArchiveExtractLimits {
            max_entries: 2,
            max_entry_bytes: MAX_BACKUP_ARCHIVE_ENTRY_BYTES,
            max_total_bytes: MAX_BACKUP_ARCHIVE_TOTAL_BYTES,
            max_stream_bytes: MAX_BACKUP_ARCHIVE_STREAM_BYTES,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("more than 2 entries"), "{err}");
}

#[test]
fn extract_rejects_total_expanded_budget() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("total.tar.gz");
    write_targz(&archive, &[("a.txt", b"hello"), ("b.txt", b"world")]);
    let out = dir.path().join("out");
    let err = extract_backup_archive_limited(
        &archive,
        &out,
        ArchiveExtractLimits {
            max_entries: MAX_BACKUP_ARCHIVE_ENTRIES,
            max_entry_bytes: MAX_BACKUP_ARCHIVE_ENTRY_BYTES,
            max_total_bytes: 8,
            max_stream_bytes: MAX_BACKUP_ARCHIVE_STREAM_BYTES,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("expanded size") && err.to_string().contains("exceeds"),
        "{err}"
    );
}

fn postgres_url() -> Option<String> {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if url.is_some() {
        return url;
    }
    assert!(
        std::env::var("BOOKCLERK_REQUIRE_POSTGRES_TESTS")
            .ok()
            .as_deref()
            != Some("1"),
        "BOOKCLERK_TEST_POSTGRES_URL is required when BOOKCLERK_REQUIRE_POSTGRES_TESTS=1"
    );
    None
}

async fn postgres_throwaway() -> Option<(DatabaseConnection, String)> {
    let url = postgres_url()?;
    let db_name = format!("bck_{}", uuid::Uuid::new_v4().as_simple());
    let admin = sea_orm::Database::connect(url.as_str())
        .await
        .unwrap_or_else(|err| panic!("connect BOOKCLERK_TEST_POSTGRES_URL: {err}"));
    let backend = admin.get_database_backend();
    admin
        .execute_raw(Statement::from_string(
            backend,
            format!("CREATE DATABASE {db_name}"),
        ))
        .await
        .unwrap_or_else(|err| panic!("CREATE DATABASE {db_name}: {err}"));
    let (base, query) = match url.split_once('?') {
        Some((base, q)) => (base, Some(q)),
        None => (url.as_str(), None),
    };
    let trimmed = base.trim_end_matches('/');
    let slash = trimmed
        .rfind('/')
        .unwrap_or_else(|| panic!("BOOKCLERK_TEST_POSTGRES_URL has no database path: {url}"));
    let db_url = match query {
        Some(q) => format!("{}/{db_name}?{q}", &trimmed[..slash]),
        None => format!("{}/{db_name}", &trimmed[..slash]),
    };
    let db = sea_orm::Database::connect(&db_url)
        .await
        .unwrap_or_else(|err| panic!("connect throwaway {db_name}: {err}"));
    Some((db, db_name))
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_library_backup_round_trip() {
    let Some((db, _)) = postgres_throwaway().await else {
        return;
    };
    apply_host_schema(&db).await.unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Postgres,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&db).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let mut req = backup_req(files.path(), state, BackupReason::Manual);
    req.backend_at_capture = "postgres".into();
    req.in_process = true;
    let outcome = backup_library(&db, &req).await.unwrap().unwrap();
    db.execute_raw(Statement::from_string(
        DbBackend::Postgres,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('extra', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    restore_backup(&db, files.path(), &outcome.manifest.id, &restore_postgres())
        .await
        .unwrap();
    let n: i64 = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT COUNT(*) FROM accounts",
        ))
        .await
        .unwrap()
        .first()
        .and_then(|row| row.try_get_by_index::<i64>(0).ok())
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_capture_orders_nulls_first_with_declared_tiebreakers() {
    let Some((db, _)) = postgres_throwaway().await else {
        return;
    };
    apply_bootstrap(&db).await;
    apply_admitted_sql(
        &db,
        &["CREATE TABLE items (k TEXT UNIQUE, extra TEXT)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    for sql in [
        "INSERT INTO items (k, extra) VALUES (NULL, 'z')",
        "INSERT INTO items (k, extra) VALUES (NULL, 'a')",
        "INSERT INTO items (k, extra) VALUES ('m', 'b')",
    ] {
        db.execute_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap();
    }
    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit =
        capture::capture_plugin_unit(&db, &repo, &export_postgres(), "demo", "items", "postgres")
            .await
            .unwrap();
    let meta = unit.tables.iter().find(|t| t.name == "items").unwrap();
    let mut rows = Vec::new();
    for digest in &meta.chunks {
        let CanonicalObject::TableChunk { rows: chunk, .. } = repo.get_object(digest).unwrap()
        else {
            panic!("expected table chunk");
        };
        rows.extend(chunk);
    }
    assert_eq!(
        rows,
        vec![
            vec![DbValue::Null(DbType::Text), DbValue::Text("a".into())],
            vec![DbValue::Null(DbType::Text), DbValue::Text("z".into())],
            vec![DbValue::Text("m".into()), DbValue::Text("b".into())],
        ]
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_capture_text_order_matches_utf8_binary() {
    let Some((db, _)) = postgres_throwaway().await else {
        return;
    };
    apply_bootstrap(&db).await;
    apply_admitted_sql(
        &db,
        &["CREATE TABLE items (k TEXT PRIMARY KEY)"],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    for value in ["a", "B"] {
        db.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            format!("INSERT INTO items (k) VALUES ('{value}')"),
        ))
        .await
        .unwrap();
    }
    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit =
        capture::capture_plugin_unit(&db, &repo, &export_postgres(), "demo", "items", "postgres")
            .await
            .unwrap();
    let meta = unit.tables.iter().find(|t| t.name == "items").unwrap();
    let mut rows = Vec::new();
    for digest in &meta.chunks {
        let CanonicalObject::TableChunk { rows: chunk, .. } = repo.get_object(digest).unwrap()
        else {
            panic!("expected table chunk");
        };
        rows.extend(chunk);
    }
    assert_eq!(
        rows,
        vec![
            vec![DbValue::Text("B".into())],
            vec![DbValue::Text("a".into())],
        ],
        "UTF-8 binary order is B (0x42) then a (0x61); locale order would flip them"
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_library_restores_to_sqlite() {
    let Some((pg, _)) = postgres_throwaway().await else {
        return;
    };
    apply_host_schema(&pg).await.unwrap();
    pg.execute_raw(Statement::from_string(
        DbBackend::Postgres,
        "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
         VALUES ('keep', 'us', 'audible', 't', 't')",
    ))
    .await
    .unwrap();
    let state = current_schema_state(&pg).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let mut req = backup_req(files.path(), state, BackupReason::Manual);
    req.backend_at_capture = "postgres".into();
    req.in_process = true;
    let outcome = backup_library(&pg, &req).await.unwrap().unwrap();
    let sqlite = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&sqlite).await.unwrap();
    restore_backup(&sqlite, files.path(), &outcome.manifest.id, &restore_ok())
        .await
        .unwrap();
    assert_eq!(
        count(
            &sqlite,
            "SELECT COUNT(*) FROM accounts WHERE account_id = 'keep'"
        )
        .await,
        1
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_sqlite_library_restores_to_postgres() {
    let Some((pg, _)) = postgres_throwaway().await else {
        return;
    };
    let sqlite = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_host_schema(&sqlite).await.unwrap();
    sqlite
        .execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
             VALUES ('keep', 'us', 'audible', 't', 't')",
        ))
        .await
        .unwrap();
    let state = current_schema_state(&sqlite).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let outcome = backup_library(
        &sqlite,
        &backup_req(files.path(), state, BackupReason::Manual),
    )
    .await
    .unwrap()
    .unwrap();
    apply_host_schema(&pg).await.unwrap();
    restore_backup(&pg, files.path(), &outcome.manifest.id, &restore_postgres())
        .await
        .unwrap();
    let n: i64 = pg
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT COUNT(*) FROM accounts WHERE account_id = 'keep'",
        ))
        .await
        .unwrap()
        .first()
        .and_then(|row| row.try_get_by_index::<i64>(0).ok())
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_sqlite_plugin_restores_to_postgres() {
    let Some((pg, _)) = postgres_throwaway().await else {
        return;
    };
    let sqlite = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .unwrap();
    apply_bootstrap(&sqlite).await;
    apply_admitted_sql(
        &sqlite,
        &[
            "CREATE TABLE z_parent (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
            "CREATE TABLE a_child (id INTEGER PRIMARY KEY AUTOINCREMENT, parent_id INTEGER NOT NULL, FOREIGN KEY (parent_id) REFERENCES z_parent (id))",
            "CREATE TABLE plugin_meta (schema_version INTEGER NOT NULL)",
        ],
        CanonicalRestoreKind::PluginBinding,
    )
    .await
    .unwrap();
    sqlite
        .execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "INSERT INTO z_parent (id, name) VALUES (1, 'p')",
        ))
        .await
        .unwrap();
    sqlite
        .execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "INSERT INTO a_child (id, parent_id) VALUES (2, 1)",
        ))
        .await
        .unwrap();
    sqlite
        .execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "INSERT INTO plugin_meta (schema_version) VALUES (7)",
        ))
        .await
        .unwrap();
    let files = tempfile::tempdir().unwrap();
    let repo = BackupRepository::open(files.path()).unwrap();
    let unit = capture::capture_plugin_unit(
        &sqlite,
        &repo,
        &CanonicalExportOpts::default(),
        "demo",
        "notes",
        "sqlite",
    )
    .await
    .unwrap();
    restore_backup_unit(
        &pg,
        &repo,
        &unit,
        CanonicalRestoreKind::PluginBinding,
        &restore_postgres_plugin(),
        false,
    )
    .await
    .unwrap();
    let version: i64 = pg
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT schema_version FROM plugin_meta",
        ))
        .await
        .unwrap()
        .first()
        .and_then(|row| row.try_get_by_index::<i64>(0).ok())
        .unwrap();
    assert_eq!(version, 7);
}

#[test]
fn d1_does_not_advertise_backup_capabilities() {
    let d1 = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
    assert!(!d1.supports_consistent_backup_read());
    assert!(!d1.supports_atomic_unit_restore());
    let sqlite = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
    assert!(sqlite.supports_consistent_backup_read());
    assert!(sqlite.supports_atomic_unit_restore());
    let pg = bookclerk_plugin_abi::DbCapabilities::advertised_postgres();
    assert!(pg.supports_consistent_backup_read());
    assert!(pg.supports_atomic_unit_restore());
}
