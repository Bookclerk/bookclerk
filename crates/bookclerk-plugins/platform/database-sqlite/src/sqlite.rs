//! Local SQLite engine for the database plugin (rusqlite SeaORM proxy).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
use tokio::sync::oneshot;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;
use tokio::task::{try_id, Id as TaskId};

/// Warn when a proxied statement takes longer than this many milliseconds.
const SLOW_SQL_WARN_MS: u128 = 250;

/// How long one `BEGIN IMMEDIATE` waits inside SQLite before returning busy.
///
/// The wait runs on a blocking thread. A multi-second timeout occupies that
/// thread while the lock holder still needs the blocking pool to finish and
/// commit, which under `cargo test --workspace` turns into `SQLITE_BUSY`.
const BEGIN_BUSY_SLICE: std::time::Duration = std::time::Duration::from_millis(50);

/// How long [`SqliteProxy::begin`] keeps retrying file-lock contention.
///
/// Attempts sleep on the async runtime so the peer transaction can be
/// scheduled. This is the bounded busy contract for two connections on one file.
/// An armed request deadline stops the loop first.
const BEGIN_CONTENTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Pause between `BEGIN IMMEDIATE` attempts after SQLite reports the file is locked.
const BEGIN_CONTENTION_PAUSE: std::time::Duration = std::time::Duration::from_millis(20);

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
    ///
    /// # Errors
    ///
    /// Returns when this connection refuses the query-deadline progress
    /// handler. rusqlite 0.38+ returns that error when the handle is not owned
    /// by this [`Connection`], which would leave statements without a deadline.
    pub fn new(conn: Connection) -> rusqlite::Result<Self> {
        // TRUNCATE journal serializes writers. Each attempt waits only
        // [`BEGIN_BUSY_SLICE`]; [`SqliteProxy::begin`] retries on the async
        // runtime up to [`BEGIN_CONTENTION_BUDGET`] so a peer can commit.
        let _ = conn.busy_timeout(BEGIN_BUSY_SLICE);
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
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(SqliteState { conn, txn_depth: 0 })),
            txn_gate: Arc::new(AsyncMutex::new(())),
            txn_lease: Arc::new(Mutex::new(None)),
            budget,
        })
    }

    /// Copies the current request budget onto this connection.
    ///
    /// Called on `BEGIN` and on autocommit statements so catalog snapshots
    /// taken before `BEGIN IMMEDIATE` use this attempt's cap (and
    /// `suspend_execute_row_cap`), not a leftover from a prior atomic on the
    /// same proxy.
    fn install_request_budget(&self) {
        let next = current_exec_budget().unwrap_or_else(ExecBudget::unlimited);
        *self.budget.lock().unwrap_or_else(|e| e.into_inner()) = next;
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

    /// True when the armed request deadline has elapsed.
    fn begin_deadline_expired(&self) -> bool {
        self.connection_budget().deadline_expired()
    }

    /// True when another busy attempt may still finish inside the budget and deadline.
    fn begin_may_retry(&self, started: Instant) -> bool {
        started.elapsed() < BEGIN_CONTENTION_BUDGET && !self.begin_deadline_expired()
    }

    /// Pause before the next busy attempt, never longer than the armed deadline.
    fn begin_retry_pause(&self) -> std::time::Duration {
        let Some(left_ms) = self.connection_budget().remaining_ms() else {
            return BEGIN_CONTENTION_PAUSE;
        };
        BEGIN_CONTENTION_PAUSE.min(std::time::Duration::from_millis(left_ms))
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

/// Rejects empty/NUL configured database paths; returns `path` otherwise.
///
/// The path is a trusted configured location (may lexically contain `..` from
/// `files_dir`). Do not reject `..` substrings or convert through UTF-8 lossy.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when the path is empty or contains an interior NUL.
fn validated_db_path(path: &Path) -> std::result::Result<PathBuf, DbErr> {
    if path.as_os_str().is_empty() {
        return Err(DbErr::Custom("refusing empty database path".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if path.as_os_str().as_bytes().contains(&0) {
            return Err(DbErr::Custom(format!(
                "refusing database path with interior NUL: {}",
                path.display()
            )));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if path.as_os_str().encode_wide().any(|c| c == 0) {
            return Err(DbErr::Custom(format!(
                "refusing database path with interior NUL: {}",
                path.display()
            )));
        }
    }
    Ok(path.to_path_buf())
}

/// Opens a SQLite file and returns a SeaORM proxy (no schema application).
///
/// The host applies DDL after `openSession` + capability negotiation.
///
/// # Errors
///
/// Returns [`DbErr`] when the parent directory cannot be created, the file
/// cannot be opened, the query-deadline handler cannot be installed, or the
/// SeaORM proxy cannot connect.
pub async fn open(path: &Path) -> std::result::Result<DatabaseConnection, DbErr> {
    let path = validated_db_path(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DbErr::Custom(e.to_string()))?;
    }
    let conn = rusqlite::Connection::open(&path).map_err(rusqlite_db_err)?;
    // TRUNCATE keeps a durable rollback journal without unlinking it on commit.
    // The jailed sqlite guest only has file-level Landlock grants for the DB and
    // sidecars (not the files-dir parent), so DELETE journal mode fails with
    // SQLITE_IOERR_DELETE when it tries to remove `*-journal`.
    conn.execute_batch("PRAGMA journal_mode = TRUNCATE;")
        .map_err(rusqlite_db_err)?;
    let db = Database::connect_proxy(
        DbBackend::Sqlite,
        Arc::new(Box::new(SqliteProxy::new(conn).map_err(rusqlite_db_err)?)),
    )
    .await?;
    db.ping().await?;
    tracing::debug!(path = %path.display(), plugin = "sqlite", "opened library database");
    Ok(db)
}

/// Deletes a SQLite binding unit and journal sidecars. Missing files are success.
///
/// Called from the sqlite adapter's `Database.dropUnit` implementation so the
/// host does not link this crate to unlink plugin-database files.
///
/// # Errors
///
/// Returns when a sidecar exists but cannot be removed.
pub fn drop_unit_files(unit_ref: &str) -> std::result::Result<(), DbErr> {
    if unit_ref.trim().is_empty() {
        return Err(DbErr::Custom(
            "sqlite dropUnit requires a non-empty unitRef path".into(),
        ));
    }
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = format!("{unit_ref}{suffix}");
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(DbErr::Custom(format!("could not delete {path}: {err}")));
            }
        }
    }
    Ok(())
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
/// Returns [`DbErr`] when the in-memory database cannot be opened, the
/// query-deadline handler cannot be installed, or the SeaORM proxy fails.
pub async fn open_memory_unmigrated() -> std::result::Result<DatabaseConnection, DbErr> {
    let conn = rusqlite::Connection::open_in_memory().map_err(rusqlite_db_err)?;
    let db = Database::connect_proxy(
        DbBackend::Sqlite,
        Arc::new(Box::new(SqliteProxy::new(conn).map_err(rusqlite_db_err)?)),
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
        let mut gate = self.txn_gate.clone().lock_owned().await;
        self.install_request_budget();
        let started = Instant::now();
        loop {
            if self.begin_deadline_expired() {
                note_begin_failed("deadline_exceeded: atomic deadline elapsed");
                tracing::error!("sqlite begin stopped at the request deadline");
                return;
            }
            // The blocking attempt owns `gate` from the moment it is queued.
            // Dropping this future only drops the oneshot receiver, so a Tokio
            // worker never waits for the blocking pool. The attempt rolls a
            // successful `BEGIN` back before it releases the gate.
            let (tx, rx) = oneshot::channel();
            let conn = Arc::clone(&self.conn);
            let budget = self.connection_budget();
            let attempt_gate = gate;
            tokio::task::spawn_blocking(move || blocking_begin(conn, attempt_gate, budget, tx));
            match rx.await {
                Ok(BeginHandoff::Opened(held)) => {
                    if self.begin_deadline_expired() {
                        note_begin_failed("deadline_exceeded: atomic deadline elapsed");
                        tracing::error!("sqlite begin stopped at the request deadline");
                        return;
                    }
                    let guard = held.adopt();
                    *self.lock_lease() = Some(TxnLease {
                        _guard: guard,
                        owner: try_id(),
                    });
                    return;
                }
                Ok(BeginHandoff::Busy(err, returned)) if self.begin_may_retry(started) => {
                    tracing::debug!(error = %err, "sqlite begin waiting for the file lock");
                    gate = returned;
                    tokio::time::sleep(self.begin_retry_pause()).await;
                }
                Ok(BeginHandoff::Busy(err, returned) | BeginHandoff::Failed(err, returned)) => {
                    drop(returned);
                    if self.begin_deadline_expired() {
                        note_begin_failed("deadline_exceeded: atomic deadline elapsed");
                        tracing::error!(error = %err, "sqlite begin stopped at the request deadline");
                    } else {
                        note_begin_failed(format_rusqlite_error(&err));
                        tracing::error!(error = %err, "sqlite begin failed");
                    }
                    return;
                }
                Ok(BeginHandoff::Deadline) => {
                    note_begin_failed("deadline_exceeded: atomic deadline elapsed");
                    tracing::error!("sqlite begin stopped at the request deadline");
                    return;
                }
                Err(_closed) => {
                    note_begin_failed("sqlite begin task failed");
                    tracing::error!("sqlite begin task ended without a result");
                    return;
                }
            }
        }
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

/// Result of one off-thread `BEGIN IMMEDIATE`, sent only if the waiter is still there.
enum BeginHandoff {
    /// Transaction is open. Drop rolls it back unless [`BeginHold::adopt`] runs.
    Opened(BeginHold),
    /// File lock is held by another connection. The gate comes back for a retry.
    Busy(rusqlite::Error, OwnedMutexGuard<()>),
    /// Engine rejected `BEGIN`. The gate comes back so the caller can drop it.
    Failed(rusqlite::Error, OwnedMutexGuard<()>),
    /// The armed request deadline elapsed before the transaction was adopted.
    Deadline,
}

/// Gate plus rollback duty for one `BEGIN`.
///
/// Lives on the blocking thread until it is sent to the waiter. Drop rolls
/// back an open transaction before releasing the gate, on whichever thread
/// drops it. That drop does not wait for other blocking work.
struct BeginHold {
    /// Shared rusqlite state.
    conn: Arc<Mutex<SqliteState>>,
    /// Exclusive connection gate.
    gate: Option<OwnedMutexGuard<()>>,
    /// `BEGIN` succeeded and has not been adopted or rolled back yet.
    open: bool,
    /// The caller stored the gate in a transaction lease.
    adopted: bool,
}

impl BeginHold {
    /// Keeps the open transaction and returns its gate.
    ///
    /// # Panics
    ///
    /// Panics if the gate was already taken.
    fn adopt(mut self) -> OwnedMutexGuard<()> {
        self.adopted = true;
        self.open = false;
        self.gate.take().expect("begin gate still held")
    }
}

impl Drop for BeginHold {
    fn drop(&mut self) {
        if self.open && !self.adopted {
            let mut state = self.conn.lock().unwrap_or_else(|err| err.into_inner());
            if let Err(err) = state.rollback() {
                tracing::error!(error = %err, "sqlite rollback of abandoned begin");
            }
        }
        drop(self.gate.take());
    }
}

/// `BEGIN IMMEDIATE` on the blocking pool.
///
/// The oneshot receiver is the cancellation signal. A closed receiver means
/// the async waiter is gone: do not `BEGIN`, or roll back a `BEGIN` that
/// already landed, then drop the gate. This function never waits for the waiter.
fn blocking_begin(
    conn: Arc<Mutex<SqliteState>>,
    gate: OwnedMutexGuard<()>,
    budget: Arc<ExecBudget>,
    tx: oneshot::Sender<BeginHandoff>,
) {
    let mut hold = BeginHold {
        conn,
        gate: Some(gate),
        open: false,
        adopted: false,
    };
    pause_begin_for_test(BeginPausePoint::BeforeBegin);
    if tx.is_closed() {
        return;
    }
    if budget.deadline_expired() {
        let _ = tx.send(BeginHandoff::Deadline);
        return;
    }
    let began = {
        let mut state = hold.conn.lock().unwrap_or_else(|err| err.into_inner());
        state.begin()
    };
    match began {
        Err(err) if is_sqlite_lock_contention(&err) => {
            if let Some(gate) = hold.gate.take() {
                let _ = tx.send(BeginHandoff::Busy(err, gate));
            }
        }
        Err(err) => {
            if let Some(gate) = hold.gate.take() {
                let _ = tx.send(BeginHandoff::Failed(err, gate));
            }
        }
        Ok(()) => {
            hold.open = true;
            pause_begin_for_test(BeginPausePoint::AfterBegin);
            if tx.is_closed() {
                return;
            }
            if budget.deadline_expired() {
                let _ = tx.send(BeginHandoff::Deadline);
                return;
            }
            let conn = Arc::clone(&hold.conn);
            let Some(gate) = hold.gate.take() else {
                return;
            };
            hold.open = false;
            let handed = BeginHold {
                conn,
                gate: Some(gate),
                open: true,
                adopted: false,
            };
            let _ = tx.send(BeginHandoff::Opened(handed));
        }
    }
}

/// Where a test may pause an in-flight `BEGIN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BeginPausePoint {
    /// Before the engine `BEGIN`, including while the attempt is still queued.
    BeforeBegin,
    /// After `BEGIN` succeeded and before the caller adopts it.
    AfterBegin,
}

#[cfg(test)]
struct BeginPause {
    /// Which point in `BEGIN` this pause occupies.
    point: BeginPausePoint,
    /// Signaled once the blocking attempt is inside the pause.
    entered: std::sync::mpsc::Sender<()>,
    /// Released by the test after it has cancelled the waiter. Not a cancel flag.
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static BEGIN_PAUSE: Mutex<Option<BeginPause>> = Mutex::new(None);

#[cfg(test)]
fn arm_begin_pause(
    point: BeginPausePoint,
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
) {
    *BEGIN_PAUSE.lock().unwrap_or_else(|err| err.into_inner()) = Some(BeginPause {
        point,
        entered,
        release,
    });
}

/// Blocks the blocking thread at `point` until the test releases it.
///
/// The pause does not cancel the attempt. Cancellation is the dropped oneshot
/// receiver, observed after this function returns.
#[cfg(test)]
fn pause_begin_for_test(point: BeginPausePoint) {
    let pause = {
        let mut slot = BEGIN_PAUSE.lock().unwrap_or_else(|err| err.into_inner());
        if slot.as_ref().is_some_and(|pause| pause.point == point) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(pause) = pause {
        let _ = pause.entered.send(());
        let _ = pause.release.recv();
    }
}

/// Production builds never pause `BEGIN`.
#[cfg(not(test))]
fn pause_begin_for_test(_point: BeginPausePoint) {}

/// True when `err` is a file lock another connection still holds.
fn is_sqlite_lock_contention(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(ffi, _)
            if matches!(
                ffi.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
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

    async fn open_pair() -> (
        sea_orm::DatabaseConnection,
        sea_orm::DatabaseConnection,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("begin.db");
        let first = super::open(&path).await.expect("first");
        let second = super::open(&path).await.expect("second");
        (first, second, dir)
    }

    async fn commit_marker(db: &sea_orm::DatabaseConnection, table: &str) {
        use sea_orm::{ConnectionTrait, TransactionTrait};
        let txn = db.begin().await.expect("begin");
        txn.execute_unprepared(&format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY)"))
            .await
            .unwrap_or_else(|err| panic!("create {table}: {err}"));
        txn.commit().await.expect("commit");
    }

    /// Aborts the in-flight begin and unblocks its pause if the test unwinds.
    struct InflightBegin {
        /// Unblocks the blocking thread. Does not itself mean cancel.
        release: Option<std::sync::mpsc::Sender<()>>,
        /// Waiter whose future is dropped on unwind.
        task: Option<tokio::task::JoinHandle<()>>,
    }

    impl Drop for InflightBegin {
        fn drop(&mut self) {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            if let Some(tx) = self.release.take() {
                let _ = tx.send(());
            }
        }
    }

    fn release_paused_begin(release: std::sync::mpsc::Sender<()>) {
        let _ = release.send(());
    }

    async fn cancel_begin_at(point: super::BeginPausePoint, label: &str) {
        use sea_orm::ConnectionTrait;
        use std::sync::mpsc;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        super::arm_begin_pause(point, entered_tx, release_rx);
        let (db, peer, _dir) = open_pair().await;
        let same = db.clone();
        let task = tokio::spawn(async move {
            use sea_orm::TransactionTrait;
            let _ = db.begin().await;
        });
        let mut inflight = InflightBegin {
            release: Some(release_tx),
            task: Some(task),
        };
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_else(|_| panic!("{label}: blocking begin did not reach {point:?}"));
        let (started_tx, started_rx) = mpsc::channel();
        let same_table = format!("{label}_same");
        let blocked = tokio::spawn(async move {
            let _ = started_tx.send(());
            commit_marker(&same, &same_table).await;
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_else(|_| panic!("{label}: same-connection request did not start"));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !blocked.is_finished(),
            "{label}: another request finished while BEGIN was still in flight"
        );
        let task = inflight.task.take().expect("begin task");
        let release = inflight.release.take().expect("pause release");
        task.abort();
        let joined = tokio::time::timeout(std::time::Duration::from_millis(500), task)
            .await
            .unwrap_or_else(|_| panic!("{label}: cancelling BEGIN waited on the blocking thread"));
        assert!(
            joined
                .expect_err("BEGIN finished instead of cancelling")
                .is_cancelled(),
            "{label}: BEGIN task was not cancelled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !blocked.is_finished(),
            "{label}: gate released before the blocking attempt cleaned up"
        );
        release_paused_begin(release);
        tokio::time::timeout(std::time::Duration::from_secs(3), blocked)
            .await
            .unwrap_or_else(|_| panic!("{label}: same connection still holds the write lock"))
            .unwrap_or_else(|err| panic!("{label}: same-connection task failed: {err}"));
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            commit_marker(&peer, &format!("{label}_peer")),
        )
        .await
        .unwrap_or_else(|_| panic!("{label}: peer connection still blocked"));
        peer.query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            format!("SELECT COUNT(*) AS n FROM {label}_same"),
        ))
        .await
        .unwrap_or_else(|err| panic!("{label}: committed work is not visible: {err}"))
        .unwrap_or_else(|| panic!("{label}: committed table missing"));
        *super::BEGIN_PAUSE
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = None;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn cancelled_begin_does_not_leak_a_write_lock_or_share_a_transaction() {
        cancel_begin_at(super::BeginPausePoint::BeforeBegin, "before").await;
        cancel_begin_at(super::BeginPausePoint::AfterBegin, "after").await;
        *super::BEGIN_PAUSE
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = None;
        let _ = bookclerk_db_exec::take_txn_fault();
        deadline_stops_a_contended_begin().await;
        queued_begin_cancel_on_a_saturated_pool_keeps_the_runtime_alive().await;
    }

    /// External watchdog around a current-thread runtime whose blocking pool has one thread.
    async fn queued_begin_cancel_on_a_saturated_pool_keeps_the_runtime_alive() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                saturated_pool_begin_scenario,
            ));
            let _ = done_tx.send(result);
        });
        match done_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(())) => {
                let _ = worker.join();
            }
            Ok(Err(payload)) => {
                let _ = worker.join();
                std::panic::resume_unwind(payload);
            }
            Err(_) => {
                eprintln!("watchdog: cancelling a queued BEGIN blocked the runtime");
                std::process::exit(1);
            }
        }
    }

    /// One blocking thread waits on an async signal. `BEGIN` queues behind it.
    ///
    /// Cancelling the waiter must let an async heartbeat keep advancing, then
    /// both connections must be able to commit once the pool drains.
    fn saturated_pool_begin_scenario() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            use sea_orm::ConnectionTrait;
            use std::sync::atomic::{AtomicU64, Ordering};
            let beats = std::sync::Arc::new(AtomicU64::new(0));
            let beats_task = std::sync::Arc::clone(&beats);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    beats_task.fetch_add(1, Ordering::SeqCst);
                }
            });

            let (db, peer, _dir) = open_pair().await;
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = entered_tx.send(());
                let _ = go_rx.blocking_recv();
            });
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("blocking pool was not occupied");

            let same = db.clone();
            let begin_task = tokio::spawn(async move {
                use sea_orm::TransactionTrait;
                let _ = db.begin().await;
            });
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            assert!(
                !begin_task.is_finished(),
                "BEGIN ran while the only blocking thread was occupied"
            );
            let before = beats.load(Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                beats.load(Ordering::SeqCst) > before,
                "heartbeat stalled while BEGIN was queued"
            );

            begin_task.abort();
            let joined = tokio::time::timeout(std::time::Duration::from_millis(500), begin_task)
                .await
                .expect("cancelling a queued BEGIN blocked the runtime");
            assert!(
                joined
                    .expect_err("queued BEGIN finished instead of cancelling")
                    .is_cancelled(),
                "queued BEGIN task was not cancelled"
            );
            let before = beats.load(Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                beats.load(Ordering::SeqCst) > before,
                "heartbeat stalled after cancelling queued BEGIN"
            );

            let _ = go_tx.send(());
            tokio::time::timeout(std::time::Duration::from_secs(2), blocker)
                .await
                .expect("pool occupant did not finish")
                .expect("pool occupant panicked");
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                commit_marker(&same, "queued_same"),
            )
            .await
            .expect("same connection still holds a transaction");
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                commit_marker(&peer, "queued_peer"),
            )
            .await
            .expect("peer connection still blocked");
            peer.query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS n FROM queued_same",
            ))
            .await
            .expect("committed work is not visible")
            .expect("committed table missing");
        });
    }

    async fn deadline_stops_a_contended_begin() {
        use sea_orm::{ConnectionTrait, TransactionTrait};
        use std::time::{SystemTime, UNIX_EPOCH};
        let (holder, waiter, _dir) = open_pair().await;
        let held = holder.begin().await.expect("holder begin");
        held.execute_unprepared("CREATE TABLE held (id INTEGER PRIMARY KEY)")
            .await
            .expect("hold lock");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        let budget = bookclerk_db_exec::ExecBudget::new(Some(now_ms.saturating_add(200)), 0);
        let started = std::time::Instant::now();
        let err = bookclerk_db_exec::with_exec_budget(budget, || async {
            let _txn = waiter.begin().await;
            waiter
                .execute_unprepared("CREATE TABLE should_not_run (id INTEGER PRIMARY KEY)")
                .await
        })
        .await
        .expect_err("deadline must fail the begin");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "deadline retry waited {:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("deadline"), "{err}");
        let _ = bookclerk_db_exec::take_txn_fault();
        held.commit().await.expect("release holder");
        commit_marker(&waiter, "after_deadline").await;
    }

    #[test]
    fn drop_unit_files_removes_db_and_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binding.db");
        std::fs::write(&path, b"sqlite").expect("db file");
        let unit = path.display().to_string();
        std::fs::write(format!("{unit}-wal"), b"wal").expect("wal");
        std::fs::write(format!("{unit}-shm"), b"shm").expect("shm");
        std::fs::write(format!("{unit}-journal"), b"j").expect("journal");
        super::drop_unit_files(&unit).expect("drop sqlite unit");
        assert!(!path.exists(), "binding file must be gone");
        assert!(!std::path::Path::new(&format!("{unit}-wal")).exists());
        assert!(!std::path::Path::new(&format!("{unit}-shm")).exists());
        assert!(!std::path::Path::new(&format!("{unit}-journal")).exists());
        super::drop_unit_files(&unit).expect("missing unit is success");
    }
}
