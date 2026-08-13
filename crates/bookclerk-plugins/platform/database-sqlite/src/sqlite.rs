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
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;
use tokio::task::{try_id, Id as TaskId};

const SLOW_SQL_WARN_MS: u128 = 250;

#[derive(Debug)]
struct SqliteState {
    conn: Connection,
    txn_depth: u32,
}

impl SqliteState {
    fn begin(&mut self) -> rusqlite::Result<()> {
        if self.txn_depth == 0 {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
        } else {
            self.conn
                .execute_batch(&format!("SAVEPOINT sp_{}", self.txn_depth))?;
        }
        self.txn_depth += 1;
        Ok(())
    }

    fn commit(&mut self) -> rusqlite::Result<()> {
        if self.txn_depth == 0 {
            return Ok(());
        }
        if self.txn_depth == 1 {
            self.conn.execute_batch("COMMIT")?;
        } else {
            self.conn
                .execute_batch(&format!("RELEASE SAVEPOINT sp_{}", self.txn_depth - 1))?;
        }
        self.txn_depth -= 1;
        Ok(())
    }

    fn rollback(&mut self) -> rusqlite::Result<()> {
        if self.txn_depth == 0 {
            return Ok(());
        }
        if self.txn_depth == 1 {
            self.conn.execute_batch("ROLLBACK")?;
        } else {
            let name = format!("sp_{}", self.txn_depth - 1);
            self.conn
                .execute_batch(&format!("ROLLBACK TO SAVEPOINT {name}"))?;
            self.conn
                .execute_batch(&format!("RELEASE SAVEPOINT {name}"))?;
        }
        self.txn_depth -= 1;
        Ok(())
    }
}

/// Exclusive connection lease for an open SeaORM transaction.
struct TxnLease {
    _guard: OwnedMutexGuard<()>,
    owner: Option<TaskId>,
}

/// Held for the duration of one statement so it cannot run inside another
/// task's open transaction on this shared connection.
enum StatementPermit {
    OwnedByTxn,
    Transient(#[allow(dead_code)] OwnedMutexGuard<()>),
}

/// SeaORM proxy over a shared rusqlite connection.
pub struct SqliteProxy {
    conn: Arc<Mutex<SqliteState>>,
    /// Serializes top-level transactions and statements from other tasks.
    txn_gate: Arc<AsyncMutex<()>>,
    txn_lease: Arc<Mutex<Option<TxnLease>>>,
}

impl SqliteProxy {
    /// Wraps an already-opened rusqlite connection for SeaORM proxy queries.
    ///
    /// Call after applying migrations (see [`open`] / [`open_memory`]).
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(SqliteState { conn, txn_depth: 0 })),
            txn_gate: Arc::new(AsyncMutex::new(())),
            txn_lease: Arc::new(Mutex::new(None)),
        }
    }

    fn same_task(owner: Option<TaskId>) -> bool {
        match (owner, try_id()) {
            (Some(a), Some(b)) => a == b,
            // `#[tokio::test]` drives the body with `block_on`, which has no
            // task id. Sequential statements in that context own the lease.
            (None, None) => true,
            _ => false,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, SqliteState> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_lease(&self) -> std::sync::MutexGuard<'_, Option<TxnLease>> {
        self.txn_lease.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn release_lease_if_idle(&self, depth: u32) {
        if depth == 0 {
            *self.lock_lease() = None;
        }
    }

    /// Wait until this task may use the shared connection.
    ///
    /// SeaORM sends every statement through the same proxy, so a `BEGIN` from
    /// one task would otherwise include other tasks' queries in that SQLite
    /// transaction. Nested `begin` from the owning task uses savepoints.
    async fn acquire_for_statement(&self) -> StatementPermit {
        {
            let lease = self.lock_lease();
            if let Some(l) = lease.as_ref() {
                if Self::same_task(l.owner) {
                    return StatementPermit::OwnedByTxn;
                }
            }
        }
        StatementPermit::Transient(self.txn_gate.clone().lock_owned().await)
    }
}

impl std::fmt::Debug for SqliteProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteProxy").finish_non_exhaustive()
    }
}

/// Opens (or creates) a SQLite file, runs library migrations, and returns a SeaORM proxy.
///
/// Use for the production `library.db` path. Journal mode is `TRUNCATE` so
/// jailed guests without parent-dir unlink rights can still commit.
///
/// # Arguments
///
/// * `path` - Absolute path to the SQLite database file.
///
/// # Errors
///
/// Returns [`LibraryError`] when the parent directory cannot be created, the
/// file cannot be opened, migrations fail, or the SeaORM proxy cannot connect.
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

/// Opens an in-memory SQLite database, migrates, and returns a SeaORM proxy.
///
/// Intended for unit tests and dry-run migrate paths (no durable file).
///
/// # Errors
///
/// Returns [`LibraryError`] when migrations or the SeaORM proxy fail.
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

/// Opens SQLite at `path`, migrates, and wraps it as a [`LibraryStore`].
///
/// Prefer this entry point from CLI / daemon hosts that need the high-level
/// library API rather than a raw [`DatabaseConnection`].
///
/// # Arguments
///
/// * `path` - Absolute path to the SQLite database file.
///
/// # Errors
///
/// Propagates errors from [`open`].
pub async fn open_store(path: &Path) -> Result<LibraryStore> {
    Ok(LibraryStore::from_connection(open(path).await?))
}

/// Opens an in-memory [`LibraryStore`] for tests.
///
/// # Errors
///
/// Propagates errors from [`open_memory`].
pub async fn open_store_memory() -> Result<LibraryStore> {
    Ok(LibraryStore::from_connection(open_memory().await?))
}

#[async_trait]
impl ProxyDatabaseTrait for SqliteProxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let _permit = self.acquire_for_statement().await;
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let sql_summary = summarize_sql(&statement.sql);
            let bind_count = statement.values.as_ref().map_or(0usize, |v| v.0.len());
            let started = Instant::now();
            let conn = conn
                .lock()
                .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
            let mut stmt = conn
                .conn
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
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let _permit = self.acquire_for_statement().await;
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let sql_summary = summarize_sql(&statement.sql);
            let bind_count = statement.values.as_ref().map_or(0usize, |v| v.0.len());
            let started = Instant::now();
            let conn = conn
                .lock()
                .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
            let binds = statement_binds(&statement);
            conn.conn
                .execute(&statement.sql, rusqlite::params_from_iter(binds.iter()))
                .map_err(|e| DbErr::Custom(e.to_string()))?;
            let elapsed_ms = started.elapsed().as_millis();
            let rows_affected = conn.conn.changes();
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
                last_insert_id: conn.conn.last_insert_rowid() as u64,
                rows_affected,
            })
        })
        .await
        .map_err(|err| DbErr::Custom(format!("sqlite execute task failed: {err}")))?
    }

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        let _permit = self.acquire_for_statement().await;
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let conn = conn
                .lock()
                .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
            conn.conn
                .prepare("SELECT 1")
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

    async fn begin(&self) {
        if bookclerk_library::consume_begin_injection() {
            bookclerk_library::note_begin_failed("injected begin failure");
            return;
        }
        {
            let lease = self.lock_lease();
            if let Some(l) = lease.as_ref() {
                if Self::same_task(l.owner) {
                    drop(lease);
                    let mut state = self.lock_state();
                    if let Err(err) = state.begin() {
                        bookclerk_library::note_begin_failed(&err);
                        tracing::error!(error = %err, "sqlite nested begin failed");
                    }
                    return;
                }
            }
        }
        let guard = self.txn_gate.clone().lock_owned().await;
        {
            let mut state = self.lock_state();
            if let Err(err) = state.begin() {
                bookclerk_library::note_begin_failed(&err);
                tracing::error!(error = %err, "sqlite begin failed");
                return;
            }
        }
        *self.lock_lease() = Some(TxnLease {
            _guard: guard,
            owner: try_id(),
        });
    }

    async fn commit(&self) {
        if bookclerk_library::consume_commit_injection() {
            bookclerk_library::note_commit_failed("injected commit failure");
            let depth = {
                let mut state = self.lock_state();
                if let Err(err) = state.rollback() {
                    tracing::error!(error = %err, "sqlite rollback after injected commit failure");
                }
                state.txn_depth
            };
            self.release_lease_if_idle(depth);
            return;
        }
        let depth = {
            let mut state = self.lock_state();
            if let Err(err) = state.commit() {
                bookclerk_library::note_commit_failed(&err);
                tracing::error!(error = %err, "sqlite commit failed");
                if let Err(rb) = state.rollback() {
                    tracing::error!(error = %rb, "sqlite rollback after commit failure");
                }
            }
            state.txn_depth
        };
        self.release_lease_if_idle(depth);
    }

    async fn rollback(&self) {
        let depth = {
            let mut state = self.lock_state();
            if let Err(err) = state.rollback() {
                tracing::error!(error = %err, "sqlite rollback failed");
            }
            state.txn_depth
        };
        self.release_lease_if_idle(depth);
    }

    fn start_rollback(&self) {
        let depth = {
            let mut state = self.lock_state();
            let _ = state.rollback();
            state.txn_depth
        };
        self.release_lease_if_idle(depth);
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
        rusqlite::types::Value::Null => bookclerk_db_guest::migrate::typed_null(decl_type, column),
        rusqlite::types::Value::Integer(n) => Value::BigInt(Some(n)),
        rusqlite::types::Value::Real(n) => Value::Double(Some(n)),
        rusqlite::types::Value::Text(s) => Value::String(Some(s)),
        rusqlite::types::Value::Blob(b) => Value::Bytes(Some(b)),
    }
}
