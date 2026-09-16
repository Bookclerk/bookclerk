//! Adapter-owned snapshot / identity / restore primitives.
//!
//! Called from first-party guests on a **real** engine connection (not the
//! host SeaORM RPC proxy). Library tests may call the same helpers on an
//! in-process sqlite/postgres `DatabaseConnection`.

#![allow(clippy::missing_docs_in_private_items)]

use bookclerk_plugin_abi::{
    postgres_identity_function_name, DbIdentityHighWater, IsolationReq, SQL_IDENTITY_TABLE,
};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, Statement,
    TransactionTrait,
};

tokio::task_local! {
    static BEGIN_ISOLATION: IsolationReq;
}

/// Isolation the next SeaORM `begin()` should ask the adapter to realize.
#[must_use]
pub fn pending_begin_isolation() -> IsolationReq {
    BEGIN_ISOLATION
        .try_with(|iso| *iso)
        .unwrap_or(IsolationReq::AtomicBatch)
}

/// Begins a consistent capture transaction.
///
/// Sets [`IsolationReq::ConsistentSnapshot`] for RPC proxies, and applies
/// `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ` on a native Postgres
/// connection (the host RPC proxy always reports the canonical sqlite backend).
///
/// # Errors
///
/// Returns when `BEGIN` or snapshot isolation fails.
pub async fn begin_consistent_snapshot(
    db: &DatabaseConnection,
) -> Result<DatabaseTransaction, DbErr> {
    BEGIN_ISOLATION
        .scope(IsolationReq::ConsistentSnapshot, async {
            let txn = db.begin().await?;
            apply_begin_isolation(&txn, IsolationReq::ConsistentSnapshot).await?;
            Ok(txn)
        })
        .await
}

/// Realizes [`IsolationReq`] on an already-open transaction.
///
/// # Errors
///
/// Returns when the engine rejects the isolation statement.
pub async fn apply_begin_isolation<C>(conn: &C, isolation: IsolationReq) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    match isolation {
        IsolationReq::AtomicBatch | IsolationReq::NestedSavepoint => Ok(()),
        IsolationReq::ConsistentSnapshot => {
            if conn.get_database_backend() == DbBackend::Postgres {
                conn.execute_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                ))
                .await?;
            }
            Ok(())
        }
    }
}

/// Identity high-water from `sqlite_sequence` and [`SQL_IDENTITY_TABLE`].
///
/// # Errors
///
/// Returns when a catalog query fails for a reason other than a missing catalog.
pub async fn export_identity<C>(conn: &C) -> Result<Vec<DbIdentityHighWater>, DbErr>
where
    C: ConnectionTrait,
{
    let backend = conn.get_database_backend();
    let mut by_table: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    if backend == DbBackend::Sqlite {
        merge_identity_rows(
            &mut by_table,
            query_optional(
                conn,
                backend,
                "SELECT name, seq FROM sqlite_sequence",
                "name",
                "seq",
                "sqlite_sequence",
            )
            .await?,
        );
    }
    merge_identity_rows(
        &mut by_table,
        query_optional(
            conn,
            backend,
            &format!("SELECT table_name, last FROM {SQL_IDENTITY_TABLE}"),
            "table_name",
            "last",
            SQL_IDENTITY_TABLE,
        )
        .await?,
    );
    Ok(by_table
        .into_iter()
        .map(|(table, last)| DbIdentityHighWater { table, last })
        .collect())
}

/// Writes identity high-water into adapter catalogs.
///
/// # Errors
///
/// Returns when a catalog write fails.
pub async fn import_identity<C>(conn: &C, rows: &[DbIdentityHighWater]) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    let backend = conn.get_database_backend();
    for row in rows {
        if !portable_ident(&row.table) {
            return Err(DbErr::Custom(format!(
                "refusing to import identity for unsafe table `{}`",
                row.table
            )));
        }
        match backend {
            DbBackend::Sqlite => {
                let _ = conn
                    .execute_raw(Statement::from_sql_and_values(
                        DbBackend::Sqlite,
                        "DELETE FROM sqlite_sequence WHERE name = ?",
                        [sea_orm::Value::from(row.table.clone())],
                    ))
                    .await;
                if row.last > 0 {
                    conn.execute_raw(Statement::from_sql_and_values(
                        DbBackend::Sqlite,
                        "INSERT INTO sqlite_sequence(name, seq) VALUES (?, ?)",
                        [
                            sea_orm::Value::from(row.table.clone()),
                            sea_orm::Value::from(row.last),
                        ],
                    ))
                    .await?;
                }
                let _ = upsert_bookclerk_identity(conn, backend, row).await;
            }
            DbBackend::Postgres => {
                upsert_bookclerk_identity(conn, backend, row).await?;
            }
            other => {
                return Err(DbErr::Custom(format!(
                    "BookclerkSQL adapters do not support SeaORM backend {other:?}"
                )));
            }
        }
    }
    Ok(())
}

/// User-visible base tables in the current schema.
///
/// # Errors
///
/// Returns when the catalog probe fails.
pub async fn list_user_relations<C>(conn: &C) -> Result<Vec<String>, DbErr>
where
    C: ConnectionTrait,
{
    let backend = conn.get_database_backend();
    let sql = if backend == DbBackend::Postgres {
        "SELECT c.relname::text AS name FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') \
         ORDER BY c.relname"
            .to_string()
    } else {
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name".to_string()
    };
    let rows = conn
        .query_all_raw(Statement::from_string(backend, sql))
        .await?;
    let mut names = Vec::new();
    for row in rows {
        let name = row
            .try_get::<String>("", "name")
            .or_else(|_| row.try_get_by_index::<String>(0))
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if backend == DbBackend::Sqlite && is_sqlite_reserved_catalog(&name) {
            continue;
        }
        names.push(name);
    }
    Ok(names)
}

/// SQLite: `PRAGMA defer_foreign_keys = ON` on the restore transaction.
///
/// # Errors
///
/// Returns when the pragma fails.
pub async fn prepare_unit_restore<C>(conn: &C) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    if conn.get_database_backend() == DbBackend::Sqlite {
        conn.execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA defer_foreign_keys = ON",
        ))
        .await?;
    }
    Ok(())
}

/// Drops named relations. PostgreSQL uses `CASCADE` plus identity functions.
///
/// # Errors
///
/// Returns when a drop fails (other than IF EXISTS no-ops).
pub async fn drop_user_relations<C>(conn: &C, names: &[String]) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    let backend = conn.get_database_backend();
    let cascade = if backend == DbBackend::Postgres {
        " CASCADE"
    } else {
        ""
    };
    for name in names.iter().rev() {
        if !portable_ident(name) {
            return Err(DbErr::Custom(format!(
                "refusing to drop unsafe identifier `{name}` during backup restore"
            )));
        }
        conn.execute_raw(Statement::from_string(
            backend,
            format!("DROP TABLE IF EXISTS {name}{cascade}"),
        ))
        .await?;
        if backend == DbBackend::Postgres {
            let fn_name = postgres_identity_function_name(name);
            let _ = conn
                .execute_raw(Statement::from_string(
                    DbBackend::Postgres,
                    format!("DROP FUNCTION IF EXISTS {fn_name}() CASCADE"),
                ))
                .await;
        }
    }
    Ok(())
}

/// SQLite: `PRAGMA foreign_key_check` must be empty before commit.
///
/// # Errors
///
/// Returns when violations remain or the pragma fails.
pub async fn assert_restore_constraints<C>(conn: &C) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    if conn.get_database_backend() != DbBackend::Sqlite {
        return Ok(());
    }
    let rows = conn
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_key_check",
        ))
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let table = rows[0]
        .try_get_by_index::<String>(0)
        .unwrap_or_else(|_| "unknown".into());
    Err(DbErr::Custom(format!(
        "restore left foreign-key violations (e.g. table `{table}`)"
    )))
}

fn portable_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && bookclerk_plugin_abi::sql_v1_ident_in_bounds(s)
}

/// SQLite engine catalogs only (`sqlite_master`, `sqlite_sequence`, …).
///
/// User tables may be named `pg_notes` or `information_schema`; those are not
/// catalogs. PostgreSQL listing already restricts to `current_schema()`.
fn is_sqlite_reserved_catalog(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("sqlite_")
}

/// True when `err` is a missing optional catalog relation, not permission or
/// other failures that mention the same name.
///
/// PostgreSQL `42P01` (undefined_table) still requires
/// [`reserved_catalog_relation_missing`] so a missing *other* relation does
/// not look like an absent identity catalog. `42501` (insufficient_privilege)
/// and every other SQLSTATE propagate.
fn optional_catalog_absent(err: &DbErr, missing_name: &str) -> bool {
    if let Some(code) = crate::classify::sqlx_engine_code(err) {
        match code.to_ascii_uppercase().as_str() {
            "42501" => return false,
            "42P01" => {
                return bookclerk_plugin_abi::reserved_catalog_relation_missing(
                    &err.to_string(),
                    missing_name,
                );
            }
            _ => {}
        }
    }
    bookclerk_plugin_abi::reserved_catalog_relation_missing(&err.to_string(), missing_name)
}

fn merge_identity_rows(
    into: &mut std::collections::BTreeMap<String, i64>,
    rows: Vec<(String, i64)>,
) {
    for (table, last) in rows {
        into.entry(table)
            .and_modify(|cur| *cur = (*cur).max(last))
            .or_insert(last);
    }
}

/// Canonical identity upsert (`?` placeholders). Postgres adapters lower `$1`/`$2`
/// at the execute edge — sending `VALUES (?, ?)` verbatim is
/// `syntax error at or near ","` at character 60.
fn identity_upsert_sql(backend: DbBackend) -> String {
    match backend {
        DbBackend::Postgres => format!(
            "INSERT INTO {SQL_IDENTITY_TABLE} (table_name, last) VALUES (?, ?) \
             ON CONFLICT (table_name) DO UPDATE SET last = GREATEST({SQL_IDENTITY_TABLE}.last, EXCLUDED.last)"
        ),
        DbBackend::Sqlite => format!(
            "INSERT OR REPLACE INTO {SQL_IDENTITY_TABLE} (table_name, last) VALUES (?, ?)"
        ),
        other => crate::exec::reject_unknown_seaorm_backend(other),
    }
}

/// Runs `op` under a PostgreSQL savepoint so a missing-catalog error does not
/// abort the current transaction (`25P02`). No-op savepoint on SQLite / when
/// `BEGIN` has not opened a transaction.
///
/// # Errors
///
/// Returns the error from `op`. Savepoint begin/rollback/release failures are
/// ignored so a missing `BEGIN` still runs `op`.
async fn with_postgres_savepoint<C, T, Fut>(
    conn: &C,
    name: &'static str,
    op: Fut,
) -> Result<T, DbErr>
where
    C: ConnectionTrait,
    Fut: std::future::Future<Output = Result<T, DbErr>>,
{
    let backend = conn.get_database_backend();
    let savepoint = backend == DbBackend::Postgres
        && conn
            .execute_raw(Statement::from_string(backend, format!("SAVEPOINT {name}")))
            .await
            .is_ok();
    match op.await {
        Ok(value) => {
            if savepoint {
                let _ = conn
                    .execute_raw(Statement::from_string(
                        backend,
                        format!("RELEASE SAVEPOINT {name}"),
                    ))
                    .await;
            }
            Ok(value)
        }
        Err(err) => {
            if savepoint {
                let _ = conn
                    .execute_raw(Statement::from_string(
                        backend,
                        format!("ROLLBACK TO SAVEPOINT {name}"),
                    ))
                    .await;
                let _ = conn
                    .execute_raw(Statement::from_string(
                        backend,
                        format!("RELEASE SAVEPOINT {name}"),
                    ))
                    .await;
            }
            Err(err)
        }
    }
}

/// Optional catalog probe (`sqlite_sequence` / [`SQL_IDENTITY_TABLE`]).
///
/// # Errors
///
/// Returns when the query fails for a reason other than a missing catalog.
async fn query_optional<C>(
    conn: &C,
    backend: DbBackend,
    sql: &str,
    table_col: &str,
    last_col: &str,
    missing_name: &str,
) -> Result<Vec<(String, i64)>, DbErr>
where
    C: ConnectionTrait,
{
    let sql = sql.to_string();
    let result = with_postgres_savepoint(conn, "bookclerk_backup_optional", async {
        conn.query_all_raw(Statement::from_string(backend, sql))
            .await
    })
    .await;
    match result {
        Ok(rows) => {
            let mut out = Vec::new();
            for row in rows {
                let table = row
                    .try_get::<String>("", table_col)
                    .or_else(|_| row.try_get_by_index::<String>(0))
                    .unwrap_or_default();
                let last = row
                    .try_get::<i64>("", last_col)
                    .ok()
                    .or_else(|| row.try_get_by_index::<i64>(1).ok())
                    .or_else(|| row.try_get::<Option<i64>>("", last_col).ok().flatten())
                    .unwrap_or(0);
                if !table.is_empty() {
                    out.push((table, last));
                }
            }
            Ok(out)
        }
        Err(err) if optional_catalog_absent(&err, missing_name) => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

/// Writes one identity high-water row, lowering `?` at the execute edge.
///
/// # Errors
///
/// Returns when the upsert fails. A missing identity table is ignored on SQLite,
/// and on Postgres when `last` is 0; a positive `last` without a table fails
/// closed.
async fn upsert_bookclerk_identity<C>(
    conn: &C,
    backend: DbBackend,
    row: &DbIdentityHighWater,
) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    let sql = identity_upsert_sql(backend);
    let table = row.table.clone();
    let last = row.last;
    let result = with_postgres_savepoint(conn, "bookclerk_identity_upsert", async {
        crate::execute_physical_sql(
            crate::PhysicalEngine::from_adapter_backend(backend),
            conn,
            &sql,
            [sea_orm::Value::from(table), sea_orm::Value::from(last)],
        )
        .await
        .map(|_| ())
    })
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(err)
            if bookclerk_plugin_abi::reserved_catalog_relation_missing(
                &err.to_string(),
                SQL_IDENTITY_TABLE,
            ) =>
        {
            if row.last > 0 && backend != DbBackend::Sqlite {
                return Err(DbErr::Custom(
                    "backup restore cannot apply identity high-water on this adapter \
                     (no bookclerk_identity table)"
                        .into(),
                ));
            }
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_begin_isolation_defaults_to_atomic_batch() {
        assert_eq!(pending_begin_isolation(), IsolationReq::AtomicBatch);
    }

    #[test]
    fn sqlite_reserved_catalogs_are_name_prefixed() {
        assert!(is_sqlite_reserved_catalog("sqlite_sequence"));
        assert!(is_sqlite_reserved_catalog("sqlite_master"));
        assert!(!is_sqlite_reserved_catalog("pg_notes"));
        assert!(!is_sqlite_reserved_catalog("pg_class"));
        assert!(!is_sqlite_reserved_catalog("information_schema"));
        assert!(!is_sqlite_reserved_catalog("books"));
        assert!(!is_sqlite_reserved_catalog("bookclerk_identity"));
    }

    #[test]
    fn optional_catalog_absent_is_structured_undefined_relation() {
        assert!(optional_catalog_absent(
            &DbErr::Custom(r#"relation "bookclerk_identity" does not exist"#.into()),
            SQL_IDENTITY_TABLE
        ));
        assert!(optional_catalog_absent(
            &DbErr::Custom("no such table: bookclerk_identity".into()),
            SQL_IDENTITY_TABLE
        ));
        assert!(
            !optional_catalog_absent(
                &DbErr::Custom("permission denied for table bookclerk_identity".into()),
                SQL_IDENTITY_TABLE
            ),
            "permission errors must not look like a missing optional catalog"
        );
        assert!(!optional_catalog_absent(
            &DbErr::Custom("syntax error at or near bookclerk_identity".into()),
            SQL_IDENTITY_TABLE
        ));
    }

    #[test]
    fn portable_ident_rejects_unsafe_names() {
        assert!(portable_ident("accounts"));
        assert!(portable_ident("_tmp"));
        assert!(!portable_ident(""));
        assert!(!portable_ident("sqlite_sequence;drop"));
        assert!(!portable_ident("pg class"));
        assert!(!portable_ident("1books"));
    }

    #[test]
    fn postgres_identity_upsert_lowers_question_marks() {
        let sql = identity_upsert_sql(DbBackend::Postgres);
        assert_eq!(
            sql.as_bytes().get(59),
            Some(&b','),
            "VALUES (?, ?) comma is at character 60 when unlowered: {sql}"
        );
        let lowered = crate::lower_canonical_sql(sea_orm::DatabaseBackend::Postgres, &sql);
        assert!(
            lowered.contains("$1") && lowered.contains("$2"),
            "{lowered}"
        );
        assert!(
            !lowered.contains('?'),
            "Postgres must not see `?` binds (syntax error at the VALUES comma): {lowered}"
        );
        assert!(lowered.contains("GREATEST("), "{lowered}");
    }
}
