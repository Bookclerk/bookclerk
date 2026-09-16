//! Shared backup I/O helpers (typed cells, identifiers, SQL exec).

use bookclerk_plugin_abi::{
    encoded_execute_request_bytes, sql_payload_exceeds, DbIdentityHighWater, DbPlanStatementKind,
    DbResultSelection, DbType, DbValue, ExecuteRequest, SharedAdapterBackupOps, SqlType,
    SqlTypeEnv, TypedDbStatement,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, QueryResult, StreamTrait};

use crate::error::{LibraryError, Result};

use super::schema::sql_type_to_db_type;
use super::CanonicalRestoreOpts;

/// True when `s` is a safe unquoted SQL-v1 identifier.
#[must_use]
pub fn ident_ok(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && bookclerk_plugin_abi::sql_v1_ident_in_bounds(s)
}

/// True when relation `name` exists (empty tables still count).
///
/// Uses a bounded canonical `SELECT` so a missing table is an engine error
/// rather than an adapter catalog probe. Never inspects the physical backend.
pub async fn table_exists<C>(conn: &C, name: &str) -> Result<bool>
where
    C: ConnectionTrait,
{
    if !ident_ok(name) {
        return Ok(false);
    }
    match bookclerk_db_exec::query_canonical(
        conn,
        &format!("SELECT 1 FROM {name} LIMIT 1"),
        std::iter::empty::<sea_orm::Value>(),
    )
    .await
    {
        Ok(_) => Ok(true),
        Err(err)
            if bookclerk_plugin_abi::reserved_catalog_relation_missing(&err.to_string(), name)
                || err
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("no such table")
                || err
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("does not exist") =>
        {
            Ok(false)
        }
        Err(err) => Err(LibraryError::from_db_err(err)),
    }
}

/// Executes one canonical statement with typed [`DbValue`] binds.
///
/// Honors negotiated `maxBinds`, `maxPayloadBytes`, and `maxRequestBytes`.
/// Placeholders stay Bookclerk `?`; adapters lower at execute.
///
/// # Errors
///
/// Returns when a bind/payload/request budget is exceeded or the engine
/// rejects the statement.
pub(crate) async fn exec_bound<C>(
    conn: &C,
    opts: &CanonicalRestoreOpts,
    sql: &str,
    params: Vec<DbValue>,
    type_env: SqlTypeEnv,
) -> Result<()>
where
    C: ConnectionTrait + StreamTrait,
{
    let nbinds = u32::try_from(params.len()).unwrap_or(u32::MAX);
    if nbinds > opts.max_binds {
        return Err(LibraryError::Schema(format!(
            "restore statement uses {nbinds} binds; adapter maxBinds is {}",
            opts.max_binds
        )));
    }
    let values_json = serde_json::to_string(&params).map_err(|err| {
        LibraryError::Schema(format!("restore cannot encode bind payload: {err}"))
    })?;
    if sql_payload_exceeds(sql, &values_json, opts.max_payload_bytes) {
        return Err(LibraryError::Schema(
            "restore statement exceeds adapter maxPayloadBytes".into(),
        ));
    }
    let req = ExecuteRequest {
        operation_id: "backup-restore".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.to_string(),
            parameters: params.clone(),
            kind: DbPlanStatementKind::Execute,
            max_rows: 0,
            result_selection: DbResultSelection::Discard,
        }],
        deadline_unix_ms: 0,
    };
    let encoded =
        encoded_execute_request_bytes(&req).map_err(|err| LibraryError::Schema(err.to_string()))?;
    let max_req = usize::try_from(opts.max_request_bytes).unwrap_or(usize::MAX);
    if encoded.len() > max_req {
        return Err(LibraryError::Schema(format!(
            "restore ExecuteRequest ({} bytes) exceeds adapter maxRequestBytes ({})",
            encoded.len(),
            opts.max_request_bytes
        )));
    }
    if opts.in_process {
        crate::sql_plan::execute_typed_on_open(conn, &req, type_env, 0).await?;
        return Ok(());
    }
    bookclerk_db_exec::execute_canonical(
        conn,
        sql,
        params.iter().map(bookclerk_db_exec::db_value_to_sea),
    )
    .await
    .map_err(LibraryError::from_db_err)?;
    Ok(())
}

/// Reads a catalog text cell by name with positional fallback.
///
/// # Errors
///
/// Returns when the column is missing.
pub fn cell_text(row: &QueryResult, name: &str) -> Result<String> {
    row.try_get::<String>("", name)
        .or_else(|_| {
            row.try_get_by_index::<String>(match name {
                "kind" => 0,
                "name" => 1,
                "table_name" => 2,
                "canonical_sql" => 3,
                _ => 0,
            })
        })
        .map_err(|err| LibraryError::Schema(format!("catalog column `{name}`: {err}")))
}

/// Converts one result cell to a portable `DbValue` using the declared column type.
///
/// # Errors
///
/// Returns when the cell is outside the portable domain.
pub fn cell_to_db_value(row: &QueryResult, name: &str, ty: DbType) -> Result<DbValue> {
    match ty {
        DbType::Bool => {
            if let Ok(v) = row.try_get::<Option<bool>>("", name) {
                return Ok(v.map_or(DbValue::Null(DbType::Bool), DbValue::Boolean));
            }
            if let Ok(v) = row.try_get::<Option<i64>>("", name) {
                return match v {
                    None => Ok(DbValue::Null(DbType::Bool)),
                    Some(0) => Ok(DbValue::Boolean(false)),
                    Some(1) => Ok(DbValue::Boolean(true)),
                    Some(other) => Err(LibraryError::Schema(format!(
                        "backup cannot represent boolean column `{name}` value {other}"
                    ))),
                };
            }
        }
        DbType::Int64 => {
            if let Ok(v) = row.try_get::<Option<i64>>("", name) {
                return Ok(v.map_or(DbValue::Null(DbType::Int64), DbValue::Int64));
            }
            if let Ok(v) = row.try_get::<Option<i32>>("", name) {
                return Ok(v.map_or(DbValue::Null(DbType::Int64), |n| {
                    DbValue::Int64(i64::from(n))
                }));
            }
        }
        DbType::Float64 => {
            if let Ok(v) = row.try_get::<Option<f64>>("", name) {
                return match v {
                    None => Ok(DbValue::Null(DbType::Float64)),
                    Some(n) if n.is_finite() => Ok(DbValue::Float64(n)),
                    Some(_) => Err(LibraryError::Schema(format!(
                        "backup cannot represent non-finite float column `{name}`"
                    ))),
                };
            }
        }
        DbType::Text => {
            if let Ok(v) = row.try_get::<Option<String>>("", name) {
                return Ok(v.map_or(DbValue::Null(DbType::Text), DbValue::Text));
            }
        }
        DbType::Bytes => {
            if let Ok(v) = row.try_get::<Option<Vec<u8>>>("", name) {
                return Ok(v.map_or(DbValue::Null(DbType::Bytes), DbValue::Bytes));
            }
        }
        DbType::Unspecified => {}
    }
    cell_to_db_value_untyped(row, name)
}

/// Decode a cell without a declared SQL type (chunk decode / type fallback).
fn cell_to_db_value_untyped(row: &QueryResult, name: &str) -> Result<DbValue> {
    if let Ok(v) = row.try_get::<Option<bool>>("", name) {
        return Ok(v.map_or(DbValue::Null(DbType::Bool), DbValue::Boolean));
    }
    if let Ok(v) = row.try_get::<Option<i64>>("", name) {
        return Ok(v.map_or(DbValue::Null(DbType::Int64), DbValue::Int64));
    }
    if let Ok(v) = row.try_get::<Option<f64>>("", name) {
        return match v {
            None => Ok(DbValue::Null(DbType::Float64)),
            Some(n) if n.is_finite() => Ok(DbValue::Float64(n)),
            Some(_) => Err(LibraryError::Schema(format!(
                "backup cannot represent non-finite float column `{name}`"
            ))),
        };
    }
    if let Ok(v) = row.try_get::<Option<String>>("", name) {
        return Ok(v.map_or(DbValue::Null(DbType::Text), DbValue::Text));
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>>("", name) {
        return Ok(v.map_or(DbValue::Null(DbType::Bytes), DbValue::Bytes));
    }
    Err(LibraryError::Schema(format!(
        "backup cannot represent column `{name}` in the portable DbValue domain"
    )))
}

/// Enforces the canonical typed value contract for one cell.
///
/// # Errors
///
/// Returns when the variant, nullability, or finiteness is wrong.
pub fn validate_cell(
    table: &str,
    column: &str,
    cell: &DbValue,
    declared: SqlType,
    not_null: bool,
) -> Result<()> {
    let expected = sql_type_to_db_type(declared);
    match cell {
        DbValue::Null(ty) => {
            if not_null {
                return Err(LibraryError::Schema(format!(
                    "backup table `{table}` column `{column}` is NOT NULL but the cell is NULL"
                )));
            }
            if *ty != expected && *ty != DbType::Unspecified && expected != DbType::Unspecified {
                return Err(LibraryError::Schema(format!(
                    "backup table `{table}` column `{column}` typed NULL {ty:?} does not match {expected:?}"
                )));
            }
            Ok(())
        }
        DbValue::Boolean(_) if declared == SqlType::Boolean => Ok(()),
        DbValue::Int64(_) if declared == SqlType::Integer => Ok(()),
        DbValue::Float64(n) if declared == SqlType::Real => {
            if n.is_finite() {
                Ok(())
            } else {
                Err(LibraryError::Schema(format!(
                    "backup table `{table}` column `{column}` has a non-finite float"
                )))
            }
        }
        DbValue::Text(s) if declared == SqlType::Text => {
            if std::str::from_utf8(s.as_bytes()).is_ok() {
                Ok(())
            } else {
                Err(LibraryError::Schema(format!(
                    "backup table `{table}` column `{column}` text is not UTF-8"
                )))
            }
        }
        DbValue::Bytes(_) if declared == SqlType::Blob => Ok(()),
        other => Err(LibraryError::Schema(format!(
            "backup table `{table}` column `{column}` value {other:?} does not match declared {declared:?}"
        ))),
    }
}

/// Maps an adapter primitive error onto [`LibraryError::Schema`].
fn adapter_err(err: bookclerk_plugin_abi::PluginError) -> LibraryError {
    LibraryError::Schema(err.to_string())
}

/// Identity high-water from the adapter, or the in-process SDK when `adapter` is `None`.
pub(crate) async fn backup_export_identity<C>(
    conn: &C,
    adapter: Option<&SharedAdapterBackupOps>,
) -> Result<Vec<DbIdentityHighWater>>
where
    C: ConnectionTrait,
{
    if let Some(adapter) = adapter {
        adapter.export_identity().await.map_err(adapter_err)
    } else {
        bookclerk_db_exec::export_identity(conn)
            .await
            .map_err(LibraryError::from_db_err)
    }
}

/// Restore identity high-water through the adapter or in-process SDK.
pub(crate) async fn backup_import_identity<C>(
    conn: &C,
    adapter: Option<&SharedAdapterBackupOps>,
    rows: &[DbIdentityHighWater],
) -> Result<()>
where
    C: ConnectionTrait,
{
    if let Some(adapter) = adapter {
        adapter.import_identity(rows).await.map_err(adapter_err)
    } else {
        bookclerk_db_exec::import_identity(conn, rows)
            .await
            .map_err(LibraryError::from_db_err)
    }
}

/// User-visible relations through the adapter or in-process SDK.
pub(crate) async fn backup_list_user_relations(
    db: &DatabaseConnection,
    adapter: Option<&SharedAdapterBackupOps>,
) -> Result<Vec<String>> {
    if let Some(adapter) = adapter {
        adapter.list_user_relations().await.map_err(adapter_err)
    } else {
        bookclerk_db_exec::list_user_relations(db)
            .await
            .map_err(LibraryError::from_db_err)
    }
}

/// Prepare the open restore transaction (deferred FK checks).
pub(crate) async fn backup_prepare_unit_restore<C>(
    conn: &C,
    adapter: Option<&SharedAdapterBackupOps>,
) -> Result<()>
where
    C: ConnectionTrait,
{
    if let Some(adapter) = adapter {
        adapter.prepare_unit_restore().await.map_err(adapter_err)
    } else {
        bookclerk_db_exec::prepare_unit_restore(conn)
            .await
            .map_err(LibraryError::from_db_err)
    }
}

/// Drop named user relations through the adapter or in-process SDK.
pub(crate) async fn backup_drop_user_relations<C>(
    conn: &C,
    adapter: Option<&SharedAdapterBackupOps>,
    names: &[String],
) -> Result<()>
where
    C: ConnectionTrait,
{
    if let Some(adapter) = adapter {
        adapter
            .drop_user_relations(names)
            .await
            .map_err(adapter_err)
    } else {
        bookclerk_db_exec::drop_user_relations(conn, names)
            .await
            .map_err(LibraryError::from_db_err)
    }
}

/// Fail closed when restore FK checks still fail.
pub(crate) async fn backup_assert_restore_constraints<C>(
    conn: &C,
    adapter: Option<&SharedAdapterBackupOps>,
) -> Result<()>
where
    C: ConnectionTrait,
{
    if let Some(adapter) = adapter {
        adapter
            .assert_restore_constraints()
            .await
            .map_err(adapter_err)
    } else {
        bookclerk_db_exec::assert_restore_constraints(conn)
            .await
            .map_err(LibraryError::from_db_err)
    }
}
