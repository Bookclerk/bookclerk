//! Native [`ExecuteRequest`] execution (no JSON `DbAtomicRequest` conversion).
//!
//! [`DbValue::Text`] stays a string even when the payload starts with `b64:`.
//! [`DbValue::Bytes`] maps to SeaORM bytes. Typed nulls use the matching
//! SeaORM `Value::…(None)` variant so column types survive the round trip.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use bookclerk_plugin_abi::{
    apply_schema_action_to_env, apply_schema_sql_to_env, assert_proof_matches_sql,
    catalog_companions_for_action, encoded_execute_reply_bytes, encoded_statement_result_bytes,
    parse_create_table_schema, require_portable_text, require_portable_text_binds,
    sql_host_bookkeeping_type_env, typecheck_execute_request_proofs, ResolvedStatement,
    SchemaAction,
};
use bookclerk_plugin_abi::{
    sql_catalog_page_rows, DbColumn, DbPlanStatementKind, DbResultSelection, DbRow, DbTiming,
    DbType, DbValue, ExecuteReply, ExecuteRequest, SqlType, SqlTypeEnv, StatementResult,
    TypedDbStatement, FIRST_PARTY_MAX_RESULT_ROWS, SQL_CATALOG_TABLE, SQL_SCHEMA_TABLE,
};
use bookclerk_plugin_abi::{GuestReceiptPersist, HostExecuteEnvelope};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbErr, QueryResult, Statement,
    TransactionTrait, Value as SeaValue,
};

use crate::exec::{
    collect_capped_query_results, exceeds_result_row_cap, remaining_deadline_ms,
    rows_affected_for_kind, AtomicSession, ExecCaps,
};
use crate::proxy_txn::{
    consume_commit_injection, consume_savepoint_release_injection,
    consume_savepoint_rollback_injection, is_txn_broken, note_commit_failed,
    suspend_execute_row_cap, take_txn_fault, with_exec_budget, AtomicInterruptPhase, ExecBudget,
};
use crate::schema_postgres::expand_host_schema_execute_request;
use crate::{
    cap_query_sql, record_query_rows_seen, set_positional_result_columns,
    take_positional_result_columns,
};
use crate::{lower_canonical_sql, lower_canonical_sql_typed};

/// Proven row bound for one statement: `maxRows` when set, otherwise the
/// negotiated adapter cap. Zero on either side means "unlimited".
fn effective_row_cap(stmt_max: u32, caps_max: u32) -> u32 {
    match (stmt_max, caps_max) {
        (0, c) => c,
        (s, 0) => s,
        (s, c) => s.min(c),
    }
}

/// Runs adapter-private catalog + identity companions after a binding DDL statement.
///
/// Companions are generated from **canonical** SQL and executed internally
/// (not extra Cap'n statements).
///
/// # Errors
///
/// Returns [`DbErr`] when a companion statement fails to execute.
async fn apply_binding_companions(
    txn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
    canonical: &str,
    action: &SchemaAction,
) -> Result<(), DbErr> {
    let mut companions = catalog_companions_for_action(canonical, Some(action));
    if backend == sea_orm::DatabaseBackend::Postgres {
        match action {
            SchemaAction::Create { noop: true, .. } => {}
            SchemaAction::None => {}
            _ => companions.extend(
                crate::schema_postgres::postgres_identity_companions_for_action(
                    canonical,
                    Some(action),
                ),
            ),
        }
    }
    for companion in companions {
        txn.execute_raw(Statement::from_string(backend, companion))
            .await?;
    }
    Ok(())
}

/// Reconstructs [`SqlTypeEnv`] from the durable binding catalog, if present.
///
/// A missing catalog table yields an empty environment. RPC/adapter/shape/type
/// errors fail closed. On PostgreSQL the probe runs under a savepoint so a
/// missing-table `SELECT` cannot abort the current transaction (`25P02`).
///
/// # Errors
///
/// Returns [`DbErr`] when the catalog query fails for a reason other than a
/// missing table, or when a catalog cell is malformed.
pub async fn load_sql_type_env(conn: &impl ConnectionTrait) -> Result<SqlTypeEnv, DbErr> {
    load_sql_type_env_capped(conn, FIRST_PARTY_MAX_RESULT_ROWS).await
}

/// [`load_sql_type_env`] paging at `min(max_result_rows, FIRST_PARTY_MAX_RESULT_ROWS)`.
///
/// # Errors
///
/// Returns [`DbErr`] when the catalog query fails for a reason other than a
/// missing table, or when a catalog cell is malformed.
pub async fn load_sql_type_env_capped(
    conn: &impl ConnectionTrait,
    max_result_rows: u32,
) -> Result<SqlTypeEnv, DbErr> {
    let _guard = suspend_execute_row_cap();
    let backend = conn.get_database_backend();
    let savepoint = backend == sea_orm::DatabaseBackend::Postgres
        && conn
            .execute_raw(Statement::from_string(
                backend,
                "SAVEPOINT bookclerk_sql_catalog_load",
            ))
            .await
            .is_ok();
    let loaded =
        load_sql_type_env_paged(conn, backend, sql_catalog_page_rows(max_result_rows)).await;
    match loaded {
        Ok(env) => {
            if savepoint {
                let _ = conn
                    .execute_raw(Statement::from_string(
                        backend,
                        "RELEASE SAVEPOINT bookclerk_sql_catalog_load",
                    ))
                    .await;
            }
            Ok(env)
        }
        Err(err) => {
            if savepoint {
                let _ = conn
                    .execute_raw(Statement::from_string(
                        backend,
                        "ROLLBACK TO SAVEPOINT bookclerk_sql_catalog_load",
                    ))
                    .await;
                let _ = conn
                    .execute_raw(Statement::from_string(
                        backend,
                        "RELEASE SAVEPOINT bookclerk_sql_catalog_load",
                    ))
                    .await;
            }
            if catalog_missing_table(&err) {
                return Ok(SqlTypeEnv::new());
            }
            Err(err)
        }
    }
}

fn catalog_missing_table(err: &DbErr) -> bool {
    bookclerk_plugin_abi::reserved_catalog_relation_missing(&err.to_string(), SQL_CATALOG_TABLE)
}

fn schema_missing_table(err: &DbErr) -> bool {
    bookclerk_plugin_abi::reserved_catalog_relation_missing(&err.to_string(), SQL_SCHEMA_TABLE)
}

/// # Errors
///
/// Returns [`DbErr`] when a catalog page query fails or a cell is malformed.
async fn load_sql_type_env_paged(
    conn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
    page: u32,
) -> Result<SqlTypeEnv, DbErr> {
    let mut env = SqlTypeEnv::new();
    let mut cursor_table = String::new();
    let mut cursor_ord: i64 = -1;
    loop {
        let sql = lower_canonical_sql(
            backend,
            &format!(
                "SELECT table_name, column_name, sql_type, ordinal, is_identity, default_sql \
                 FROM {SQL_CATALOG_TABLE} \
                 WHERE table_name > ? OR (table_name = ? AND ordinal > ?) \
                 ORDER BY table_name, ordinal LIMIT {page}"
            ),
        );
        let rows = conn
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                sql,
                [
                    SeaValue::String(Some(cursor_table.clone())),
                    SeaValue::String(Some(cursor_table.clone())),
                    SeaValue::BigInt(Some(cursor_ord)),
                ],
            ))
            .await?;
        if rows.is_empty() {
            break;
        }
        let n = rows.len();
        for row in rows {
            let table = query_result_text(&row, "table_name");
            let column = query_result_text(&row, "column_name");
            let ty = query_result_text(&row, "sql_type");
            if table.is_empty() || column.is_empty() {
                return Err(DbErr::Custom(
                    "bookclerk_sql_catalog row is missing table_name or column_name".into(),
                ));
            }
            let Some(sql_ty) = SqlType::from_column_ident(ty.to_ascii_lowercase().as_str()) else {
                return Err(DbErr::Custom(format!(
                    "bookclerk_sql_catalog has unknown sql_type {ty}"
                )));
            };
            let ordinal = query_result_i64(&row, "ordinal").ok_or_else(|| {
                DbErr::Custom("bookclerk_sql_catalog row is missing ordinal".into())
            })?;
            env.insert_column(&table, &column, sql_ty);
            cursor_table = table;
            cursor_ord = ordinal;
            let _ = column;
        }
        if n < usize::try_from(page).unwrap_or(usize::MAX) {
            break;
        }
    }
    let mut schema_cursor = String::new();
    loop {
        let schema_sql = lower_canonical_sql(
            backend,
            &format!(
                "SELECT table_name, fingerprint, identity_column FROM {SQL_SCHEMA_TABLE} \
                 WHERE table_name > ? ORDER BY table_name LIMIT {page}"
            ),
        );
        match conn
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                schema_sql,
                [SeaValue::String(Some(schema_cursor.clone()))],
            ))
            .await
        {
            Ok(rows) => {
                if rows.is_empty() {
                    break;
                }
                let n = rows.len();
                for row in rows {
                    let table = query_result_text(&row, "table_name");
                    let fingerprint = query_result_text(&row, "fingerprint");
                    let identity = query_result_text(&row, "identity_column");
                    if table.is_empty() || fingerprint.is_empty() {
                        return Err(DbErr::Custom(
                            "bookclerk_sql_schema row is missing table_name or fingerprint".into(),
                        ));
                    }
                    let cols = env.table_columns(&table).unwrap_or(&[]).to_vec();
                    env.insert_table_schema(
                        table.clone(),
                        cols,
                        if identity.is_empty() {
                            None
                        } else {
                            Some(identity)
                        },
                        fingerprint,
                    );
                    schema_cursor = table;
                }
                if n < usize::try_from(page).unwrap_or(usize::MAX) {
                    break;
                }
            }
            Err(err) if schema_missing_table(&err) => break,
            Err(err) => return Err(err),
        }
    }
    Ok(env)
}

/// Live physical tables on this connection (host execute only).
///
/// Plugin bindings type against [`SQL_CATALOG_TABLE`], not sqlite_master.
/// Host plans may query tables created outside the canonical host snapshot
/// (tests, `PRAGMA`/raw DDL); those columns are merged here so typing still
/// runs.
///
/// # Errors
///
/// Returns [`DbErr`] when the physical catalog query fails.
pub async fn load_physical_sql_type_env(conn: &impl ConnectionTrait) -> Result<SqlTypeEnv, DbErr> {
    let _guard = suspend_execute_row_cap();
    let backend = conn.get_database_backend();
    if backend == sea_orm::DatabaseBackend::Postgres {
        load_physical_postgres(conn, backend).await
    } else {
        load_physical_sqlite(conn, backend).await
    }
}

/// Catalog snapshot for typed execute.
///
/// Plugin bindings (`session.type_env` empty) see only [`SQL_CATALOG_TABLE`].
/// Host sessions merge live physical tables, then the canonical host schema,
/// so ad-hoc host test tables typecheck without adopting plugin orphans.
///
/// New atomic batches snapshot this in autocommit **before** `BEGIN` so SQLite
/// does not hold a reserved lock across paged catalog/physical loads. Nested
/// execute on an open transaction still snapshots inside that transaction.
///
/// # Errors
///
/// Returns [`DbErr`] when the catalog or physical snapshot cannot be loaded.
async fn catalog_env_for_typed(
    conn: &impl ConnectionTrait,
    session: &AtomicSession,
) -> Result<SqlTypeEnv, DbErr> {
    let catalog = load_sql_type_env(conn).await?;
    if session.type_env.is_empty() {
        return Ok(catalog);
    }
    let mut env = load_physical_sql_type_env(conn).await?;
    env.merge(&catalog);
    env.merge(&session.type_env);
    Ok(env)
}

/// # Errors
///
/// Returns [`DbErr`] when `sqlite_master` cannot be queried.
async fn load_physical_sqlite(
    conn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
) -> Result<SqlTypeEnv, DbErr> {
    let rows = conn
        .query_all_raw(Statement::from_string(
            backend,
            "SELECT name, sql FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%'"
                .to_string(),
        ))
        .await?;
    let mut env = SqlTypeEnv::new();
    for row in rows {
        let name = query_result_text(&row, "name");
        let ddl = query_result_text(&row, "sql");
        if name.is_empty() || !portable_physical_ident(&name) {
            continue;
        }
        if let Some(schema) = parse_create_table_schema(&ddl) {
            env.insert_table(schema.table, schema.columns);
            continue;
        }
        match sqlite_pragma_columns(conn, backend, &name).await {
            Ok(cols) if !cols.is_empty() => env.insert_table(name, cols),
            Ok(_) => {}
            Err(err) => {
                return Err(DbErr::Custom(format!(
                    "unavailable: declared types for {name} could not be loaded: {err}"
                )));
            }
        }
    }
    Ok(env)
}

fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// # Errors
///
/// Returns [`DbErr`] when `PRAGMA table_info` fails.
async fn sqlite_pragma_columns(
    conn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
    table: &str,
) -> Result<Vec<(String, SqlType)>, DbErr> {
    let sql = format!(
        "SELECT name, type FROM pragma_table_info({})",
        sql_string_literal(table)
    );
    let rows = conn
        .query_all_raw(Statement::from_string(backend, sql))
        .await?;
    let mut cols = Vec::with_capacity(rows.len());
    for row in rows {
        let name = query_result_text(&row, "name");
        let ty_name = query_result_text(&row, "type");
        if name.is_empty() {
            continue;
        }
        let Some(ty) = declared_sql_type(&ty_name) else {
            continue;
        };
        cols.push((name, ty));
    }
    Ok(cols)
}

fn portable_physical_ident(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= bookclerk_plugin_abi::SQL_V1_MAX_IDENT_BYTES
        && matches!(b[0], b'A'..=b'Z' | b'a'..=b'z' | b'_')
        && b[1..]
            .iter()
            .all(|c| matches!(c, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn declared_sql_type(ty: &str) -> Option<SqlType> {
    let t = ty.trim().to_ascii_lowercase();
    if let Some(parsed) = SqlType::from_column_ident(&t) {
        return Some(parsed);
    }
    let head = t.split(|c: char| c == '(' || c.is_whitespace()).next()?;
    SqlType::from_column_ident(head)
}

/// # Errors
///
/// Returns [`DbErr`] when `pg_catalog` cannot be queried.
async fn load_physical_postgres(
    conn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
) -> Result<SqlTypeEnv, DbErr> {
    let sql = "SELECT c.relname::text AS table_name, a.attname::text AS column_name, \
               t.typname::text AS data_type \
               FROM pg_catalog.pg_class c \
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
               JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
               JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
               WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') \
               AND a.attnum > 0 AND NOT a.attisdropped \
               ORDER BY c.relname, a.attnum";
    let rows = conn
        .query_all_raw(Statement::from_string(backend, sql.to_string()))
        .await?;
    let mut env = SqlTypeEnv::new();
    for row in rows {
        let table = query_result_text(&row, "table_name");
        let column = query_result_text(&row, "column_name");
        let ty_name = query_result_text(&row, "data_type");
        if table.is_empty()
            || column.is_empty()
            || !portable_physical_ident(&table)
            || !portable_physical_ident(&column)
        {
            continue;
        }
        let Some(ty) = postgres_typname_to_sql_type(&ty_name) else {
            continue;
        };
        env.insert_column(&table, &column, ty);
    }
    Ok(env)
}

/// # Errors
///
/// Returns [`DbErr`] when a stamped proof is not bound to its SQL, or when
/// typecheck fails for an unstamped pre-admission request.
fn proofs_for_request(
    catalog: &SqlTypeEnv,
    req: &ExecuteRequest,
    stamped: &[ResolvedStatement],
    require_stamped: bool,
) -> Result<Vec<ResolvedStatement>, DbErr> {
    if require_stamped {
        if stamped.len() != req.statements.len() {
            return Err(DbErr::Custom(
                "host execute envelope proofs must match statement count".into(),
            ));
        }
        for (stmt, proof) in req.statements.iter().zip(stamped.iter()) {
            assert_proof_matches_sql(proof, stmt.sql.trim())
                .map_err(|err| DbErr::Custom(err.to_string()))?;
        }
        return Ok(stamped.to_vec());
    }
    if stamped.len() == req.statements.len() && !stamped.is_empty() {
        for (stmt, proof) in req.statements.iter().zip(stamped.iter()) {
            assert_proof_matches_sql(proof, stmt.sql.trim())
                .map_err(|err| DbErr::Custom(err.to_string()))?;
        }
        return Ok(stamped.to_vec());
    }
    if !stamped.is_empty() {
        return Err(DbErr::Custom(
            "host execute envelope proofs must match statement count".into(),
        ));
    }
    let env = type_env_with_bookkeeping(catalog);
    typecheck_execute_request_proofs(req, &env).map_err(|err| DbErr::Custom(err.to_string()))
}

fn type_env_with_bookkeeping(catalog: &SqlTypeEnv) -> SqlTypeEnv {
    let mut env = sql_host_bookkeeping_type_env();
    env.merge(catalog);
    env
}

/// # Errors
///
/// Returns [`DbErr`] when the physical table is missing, is an uncatalogued
/// orphan, or does not match the catalog fingerprint.
async fn reconcile_physical(
    txn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
    action: &SchemaAction,
    env: &SqlTypeEnv,
) -> Result<(), DbErr> {
    match action {
        SchemaAction::Create {
            schema,
            fingerprint,
            noop,
        } => {
            let table = schema.table.as_str();
            let exists = physical_table_exists(txn, backend, table).await?;
            if *noop {
                if !exists {
                    return Err(DbErr::Custom(format!(
                        "binding catalog lists table {table} but the physical table is missing"
                    )));
                }
            } else if exists {
                return Err(DbErr::Custom(format!(
                    "physical table {table} exists without catalog metadata; refusing to adopt it"
                )));
            }
            if exists {
                if let Some(physical) = physical_table_fingerprint(txn, backend, table).await? {
                    if physical != *fingerprint {
                        return Err(DbErr::Custom(format!(
                            "catalog fingerprint for {table} does not match the physical schema"
                        )));
                    }
                } else if let Some(physical_cols) =
                    physical_table_columns(txn, backend, table).await?
                {
                    let Some(catalog_cols) = env.table_columns(table) else {
                        return Err(DbErr::Custom(format!(
                            "catalog fingerprint for {table} cannot be compared to the physical schema"
                        )));
                    };
                    if !physical_columns_match(catalog_cols, &physical_cols) {
                        return Err(DbErr::Custom(format!(
                            "catalog fingerprint for {table} does not match the physical schema"
                        )));
                    }
                }
            }
            Ok(())
        }
        SchemaAction::Drop { table } => {
            let _ = table;
            Ok(())
        }
        SchemaAction::None => Ok(()),
    }
}

fn physical_columns_match(catalog: &[(String, SqlType)], physical: &[(String, SqlType)]) -> bool {
    catalog.len() == physical.len()
        && catalog
            .iter()
            .zip(physical.iter())
            .all(|((a, ta), (b, tb))| a == b && ta == tb)
}

/// # Errors
///
/// Returns [`DbErr`] when the existence probe query fails.
async fn physical_table_exists(
    txn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
    table: &str,
) -> Result<bool, DbErr> {
    let sql = if backend == sea_orm::DatabaseBackend::Postgres {
        format!(
            "SELECT 1 FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') \
             AND c.relname = {}",
            sql_string_literal(&table.to_ascii_lowercase())
        )
    } else {
        format!(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = {}",
            sql_string_literal(table)
        )
    };
    let rows = txn
        .query_all_raw(Statement::from_string(backend, sql))
        .await?;
    Ok(!rows.is_empty())
}

/// # Errors
///
/// Returns [`DbErr`] when `sqlite_master` cannot be queried.
async fn physical_table_fingerprint(
    txn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
    table: &str,
) -> Result<Option<String>, DbErr> {
    if backend == sea_orm::DatabaseBackend::Postgres {
        return Ok(None);
    }
    let sql = format!(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = {}",
        sql_string_literal(table)
    );
    let rows = txn
        .query_all_raw(Statement::from_string(backend, sql))
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let ddl = query_result_text(row, "sql");
    Ok(parse_create_table_schema(&ddl).map(|s| s.fingerprint()))
}

/// # Errors
///
/// Returns [`DbErr`] when the PostgreSQL attribute catalog cannot be queried.
async fn physical_table_columns(
    txn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
    table: &str,
) -> Result<Option<Vec<(String, SqlType)>>, DbErr> {
    if backend != sea_orm::DatabaseBackend::Postgres {
        return Ok(None);
    }
    let sql = format!(
        "SELECT a.attname::text AS column_name, t.typname::text AS data_type \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
         WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') \
         AND c.relname = {} AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
        sql_string_literal(&table.to_ascii_lowercase())
    );
    let rows = txn
        .query_all_raw(Statement::from_string(backend, sql))
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut cols = Vec::with_capacity(rows.len());
    for row in rows {
        let name = query_result_text(&row, "column_name").to_ascii_lowercase();
        let ty_name = query_result_text(&row, "data_type");
        let Some(ty) = postgres_typname_to_sql_type(&ty_name) else {
            return Err(DbErr::Custom(format!(
                "physical column {table}.{name} has non-v1 type {ty_name}"
            )));
        };
        cols.push((name, ty));
    }
    Ok(Some(cols))
}

fn postgres_typname_to_sql_type(ty: &str) -> Option<SqlType> {
    match ty.to_ascii_lowercase().as_str() {
        "int2" | "int4" | "int8" | "integer" | "bigint" | "smallint" => Some(SqlType::Integer),
        "float4" | "float8" | "real" | "double precision" => Some(SqlType::Real),
        "text" | "varchar" | "bpchar" | "name" => Some(SqlType::Text),
        "bytea" => Some(SqlType::Blob),
        "bool" | "boolean" => Some(SqlType::Boolean),
        _ => SqlType::from_column_ident(ty),
    }
}

/// Reads a TEXT catalog cell by column name (ProxyRow maps are not SELECT-ordered).
fn query_result_text(row: &QueryResult, column: &str) -> String {
    row.try_get::<String>("", column)
        .or_else(|_| {
            row.try_get::<Option<String>>("", column)
                .map(|s| s.unwrap_or_default())
        })
        .or_else(|_| row.try_get::<String>(SQL_CATALOG_TABLE, column))
        .or_else(|_| {
            row.try_get::<Option<String>>(SQL_CATALOG_TABLE, column)
                .map(|s| s.unwrap_or_default())
        })
        .unwrap_or_default()
}

fn query_result_i64(row: &QueryResult, column: &str) -> Option<i64> {
    row.try_get::<i64>("", column)
        .ok()
        .or_else(|| row.try_get::<i32>("", column).ok().map(i64::from))
        .or_else(|| row.try_get::<Option<i64>>("", column).ok().flatten())
        .or_else(|| {
            row.try_get::<Option<i32>>("", column)
                .ok()
                .flatten()
                .map(i64::from)
        })
}

/// Convert a typed bind into a SeaORM value without JSON / `b64:` decoding.
#[must_use]
pub fn db_value_to_sea(value: &DbValue) -> SeaValue {
    match value {
        DbValue::Null(DbType::Unspecified | DbType::Text) => SeaValue::String(None),
        DbValue::Null(DbType::Int64) => SeaValue::BigInt(None),
        DbValue::Null(DbType::Float64) => SeaValue::Double(None),
        DbValue::Null(DbType::Bytes) => SeaValue::Bytes(None),
        DbValue::Null(DbType::Bool) => SeaValue::Bool(None),
        DbValue::Text(s) => SeaValue::String(Some(s.clone())),
        DbValue::Int64(n) => SeaValue::BigInt(Some(*n)),
        DbValue::Float64(n) => SeaValue::Double(Some(*n)),
        DbValue::Bytes(b) => SeaValue::Bytes(Some(b.clone())),
        DbValue::Boolean(b) => SeaValue::Bool(Some(*b)),
    }
}

/// Reconstruct a typed cell from a SeaORM value.
///
/// Unlike the JSON guest path, this preserves engine types: a BLOB column
/// becomes [`DbValue::Bytes`], and `b64:` text is left as [`DbValue::Text`].
///
/// # Errors
///
/// Returns when the SeaORM value is outside the universal domain, or TEXT
/// contains U+0000.
pub fn db_value_from_sea(v: &SeaValue) -> Result<DbValue, String> {
    match v {
        SeaValue::Bool(Some(b)) => Ok(DbValue::Boolean(*b)),
        SeaValue::TinyInt(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        SeaValue::SmallInt(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        SeaValue::Int(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        SeaValue::BigInt(Some(n)) => Ok(DbValue::Int64(*n)),
        SeaValue::TinyUnsigned(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        SeaValue::SmallUnsigned(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        SeaValue::Unsigned(Some(n)) => Ok(DbValue::Int64(i64::from(*n))),
        SeaValue::BigUnsigned(Some(n)) => i64::try_from(*n)
            .map(DbValue::Int64)
            .map_err(|_| format!("unsigned integer {n} overflows int64")),
        SeaValue::Float(Some(n)) => {
            let f = f64::from(*n);
            if !f.is_finite() {
                return Err("float64 value is not finite".into());
            }
            Ok(DbValue::Float64(f))
        }
        SeaValue::Double(Some(n)) => {
            if !n.is_finite() {
                return Err("float64 value is not finite".into());
            }
            Ok(DbValue::Float64(*n))
        }
        SeaValue::String(Some(s)) => {
            require_portable_text(s).map_err(|err| err.to_string())?;
            Ok(DbValue::Text(s.to_string()))
        }
        SeaValue::Char(Some(c)) => {
            let s = c.to_string();
            require_portable_text(&s).map_err(|err| err.to_string())?;
            Ok(DbValue::Text(s))
        }
        SeaValue::Bytes(Some(b)) => Ok(DbValue::Bytes(b.to_vec())),
        SeaValue::ChronoDateTimeUtc(Some(dt)) => Ok(DbValue::Text(dt.to_rfc3339())),
        SeaValue::ChronoDateTime(Some(dt)) => Ok(DbValue::Text(dt.and_utc().to_rfc3339())),
        SeaValue::ChronoDate(Some(d)) => Ok(DbValue::Text(d.to_string())),
        SeaValue::ChronoTime(Some(t)) => Ok(DbValue::Text(t.to_string())),
        SeaValue::ChronoDateTimeWithTimeZone(Some(dt)) => Ok(DbValue::Text(dt.to_rfc3339())),
        SeaValue::ChronoDateTimeLocal(Some(dt)) => Ok(DbValue::Text(dt.to_rfc3339())),
        SeaValue::Uuid(Some(u)) => Ok(DbValue::Text(u.to_string())),
        SeaValue::Json(Some(_)) => Err("json is not a baseline DbValue".into()),
        SeaValue::Enum(_) => Err("enums are not a baseline DbValue".into()),
        SeaValue::Array(_, _) => Err("arrays are not a baseline DbValue".into()),
        SeaValue::Bool(None) => Ok(DbValue::Null(DbType::Bool)),
        SeaValue::TinyInt(None)
        | SeaValue::SmallInt(None)
        | SeaValue::Int(None)
        | SeaValue::BigInt(None)
        | SeaValue::TinyUnsigned(None)
        | SeaValue::SmallUnsigned(None)
        | SeaValue::Unsigned(None)
        | SeaValue::BigUnsigned(None) => Ok(DbValue::Null(DbType::Int64)),
        SeaValue::Float(None) | SeaValue::Double(None) => Ok(DbValue::Null(DbType::Float64)),
        SeaValue::Bytes(None) => Ok(DbValue::Null(DbType::Bytes)),
        SeaValue::String(None)
        | SeaValue::Char(None)
        | SeaValue::ChronoDateTimeUtc(None)
        | SeaValue::ChronoDateTime(None)
        | SeaValue::ChronoDate(None)
        | SeaValue::ChronoTime(None)
        | SeaValue::ChronoDateTimeWithTimeZone(None)
        | SeaValue::ChronoDateTimeLocal(None)
        | SeaValue::Json(None)
        | SeaValue::Uuid(None) => Ok(DbValue::Null(DbType::Text)),
    }
}

/// `DbType` of a non-null cell (typed nulls keep their declared type).
fn db_type_of(v: &DbValue) -> DbType {
    match v {
        DbValue::Null(ty) => *ty,
        DbValue::Boolean(_) => DbType::Bool,
        DbValue::Int64(_) => DbType::Int64,
        DbValue::Float64(_) => DbType::Float64,
        DbValue::Text(_) => DbType::Text,
        DbValue::Bytes(_) => DbType::Bytes,
    }
}

/// UTF-8 / blob byte length of a cell (0 for scalars).
fn db_value_cell_len(v: &DbValue) -> usize {
    match v {
        DbValue::Text(s) => s.len(),
        DbValue::Bytes(b) => b.len(),
        _ => 0,
    }
}

/// Builds a positional [`StatementResult`] from engine [`QueryResult`]s.
///
/// Column names and declared types come from rusqlite/SQLite metadata when the
/// adapter recorded them ([`take_positional_result_columns`]), otherwise from
/// the first engine row (Postgres `type_info`, else `column_names`). Duplicate
/// names are rejected here, before any name-keyed map conversion. Empty
/// Postgres `SELECT`s record metadata via a one-row probe first.
///
/// # Errors
///
/// Returns when a row exceeds the result cap, a cell exceeds `max_cell_bytes`,
/// the encoded statement exceeds `max_result_bytes`, a SeaORM value is outside
/// the universal domain, or column names are duplicated.
fn statement_result_from_query_results(
    engine_rows: &[QueryResult],
    kind: DbPlanStatementKind,
    caps: ExecCaps,
    row_cap: u32,
) -> Result<StatementResult, DbErr> {
    if exceeds_result_row_cap(engine_rows.len(), row_cap) {
        return Err(DbErr::Custom(format!(
            "query returned {} rows; maxRows/maxResultRows is {row_cap}",
            engine_rows.len(),
        )));
    }
    let mut db_columns = take_positional_result_columns().unwrap_or_else(|| {
        engine_rows
            .first()
            .map(db_columns_from_engine_row)
            .unwrap_or_default()
    });
    reject_duplicate_column_names(&db_columns)?;
    let mut db_rows = Vec::with_capacity(engine_rows.len());
    for engine in engine_rows {
        let values = db_values_from_query_result(engine, &db_columns)?;
        if caps.max_cell_bytes > 0 {
            let cap = usize::try_from(caps.max_cell_bytes).unwrap_or(usize::MAX);
            for (col, cell) in db_columns.iter().zip(values.iter()) {
                let n = db_value_cell_len(cell);
                if n > cap {
                    return Err(DbErr::Custom(format!(
                        "column `{}` is {n} bytes; maxCellBytes is {}",
                        col.name, caps.max_cell_bytes
                    )));
                }
            }
        }
        db_rows.push(DbRow { values });
    }
    for (i, col) in db_columns.iter_mut().enumerate() {
        if col.db_type != DbType::Unspecified {
            continue;
        }
        for row in &db_rows {
            if let Some(cell) = row.values.get(i) {
                if !matches!(cell, DbValue::Null(_)) {
                    col.db_type = db_type_of(cell);
                    break;
                }
            }
        }
        if col.db_type == DbType::Unspecified {
            if let Some(DbValue::Null(ty)) = db_rows.first().and_then(|r| r.values.get(i)) {
                col.db_type = *ty;
            }
        }
    }
    let mut result = StatementResult::from_rows(db_columns, db_rows).map_err(DbErr::Custom)?;
    result.rows_affected = rows_affected_for_kind(kind, result.rows.len());
    reject_statement_result_bytes(&result, caps.max_result_bytes)?;
    Ok(result)
}

/// Records column names/types for a Postgres `SELECT` that returned no rows.
///
/// sqlx only materializes `RowDescription` on a `PgRow`. Extended-query
/// `prepare` (Parse + Describe, no Execute) returns the same field metadata
/// without running the statement, so volatile functions and data-modifying
/// CTEs are not evaluated a second time.
///
/// # Errors
///
/// Returns when the driver cannot describe the statement.
async fn record_postgres_empty_result_columns(
    db: &DatabaseConnection,
    sql: &str,
) -> Result<(), DbErr> {
    use sea_orm::sqlx::{AssertSqlSafe, Column, Executor, SqlSafeStr, Statement, TypeInfo};
    let pool = db.get_postgres_connection_pool();
    let prepared = pool
        .prepare(AssertSqlSafe(sql.to_owned()).into_sql_str())
        .await
        .map_err(|err| DbErr::Custom(format!("postgres prepare/describe: {err}")))?;
    let columns = prepared
        .columns()
        .iter()
        .map(|c| DbColumn {
            name: c.name().to_string(),
            db_type: db_type_from_pg_type_name(c.type_info().name()),
        })
        .collect();
    set_positional_result_columns(columns);
    Ok(())
}

/// Positional [`DbColumn`]s from one engine row (Postgres OIDs when present).
fn db_columns_from_engine_row(row: &QueryResult) -> Vec<DbColumn> {
    if let Some(pg) = row.try_as_pg_row() {
        use sea_orm::sqlx::{Column, Row, TypeInfo};
        return pg
            .columns()
            .iter()
            .map(|c| DbColumn {
                name: c.name().to_string(),
                db_type: db_type_from_pg_type_name(c.type_info().name()),
            })
            .collect();
    }
    row.column_names()
        .into_iter()
        .map(|name| DbColumn {
            name,
            db_type: DbType::Unspecified,
        })
        .collect()
}

/// Maps a sqlx Postgres `TypeInfo::name` onto the universal [`DbType`].
fn db_type_from_pg_type_name(name: &str) -> DbType {
    match name {
        "BOOL" => DbType::Bool,
        "INT2" | "INT4" | "INT8" | "SMALLINT" | "INT" | "INTEGER" | "BIGINT" | "SMALLSERIAL"
        | "SERIAL" | "BIGSERIAL" | "OID" => DbType::Int64,
        "FLOAT4" | "FLOAT8" | "REAL" | "DOUBLE PRECISION" => DbType::Float64,
        "BYTEA" => DbType::Bytes,
        "TEXT" | "VARCHAR" | "NAME" | "BPCHAR" | "CHAR" | "CSTRING" | "UNKNOWN" => DbType::Text,
        _ => DbType::Unspecified,
    }
}

/// Fails when two positional columns share a name.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when a duplicate column name is present.
fn reject_duplicate_column_names(columns: &[DbColumn]) -> Result<(), DbErr> {
    let mut seen = HashSet::new();
    for col in columns {
        if !col.name.is_empty() && !seen.insert(col.name.as_str()) {
            return Err(DbErr::Custom(format!(
                "duplicate column name `{}`",
                col.name
            )));
        }
    }
    Ok(())
}

/// Fails when the Cap'n-encoded [`StatementResult`] exceeds `max_result_bytes`.
///
/// # Errors
///
/// Returns when the encoded result cannot be serialized (including TEXT
/// U+0000) or exceeds `max_result_bytes`.
fn reject_statement_result_bytes(
    result: &StatementResult,
    max_result_bytes: u32,
) -> Result<(), DbErr> {
    let used = encoded_statement_result_bytes(result)
        .map_err(|err| DbErr::Custom(err.to_string()))?
        .len();
    if max_result_bytes == 0 {
        return Ok(());
    }
    let cap = usize::try_from(max_result_bytes).unwrap_or(usize::MAX);
    if used > cap {
        return Err(DbErr::Custom(format!(
            "query result is {used} bytes; maxResultBytes is {max_result_bytes}"
        )));
    }
    Ok(())
}

/// Rejects BookclerkSQL TEXT that contains U+0000 (BYTES may contain 0x00).
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when `stmt.sql` or a TEXT bind contains U+0000.
fn reject_nonportable_statement(index: usize, stmt: &TypedDbStatement) -> Result<(), DbErr> {
    require_portable_text(&stmt.sql)
        .map_err(|err| DbErr::Custom(format!("statement {index}: {err}")))?;
    require_portable_text_binds(&stmt.parameters)
        .map_err(|err| DbErr::Custom(format!("statement {index}: {err}")))?;
    Ok(())
}

/// Rejects U+0000 in every statement SQL string and TEXT bind of `req`.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when any statement SQL or TEXT bind contains
/// U+0000.
fn reject_nonportable_text(req: &ExecuteRequest) -> Result<(), DbErr> {
    for (i, stmt) in req.statements.iter().enumerate() {
        reject_nonportable_statement(i, stmt)?;
    }
    Ok(())
}

/// Positional cells from one engine row (proxy rows looked up by recorded name).
///
/// # Errors
///
/// Returns [`DbErr`] when a column is missing or a cell cannot be decoded.
fn db_values_from_query_result(
    row: &QueryResult,
    columns: &[DbColumn],
) -> Result<Vec<DbValue>, DbErr> {
    if let Some(proxy) = row.try_as_proxy_row() {
        let mut values = Vec::with_capacity(columns.len());
        for col in columns {
            let sea = proxy.values.get(&col.name).ok_or_else(|| {
                DbErr::Custom(format!("result row missing column `{}`", col.name))
            })?;
            values.push(db_value_for_column(sea, col)?);
        }
        return Ok(values);
    }
    let mut values = Vec::with_capacity(columns.len());
    for (i, col) in columns.iter().enumerate() {
        let sea = sea_value_from_index(row, i, col.db_type).map_err(|err| {
            DbErr::Custom(format!(
                "{err}; column `{}` declared {:?}",
                col.name, col.db_type
            ))
        })?;
        values.push(db_value_for_column(&sea, col)?);
    }
    Ok(values)
}

/// Converts a SeaORM cell, normalizing against the declared column type.
///
/// Declared metadata decides the observable variant (typed NULLs, `Boolean`
/// for `0`/`1` in BOOL columns) so every adapter reports identical
/// `DbValue`s — see
/// [`bookclerk_plugin_abi::normalize_db_value_for_column`].
///
/// # Errors
///
/// Returns [`DbErr`] when the SeaORM value is outside the universal domain.
fn db_value_for_column(sea: &SeaValue, col: &DbColumn) -> Result<DbValue, DbErr> {
    let value = db_value_from_sea(sea).map_err(DbErr::Custom)?;
    Ok(bookclerk_plugin_abi::normalize_db_value_for_column(
        value,
        col.db_type,
    ))
}

/// Decodes one positional cell without going through a name-keyed map.
///
/// `Option<T>` succeeds for every SQL NULL, so the declared [`DbType`] is tried
/// first. Untyped nulls stay `Null(Unspecified)` rather than the first match
/// (`Bytes`).
///
/// # Errors
///
/// Returns [`DbErr`] when the cell cannot be decoded for any preferred type.
fn sea_value_from_index(row: &QueryResult, idx: usize, prefer: DbType) -> Result<SeaValue, DbErr> {
    let order: &[DbType] = match prefer {
        DbType::Bytes => &[
            DbType::Bytes,
            DbType::Int64,
            DbType::Float64,
            DbType::Text,
            DbType::Bool,
        ],
        DbType::Int64 | DbType::Bool => &[
            DbType::Int64,
            DbType::Bool,
            DbType::Float64,
            DbType::Text,
            DbType::Bytes,
        ],
        DbType::Float64 => &[
            DbType::Float64,
            DbType::Int64,
            DbType::Text,
            DbType::Bytes,
            DbType::Bool,
        ],
        DbType::Text | DbType::Unspecified => &[
            DbType::Int64,
            DbType::Float64,
            DbType::Text,
            DbType::Bool,
            DbType::Bytes,
        ],
    };
    let mut saw_null = false;
    for ty in order {
        match ty {
            DbType::Bytes => {
                if let Ok(v) = row.try_get_by_index::<Option<Vec<u8>>>(idx) {
                    if v.is_some() {
                        return Ok(SeaValue::Bytes(v));
                    }
                    saw_null = true;
                }
            }
            DbType::Int64 => {
                if let Ok(v) = row.try_get_by_index::<Option<i64>>(idx) {
                    if v.is_some() {
                        return Ok(SeaValue::BigInt(v));
                    }
                    saw_null = true;
                }
                if let Ok(v) = row.try_get_by_index::<Option<i32>>(idx) {
                    if let Some(n) = v {
                        return Ok(SeaValue::BigInt(Some(i64::from(n))));
                    }
                    saw_null = true;
                }
                if let Ok(v) = row.try_get_by_index::<Option<i16>>(idx) {
                    if let Some(n) = v {
                        return Ok(SeaValue::BigInt(Some(i64::from(n))));
                    }
                    saw_null = true;
                }
            }
            DbType::Float64 => {
                if let Ok(v) = row.try_get_by_index::<Option<f64>>(idx) {
                    if v.is_some() {
                        return Ok(SeaValue::Double(v));
                    }
                    saw_null = true;
                }
            }
            DbType::Text => {
                if let Ok(v) = row.try_get_by_index::<Option<String>>(idx) {
                    if v.is_some() {
                        return Ok(SeaValue::String(v));
                    }
                    saw_null = true;
                }
            }
            DbType::Bool => {
                if let Ok(v) = row.try_get_by_index::<Option<bool>>(idx) {
                    if v.is_some() {
                        return Ok(SeaValue::Bool(v));
                    }
                    saw_null = true;
                }
            }
            DbType::Unspecified => {}
        }
    }
    if saw_null {
        return Ok(match prefer {
            DbType::Bytes => SeaValue::Bytes(None),
            DbType::Int64 | DbType::Bool => SeaValue::BigInt(None),
            DbType::Float64 => SeaValue::Double(None),
            DbType::Text | DbType::Unspecified => SeaValue::String(None),
        });
    }
    Err(DbErr::Custom(format!(
        "column {idx} is outside the universal DbValue domain"
    )))
}

/// Run a typed atomic batch on an existing SeaORM connection.
///
/// Encodes the [`ExecuteReply`] **before** COMMIT. Encoding or result-budget
/// failures roll the transaction back.
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails, the encoded reply exceeds
/// `max_atomic_result_bytes`, TEXT contains U+0000, or the session is
/// interrupted.
pub async fn execute_typed_on_session(
    db: &DatabaseConnection,
    req: &ExecuteRequest,
    guest_receipt: GuestReceiptPersist,
    timing_source: &str,
    caps: impl Into<ExecCaps>,
    session: AtomicSession,
) -> Result<ExecuteReply, DbErr> {
    execute_typed_on_session_proofs(
        db,
        req,
        guest_receipt,
        &[],
        timing_source,
        caps,
        session,
        false,
    )
    .await
}

/// [`execute_typed_on_session`] using host-private proofs already stamped on `envelope`.
///
/// Envelope proofs are required 1:1 with `envelope.request.statements`. Catalog
/// changes after admission must not rebuild lowering. Pre-admission public
/// execute uses [`execute_typed_on_session`], which may typecheck.
///
/// # Errors
///
/// Returns [`DbErr`] when proofs are missing or mismatched, a statement fails,
/// the encoded reply exceeds `max_atomic_result_bytes`, or the session is
/// interrupted.
pub async fn execute_typed_envelope(
    db: &DatabaseConnection,
    envelope: &HostExecuteEnvelope,
    timing_source: &str,
    caps: impl Into<ExecCaps>,
    session: AtomicSession,
) -> Result<ExecuteReply, DbErr> {
    if envelope.proofs.len() != envelope.request.statements.len() {
        return Err(DbErr::Custom(format!(
            "host execute envelope proofs must match statement count ({} proofs, {} statements)",
            envelope.proofs.len(),
            envelope.request.statements.len()
        )));
    }
    execute_typed_on_session_proofs(
        db,
        &envelope.request,
        envelope.guest_receipt.clone(),
        &envelope.proofs,
        timing_source,
        caps,
        session,
        true,
    )
    .await
}

/// # Errors
///
/// Returns [`DbErr`] when a statement fails, encoding fails, or COMMIT fails.
#[allow(clippy::too_many_arguments)]
async fn execute_typed_on_session_proofs(
    db: &DatabaseConnection,
    req: &ExecuteRequest,
    guest_receipt: GuestReceiptPersist,
    stamped: &[ResolvedStatement],
    timing_source: &str,
    caps: impl Into<ExecCaps>,
    session: AtomicSession,
    require_stamped: bool,
) -> Result<ExecuteReply, DbErr> {
    reject_nonportable_text(req)?;
    if guest_receipt.is_absent() {
        let caps = caps.into();
        session.check(AtomicInterruptPhase::BeforeBegin)?;
        let budget = ExecBudget::new(session.deadline_unix_ms, caps.max_result_rows);
        let seen_budget = Arc::clone(&budget);
        let result = with_exec_budget(Arc::clone(&budget), || {
            execute_typed_body(
                db,
                req,
                timing_source,
                caps,
                session,
                None::<fn(ExecuteReply) -> Result<Vec<TypedDbStatement>, DbErr>>,
                None,
                stamped,
                require_stamped,
            )
        })
        .await;
        record_query_rows_seen(seen_budget.rows_seen());
        return result;
    }
    let hint = guest_receipt;
    let guest_hash = hint.guest_request_hash.clone();
    execute_typed_on_session_then_proofs(
        db,
        req,
        timing_source,
        caps,
        session,
        move |partial| {
            crate::guest_receipt::guest_receipt_finalize_stmts(
                &partial,
                usize::try_from(hint.guest_statement_len).unwrap_or(usize::MAX),
                &hint.guest_request_hash,
            )
        },
        Some(guest_hash),
        stamped,
        require_stamped,
    )
    .await
}

/// Like [`execute_typed_on_session`], running extra statements in the same transaction
/// before COMMIT (used to persist guest replay payloads on `db_atomic_receipts`).
///
/// `guest_hash` is the guest `requestHash` used to decide whether a claimed
/// prior receipt should resume remaining guest SQL instead of skipping it.
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails, `then` fails, encoding fails, or COMMIT fails.
pub async fn execute_typed_on_session_then<F>(
    db: &DatabaseConnection,
    req: &ExecuteRequest,
    timing_source: &str,
    caps: impl Into<ExecCaps>,
    session: AtomicSession,
    then: F,
    guest_hash: Option<String>,
) -> Result<ExecuteReply, DbErr>
where
    F: FnOnce(ExecuteReply) -> Result<Vec<TypedDbStatement>, DbErr>,
{
    execute_typed_on_session_then_proofs(
        db,
        req,
        timing_source,
        caps,
        session,
        then,
        guest_hash,
        &[],
        false,
    )
    .await
}

/// # Errors
///
/// Returns [`DbErr`] when a statement fails, `then` fails, encoding fails, or
/// COMMIT fails.
#[allow(clippy::too_many_arguments)]
async fn execute_typed_on_session_then_proofs<F>(
    db: &DatabaseConnection,
    req: &ExecuteRequest,
    timing_source: &str,
    caps: impl Into<ExecCaps>,
    session: AtomicSession,
    then: F,
    guest_hash: Option<String>,
    stamped: &[ResolvedStatement],
    require_stamped: bool,
) -> Result<ExecuteReply, DbErr>
where
    F: FnOnce(ExecuteReply) -> Result<Vec<TypedDbStatement>, DbErr>,
{
    let caps = caps.into();
    session.check(AtomicInterruptPhase::BeforeBegin)?;
    let budget = ExecBudget::new(session.deadline_unix_ms, caps.max_result_rows);
    let seen_budget = Arc::clone(&budget);
    let result = with_exec_budget(Arc::clone(&budget), || {
        execute_typed_body(
            db,
            req,
            timing_source,
            caps,
            session,
            Some(then),
            guest_hash,
            stamped,
            require_stamped,
        )
    })
    .await;
    record_query_rows_seen(seen_budget.rows_seen());
    result
}

/// Run a typed batch on an already-open transaction (no BEGIN/COMMIT).
///
/// Used by nested SeaORM work: the guest interactive txn is already open, so
/// a second `executeAtomic` BEGIN would fail. The batch runs inside a
/// `SAVEPOINT`; statement, encoding, or budget failures roll back to that
/// savepoint so a later outer `commit()` cannot persist a partial batch.
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails or the encoded reply exceeds
/// `max_atomic_result_bytes`.
pub async fn execute_typed_on_txn(
    txn: &DatabaseTransaction,
    req: &ExecuteRequest,
    timing_source: &str,
    caps: impl Into<ExecCaps>,
    session: AtomicSession,
    describe: Option<&DatabaseConnection>,
) -> Result<ExecuteReply, DbErr> {
    execute_typed_on_txn_envelope(
        txn,
        &HostExecuteEnvelope::new(req.clone(), GuestReceiptPersist::default()),
        timing_source,
        caps,
        session,
        describe,
    )
    .await
}

/// [`execute_typed_on_txn`] with a host-only guest-receipt persist hint and proofs.
///
/// Finalize SQL runs inside the nested savepoint before returning, so an outer
/// `commit()` persists the caller-visible payload.
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails, finalize fails, or the encoded
/// reply exceeds `max_atomic_result_bytes`.
pub async fn execute_typed_on_txn_envelope(
    txn: &DatabaseTransaction,
    envelope: &HostExecuteEnvelope,
    timing_source: &str,
    caps: impl Into<ExecCaps>,
    session: AtomicSession,
    describe: Option<&DatabaseConnection>,
) -> Result<ExecuteReply, DbErr> {
    let caps = caps.into();
    if envelope.request.statements.is_empty() {
        return Err(DbErr::Custom(
            "executeAtomic statements must be non-empty".into(),
        ));
    }
    if !envelope.proofs.is_empty() && envelope.proofs.len() != envelope.request.statements.len() {
        return Err(DbErr::Custom(format!(
            "host execute envelope proofs must match statement count ({} proofs, {} statements)",
            envelope.proofs.len(),
            envelope.request.statements.len()
        )));
    }
    session.check(AtomicInterruptPhase::BetweenStatements)?;
    let budget = ExecBudget::new(session.deadline_unix_ms, caps.max_result_rows);
    let seen_budget = Arc::clone(&budget);
    let persist = envelope.guest_receipt.clone();
    let result = with_exec_budget(Arc::clone(&budget), || {
        nested_savepoint(txn, || {
            execute_typed_join_body(
                txn,
                describe,
                &envelope.request,
                timing_source,
                caps,
                session,
                persist,
                &envelope.proofs,
            )
        })
    })
    .await;
    record_query_rows_seen(seen_budget.rows_seen());
    result
}

/// Savepoint name for one nested `executeAtomic` on an open transaction.
const NESTED_ATOMIC_SAVEPOINT: &str = "bookclerk_nested_atomic";

/// Runs `f` inside `SAVEPOINT bookclerk_nested_atomic` and rolls back to it
/// on any error so a later outer commit cannot persist a partial batch.
///
/// # Errors
///
/// Returns [`DbErr`] when savepoint setup, `f`, or savepoint release fails.
async fn nested_savepoint<F, Fut, T>(txn: &DatabaseTransaction, f: F) -> Result<T, DbErr>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, DbErr>>,
{
    let backend = ConnectionTrait::get_database_backend(txn);
    txn.execute_raw(Statement::from_string(
        backend,
        format!("SAVEPOINT {NESTED_ATOMIC_SAVEPOINT}"),
    ))
    .await?;
    match f().await {
        Ok(value) => {
            let release_err = if consume_savepoint_release_injection() {
                Some(DbErr::Custom(
                    "database savepoint RELEASE failed: injected savepoint RELEASE failure".into(),
                ))
            } else {
                txn.execute_raw(Statement::from_string(
                    backend,
                    format!("RELEASE SAVEPOINT {NESTED_ATOMIC_SAVEPOINT}"),
                ))
                .await
                .err()
            };
            if let Some(err) = release_err {
                note_commit_failed(format!("database savepoint RELEASE failed: {err}"));
                let _ = txn
                    .execute_raw(Statement::from_string(
                        backend,
                        format!("ROLLBACK TO SAVEPOINT {NESTED_ATOMIC_SAVEPOINT}"),
                    ))
                    .await;
                return Err(err);
            }
            Ok(value)
        }
        Err(err) => {
            let rollback_err = if consume_savepoint_rollback_injection() {
                Some("injected savepoint ROLLBACK failure".to_string())
            } else {
                txn.execute_raw(Statement::from_string(
                    backend,
                    format!("ROLLBACK TO SAVEPOINT {NESTED_ATOMIC_SAVEPOINT}"),
                ))
                .await
                .err()
                .map(|e| e.to_string())
            };
            let release_err = if consume_savepoint_release_injection() {
                Some("injected savepoint RELEASE failure".to_string())
            } else {
                txn.execute_raw(Statement::from_string(
                    backend,
                    format!("RELEASE SAVEPOINT {NESTED_ATOMIC_SAVEPOINT}"),
                ))
                .await
                .err()
                .map(|e| e.to_string())
            };
            if rollback_err.is_none() && release_err.is_none() {
                return Err(err);
            }
            let cleanup = [rollback_err.as_deref(), release_err.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ");
            note_commit_failed(format!(
                "database savepoint cleanup failed after inner error: {cleanup}"
            ));
            Err(DbErr::Custom(format!("{err}; {cleanup}")))
        }
    }
}

/// Statement loop for [`execute_typed_on_txn`] (no COMMIT / ROLLBACK).
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails, encoding fails, or a result budget is exceeded.
#[allow(clippy::too_many_arguments)]
async fn execute_typed_join_body(
    txn: &DatabaseTransaction,
    describe: Option<&DatabaseConnection>,
    req: &ExecuteRequest,
    timing_source: &str,
    caps: ExecCaps,
    session: AtomicSession,
    guest_receipt: GuestReceiptPersist,
    stamped: &[ResolvedStatement],
) -> Result<ExecuteReply, DbErr> {
    reject_nonportable_text(req)?;
    let started = Instant::now();
    let backend = ConnectionTrait::get_database_backend(txn);
    let sql_started = Instant::now();
    let mut env = catalog_env_for_typed(txn, &session).await?;
    let require_stamped = !stamped.is_empty();
    let proofs = proofs_for_request(&env, req, stamped, require_stamped)?;
    let mut statements = Vec::with_capacity(req.statements.len());
    let skip_guest_on_prior = !guest_receipt.is_absent();
    for (stmt, proof) in req.statements.iter().zip(proofs.iter()) {
        session.check(AtomicInterruptPhase::BetweenStatements)?;
        let values: Vec<SeaValue> = stmt.parameters.iter().map(db_value_to_sea).collect();
        let row_cap = effective_row_cap(stmt.max_rows, caps.max_result_rows);
        let canonical = stmt.sql.clone();
        reconcile_physical(txn, backend, &proof.schema_action, &env).await?;
        let sql = if bookclerk_plugin_abi::statement_is_ddl(&canonical) {
            crate::schema_postgres::lower_binding_sql_for_backend(backend, &canonical).into_owned()
        } else {
            let lowered = lower_canonical_sql_typed(backend, canonical.trim(), Some(proof))
                .map_err(|err| DbErr::Custom(err.to_string()))?;
            if stmt.kind.wrap_select_limit() {
                cap_query_sql(&lowered, row_cap)
            } else {
                lowered
            }
        };
        let sea_stmt = Statement::from_sql_and_values(backend, &sql, values);
        let stmt_result = match stmt.result_selection {
            DbResultSelection::Rows => {
                if matches!(stmt.kind, DbPlanStatementKind::Execute) {
                    let exec = txn.execute_raw(sea_stmt).await?;
                    let result = StatementResult::from_affected(exec.rows_affected());
                    reject_statement_result_bytes(&result, caps.max_result_bytes)?;
                    result
                } else {
                    let engine_rows = collect_capped_query_results(txn, sea_stmt, row_cap).await?;
                    if engine_rows.is_empty()
                        && backend == sea_orm::DatabaseBackend::Postgres
                        && stmt.kind.wrap_select_limit()
                    {
                        if let Some(db) = describe {
                            record_postgres_empty_result_columns(db, &sql).await?;
                        }
                    }
                    statement_result_from_query_results(&engine_rows, stmt.kind, caps, row_cap)?
                }
            }
            DbResultSelection::AffectedRows | DbResultSelection::Discard => {
                let exec = txn.execute_raw(sea_stmt).await?;
                if matches!(stmt.result_selection, DbResultSelection::Discard) {
                    StatementResult::from_affected(0)
                } else {
                    let result = StatementResult::from_affected(exec.rows_affected());
                    reject_statement_result_bytes(&result, caps.max_result_bytes)?;
                    result
                }
            }
        };
        apply_binding_companions(txn, backend, &canonical, &proof.schema_action).await?;
        apply_schema_action_to_env(&mut env, &proof.schema_action);
        statements.push(stmt_result);
        if skip_guest_on_prior
            && crate::guest_receipt::should_skip_remaining_guest_work(
                &statements,
                req.statements.len(),
                &guest_receipt.guest_request_hash,
            )
        {
            crate::guest_receipt::pad_skipped_guest_results(&mut statements, req.statements.len());
            break;
        }
    }
    if !guest_receipt.is_absent() {
        let partial = ExecuteReply {
            operation_id: req.operation_id.clone(),
            statements: statements.clone(),
            timing: DbTiming {
                attempt_elapsed_us: 0,
                db_execution_us: 0,
                db_timing_source: timing_source.to_string(),
            },
        };
        let guest_len = usize::try_from(guest_receipt.guest_statement_len).unwrap_or(usize::MAX);
        for (i, stmt) in crate::guest_receipt::guest_receipt_finalize_stmts(
            &partial,
            guest_len,
            &guest_receipt.guest_request_hash,
        )?
        .into_iter()
        .enumerate()
        {
            session.check(AtomicInterruptPhase::BetweenStatements)?;
            reject_nonportable_statement(i, &stmt)?;
            let values: Vec<SeaValue> = stmt.parameters.iter().map(db_value_to_sea).collect();
            let sql = if bookclerk_plugin_abi::statement_is_ddl(&stmt.sql) {
                stmt.sql.clone()
            } else {
                lower_canonical_sql_typed(backend, &stmt.sql, None)
                    .map_err(|err| DbErr::Custom(err.to_string()))?
            };
            let sea_stmt = Statement::from_sql_and_values(backend, &sql, values);
            txn.execute_raw(sea_stmt).await?;
        }
    }
    session.check(AtomicInterruptPhase::AroundCommit)?;
    let db_execution_us = u64::try_from(sql_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let reply = ExecuteReply {
        operation_id: req.operation_id.clone(),
        statements,
        timing: DbTiming {
            attempt_elapsed_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            db_execution_us,
            db_timing_source: timing_source.to_string(),
        },
    };
    reply.validate_positional().map_err(DbErr::Custom)?;
    match encoded_execute_reply_bytes(&reply) {
        Ok(bytes) => {
            if caps.max_atomic_result_bytes > 0 {
                let used = bytes.len();
                let cap = usize::try_from(caps.max_atomic_result_bytes).unwrap_or(usize::MAX);
                if used > cap {
                    return Err(DbErr::Custom(format!(
                        "atomic result is {used} bytes; maxAtomicResultBytes is {}",
                        caps.max_atomic_result_bytes
                    )));
                }
            }
        }
        Err(err) => {
            return Err(DbErr::Custom(format!(
                "failed to encode ExecuteReply on open transaction: {err}"
            )));
        }
    }
    Ok(reply)
}

/// Transaction body for [`execute_typed_on_session`]: run, encode, then COMMIT.
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails, encoding fails, or COMMIT fails.
#[allow(clippy::too_many_arguments)]
async fn execute_typed_body<F>(
    db: &DatabaseConnection,
    req: &ExecuteRequest,
    timing_source: &str,
    caps: ExecCaps,
    session: AtomicSession,
    then: Option<F>,
    guest_hash: Option<String>,
    stamped: &[ResolvedStatement],
    require_stamped: bool,
) -> Result<ExecuteReply, DbErr>
where
    F: FnOnce(ExecuteReply) -> Result<Vec<TypedDbStatement>, DbErr>,
{
    if req.statements.is_empty() {
        return Err(DbErr::Custom(
            "executeAtomic statements must be non-empty".into(),
        ));
    }
    reject_nonportable_text(req)?;
    let started = Instant::now();
    let backend = ConnectionTrait::get_database_backend(db);
    // Host schema batches travel canonical; this adapter edge lowers/splits
    // them for the live backend and collapses the results back to the wire
    // request shape below.
    let wire_len = req.statements.len();
    let req = expand_host_schema_execute_request(backend, req);
    let canonical_sqls: Vec<String> = req.statements.iter().map(|s| s.sql.clone()).collect();
    // Binding CREATE/DROP stays canonical on the wire; Postgres adapters
    // lower types/`AUTOINCREMENT` here (not in `lower_canonical_sql`).
    let req = crate::schema_postgres::lower_binding_ddl_execute_request(backend, &req);
    let sql_started = Instant::now();
    let mut env = catalog_env_for_typed(db, &session).await?;
    let mut type_req = req.clone();
    for (stmt, canon) in type_req.statements.iter_mut().zip(canonical_sqls.iter()) {
        stmt.sql = canon.clone();
    }
    let proofs = proofs_for_request(&env, &type_req, stamped, require_stamped)?;
    let txn = db.begin().await?;
    if is_txn_broken() {
        let _ = txn.rollback().await;
        let fault = take_txn_fault().unwrap_or_else(|| "database begin failed".into());
        return Err(DbErr::Custom(fault));
    }
    if backend == sea_orm::DatabaseBackend::Postgres {
        if let Some(ms) = remaining_deadline_ms(session.deadline_unix_ms) {
            let sql = format!("SET LOCAL statement_timeout = '{ms}ms'");
            if let Err(err) = txn.execute_raw(Statement::from_string(backend, sql)).await {
                let _ = txn.rollback().await;
                return Err(err);
            }
        }
    }
    let reconcile = guest_hash.is_some();
    let mut statements = Vec::with_capacity(req.statements.len());
    // Guest-typed wrap only: host library plans also SELECT a prior receipt
    // at index 1, and skipping their remaining selectors would break replay.
    let skip_guest_on_prior = then.is_some();
    for (idx, stmt) in req.statements.iter().enumerate() {
        if let Err(err) = session.check(AtomicInterruptPhase::BetweenStatements) {
            let _ = txn.rollback().await;
            let _ = take_txn_fault();
            return Err(err);
        }
        let values: Vec<SeaValue> = stmt.parameters.iter().map(db_value_to_sea).collect();
        let row_cap = effective_row_cap(stmt.max_rows, caps.max_result_rows);
        let canonical = canonical_sqls
            .get(idx)
            .cloned()
            .unwrap_or_else(|| stmt.sql.clone());
        let proof = proofs.get(idx);
        if reconcile {
            if let Some(proof) = proof {
                if let Err(err) =
                    reconcile_physical(&txn, backend, &proof.schema_action, &env).await
                {
                    let _ = txn.rollback().await;
                    let _ = take_txn_fault();
                    return Err(err);
                }
            }
        }
        let sql = if bookclerk_plugin_abi::statement_is_ddl(&canonical) {
            stmt.sql.clone()
        } else {
            let lowered = match lower_canonical_sql_typed(backend, canonical.trim(), proof) {
                Ok(sql) => sql,
                Err(err) => {
                    let _ = txn.rollback().await;
                    let _ = take_txn_fault();
                    return Err(DbErr::Custom(err.to_string()));
                }
            };
            if stmt.kind.wrap_select_limit() {
                cap_query_sql(&lowered, row_cap)
            } else {
                lowered
            }
        };
        let sea_stmt = Statement::from_sql_and_values(backend, &sql, values);
        let stmt_result = match stmt.result_selection {
            DbResultSelection::Rows => {
                if matches!(stmt.kind, DbPlanStatementKind::Execute) {
                    let exec = match txn.execute_raw(sea_stmt).await {
                        Ok(exec) => exec,
                        Err(err) => {
                            let _ = txn.rollback().await;
                            let _ = take_txn_fault();
                            return Err(err);
                        }
                    };
                    let result = StatementResult::from_affected(exec.rows_affected());
                    if let Err(err) = reject_statement_result_bytes(&result, caps.max_result_bytes)
                    {
                        let _ = txn.rollback().await;
                        let _ = take_txn_fault();
                        return Err(err);
                    }
                    result
                } else {
                    let engine_rows =
                        match collect_capped_query_results(&txn, sea_stmt, row_cap).await {
                            Ok(rows) => rows,
                            Err(err) => {
                                let _ = txn.rollback().await;
                                let _ = take_txn_fault();
                                return Err(err);
                            }
                        };
                    if engine_rows.is_empty()
                        && backend == sea_orm::DatabaseBackend::Postgres
                        && stmt.kind.wrap_select_limit()
                    {
                        if let Err(err) = record_postgres_empty_result_columns(db, &sql).await {
                            let _ = txn.rollback().await;
                            let _ = take_txn_fault();
                            return Err(err);
                        }
                    }
                    match statement_result_from_query_results(
                        &engine_rows,
                        stmt.kind,
                        caps,
                        row_cap,
                    ) {
                        Ok(result) => result,
                        Err(err) => {
                            let _ = txn.rollback().await;
                            let _ = take_txn_fault();
                            return Err(err);
                        }
                    }
                }
            }
            DbResultSelection::AffectedRows | DbResultSelection::Discard => {
                let exec = match txn.execute_raw(sea_stmt).await {
                    Ok(exec) => exec,
                    Err(err) => {
                        let _ = txn.rollback().await;
                        let _ = take_txn_fault();
                        return Err(err);
                    }
                };
                if matches!(stmt.result_selection, DbResultSelection::Discard) {
                    StatementResult::from_affected(0)
                } else {
                    let result = StatementResult::from_affected(exec.rows_affected());
                    if let Err(err) = reject_statement_result_bytes(&result, caps.max_result_bytes)
                    {
                        let _ = txn.rollback().await;
                        let _ = take_txn_fault();
                        return Err(err);
                    }
                    result
                }
            }
        };
        if let Err(err) = apply_binding_companions(
            &txn,
            backend,
            &canonical,
            proof
                .map(|p| &p.schema_action)
                .unwrap_or(&SchemaAction::None),
        )
        .await
        {
            let _ = txn.rollback().await;
            let _ = take_txn_fault();
            return Err(err);
        }
        if let Some(proof) = proof {
            apply_schema_action_to_env(&mut env, &proof.schema_action);
        } else {
            apply_schema_sql_to_env(&mut env, &canonical);
        }
        statements.push(stmt_result);
        if skip_guest_on_prior
            && crate::guest_receipt::should_skip_remaining_guest_work(
                &statements,
                req.statements.len(),
                guest_hash.as_deref().unwrap_or(""),
            )
        {
            crate::guest_receipt::pad_skipped_guest_results(&mut statements, req.statements.len());
            break;
        }
    }
    let statements = crate::schema_postgres::collapse_host_schema_results(wire_len, statements);
    if let Some(then) = then {
        let partial = ExecuteReply {
            operation_id: req.operation_id.clone(),
            statements: statements.clone(),
            timing: DbTiming {
                attempt_elapsed_us: u64::try_from(started.elapsed().as_micros())
                    .unwrap_or(u64::MAX),
                db_execution_us: u64::try_from(sql_started.elapsed().as_micros())
                    .unwrap_or(u64::MAX),
                db_timing_source: timing_source.to_string(),
            },
        };
        for (i, stmt) in then(partial)?.into_iter().enumerate() {
            if let Err(err) = reject_nonportable_statement(i, &stmt) {
                let _ = txn.rollback().await;
                let _ = take_txn_fault();
                return Err(err);
            }
            if let Err(err) = session.check(AtomicInterruptPhase::BetweenStatements) {
                let _ = txn.rollback().await;
                let _ = take_txn_fault();
                return Err(err);
            }
            let values: Vec<SeaValue> = stmt.parameters.iter().map(db_value_to_sea).collect();
            let sql = if bookclerk_plugin_abi::statement_is_ddl(&stmt.sql) {
                stmt.sql.clone()
            } else {
                lower_canonical_sql_typed(backend, &stmt.sql, None)
                    .map_err(|err| DbErr::Custom(err.to_string()))?
            };
            let sea_stmt = Statement::from_sql_and_values(backend, &sql, values);
            match txn.execute_raw(sea_stmt).await {
                Ok(_) => {}
                Err(err) => {
                    let _ = txn.rollback().await;
                    let _ = take_txn_fault();
                    return Err(err);
                }
            }
        }
    }
    if consume_commit_injection() {
        let _ = txn.rollback().await;
        let _ = take_txn_fault();
        return Err(DbErr::Custom(
            "database commit failed: injected commit failure".into(),
        ));
    }
    if let Err(err) = session.check(AtomicInterruptPhase::AroundCommit) {
        let _ = txn.rollback().await;
        let _ = take_txn_fault();
        return Err(err);
    }
    let db_execution_us = u64::try_from(sql_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let reply = ExecuteReply {
        operation_id: req.operation_id.clone(),
        statements,
        timing: DbTiming {
            attempt_elapsed_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            db_execution_us,
            db_timing_source: timing_source.to_string(),
        },
    };
    if let Err(err) = reply.validate_positional() {
        let _ = txn.rollback().await;
        let _ = take_txn_fault();
        return Err(DbErr::Custom(err));
    }
    match encoded_execute_reply_bytes(&reply) {
        Ok(bytes) => {
            if caps.max_atomic_result_bytes > 0 {
                let used = bytes.len();
                let cap = usize::try_from(caps.max_atomic_result_bytes).unwrap_or(usize::MAX);
                if used > cap {
                    let _ = txn.rollback().await;
                    let _ = take_txn_fault();
                    return Err(DbErr::Custom(format!(
                        "atomic result is {used} bytes; maxAtomicResultBytes is {}",
                        caps.max_atomic_result_bytes
                    )));
                }
            }
        }
        Err(err) => {
            let _ = txn.rollback().await;
            let _ = take_txn_fault();
            return Err(DbErr::Custom(format!(
                "failed to encode ExecuteReply before COMMIT: {err}"
            )));
        }
    }
    txn.commit().await.map_err(|err| {
        let _ = take_txn_fault();
        DbErr::Custom(format!("database commit failed: {err}"))
    })?;
    if let Some(fault) = take_txn_fault() {
        return Err(DbErr::Custom(fault));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_page_sql_rewrites_placeholders_for_postgres() {
        let sql = format!(
            "SELECT table_name FROM {SQL_CATALOG_TABLE} \
             WHERE table_name > ? OR (table_name = ? AND ordinal > ?)"
        );
        let pg = lower_canonical_sql(sea_orm::DatabaseBackend::Postgres, &sql);
        assert!(
            pg.contains("$1") && pg.contains("$2") && pg.contains("$3"),
            "postgres catalog page SQL must not keep SQLite `?` (jsonb operator): {pg}"
        );
        assert!(
            !pg.contains('?'),
            "leftover `?` is a jsonb operator on postgres: {pg}"
        );
        let sqlite = lower_canonical_sql(sea_orm::DatabaseBackend::Sqlite, &sql);
        assert!(
            sqlite.contains('?'),
            "sqlite catalog page SQL keeps `?`: {sqlite}"
        );
    }

    #[test]
    fn text_starting_with_b64_stays_text() {
        let v = db_value_to_sea(&DbValue::Text("b64:AAAA".into()));
        match v {
            SeaValue::String(Some(s)) => assert_eq!(&*s, "b64:AAAA"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn bytes_stay_bytes() {
        let v = db_value_to_sea(&DbValue::Bytes(vec![0, 1, 2]));
        match v {
            SeaValue::Bytes(Some(b)) => assert_eq!(&*b, &[0, 1, 2]),
            other => panic!("expected Bytes, got {other:?}"),
        }
    }

    #[test]
    fn reject_duplicate_column_names_fails_closed() {
        let cols = [
            DbColumn {
                name: "n".into(),
                db_type: DbType::Int64,
            },
            DbColumn {
                name: "n".into(),
                db_type: DbType::Int64,
            },
        ];
        assert!(reject_duplicate_column_names(&cols).is_err());
    }

    #[test]
    fn typed_nulls_use_matching_sea_variants() {
        assert!(matches!(
            db_value_to_sea(&DbValue::Null(DbType::Bytes)),
            SeaValue::Bytes(None)
        ));
        assert!(matches!(
            db_value_to_sea(&DbValue::Null(DbType::Int64)),
            SeaValue::BigInt(None)
        ));
        assert!(matches!(
            db_value_to_sea(&DbValue::Null(DbType::Bool)),
            SeaValue::Bool(None)
        ));
        assert!(matches!(
            db_value_to_sea(&DbValue::Null(DbType::Float64)),
            SeaValue::Double(None)
        ));
        assert!(matches!(
            db_value_to_sea(&DbValue::Null(DbType::Text)),
            SeaValue::String(None)
        ));
    }

    #[test]
    fn typed_null_inherits_declared_column_type() {
        let col = DbColumn {
            name: "x".into(),
            db_type: DbType::Int64,
        };
        let v = db_value_for_column(&SeaValue::Bytes(None), &col).unwrap();
        assert!(matches!(v, DbValue::Null(DbType::Int64)));
    }

    #[test]
    fn postgres_type_names_map_onto_universal_db_type() {
        assert_eq!(db_type_from_pg_type_name("INT4"), DbType::Int64);
        assert_eq!(db_type_from_pg_type_name("INT8"), DbType::Int64);
        assert_eq!(db_type_from_pg_type_name("TEXT"), DbType::Text);
        assert_eq!(db_type_from_pg_type_name("BYTEA"), DbType::Bytes);
        assert_eq!(db_type_from_pg_type_name("BOOL"), DbType::Bool);
        assert_eq!(db_type_from_pg_type_name("FLOAT8"), DbType::Float64);
        assert_eq!(db_type_from_pg_type_name("INTERVAL"), DbType::Unspecified);
        assert_eq!(db_type_from_pg_type_name("NUMERIC"), DbType::Unspecified);
    }
}
