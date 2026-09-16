//! Complete per-unit replacement restore from a verified recovery point.

use std::collections::BTreeMap;

use bookclerk_plugin_abi::{
    parse_create_index_sql, parse_create_table_schema, DbValue, SQL_CATALOG_TABLE, SQL_DDL_TABLE,
    SQL_IDENTITY_TABLE, SQL_SCHEMA_TABLE,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};

use super::capture::plugin_canonical_schema_from_ddl_catalog;
use super::encode::CanonicalObject;
use super::repository::BackupRepository;
use super::schema::{library_canonical_schema, sort_tables_by_foreign_keys};
use super::util::{cell_text, exec_bound, exec_sql, ident_ok, table_exists};
use super::verify::{load_admitted_schema, verify_unit};
use super::{
    BackupUnit, CanonicalDatabaseSchema, CanonicalRestoreKind, CanonicalRestoreOpts,
    CanonicalTableSchema, IdentityHighWater, LIBRARY_SKIP_TABLES,
};
use crate::error::{LibraryError, Result};

/// Destructively replaces one logical database with `unit`. Never merges.
///
/// Verifies every referenced object before the first `DROP`. Plugin restore
/// rebuilds catalog companions from admitted SQL. It does not run plugin-owned
/// migrations. When `preserve_plugin_registry` is true, library restore does
/// not drop or replay `plugin_databases` (environment-local placement).
/// Row inserts are parameterized canonical Bookclerk SQL (`?` + typed
/// [`DbValue`] binds); adapters lower placeholders at the execute edge.
/// SQLite restore defers foreign keys on the restore transaction and runs
/// `PRAGMA foreign_key_check` before commit. It never toggles
/// `PRAGMA foreign_keys` on the pool connection.
///
/// # Errors
///
/// Returns when validation fails, the adapter cannot provide complete unit
/// replacement, a bind/payload/request budget is exceeded, or DDL/DML cannot
/// be applied.
pub async fn restore_backup_unit(
    db: &DatabaseConnection,
    repo: &BackupRepository,
    unit: &BackupUnit,
    kind: CanonicalRestoreKind,
    opts: &CanonicalRestoreOpts,
    preserve_plugin_registry: bool,
) -> Result<()> {
    if !opts.atomic_unit_restore {
        return Err(LibraryError::Schema(
            "database adapter does not advertise atomicUnitRestore; \
             restore of this backend is unsupported"
                .into(),
        ));
    }
    verify_unit(repo, unit)?;
    let schema = load_admitted_schema(repo, unit)?;
    let identity = load_identity(repo, unit)?;
    let extra_drop = if kind == CanonicalRestoreKind::PluginBinding {
        match plugin_canonical_schema_from_ddl_catalog(db).await {
            Ok(live) => live.table_names(),
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("no durable canonical DDL catalog")
                    || msg.contains(&format!("plugin backup cannot read `{SQL_DDL_TABLE}`"))
                {
                    if binding_has_user_tables(db).await? {
                        return Err(LibraryError::Schema(format!(
                            "plugin database has no durable canonical DDL catalog (`{SQL_DDL_TABLE}`); \
                             reset or recreate the binding — Bookclerk will not merge captured schema \
                             onto an unknown existing database"
                        )));
                    }
                    Vec::new()
                } else {
                    return Err(err);
                }
            }
        }
    } else {
        Vec::new()
    };
    let backend = db.get_database_backend();
    // SQLite `PRAGMA foreign_keys` is per physical connection. Do not toggle it on
    // `db`: a pooled adapter may run OFF/ON on a different connection than `begin()`,
    // leaking OFF onto later writes or leaving the restore txn with FKs still ON.
    // Defer checks on the restore transaction, then `PRAGMA foreign_key_check`
    // before commit. Changing `foreign_keys` inside an open transaction is a no-op.
    let txn = db.begin().await.map_err(LibraryError::from_db_err)?;
    if backend == DbBackend::Sqlite {
        exec_sql(&txn, backend, "PRAGMA defer_foreign_keys = ON").await?;
    }
    match restore_unit_on(
        &txn,
        repo,
        unit,
        &schema,
        &identity,
        kind,
        opts,
        &extra_drop,
        preserve_plugin_registry,
    )
    .await
    {
        Ok(()) => {
            if backend == DbBackend::Sqlite {
                if let Err(err) = sqlite_assert_no_fk_violations(&txn, backend).await {
                    let _ = txn.rollback().await;
                    return Err(err);
                }
            }
            txn.commit().await.map_err(LibraryError::from_db_err)?;
            Ok(())
        }
        Err(err) => {
            let _ = txn.rollback().await;
            Err(err)
        }
    }
}

/// Replay one unit onto `conn`: drop owned schema, apply DDL, load rows, restore identity.
#[allow(clippy::too_many_arguments)]
async fn restore_unit_on<C>(
    conn: &C,
    repo: &BackupRepository,
    unit: &BackupUnit,
    schema: &CanonicalDatabaseSchema,
    identity: &BTreeMap<String, IdentityHighWater>,
    kind: CanonicalRestoreKind,
    opts: &CanonicalRestoreOpts,
    extra_drop: &[String],
    preserve_plugin_registry: bool,
) -> Result<()>
where
    C: ConnectionTrait,
{
    let backend = conn.get_database_backend();
    let preserve_registry = kind == CanonicalRestoreKind::Library
        && preserve_plugin_registry
        && table_exists(conn, backend, "plugin_databases").await?;
    drop_owned_schema(conn, backend, schema, kind, extra_drop, preserve_registry).await?;
    for table in &schema.tables {
        if preserve_registry && is_registry_table(&table.parsed.table) {
            continue;
        }
        apply_canonical_ddl(conn, backend, &table.create_sql, kind).await?;
    }
    let ordered = sort_tables_by_foreign_keys(schema.tables.clone())?;
    for table in &ordered {
        if preserve_registry && is_registry_table(&table.parsed.table) {
            continue;
        }
        restore_table_rows(conn, repo, unit, table, opts).await?;
    }
    for index in &schema.indexes {
        if preserve_registry && is_registry_table(&index.table) {
            continue;
        }
        apply_canonical_ddl(conn, backend, &index.canonical_sql, kind).await?;
    }
    restore_identity(conn, backend, identity, opts).await?;
    Ok(())
}

/// Insert every chunk of `table` using parameterized typed `DbValue` cells.
async fn restore_table_rows<C>(
    conn: &C,
    repo: &BackupRepository,
    unit: &BackupUnit,
    table: &CanonicalTableSchema,
    opts: &CanonicalRestoreOpts,
) -> Result<()>
where
    C: ConnectionTrait,
{
    let name = table.parsed.table.as_str();
    let Some(meta) = unit.tables.iter().find(|t| t.name == name) else {
        if is_registry_table(name) {
            return Ok(());
        }
        return Err(LibraryError::Schema(format!(
            "backup unit is missing table `{name}` (empty tables still require a BackupTable entry)"
        )));
    };
    let backend = conn.get_database_backend();
    for digest in &meta.chunks {
        let CanonicalObject::TableChunk { columns, rows, .. } = repo.get_object(digest)? else {
            return Err(LibraryError::Schema(format!(
                "backup chunk `{digest}` is not table data"
            )));
        };
        for col in &columns {
            if !ident_ok(col) {
                return Err(LibraryError::Schema(format!(
                    "restore refuses unsafe column `{name}.{col}`"
                )));
            }
        }
        if !ident_ok(name) {
            return Err(LibraryError::Schema(format!(
                "restore refuses unsafe table `{name}`"
            )));
        }
        for row in rows {
            let insert = insert_sql(name, &columns, row.len())?;
            exec_bound(conn, backend, opts, &insert, row).await?;
        }
    }
    Ok(())
}

/// Decode the identity object for a unit, or reject a mismatched object kind.
fn load_identity(
    repo: &BackupRepository,
    unit: &BackupUnit,
) -> Result<BTreeMap<String, IdentityHighWater>> {
    match repo.get_object(&unit.identity_object)? {
        CanonicalObject::Identity { entries } => Ok(entries),
        _ => Err(LibraryError::Schema(
            "backup identity object is not Identity".into(),
        )),
    }
}

/// Fail restore when the open SQLite transaction has foreign-key violations.
async fn sqlite_assert_no_fk_violations<C>(conn: &C, backend: DbBackend) -> Result<()>
where
    C: ConnectionTrait,
{
    let rows = conn
        .query_all_raw(Statement::from_string(
            backend,
            "PRAGMA foreign_key_check".to_string(),
        ))
        .await
        .map_err(LibraryError::from_db_err)?;
    if rows.is_empty() {
        return Ok(());
    }
    let table = rows[0]
        .try_get_by_index::<String>(0)
        .unwrap_or_else(|_| "unknown".into());
    Err(LibraryError::Schema(format!(
        "restore left foreign-key violations (e.g. table `{table}`)"
    )))
}

/// Drop indexes then tables owned by `schema` (optionally keeping `plugin_databases`).
async fn drop_owned_schema<C>(
    conn: &C,
    backend: DbBackend,
    schema: &CanonicalDatabaseSchema,
    kind: CanonicalRestoreKind,
    extra_drop: &[String],
    preserve_registry: bool,
) -> Result<()>
where
    C: ConnectionTrait,
{
    let mut names: Vec<String> = schema.table_names();
    if kind == CanonicalRestoreKind::Library {
        if let Ok(latest) = library_canonical_schema() {
            for n in latest.table_names() {
                if !names.iter().any(|e| e == &n) {
                    names.push(n);
                }
            }
        }
    }
    for extra in extra_drop {
        if !names.iter().any(|e| e == extra) {
            names.push(extra.clone());
        }
    }
    if kind == CanonicalRestoreKind::PluginBinding {
        for reserved in [SQL_CATALOG_TABLE, SQL_SCHEMA_TABLE, SQL_DDL_TABLE] {
            if !names.iter().any(|e| e == reserved) {
                names.push(reserved.to_string());
            }
        }
    }
    if !names.iter().any(|e| e == SQL_IDENTITY_TABLE) {
        names.push(SQL_IDENTITY_TABLE.to_string());
    }
    names.retain(|n| !(preserve_registry && is_registry_table(n)));
    let drop = if backend == DbBackend::Postgres {
        "CASCADE"
    } else {
        ""
    };
    for name in names.iter().rev() {
        if LIBRARY_SKIP_TABLES.contains(&name.as_str()) && preserve_registry {
            continue;
        }
        if !ident_ok(name) {
            return Err(LibraryError::Schema(format!(
                "refusing to drop unsafe identifier `{name}` during backup restore"
            )));
        }
        let sql = format!("DROP TABLE IF EXISTS {name} {drop}");
        exec_sql(conn, backend, sql.trim()).await?;
    }
    if backend == DbBackend::Postgres {
        for table in schema.tables.iter().rev() {
            if table.parsed.identity_column.is_some() {
                let fn_name =
                    bookclerk_plugin_abi::postgres_identity_function_name(&table.parsed.table);
                exec_sql(
                    conn,
                    backend,
                    &format!("DROP FUNCTION IF EXISTS {fn_name}() CASCADE"),
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Execute one admitted CREATE TABLE / CREATE INDEX statement plus companions.
async fn apply_canonical_ddl<C>(
    conn: &C,
    backend: DbBackend,
    canonical: &str,
    kind: CanonicalRestoreKind,
) -> Result<()>
where
    C: ConnectionTrait,
{
    if !statement_is_admitted(canonical) {
        return Err(LibraryError::Schema(format!(
            "restore refuses SQL that is not admitted Bookclerk DDL: `{canonical}`"
        )));
    }
    let lowered = bookclerk_db_exec::schema_sql_for_backend(backend, canonical);
    exec_sql(conn, backend, lowered.as_ref()).await?;
    let companions = match kind {
        CanonicalRestoreKind::Library => {
            if backend == DbBackend::Postgres {
                bookclerk_db_exec::postgres_identity_companions(canonical)
            } else {
                Vec::new()
            }
        }
        CanonicalRestoreKind::PluginBinding => {
            bookclerk_db_exec::binding_companions(backend, canonical)
        }
    };
    for sql in companions {
        exec_sql(conn, backend, &sql).await?;
    }
    Ok(())
}

/// Restore identity high-water values using parameterized adapter-specific SQL.
async fn restore_identity<C>(
    conn: &C,
    backend: DbBackend,
    identity: &BTreeMap<String, IdentityHighWater>,
    opts: &CanonicalRestoreOpts,
) -> Result<()>
where
    C: ConnectionTrait,
{
    for (table, hw) in identity {
        if !ident_ok(table) || !ident_ok(&hw.column) {
            return Err(LibraryError::Schema(format!(
                "unsafe identity identifier `{table}.{}`",
                hw.column
            )));
        }
        let table_val = DbValue::Text(table.clone());
        let last_val = DbValue::Int64(hw.last);
        match backend {
            DbBackend::Postgres => {
                let sql = format!(
                    "INSERT INTO {SQL_IDENTITY_TABLE} (table_name, last) VALUES (?, ?) \
                     ON CONFLICT (table_name) DO UPDATE SET last = GREATEST({SQL_IDENTITY_TABLE}.last, EXCLUDED.last)"
                );
                exec_bound(conn, backend, opts, &sql, vec![table_val, last_val]).await?;
            }
            DbBackend::Sqlite => {
                let _ = exec_bound(
                    conn,
                    backend,
                    opts,
                    "DELETE FROM sqlite_sequence WHERE name = ?",
                    vec![table_val.clone()],
                )
                .await;
                if hw.last > 0 {
                    exec_bound(
                        conn,
                        backend,
                        opts,
                        "INSERT INTO sqlite_sequence(name, seq) VALUES (?, ?)",
                        vec![table_val, last_val],
                    )
                    .await?;
                }
            }
            _ => {
                if table_exists(conn, backend, SQL_IDENTITY_TABLE).await? {
                    let sql = format!(
                        "INSERT INTO {SQL_IDENTITY_TABLE} (table_name, last) VALUES (?, ?)"
                    );
                    exec_bound(conn, backend, opts, &sql, vec![table_val, last_val]).await?;
                } else if hw.last > 0 {
                    return Err(LibraryError::Schema(
                        "backup restore cannot apply identity high-water on this adapter \
                         (no bookclerk_identity table)"
                            .into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Build a single-row INSERT with positional `?` placeholders.
fn insert_sql(table: &str, columns: &[String], n: usize) -> Result<String> {
    if n != columns.len() {
        return Err(LibraryError::Schema(format!(
            "restore row for `{table}` has {n} cells; expected {}",
            columns.len()
        )));
    }
    if n == 0 {
        return Err(LibraryError::Schema(format!(
            "restore refuses empty INSERT into `{table}`"
        )));
    }
    let placeholders = vec!["?"; n].join(", ");
    Ok(format!(
        "INSERT INTO {table} ({}) VALUES ({placeholders})",
        columns.join(", ")
    ))
}

/// True when `name` is the library plugin-database registry table.
fn is_registry_table(name: &str) -> bool {
    LIBRARY_SKIP_TABLES
        .iter()
        .any(|t| t.eq_ignore_ascii_case(name))
}

/// True when the binding has any non-sqlite user table (catalog missing + tables
/// means restore would merge).
async fn binding_has_user_tables(db: &DatabaseConnection) -> Result<bool> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DbBackend::Postgres => {
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'"
        }
        _ => "SELECT name FROM sqlite_master WHERE type = 'table'",
    };
    let rows = db
        .query_all_raw(Statement::from_string(backend, sql))
        .await
        .map_err(LibraryError::from_db_err)?;
    for row in rows {
        let name = match backend {
            DbBackend::Postgres => cell_text(&row, "table_name")?,
            _ => cell_text(&row, "name")?,
        };
        if name.starts_with("sqlite_") {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

/// Applies admitted canonical DDL plus binding companions (tests / bootstrap).
///
/// # Errors
///
/// Returns when a statement fails.
pub async fn apply_admitted_sql(
    db: &DatabaseConnection,
    statements: &[&str],
    kind: CanonicalRestoreKind,
) -> Result<()> {
    let backend = db.get_database_backend();
    for sql in statements {
        apply_canonical_ddl(db, backend, sql, kind).await?;
    }
    Ok(())
}

/// True when `sql` is admitted `CREATE TABLE` or `CREATE INDEX` (tests).
#[must_use]
pub fn statement_is_admitted(sql: &str) -> bool {
    parse_create_table_schema(sql).is_some() || parse_create_index_sql(sql).is_some()
}
