//! Streaming canonical capture from one consistent read view.

use std::collections::BTreeSet;

use bookclerk_plugin_abi::{
    encoded_statement_result_bytes, parse_create_index_sql, parse_create_table_schema,
    reserved_catalog_relation_missing, sql_ddl_create_table_sql, sql_schema_create_table_sql,
    sql_type_env_from_canonical_statements, typecheck_execute_request_proofs, DbColumn,
    DbPlanStatementKind, DbResultSelection, DbRow, DbType, DbValue, ExecuteRequest, SqlTypeEnv,
    StatementResult, TypedDbStatement, SQL_CATALOG_TABLE, SQL_CONTRACT_VERSION, SQL_DDL_TABLE,
    SQL_IDENTITY_TABLE, SQL_SCHEMA_TABLE,
};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, Statement,
    TransactionTrait,
};

use super::encode::{chunk_would_overflow, CanonicalObject};
use super::repository::BackupRepository;
use super::schema::{
    admit_canonical_statements, canonical_order_by_sql, library_canonical_schema_for_state,
    order_key_columns, sort_schema, sql_type_to_db_type,
};
use super::util::{cell_text, cell_to_db_value, ident_ok, int_cell};
use super::{
    BackupTable, BackupUnit, CanonicalDatabaseSchema, CanonicalExportOpts, CanonicalTableSchema,
    DatabaseUnitKind, IdentityHighWater, LIBRARY_SKIP_TABLES,
};
use crate::error::{LibraryError, Result};
use crate::host_schema::{schema_state_from_conn, verify_applied_checksums};
use crate::migrations::host_migration_plan;
use crate::schema_state::SchemaState;

/// Adapter-private catalog tables that must never appear in a portable backup.
const ADAPTER_PRIVATE_TABLES: &[&str] = &[
    SQL_CATALOG_TABLE,
    SQL_SCHEMA_TABLE,
    SQL_DDL_TABLE,
    SQL_IDENTITY_TABLE,
    "sqlite_sequence",
    "sqlite_master",
    "sqlite_schema",
    "sqlite_temp_master",
    "sqlite_temp_schema",
];

/// Captures the host library from `state`'s schema into `repo`.
///
/// Schema, rows, and identity are read inside one consistent view.
///
/// # Errors
///
/// Returns when the adapter cannot provide a consistent read, schema does not
/// match `state`, or paging/encoding fails.
pub async fn capture_library_unit(
    db: &DatabaseConnection,
    repo: &BackupRepository,
    state: &SchemaState,
    opts: &CanonicalExportOpts,
    backend_at_capture: &str,
) -> Result<BackupUnit> {
    let schema = library_canonical_schema_for_state(state)?;
    capture_unit(
        db,
        repo,
        &schema,
        opts,
        DatabaseUnitKind::Library,
        None,
        None,
        backend_at_capture,
        Some(state),
    )
    .await
}

/// Captures a plugin binding: DDL catalog, rows, and identity share one view.
///
/// # Errors
///
/// Returns when the DDL catalog is missing or capture fails.
pub async fn capture_plugin_unit(
    db: &DatabaseConnection,
    repo: &BackupRepository,
    opts: &CanonicalExportOpts,
    plugin_id: &str,
    binding: &str,
    backend_at_capture: &str,
) -> Result<BackupUnit> {
    if !opts.consistent_backup_read {
        return Err(LibraryError::Schema(
            "database adapter does not advertise consistentBackupRead; \
             backup of this backend is unsupported"
                .into(),
        ));
    }
    match db.get_database_backend() {
        DbBackend::Postgres => {
            let txn = begin_repeatable_read(db).await?;
            let result =
                capture_plugin_on(&txn, repo, opts, plugin_id, binding, backend_at_capture).await;
            finish_txn(txn, result.is_ok()).await?;
            result
        }
        _ => {
            let txn = db.begin().await.map_err(LibraryError::from_db_err)?;
            let result =
                capture_plugin_on(&txn, repo, opts, plugin_id, binding, backend_at_capture).await;
            finish_txn(txn, result.is_ok()).await?;
            result
        }
    }
}

/// Capture plugin schema, rows, and identity from an already-open view.
async fn capture_plugin_on<C>(
    conn: &C,
    repo: &BackupRepository,
    opts: &CanonicalExportOpts,
    plugin_id: &str,
    binding: &str,
    backend_at_capture: &str,
) -> Result<BackupUnit>
where
    C: ConnectionTrait,
{
    let schema = plugin_canonical_schema_from_ddl_catalog_on(conn, opts).await?;
    capture_unit_on(
        conn,
        repo,
        &schema,
        opts,
        DatabaseUnitKind::PluginBinding,
        Some(plugin_id.to_string()),
        Some(binding.to_string()),
        backend_at_capture,
        None,
    )
    .await
}

/// Open a consistent capture transaction, then export `schema` into `repo`.
#[allow(clippy::too_many_arguments)]
async fn capture_unit(
    db: &DatabaseConnection,
    repo: &BackupRepository,
    schema: &CanonicalDatabaseSchema,
    opts: &CanonicalExportOpts,
    kind: DatabaseUnitKind,
    plugin_id: Option<String>,
    binding: Option<String>,
    backend_at_capture: &str,
    expected_state: Option<&SchemaState>,
) -> Result<BackupUnit> {
    if !opts.consistent_backup_read {
        return Err(LibraryError::Schema(
            "database adapter does not advertise consistentBackupRead; \
             backup of this backend is unsupported"
                .into(),
        ));
    }
    match db.get_database_backend() {
        DbBackend::Postgres => {
            let txn = begin_repeatable_read(db).await?;
            let result = capture_unit_on(
                &txn,
                repo,
                schema,
                opts,
                kind,
                plugin_id,
                binding,
                backend_at_capture,
                expected_state,
            )
            .await;
            finish_txn(txn, result.is_ok()).await?;
            result
        }
        _ => {
            let txn = db.begin().await.map_err(LibraryError::from_db_err)?;
            let result = capture_unit_on(
                &txn,
                repo,
                schema,
                opts,
                kind,
                plugin_id,
                binding,
                backend_at_capture,
                expected_state,
            )
            .await;
            finish_txn(txn, result.is_ok()).await?;
            result
        }
    }
}

/// Commit a successful capture transaction, or roll it back on failure.
async fn finish_txn(txn: DatabaseTransaction, commit: bool) -> Result<()> {
    if commit {
        txn.commit().await.map_err(LibraryError::from_db_err)?;
    } else {
        let _ = txn.rollback().await;
    }
    Ok(())
}

/// Begin a PostgreSQL REPEATABLE READ transaction for a stable read view.
async fn begin_repeatable_read(db: &DatabaseConnection) -> Result<DatabaseTransaction> {
    let txn = db.begin().await.map_err(LibraryError::from_db_err)?;
    txn.execute_raw(Statement::from_string(
        DbBackend::Postgres,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    ))
    .await
    .map_err(LibraryError::from_db_err)?;
    Ok(txn)
}

/// Export admitted schema, paged table chunks, and identity on `conn`.
#[allow(clippy::too_many_arguments)]
async fn capture_unit_on<C>(
    conn: &C,
    repo: &BackupRepository,
    schema: &CanonicalDatabaseSchema,
    opts: &CanonicalExportOpts,
    kind: DatabaseUnitKind,
    plugin_id: Option<String>,
    binding: Option<String>,
    backend_at_capture: &str,
    expected_state: Option<&SchemaState>,
) -> Result<BackupUnit>
where
    C: ConnectionTrait,
{
    if let Some(expected) = expected_state {
        let live = schema_state_from_conn(conn).await?;
        if &live != expected {
            return Err(LibraryError::Schema(format!(
                "schema state changed during backup capture (expected {}, found {})",
                expected.display(),
                live.display()
            )));
        }
        let through = match &live {
            SchemaState::Unreleased { base_version, .. } => *base_version,
            SchemaState::Frozen { version, .. } => *version,
            SchemaState::Uninitialized => 0,
        };
        verify_applied_checksums(conn, &host_migration_plan(), through).await?;
    }
    let schema_object = repo.put_object(&CanonicalObject::Schema {
        sql_contract_version: schema.sql_contract_version,
        statements: schema.schema_sql(),
    })?;
    let mut tables_meta = Vec::new();
    let mut identity = std::collections::BTreeMap::new();
    for table in &schema.tables {
        let name = table.parsed.table.as_str();
        if skip_table(name, &opts.skip_tables) {
            continue;
        }
        if !ident_ok(name) {
            return Err(LibraryError::Schema(format!(
                "backup refuses unsafe table name `{name}`"
            )));
        }
        let (columns, chunks, last) = capture_table(conn, repo, table, opts).await?;
        if let Some(col) = table.parsed.identity_column.as_deref() {
            let hw = identity_high_water(conn, conn.get_database_backend(), name, last).await?;
            identity.insert(
                name.to_string(),
                IdentityHighWater {
                    column: col.to_string(),
                    last: hw,
                },
            );
        }
        tables_meta.push(BackupTable {
            name: name.to_string(),
            columns,
            chunks,
        });
    }
    let identity_object = repo.put_object(&CanonicalObject::Identity { entries: identity })?;
    let (plugin_schema_namespace, plugin_schema_state, plugin_schema_checksum) =
        match (kind, plugin_id.as_deref()) {
            (DatabaseUnitKind::PluginBinding, Some(id)) => {
                let history = crate::migrations::load_plugin_migration_history_on(conn).await?;
                (
                    Some(id.to_string()),
                    Some(history.display()),
                    Some(history.digest()),
                )
            }
            _ => (None, None, None),
        };
    Ok(BackupUnit {
        kind,
        plugin_id,
        binding,
        backend_at_capture: backend_at_capture.to_string(),
        sql_contract_version: schema.sql_contract_version,
        schema_object,
        identity_object,
        tables: tables_meta,
        plugin_schema_namespace,
        plugin_schema_state,
        plugin_schema_checksum,
    })
}

/// Page `table` in canonical order and write bounded JSON chunks.
async fn capture_table<C>(
    conn: &C,
    repo: &BackupRepository,
    table: &CanonicalTableSchema,
    opts: &CanonicalExportOpts,
) -> Result<(Vec<String>, Vec<String>, i64)>
where
    C: ConnectionTrait,
{
    let name = table.parsed.table.as_str();
    let columns: Vec<String> = table
        .parsed
        .columns
        .iter()
        .map(|(c, _)| c.clone())
        .collect();
    if columns.is_empty() {
        return Err(LibraryError::Schema(format!(
            "backup table `{name}` has no columns"
        )));
    }
    for col in &columns {
        if !ident_ok(col) {
            return Err(LibraryError::Schema(format!(
                "backup refuses unsafe column `{name}.{col}`"
            )));
        }
    }
    let types: Vec<_> = table
        .parsed
        .columns
        .iter()
        .map(|(_, ty)| sql_type_to_db_type(*ty))
        .collect();
    let order = order_key_columns(&table.parsed);
    for col in &order {
        if !ident_ok(col) {
            return Err(LibraryError::Schema(format!(
                "backup refuses unsafe ORDER BY column `{name}.{col}`"
            )));
        }
    }
    let order_sql = canonical_order_by_sql(&table.parsed);
    let max_rows = opts.max_result_rows.max(1);
    let byte_budget = opts.page_byte_budget();
    let backend = conn.get_database_backend();
    let mut env = SqlTypeEnv::new();
    env.insert_table(
        table.parsed.table.clone(),
        table.parsed.columns.iter().cloned(),
    );
    let mut page = max_rows;
    let mut offset: u64 = 0;
    let mut chunks = Vec::new();
    let mut chunk_rows: Vec<Vec<DbValue>> = Vec::new();
    let mut chunk_bytes = 0usize;
    let mut identity_last = 0_i64;
    let identity_idx = table
        .parsed
        .identity_column
        .as_ref()
        .and_then(|c| columns.iter().position(|n| n == c));
    loop {
        let canonical_sql = format!(
            "SELECT {} FROM {name} ORDER BY {order_sql} LIMIT {page} OFFSET {offset}",
            columns.join(", "),
        );
        let sql = lower_capture_select(backend, &canonical_sql, &env, page)?;
        let rows = match conn
            .query_all_raw(Statement::from_string(backend, sql))
            .await
        {
            Ok(rows) => rows,
            Err(err) if result_bytes_exceeded(&err.to_string()) => {
                if page <= 1 {
                    return Err(LibraryError::Schema(format!(
                        "backup cannot query table `{name}`: a single-row page exceeds \
                         maxResultBytes/maxAtomicResultBytes: {err}"
                    )));
                }
                page = (page / 2).max(1);
                continue;
            }
            Err(err) => {
                return Err(LibraryError::Schema(format!(
                    "backup cannot query table `{name}`: {err}"
                )));
            }
        };
        if rows.is_empty() {
            break;
        }
        let mut cells_rows = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut cells = Vec::with_capacity(columns.len());
            for (col, ty) in columns.iter().zip(types.iter()) {
                cells.push(cell_to_db_value(row, col, *ty)?);
            }
            cells_rows.push(cells);
        }
        let encoded = encoded_select_page_bytes(&columns, &types, &cells_rows)?;
        if encoded > byte_budget {
            if cells_rows.len() == 1 {
                return Err(LibraryError::Schema(format!(
                    "backup table `{name}` has a row whose encoded result \
                     ({encoded} bytes) exceeds maxResultBytes/maxAtomicResultBytes \
                     ({byte_budget})"
                )));
            }
            page = (u32::try_from(cells_rows.len()).unwrap_or(page) / 2).max(1);
            continue;
        }
        let n = u32::try_from(cells_rows.len()).unwrap_or(page);
        for cells in cells_rows {
            if let Some(idx) = identity_idx {
                if let Some(DbValue::Int64(v)) = cells.get(idx) {
                    identity_last = identity_last.max(*v);
                }
            }
            if chunk_would_overflow(chunk_bytes, &cells, opts.chunk_target_bytes) {
                chunks.push(flush_chunk(
                    repo,
                    name,
                    &columns,
                    &mut chunk_rows,
                    &mut chunk_bytes,
                )?);
            }
            let row_len = serde_json::to_vec(&cells).map(|b| b.len()).unwrap_or(0);
            chunk_bytes = chunk_bytes.saturating_add(row_len).saturating_add(1);
            chunk_rows.push(cells);
        }
        offset = offset.saturating_add(u64::from(n));
        if n < page {
            break;
        }
        page = max_rows;
    }
    if !chunk_rows.is_empty() {
        chunks.push(flush_chunk(
            repo,
            name,
            &columns,
            &mut chunk_rows,
            &mut chunk_bytes,
        )?);
    }
    Ok((columns, chunks, identity_last))
}

/// Encoded Cap'n size of one SELECT page as a [`StatementResult`].
fn encoded_select_page_bytes(
    columns: &[String],
    types: &[DbType],
    rows: &[Vec<DbValue>],
) -> Result<usize> {
    let cols = columns
        .iter()
        .zip(types.iter())
        .map(|(name, db_type)| DbColumn {
            name: name.clone(),
            db_type: *db_type,
        })
        .collect();
    let db_rows = rows
        .iter()
        .map(|values| DbRow {
            values: values.clone(),
        })
        .collect();
    let stmt = StatementResult::from_rows(cols, db_rows).map_err(LibraryError::Schema)?;
    let bytes = encoded_statement_result_bytes(&stmt)
        .map_err(|err| LibraryError::Schema(err.to_string()))?;
    Ok(bytes.len())
}

/// True when an adapter error is a result-byte budget rejection.
fn result_bytes_exceeded(err: &str) -> bool {
    let m = err.to_ascii_lowercase();
    m.contains("maxresultbytes") || m.contains("maxatomicresultbytes")
}

/// Encode one chunk of canonical rows and store it in the object repository.
fn flush_chunk(
    repo: &BackupRepository,
    table: &str,
    columns: &[String],
    rows: &mut Vec<Vec<DbValue>>,
    chunk_bytes: &mut usize,
) -> Result<String> {
    let object = CanonicalObject::TableChunk {
        table: table.to_string(),
        columns: columns.to_vec(),
        rows: std::mem::take(rows),
    };
    *chunk_bytes = 0;
    repo.put_object(&object)
}

/// True when `name` is adapter-private or explicitly excluded from this unit.
fn skip_table(name: &str, extra: &BTreeSet<String>) -> bool {
    let folded = name.to_ascii_lowercase();
    ADAPTER_PRIVATE_TABLES.contains(&folded.as_str()) || extra.contains(&folded)
}

/// Highest identity value for `table` from rows plus adapter catalogs.
async fn identity_high_water<C>(
    conn: &C,
    backend: DbBackend,
    table: &str,
    from_rows: i64,
) -> Result<i64>
where
    C: ConnectionTrait,
{
    let mut last = from_rows;
    if backend == DbBackend::Sqlite {
        if let Some(seq) = sqlite_sequence(conn, table).await? {
            last = last.max(seq);
        }
    }
    if let Some(hw) = bookclerk_identity_last(conn, backend, table).await? {
        last = last.max(hw);
    }
    Ok(last)
}

/// SQLite `sqlite_sequence` high-water for `table`, if present.
async fn sqlite_sequence<C>(conn: &C, table: &str) -> Result<Option<i64>>
where
    C: ConnectionTrait,
{
    let sql = format!("SELECT seq FROM sqlite_sequence WHERE name = '{table}'");
    match conn
        .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
    {
        Ok(rows) => Ok(rows.first().and_then(|row| int_cell(row, "seq"))),
        Err(err)
            if err
                .to_string()
                .to_ascii_lowercase()
                .contains("sqlite_sequence") =>
        {
            Ok(None)
        }
        Err(err) => Err(LibraryError::Schema(format!(
            "backup cannot read sqlite_sequence for `{table}`: {err}"
        ))),
    }
}

/// Bookclerk identity high-water from `bookclerk_identity_last`, if present.
async fn bookclerk_identity_last<C>(
    conn: &C,
    backend: DbBackend,
    table: &str,
) -> Result<Option<i64>>
where
    C: ConnectionTrait,
{
    let sql = format!("SELECT last FROM {SQL_IDENTITY_TABLE} WHERE table_name = '{table}'");
    match conn
        .query_all_raw(Statement::from_string(backend, sql))
        .await
    {
        Ok(rows) => Ok(rows.first().and_then(|row| int_cell(row, "last"))),
        Err(err) if reserved_catalog_relation_missing(&err.to_string(), SQL_IDENTITY_TABLE) => {
            Ok(None)
        }
        Err(err) => Err(LibraryError::Schema(format!(
            "backup cannot read `{SQL_IDENTITY_TABLE}` for `{table}`: {err}"
        ))),
    }
}

/// Rebuilds plugin schema from durable `bookclerk_sql_ddl` on an open connection.
///
/// # Errors
///
/// Returns when the DDL catalog is missing, incomplete, or malformed.
pub async fn plugin_canonical_schema_from_ddl_catalog(
    db: &DatabaseConnection,
) -> Result<CanonicalDatabaseSchema> {
    plugin_canonical_schema_from_ddl_catalog_on(db, &CanonicalExportOpts::default()).await
}

/// Read plugin canonical DDL from `bookclerk_sql_ddl` on an open connection.
pub(super) async fn plugin_canonical_schema_from_ddl_catalog_on<C>(
    conn: &C,
    opts: &CanonicalExportOpts,
) -> Result<CanonicalDatabaseSchema>
where
    C: ConnectionTrait,
{
    let backend = conn.get_database_backend();
    let env = catalog_type_env();
    let rows = match paged_select(
        conn,
        backend,
        &format!(
            "SELECT kind, name, table_name, canonical_sql FROM {SQL_DDL_TABLE} \
             ORDER BY kind, name"
        ),
        opts,
        &env,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            if reserved_catalog_relation_missing(&err.to_string(), SQL_DDL_TABLE) {
                return Err(LibraryError::Schema(format!(
                    "plugin database has no durable canonical DDL catalog (`{SQL_DDL_TABLE}`); \
                     reset or recreate the binding — Bookclerk will not merge captured schema \
                     onto an unknown existing database"
                )));
            }
            return Err(LibraryError::Schema(format!(
                "plugin backup cannot read `{SQL_DDL_TABLE}`: {err}"
            )));
        }
    };
    let mut statements = Vec::new();
    let mut table_names = BTreeSet::new();
    for row in rows {
        let kind = cell_text(&row, "kind")?.to_ascii_lowercase();
        let canonical = cell_text(&row, "canonical_sql")?;
        let trimmed = canonical.trim().trim_end_matches(';').trim().to_string();
        match kind.as_str() {
            "table" => {
                let parsed = parse_create_table_schema(&trimmed).ok_or_else(|| {
                    LibraryError::Schema(format!(
                        "plugin `{SQL_DDL_TABLE}` row is not admitted CREATE TABLE SQL"
                    ))
                })?;
                table_names.insert(parsed.table.clone());
                statements.push(trimmed);
            }
            "index" => {
                let _ = parse_create_index_sql(&trimmed).ok_or_else(|| {
                    LibraryError::Schema(format!(
                        "plugin `{SQL_DDL_TABLE}` row is not admitted CREATE INDEX SQL"
                    ))
                })?;
                statements.push(trimmed);
            }
            other => {
                return Err(LibraryError::Schema(format!(
                    "plugin `{SQL_DDL_TABLE}` kind `{other}` is not supported"
                )));
            }
        }
    }
    let schema_sql = format!("SELECT table_name FROM {SQL_SCHEMA_TABLE} ORDER BY table_name");
    match paged_select(conn, backend, &schema_sql, opts, &env).await {
        Ok(schema_rows) => {
            for row in schema_rows {
                let name = cell_text(&row, "table_name")?;
                if !table_names.contains(&name) {
                    return Err(LibraryError::Schema(format!(
                        "plugin table `{name}` is fingerprint-catalogued but has no canonical DDL \
                         row in `{SQL_DDL_TABLE}`; reset or recreate the binding"
                    )));
                }
            }
        }
        Err(err) if reserved_catalog_relation_missing(&err.to_string(), SQL_SCHEMA_TABLE) => {}
        Err(err) => {
            return Err(LibraryError::Schema(format!(
                "plugin backup cannot read `{SQL_SCHEMA_TABLE}`: {err}"
            )));
        }
    }
    let refs: Vec<&str> = statements.iter().map(String::as_str).collect();
    sort_schema(admit_canonical_statements(SQL_CONTRACT_VERSION, &refs)?)
}

/// Type environment for reserved catalog companion tables.
fn catalog_type_env() -> SqlTypeEnv {
    sql_type_env_from_canonical_statements([
        sql_ddl_create_table_sql(),
        sql_schema_create_table_sql(),
    ])
}

/// Proof-directed lowering so Postgres TEXT ORDER BY uses `COLLATE "C"`.
fn lower_capture_select(
    backend: DbBackend,
    sql: &str,
    env: &SqlTypeEnv,
    max_rows: u32,
) -> Result<String> {
    let req = ExecuteRequest {
        operation_id: "backup-capture".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.to_string(),
            parameters: Vec::new(),
            kind: DbPlanStatementKind::Select,
            max_rows,
            result_selection: DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    };
    let proofs = typecheck_execute_request_proofs(&req, env).map_err(|err| {
        LibraryError::Schema(format!("backup SELECT is not admitted SQL v1: {err}"))
    })?;
    bookclerk_db_exec::lower_canonical_sql_typed(backend, sql, proofs.first())
        .map_err(|err| LibraryError::Schema(format!("backup SELECT cannot be lowered: {err}")))
}

/// Default skip set for library capture (`plugin_databases` is environment-local).
#[must_use]
pub fn library_skip_tables() -> BTreeSet<String> {
    LIBRARY_SKIP_TABLES
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Run `ordered_select` with `LIMIT/OFFSET` using adapter page size and byte budget.
async fn paged_select<C>(
    conn: &C,
    backend: DbBackend,
    ordered_select: &str,
    opts: &CanonicalExportOpts,
    env: &SqlTypeEnv,
) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr>
where
    C: ConnectionTrait,
{
    let max_rows = opts.max_result_rows.max(1);
    let mut page = max_rows;
    let mut offset = 0u64;
    let mut out = Vec::new();
    loop {
        let canonical = format!("{ordered_select} LIMIT {page} OFFSET {offset}");
        let sql = match lower_capture_select(backend, &canonical, env, page) {
            Ok(sql) => sql,
            Err(err) => {
                return Err(sea_orm::DbErr::Custom(err.to_string()));
            }
        };
        let batch = match conn
            .query_all_raw(Statement::from_string(backend, sql))
            .await
        {
            Ok(batch) => batch,
            Err(err) if result_bytes_exceeded(&err.to_string()) => {
                if page <= 1 {
                    return Err(err);
                }
                page = (page / 2).max(1);
                continue;
            }
            Err(err) => return Err(err),
        };
        if batch.is_empty() {
            break;
        }
        let n = u32::try_from(batch.len()).unwrap_or(page);
        offset = offset.saturating_add(u64::from(n));
        out.extend(batch);
        if n < page {
            break;
        }
        page = max_rows;
    }
    Ok(out)
}
