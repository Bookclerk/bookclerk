//! Local SQLite engine for the database plugin (rusqlite SeaORM proxy).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use bookclerk_db_exec::{
    consume_begin_injection, consume_commit_injection, current_exec_budget, is_txn_broken,
    note_begin_failed, note_commit_failed, set_positional_result_columns, txn_broken_err,
    ExecBudget,
};
#[cfg(feature = "host-helpers")]
use bookclerk_library::{apply_host_schema, LibraryStore};
use bookclerk_plugin_abi::DbCapabilities;
use bookclerk_plugin_sdk::{DbColumn, DbType};
use rusqlite::Connection;
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement, Value,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;
use tokio::task::{try_id, Id as TaskId};

/// Warn when a proxied statement takes longer than this many milliseconds.
const SLOW_SQL_WARN_MS: u128 = 250;

#[derive(Debug)]
/// Shared rusqlite connection plus nested-transaction depth.
struct SqliteState {
    /// Process-wide rusqlite handle used by the SeaORM proxy.
    conn: Connection,
    /// Open transaction nesting (`0` = autocommit; savepoints when `> 1`).
    txn_depth: u32,
}

impl SqliteState {
    /// Starts `BEGIN IMMEDIATE` or a numbered savepoint; increments `txn_depth`.
    ///
    /// # Errors
    ///
    /// Returns a rusqlite error when the engine rejects `BEGIN` or `SAVEPOINT`.
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

    /// Commits the outer transaction or releases the innermost savepoint; no-op at depth 0.
    ///
    /// # Errors
    ///
    /// Returns a rusqlite error when the engine rejects `COMMIT` or `RELEASE`.
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

    /// Rolls back the outer transaction or the innermost savepoint; no-op at depth 0.
    ///
    /// # Errors
    ///
    /// Returns a rusqlite error when the engine rejects `ROLLBACK` or savepoint cleanup.
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
    /// Held until the outer transaction ends so other tasks cannot interleave statements.
    _guard: OwnedMutexGuard<()>,
    /// Tokio task that opened the transaction (`None` under `#[tokio::test]` `block_on`).
    owner: Option<TaskId>,
}

/// Held for the duration of one statement so it cannot run inside another
/// task's open transaction on this shared connection.
enum StatementPermit {
    /// This task already holds the exclusive transaction lease.
    OwnedByTxn,
    /// Short-lived gate lock for a statement outside an open transaction.
    Transient(#[allow(dead_code)] OwnedMutexGuard<()>),
}

/// SeaORM proxy over a shared rusqlite connection.
pub struct SqliteProxy {
    /// Shared rusqlite state (connection + transaction depth).
    conn: Arc<Mutex<SqliteState>>,
    /// Serializes top-level transactions and statements from other tasks.
    txn_gate: Arc<AsyncMutex<()>>,
    /// Current exclusive transaction lease, if a task has begun one.
    txn_lease: Arc<Mutex<Option<TxnLease>>>,
    /// Budget installed when this connection's exclusive lease is acquired.
    budget: Arc<Mutex<Arc<ExecBudget>>>,
}

impl SqliteProxy {
    /// Wraps an already-opened rusqlite connection for SeaORM proxy queries.
    ///
    /// Call after the host applies schema (see [`open`] / [`open_memory`]).
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        // TRUNCATE journal serializes writers. Two LibraryStores (or CLI +
        // daemon) on one file wait here through BEGIN IMMEDIATE. 250ms was
        // shorter than catalog paging under CI `spawn_blocking`, which turned
        // snapshot CAS into SQLITE_BUSY instead of a lost update.
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        let _ = conn.execute_batch("PRAGMA foreign_keys = ON;");
        let budget = Arc::new(Mutex::new(ExecBudget::unlimited()));
        let handler_budget = Arc::clone(&budget);
        conn.progress_handler(
            250,
            Some(move || {
                handler_budget
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .deadline_expired()
            }),
        );
        Self {
            conn: Arc::new(Mutex::new(SqliteState { conn, txn_depth: 0 })),
            txn_gate: Arc::new(AsyncMutex::new(())),
            txn_lease: Arc::new(Mutex::new(None)),
            budget,
        }
    }

    /// Copies the current request budget onto this connection.
    ///
    /// Called on `BEGIN` and on autocommit statements so catalog snapshots
    /// taken before `BEGIN IMMEDIATE` use this attempt's cap (and
    /// `suspend_execute_row_cap`), not a leftover from a prior atomic on the
    /// same proxy.
    fn install_request_budget(&self) {
        if let Some(budget) = current_exec_budget() {
            *self.budget.lock().unwrap_or_else(|e| e.into_inner()) = budget;
        }
    }

    /// Budget installed on this connection (cloned for `spawn_blocking`).
    fn connection_budget(&self) -> Arc<ExecBudget> {
        self.budget
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// True when `owner` is this Tokio task, or both sides lack a task id (sync tests).
    fn same_task(owner: Option<TaskId>) -> bool {
        match (owner, try_id()) {
            (Some(a), Some(b)) => a == b,
            // `#[tokio::test]` drives the body with `block_on`, which has no
            // task id. Sequential statements in that context own the lease.
            (None, None) => true,
            _ => false,
        }
    }

    /// Locks the rusqlite state, recovering from a poisoned mutex.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, SqliteState> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Locks the transaction lease, recovering from a poisoned mutex.
    fn lock_lease(&self) -> std::sync::MutexGuard<'_, Option<TxnLease>> {
        self.txn_lease.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drops the exclusive lease once transaction depth returns to zero.
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

/// Opens a SQLite file and returns a SeaORM proxy (no schema application).
///
/// The host applies DDL after `openSession` + capability negotiation.
///
/// # Errors
///
/// Returns [`DbErr`] when the parent directory cannot be created, the file
/// cannot be opened, or the SeaORM proxy cannot connect.
pub async fn open(path: &Path) -> std::result::Result<DatabaseConnection, DbErr> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DbErr::Custom(e.to_string()))?;
    }
    let conn = rusqlite::Connection::open(path).map_err(rusqlite_db_err)?;
    // TRUNCATE keeps a durable rollback journal without unlinking it on commit.
    // The jailed sqlite guest only has file-level Landlock grants for the DB and
    // sidecars (not the files-dir parent), so DELETE journal mode fails with
    // SQLITE_IOERR_DELETE when it tries to remove `*-journal`.
    conn.execute_batch("PRAGMA journal_mode = TRUNCATE;")
        .map_err(rusqlite_db_err)?;
    let db = Database::connect_proxy(
        DbBackend::Sqlite,
        Arc::new(Box::new(SqliteProxy::new(conn))),
    )
    .await?;
    db.ping().await?;
    tracing::debug!(path = %path.display(), plugin = "sqlite", "opened library database");
    Ok(db)
}

/// Opens an in-memory SQLite database, applies host schema, and returns a SeaORM proxy.
///
/// Intended for unit tests and dry-run paths (no durable file). Version
/// selection lives in [`bookclerk_library::apply_host_schema`].
///
/// # Errors
///
/// Returns [`bookclerk_library::LibraryError`] when the SeaORM proxy or host schema fails.
#[cfg(feature = "host-helpers")]
pub async fn open_memory() -> bookclerk_library::Result<DatabaseConnection> {
    let db = open_memory_unmigrated()
        .await
        .map_err(bookclerk_library::LibraryError::Orm)?;
    apply_host_schema(&db).await?;
    Ok(db)
}

/// Opens in-memory SQLite without applying schema (guest-shaped connect).
///
/// # Errors
///
/// Returns [`DbErr`] when the SeaORM proxy fails.
pub async fn open_memory_unmigrated() -> std::result::Result<DatabaseConnection, DbErr> {
    let conn = rusqlite::Connection::open_in_memory().map_err(rusqlite_db_err)?;
    let db = Database::connect_proxy(
        DbBackend::Sqlite,
        Arc::new(Box::new(SqliteProxy::new(conn))),
    )
    .await?;
    db.ping().await?;
    Ok(db)
}

/// Opens SQLite at `path`, applies host schema, and wraps it as a [`LibraryStore`].
///
/// Prefer this entry point from CLI / tests that need the high-level library
/// API rather than a raw [`DatabaseConnection`]. Production guests use [`open`].
///
/// # Arguments
///
/// * `path` - Absolute path to the SQLite database file.
///
/// # Errors
///
/// Propagates errors from [`open`] or host schema application.
#[cfg(feature = "host-helpers")]
pub async fn open_store(path: &Path) -> bookclerk_library::Result<LibraryStore> {
    let db = open(path)
        .await
        .map_err(bookclerk_library::LibraryError::Orm)?;
    apply_host_schema(&db).await?;
    Ok(LibraryStore::from_connection(db))
}

/// Opens an in-memory [`LibraryStore`] for tests.
///
/// # Errors
///
/// Propagates errors from [`open_memory`].
#[cfg(feature = "host-helpers")]
pub async fn open_store_memory() -> bookclerk_library::Result<LibraryStore> {
    Ok(LibraryStore::from_connection(open_memory().await?))
}

#[async_trait]
impl ProxyDatabaseTrait for SqliteProxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        if is_txn_broken() {
            return Err(txn_broken_err());
        }
        let _permit = self.acquire_for_statement().await;
        self.install_request_budget();
        let conn = self.conn.clone();
        let budget = self.connection_budget();
        budget.reset_rows_seen();
        tokio::task::spawn_blocking(move || {
            let sql_summary = summarize_sql(&statement.sql);
            let bind_count = statement.values.as_ref().map_or(0usize, |v| v.0.len());
            let started = Instant::now();
            let conn = conn
                .lock()
                .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
            let mut stmt = conn.conn.prepare(&statement.sql).map_err(rusqlite_db_err)?;
            let binds = statement_binds(&statement);
            let names: Vec<String> = (0..stmt.column_count())
                .map(|i| stmt.column_name(i).unwrap_or("").to_string())
                .collect();
            let mut seen_names = HashSet::new();
            for name in &names {
                if !name.is_empty() && !seen_names.insert(name.as_str()) {
                    return Err(DbErr::Custom(format!("duplicate column name `{name}`")));
                }
            }
            let decltypes: Vec<Option<String>> = stmt
                .columns()
                .iter()
                .map(|c| c.decl_type().map(str::to_ascii_uppercase))
                .collect();
            let positional: Vec<DbColumn> = names
                .iter()
                .zip(decltypes.iter())
                .map(|(name, decl)| DbColumn {
                    name: name.clone(),
                    db_type: db_type_from_decl(decl.as_deref()),
                })
                .collect();
            budget.set_positional_columns(positional.clone());
            set_positional_result_columns(positional);
            let mut rows = stmt
                .query(rusqlite::params_from_iter(binds.iter()))
                .map_err(rusqlite_db_err)?;
            let caps = DbCapabilities::advertised_sqlite();
            let cell_cap = usize::try_from(caps.max_cell_bytes).unwrap_or(usize::MAX);
            let mut out = Vec::new();
            let mut result_bytes = 0usize;
            while let Some(row) = rows.next().map_err(rusqlite_db_err)? {
                let mut values = BTreeMap::new();
                for (i, name) in names.iter().enumerate() {
                    let v: rusqlite::types::Value = row.get(i).map_err(rusqlite_db_err)?;
                    let decl = decltypes.get(i).and_then(Option::as_deref);
                    values.insert(name.clone(), rusqlite_to_sea(v, decl, name));
                }
                if caps.max_cell_bytes > 0 {
                    for (name, value) in &values {
                        let cell = bookclerk_db_exec::sea_value_to_json(value);
                        let n = bookclerk_db_exec::json_cell_utf8_len(&cell);
                        if n > cell_cap {
                            return Err(DbErr::Custom(format!(
                                "column `{name}` is {n} bytes; maxCellBytes is {}",
                                caps.max_cell_bytes
                            )));
                        }
                    }
                }
                let nbytes = bookclerk_db_exec::encoded_proxy_row_len(&values);
                bookclerk_db_exec::note_encoded_result_bytes(
                    &mut result_bytes,
                    nbytes,
                    caps.max_result_bytes,
                )?;
                out.push(ProxyRow { values });
                if budget.note_row() {
                    return Err(DbErr::Custom(format!(
                        "query returned {} rows; maxResultRows exceeded",
                        out.len()
                    )));
                }
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
        if is_txn_broken() {
            return Err(txn_broken_err());
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
                .map_err(rusqlite_db_err)?;
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
            conn.conn.prepare("SELECT 1").map_err(rusqlite_db_err)?;
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
        if consume_begin_injection() {
            note_begin_failed("injected begin failure");
            return;
        }
        {
            let lease = self.lock_lease();
            if let Some(l) = lease.as_ref() {
                if Self::same_task(l.owner) {
                    drop(lease);
                    let mut state = self.lock_state();
                    if let Err(err) = state.begin() {
                        note_begin_failed(format_rusqlite_error(&err));
                        tracing::error!(error = %err, "sqlite nested begin failed");
                    }
                    return;
                }
            }
        }
        let guard = self.txn_gate.clone().lock_owned().await;
        self.install_request_budget();
        {
            let mut state = self.lock_state();
            if let Err(err) = state.begin() {
                note_begin_failed(format_rusqlite_error(&err));
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
        if is_txn_broken() {
            let depth = {
                let mut state = self.lock_state();
                if let Err(err) = state.rollback() {
                    tracing::error!(error = %err, "sqlite rollback of poisoned transaction");
                }
                state.txn_depth
            };
            self.release_lease_if_idle(depth);
            return;
        }
        if consume_commit_injection() {
            note_commit_failed("injected commit failure");
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
                note_commit_failed(format_rusqlite_error(&err));
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

/// Formats a rusqlite failure so guests can classify by `SQLITE_*` code.
fn format_rusqlite_error(err: &rusqlite::Error) -> String {
    match err {
        rusqlite::Error::SqliteFailure(ffi, msg) => {
            let name = match ffi.code {
                rusqlite::ErrorCode::ConstraintViolation => "SQLITE_CONSTRAINT",
                rusqlite::ErrorCode::DatabaseBusy => "SQLITE_BUSY",
                rusqlite::ErrorCode::DatabaseLocked => "SQLITE_LOCKED",
                rusqlite::ErrorCode::OperationInterrupted => "SQLITE_INTERRUPT",
                rusqlite::ErrorCode::SystemIoFailure => "SQLITE_IOERR",
                rusqlite::ErrorCode::CannotOpen => "SQLITE_CANTOPEN",
                _ => "SQLITE_ERROR",
            };
            match msg {
                Some(detail) => format!("{name} ({}): {detail}", ffi.extended_code),
                None => format!("{name} ({})", ffi.extended_code),
            }
        }
        other => other.to_string(),
    }
}

/// Wraps a rusqlite error as [`DbErr::Custom`] with a stable `SQLITE_*` prefix.
fn rusqlite_db_err(err: rusqlite::Error) -> DbErr {
    DbErr::Custom(format_rusqlite_error(&err))
}

/// Collapses whitespace and truncates SQL to 180 UTF-8 bytes for slow-query logs.
fn summarize_sql(raw: &str) -> String {
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_LEN: usize = 180;
    if compact.len() <= MAX_LEN {
        compact
    } else {
        let end = compact.floor_char_boundary(MAX_LEN);
        format!("{}...", &compact[..end])
    }
}

/// Converts SeaORM bind values into rusqlite parameters (empty when unbound).
fn statement_binds(statement: &Statement) -> Vec<rusqlite::types::Value> {
    match &statement.values {
        Some(values) => values.0.iter().map(sea_to_rusqlite).collect(),
        None => Vec::new(),
    }
}

/// Maps a SeaORM [`Value`] to rusqlite; unhandled / NULL variants become SQL NULL.
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

/// Maps a SQLite `decl_type` onto the universal [`DbType`] (empty → Unspecified).
fn db_type_from_decl(decl: Option<&str>) -> DbType {
    decl.map_or(
        DbType::Unspecified,
        bookclerk_plugin_abi::db_type_from_declared,
    )
}

/// Maps a rusqlite cell back to SeaORM, using `decl_type` for typed NULLs.
fn rusqlite_to_sea(v: rusqlite::types::Value, decl_type: Option<&str>, column: &str) -> Value {
    match v {
        rusqlite::types::Value::Null => {
            bookclerk_plugin_sdk::database_adapter::typed_null(decl_type, column)
        }
        rusqlite::types::Value::Integer(n) => Value::BigInt(Some(n)),
        rusqlite::types::Value::Real(n) => Value::Double(Some(n)),
        rusqlite::types::Value::Text(s) => Value::String(Some(s)),
        rusqlite::types::Value::Blob(b) => Value::Bytes(Some(b)),
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::summarize_sql;

    #[test]
    fn summarize_sql_truncates_ascii() {
        let sql = "SELECT ".to_string() + &"x".repeat(200);
        let summary = summarize_sql(&sql);
        assert!(summary.ends_with("..."));
        assert_eq!(summary.len(), 183);
    }

    #[test]
    fn summarize_sql_truncates_on_utf8_char_boundary() {
        // Thai vowel U+0E35 is 3 UTF-8 bytes; a 180-byte slice lands inside it.
        let sql = format!("SELECT {}", "สวัสดี".repeat(40));
        let summary = summarize_sql(&sql);
        assert!(summary.ends_with("..."));
        let body = &summary[..summary.len() - 3];
        assert!(body.len() <= 180);
        assert!(summary.is_char_boundary(body.len()));
        assert!(!body.is_empty());
    }
}
