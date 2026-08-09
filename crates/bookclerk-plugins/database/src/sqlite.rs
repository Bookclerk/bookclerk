//! Local SQLite engine for the database plugin (rusqlite SeaORM proxy).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use bookclerk_library::{migrations, LibraryError, LibraryStore, Result};
use rusqlite::Connection;
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement, Value,
};

const SLOW_SQL_WARN_MS: u128 = 250;

/// SeaORM proxy over a shared rusqlite connection.
#[derive(Debug)]
pub struct SqliteProxy {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteProxy {
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }
}

/// Open (or create) a local SQLite database, migrate, return a SeaORM proxy.
pub async fn open(path: &Path) -> Result<DatabaseConnection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = rusqlite::Connection::open(path)?;
    // TRUNCATE keeps a durable rollback journal without unlinking it on commit.
    // The jailed sqlite guest only has file-level Landlock grants for the DB and
    // sidecars (not the files-dir parent), so DELETE journal mode fails with
    // SQLITE_IOERR_DELETE when it tries to remove `*-journal`.
    conn.execute_batch(
        "PRAGMA journal_mode = TRUNCATE;
         PRAGMA foreign_keys = ON;",
    )?;
    migrations::migrations().to_latest(&mut conn)?;
    let db = Database::connect_proxy(
        DbBackend::Sqlite,
        Arc::new(Box::new(SqliteProxy::new(conn))),
    )
    .await
    .map_err(LibraryError::Orm)?;
    tracing::debug!(path = %path.display(), plugin = "sqlite", "opened library database");
    Ok(db)
}

/// In-memory SQLite (tests / dry-run migrate).
pub async fn open_memory() -> Result<DatabaseConnection> {
    let mut conn = rusqlite::Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrations::migrations().to_latest(&mut conn)?;
    let db = Database::connect_proxy(
        DbBackend::Sqlite,
        Arc::new(Box::new(SqliteProxy::new(conn))),
    )
    .await
    .map_err(LibraryError::Orm)?;
    Ok(db)
}

/// Open SQLite at `path` and wrap as [`LibraryStore`].
pub async fn open_store(path: &Path) -> Result<LibraryStore> {
    Ok(LibraryStore::from_connection(open(path).await?))
}

/// In-memory [`LibraryStore`] (tests).
pub async fn open_store_memory() -> Result<LibraryStore> {
    Ok(LibraryStore::from_connection(open_memory().await?))
}

#[async_trait]
impl ProxyDatabaseTrait for SqliteProxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let sql_summary = summarize_sql(&statement.sql);
            let bind_count = statement.values.as_ref().map_or(0usize, |v| v.0.len());
            let started = Instant::now();
            let conn = conn
                .lock()
                .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
            let mut stmt = conn
                .prepare(&statement.sql)
                .map_err(|e| DbErr::Custom(e.to_string()))?;
            let binds = statement_binds(&statement);
            let names: Vec<String> = (0..stmt.column_count())
                .map(|i| stmt.column_name(i).unwrap_or("").to_string())
                .collect();
            let decltypes: Vec<Option<String>> = stmt
                .columns()
                .iter()
                .map(|c| c.decl_type().map(str::to_ascii_uppercase))
                .collect();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(binds.iter()))
                .map_err(|e| DbErr::Custom(e.to_string()))?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(|e| DbErr::Custom(e.to_string()))? {
                let mut values = BTreeMap::new();
                for (i, name) in names.iter().enumerate() {
                    let v: rusqlite::types::Value =
                        row.get(i).map_err(|e| DbErr::Custom(e.to_string()))?;
                    let decl = decltypes.get(i).and_then(Option::as_deref);
                    values.insert(name.clone(), rusqlite_to_sea(v, decl, name));
                }
                out.push(ProxyRow { values });
            }
            let elapsed_ms = started.elapsed().as_millis();
            if elapsed_ms >= SLOW_SQL_WARN_MS {
                tracing::warn!(
                    op = "query",
                    elapsed_ms,
                    rows = out.len(),
                    bind_count,
                    sql = %sql_summary,
                    "slow sqlite query"
                );
            } else {
                tracing::debug!(
                    op = "query",
                    elapsed_ms,
                    rows = out.len(),
                    bind_count,
                    sql = %sql_summary,
                    "sqlite query"
                );
            }
            Ok(out)
        })
        .await
        .map_err(|err| DbErr::Custom(format!("sqlite query task failed: {err}")))?
    }

    async fn execute(&self, statement: Statement) -> std::result::Result<ProxyExecResult, DbErr> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let sql_summary = summarize_sql(&statement.sql);
            let bind_count = statement.values.as_ref().map_or(0usize, |v| v.0.len());
            let started = Instant::now();
            let conn = conn
                .lock()
                .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
            let binds = statement_binds(&statement);
            conn.execute(&statement.sql, rusqlite::params_from_iter(binds.iter()))
                .map_err(|e| DbErr::Custom(e.to_string()))?;
            let elapsed_ms = started.elapsed().as_millis();
            let rows_affected = conn.changes();
            if elapsed_ms >= SLOW_SQL_WARN_MS {
                tracing::warn!(
                    op = "execute",
                    elapsed_ms,
                    rows_affected,
                    bind_count,
                    sql = %sql_summary,
                    "slow sqlite execute"
                );
            } else {
                tracing::debug!(
                    op = "execute",
                    elapsed_ms,
                    rows_affected,
                    bind_count,
                    sql = %sql_summary,
                    "sqlite execute"
                );
            }
            Ok(ProxyExecResult {
                last_insert_id: conn.last_insert_rowid() as u64,
                rows_affected,
            })
        })
        .await
        .map_err(|err| DbErr::Custom(format!("sqlite execute task failed: {err}")))?
    }

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let conn = conn
                .lock()
                .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
            conn.prepare("SELECT 1")
                .map_err(|e| DbErr::Custom(e.to_string()))?;
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "sqlite ping"
            );
            Ok(())
        })
        .await
        .map_err(|err| DbErr::Custom(format!("sqlite ping task failed: {err}")))?
    }
}

fn summarize_sql(raw: &str) -> String {
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_LEN: usize = 180;
    if compact.len() <= MAX_LEN {
        compact
    } else {
        let mut out = compact[..MAX_LEN].to_string();
        out.push_str("...");
        out
    }
}

fn statement_binds(statement: &Statement) -> Vec<rusqlite::types::Value> {
    match &statement.values {
        Some(values) => values.0.iter().map(sea_to_rusqlite).collect(),
        None => Vec::new(),
    }
}

fn sea_to_rusqlite(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as R;
    match v {
        Value::Bool(Some(b)) => R::Integer(i64::from(*b)),
        Value::TinyInt(Some(n)) => R::Integer(i64::from(*n)),
        Value::SmallInt(Some(n)) => R::Integer(i64::from(*n)),
        Value::Int(Some(n)) => R::Integer(i64::from(*n)),
        Value::BigInt(Some(n)) => R::Integer(*n),
        Value::TinyUnsigned(Some(n)) => R::Integer(i64::from(*n)),
        Value::SmallUnsigned(Some(n)) => R::Integer(i64::from(*n)),
        Value::Unsigned(Some(n)) => R::Integer(i64::from(*n)),
        Value::BigUnsigned(Some(n)) => {
            i64::try_from(*n).map_or_else(|_| R::Real(*n as f64), R::Integer)
        }
        Value::Float(Some(n)) => R::Real(f64::from(*n)),
        Value::Double(Some(n)) => R::Real(*n),
        Value::String(Some(s)) => R::Text(s.to_string()),
        Value::Char(Some(c)) => R::Text(c.to_string()),
        Value::Bytes(Some(b)) => R::Blob(b.to_vec()),
        Value::ChronoDateTimeUtc(Some(dt)) => R::Text(dt.to_rfc3339()),
        Value::ChronoDateTime(Some(dt)) => R::Text(dt.and_utc().to_rfc3339()),
        _ => R::Null,
    }
}

fn rusqlite_to_sea(v: rusqlite::types::Value, decl_type: Option<&str>, column: &str) -> Value {
    match v {
        rusqlite::types::Value::Null => crate::migrate::typed_null(decl_type, column),
        rusqlite::types::Value::Integer(n) => Value::BigInt(Some(n)),
        rusqlite::types::Value::Real(n) => Value::Double(Some(n)),
        rusqlite::types::Value::Text(s) => Value::String(Some(s)),
        rusqlite::types::Value::Blob(b) => Value::Bytes(Some(b)),
    }
}
