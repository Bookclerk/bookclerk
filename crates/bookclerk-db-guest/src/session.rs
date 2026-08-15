//! Per-process SeaORM connection and transaction workers for a database guest.
//!
//! Each RPC arrives on a new Tokio task. SQLite's in-process proxy leases an
//! open `BEGIN` to the task that called `begin`, so routing statements through
//! a dedicated worker task keeps that lease valid until commit/rollback.
//! The same worker serializes Postgres connection use. D1 guests reject
//! `dbBegin` and implement `dbAtomic` (one HTTP batch). SQLite and Postgres
//! also implement `dbAtomic` as one native transaction plus a durable receipt.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};

use bookclerk_plugin_sdk::v2::{QueryPage, MAX_LIST_PAGE, MAX_SCALAR_BYTES};
use bookclerk_plugin_sdk::{
    proxy_rows_to_dto, statement_from_dto, DbAtomicRequest, DbAtomicResult, ExecResultDto,
    ProxyRowDto, QueryResultDto, StatementDto,
};
use futures::TryStreamExt;
use sea_orm::{
    from_query_result_to_proxy_row, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    DbBackend, StreamTrait, TransactionSession, TransactionTrait,
};
use tokio::sync::{mpsc, oneshot, Mutex, OwnedMutexGuard};

/// Guest RPC result; errors are operator-facing strings (no structured code).
type Result<T> = std::result::Result<T, String>;

/// Process-wide SeaORM connection and txn-id → worker routing table.
struct Session {
    /// Opened engine connection; `None` until [`set_connection`] runs.
    conn: Option<DatabaseConnection>,
    /// Every live txn id (root and nested) routes to its worker.
    routes: HashMap<String, mpsc::Sender<TxnOp>>,
}

/// Work item sent to a dedicated transaction worker task.
enum TxnOp {
    /// Runs a read-only statement on the named txn (or nested savepoint).
    Query {
        /// Opaque txn id the host attached to this statement or finish call.
        txn_id: String,
        /// RPC statement DTO (SQL + params) from the host bridge.
        dto: StatementDto,
        /// Oneshot used to return query rows to the RPC task.
        reply: oneshot::Sender<Result<QueryResultDto>>,
    },
    /// Paged read: adapter fetches at most `limit + 1` rows.
    QueryPage {
        /// Opaque txn id the host attached to this statement or finish call.
        txn_id: String,
        /// RPC statement DTO (SQL + params) from the host bridge.
        dto: StatementDto,
        /// Numeric offset cursor (`""` for the first page).
        cursor: String,
        /// Page size; `0` means [`MAX_LIST_PAGE`].
        limit: u32,
        /// Oneshot used to return one bounded page.
        reply: oneshot::Sender<Result<QueryPage>>,
    },
    /// Runs a mutating statement on the named txn (or nested savepoint).
    Execute {
        /// Opaque txn id the host attached to this statement or finish call.
        txn_id: String,
        /// RPC statement DTO (SQL + params) from the host bridge.
        dto: StatementDto,
        /// Oneshot used to return last-insert id / rows-affected to the RPC task.
        reply: oneshot::Sender<Result<ExecResultDto>>,
    },
    /// Opens a nested savepoint on the parent worker and returns a new txn id.
    BeginNested {
        /// Parent txn id that must be the innermost savepoint on this worker.
        parent_txn_id: String,
        /// Oneshot used to return the nested txn id (or an error string).
        reply: oneshot::Sender<Result<String>>,
    },
    /// Commits the named txn or savepoint; root commit ends the worker.
    Commit {
        /// Opaque txn id the host attached to this statement or finish call.
        txn_id: String,
        /// Oneshot used to return commit/rollback success or an engine error.
        reply: oneshot::Sender<Result<()>>,
    },
    /// Rolls back the named txn or savepoint; root rollback ends the worker.
    Rollback {
        /// Opaque txn id the host attached to this statement or finish call.
        txn_id: String,
        /// Oneshot used to return commit/rollback success or an engine error.
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Process-wide session; one connection and a map of live txn routes.
static SESSION: LazyLock<Mutex<Session>> = LazyLock::new(|| {
    Mutex::new(Session {
        conn: None,
        routes: HashMap::new(),
    })
});

/// Mutex that serializes top-level begins so SQLite never interleaves writers.
fn txn_gate() -> Arc<Mutex<()>> {
    static GATE: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(Mutex::new(()))).clone()
}

/// Store the opened engine connection for subsequent ping/query/execute calls.
///
/// # Arguments
///
/// * `conn` - Opened SeaORM database connection for this guest process.
pub async fn set_connection(conn: DatabaseConnection) {
    SESSION.lock().await.conn = Some(conn);
}

/// Issues a guest `db.ping` RPC against the process-local SeaORM connection.
///
/// # Returns
///
/// `Ok(())` when the engine responds to ping.
///
/// # Errors
///
/// Returns an error string when [`set_connection`] was never called or ping fails.
pub async fn guest_ping() -> Result<()> {
    let gate = txn_gate();
    let _gate = gate.lock().await;
    let conn = connection().await?;
    conn.ping().await.map_err(|e| e.to_string())
}

/// Begins a top-level transaction or a nested savepoint.
///
/// Top-level begins wait until no other transaction is open so SQLite
/// never interleaves writers. Nested begins run on the parent worker task.
///
/// # Arguments
///
/// * `parent_txn_id` - Existing txn id to nest under; `None` for top-level.
///
/// # Returns
///
/// Opaque txn id the host must attach to subsequent statements.
///
/// # Errors
///
/// Returns an error string when not connected, the parent is unknown, or the
/// engine rejects `BEGIN`.
pub async fn guest_begin(parent_txn_id: Option<String>) -> Result<String> {
    if bookclerk_library::consume_begin_injection() {
        return Err("database begin failed: injected begin failure".into());
    }
    if let Some(parent_txn_id) = parent_txn_id {
        let tx = {
            let session = SESSION.lock().await;
            session
                .routes
                .get(&parent_txn_id)
                .cloned()
                .ok_or_else(|| format!("unknown parent txn {parent_txn_id}"))?
        };
        let (reply, rx) = oneshot::channel();
        tx.send(TxnOp::BeginNested {
            parent_txn_id,
            reply,
        })
        .await
        .map_err(|_| "transaction worker closed".to_string())?;
        let nested_id = rx
            .await
            .map_err(|_| "transaction worker closed".to_string())??;
        SESSION.lock().await.routes.insert(nested_id.clone(), tx);
        return Ok(nested_id);
    }

    let permit = txn_gate().lock_owned().await;
    let conn = connection().await?;
    let (op_tx, op_rx) = mpsc::channel(32);
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(async move {
        txn_worker(conn, permit, op_rx, ready_tx).await;
    });
    let root_id = ready_rx
        .await
        .map_err(|_| "transaction worker closed".to_string())??;
    SESSION.lock().await.routes.insert(root_id.clone(), op_tx);
    Ok(root_id)
}

/// Commits a transaction previously returned by [`guest_begin`].
///
/// # Arguments
///
/// * `txn_id` - Id from [`guest_begin`].
///
/// # Errors
///
/// Returns an error string when the id is unknown or the engine rejects commit.
pub async fn guest_commit(txn_id: String) -> Result<()> {
    finish_txn(txn_id, true).await
}

/// Rolls back a transaction previously returned by [`guest_begin`].
///
/// # Arguments
///
/// * `txn_id` - Id from [`guest_begin`].
///
/// # Errors
///
/// Returns an error string when the id is unknown or the engine rejects rollback.
pub async fn guest_rollback(txn_id: String) -> Result<()> {
    finish_txn(txn_id, false).await
}

/// Runs a named library operation as one native SQL transaction with a receipt.
///
/// # Arguments
///
/// * `req` - Idempotency envelope (`operationId` + named command).
///
/// # Errors
///
/// Returns an error string when not connected or the engine rejects the work.
pub async fn guest_atomic(req: DbAtomicRequest) -> Result<DbAtomicResult> {
    let gate = txn_gate();
    let _gate = gate.lock().await;
    let conn = connection().await?;
    bookclerk_library::execute_db_atomic(&conn, req)
        .await
        .map_err(|e| e.to_string())
}

/// Runs a read-only SQL query through the guest database bridge.
///
/// # Arguments
///
/// * `dto` - RPC statement DTO from the host bridge.
///
/// # Returns
///
/// Query rows projected into [`QueryResultDto`].
///
/// # Errors
///
/// Returns an error string when not connected or the engine rejects the statement.
pub async fn guest_query(dto: StatementDto) -> Result<QueryResultDto> {
    if let Some(txn_id) = dto.txn_id.clone() {
        let tx = route(&txn_id).await?;
        let (reply, rx) = oneshot::channel();
        tx.send(TxnOp::Query { txn_id, dto, reply })
            .await
            .map_err(|_| "transaction worker closed".to_string())?;
        return rx
            .await
            .map_err(|_| "transaction worker closed".to_string())?;
    }
    let gate = txn_gate();
    let _gate = gate.lock().await;
    let conn = connection().await?;
    query_on(&conn, dto).await
}

/// Runs a read-only statement and returns one bounded page (`limit + 1` fetch).
///
/// The adapter wraps `SELECT`/`WITH` SQL once with `LIMIT`/`OFFSET` so later
/// pages do not rematerialize the full result set. Data-modifying CTEs and
/// `FOR UPDATE`/`FOR SHARE` are rejected with a literal/comment-aware scan.
/// Postgres pins one transaction for create → probe → stream → drop:
/// autocommit pages `BEGIN` + `CREATE TEMP TABLE … ON COMMIT DROP AS` (caller
/// SQL once), then `pg_column_size` and an ordered select from that table.
/// The adapter-private `ROW_NUMBER` column reuses the temp-table UUID as a
/// suffix so it cannot collide with a caller alias such as `_bookclerk_page_ord`.
/// Guest-txn pages reuse the already-pinned `DatabaseTransaction`.
/// `SET TRANSACTION READ ONLY` cannot wrap this path: PostgreSQL rejects
/// `CREATE TABLE` in a read-only transaction. SQLite/D1 issue one
/// `query_all_raw`. Encoding stops at [`MAX_SCALAR_BYTES`]. Cursors are parsed
/// as a bounded `u64` so `offset + page` cannot overflow.
///
/// Huge *server-side* expressions can still stress the engine during
/// materialize; the protocol bound is the decode/scalar cap.
///
/// # Errors
///
/// Returns an error string when not connected, the cursor is invalid, the
/// engine rejects the statement, a row exceeds the scalar budget, or the
/// driver returns more than `limit + 1` rows.
pub async fn guest_query_page(dto: StatementDto, cursor: &str, limit: u32) -> Result<QueryPage> {
    if let Some(txn_id) = dto.txn_id.clone() {
        let tx = route(&txn_id).await?;
        let (reply, rx) = oneshot::channel();
        tx.send(TxnOp::QueryPage {
            txn_id,
            dto,
            cursor: cursor.to_string(),
            limit,
            reply,
        })
        .await
        .map_err(|_| "transaction worker closed".to_string())?;
        return rx
            .await
            .map_err(|_| "transaction worker closed".to_string())?;
    }
    let gate = txn_gate();
    let _gate = gate.lock().await;
    let conn = connection().await?;
    query_page_on(
        &conn,
        dto,
        cursor,
        limit,
        PostgresPageIsolation::BeginPinned,
    )
    .await
}

/// Runs a mutating SQL statement through the guest database bridge.
///
/// # Arguments
///
/// * `dto` - RPC statement DTO from the host bridge.
///
/// # Returns
///
/// Last-insert id and rows-affected in an [`ExecResultDto`].
///
/// # Errors
///
/// Returns an error string when not connected or the engine rejects the statement.
pub async fn guest_execute(dto: StatementDto) -> Result<ExecResultDto> {
    if let Some(txn_id) = dto.txn_id.clone() {
        let tx = route(&txn_id).await?;
        let (reply, rx) = oneshot::channel();
        tx.send(TxnOp::Execute { txn_id, dto, reply })
            .await
            .map_err(|_| "transaction worker closed".to_string())?;
        return rx
            .await
            .map_err(|_| "transaction worker closed".to_string())?;
    }
    let gate = txn_gate();
    let _gate = gate.lock().await;
    let conn = connection().await?;
    execute_on(&conn, dto).await
}

/// Sends commit or rollback to the worker and drops the route on success.
async fn finish_txn(txn_id: String, commit: bool) -> Result<()> {
    let tx = route(&txn_id).await?;
    if commit && bookclerk_library::consume_commit_injection() {
        let (reply, rx) = oneshot::channel();
        tx.send(TxnOp::Rollback {
            txn_id: txn_id.clone(),
            reply,
        })
        .await
        .map_err(|_| "transaction worker closed".to_string())?;
        let _ = rx.await;
        SESSION.lock().await.routes.remove(&txn_id);
        return Err("database commit failed: injected commit failure".into());
    }
    let (reply, rx) = oneshot::channel();
    let op = if commit {
        TxnOp::Commit {
            txn_id: txn_id.clone(),
            reply,
        }
    } else {
        TxnOp::Rollback {
            txn_id: txn_id.clone(),
            reply,
        }
    };
    tx.send(op)
        .await
        .map_err(|_| "transaction worker closed".to_string())?;
    rx.await
        .map_err(|_| "transaction worker closed".to_string())??;
    SESSION.lock().await.routes.remove(&txn_id);
    Ok(())
}

/// Looks up the worker channel for a live txn id.
async fn route(txn_id: &str) -> Result<mpsc::Sender<TxnOp>> {
    SESSION
        .lock()
        .await
        .routes
        .get(txn_id)
        .cloned()
        .ok_or_else(|| format!("unknown txn {txn_id}"))
}

/// Clones the opened connection, or errors if [`set_connection`] was never called.
async fn connection() -> Result<DatabaseConnection> {
    SESSION
        .lock()
        .await
        .conn
        .clone()
        .ok_or_else(|| "database not connected — call db.connect first".into())
}

/// Owns one SeaORM transaction and serializes nested ops until the root ends.
async fn txn_worker(
    conn: DatabaseConnection,
    _permit: OwnedMutexGuard<()>,
    mut ops: mpsc::Receiver<TxnOp>,
    ready: oneshot::Sender<Result<String>>,
) {
    let txn = match conn.begin().await {
        Ok(txn) => txn,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    if bookclerk_library::is_txn_broken() {
        let fault =
            bookclerk_library::take_txn_fault().unwrap_or_else(|| "database begin failed".into());
        let _ = txn.rollback().await;
        let _ = ready.send(Err(fault));
        return;
    }
    let root_id = uuid::Uuid::new_v4().to_string();
    let mut stack = vec![(root_id.clone(), txn)];
    if ready.send(Ok(root_id)).is_err() {
        let _ = rollback_stack(&mut stack).await;
        return;
    }
    while let Some(op) = ops.recv().await {
        match op {
            TxnOp::Query { txn_id, dto, reply } => {
                let result = match stack_txn(&stack, &txn_id) {
                    Ok(txn) => query_on(txn, dto).await,
                    Err(err) => Err(err),
                };
                let _ = reply.send(result);
            }
            TxnOp::QueryPage {
                txn_id,
                dto,
                cursor,
                limit,
                reply,
            } => {
                let result = match stack_txn(&stack, &txn_id) {
                    Ok(txn) => {
                        query_page_on(txn, dto, &cursor, limit, PostgresPageIsolation::Pinned).await
                    }
                    Err(err) => Err(err),
                };
                let _ = reply.send(result);
            }
            TxnOp::Execute { txn_id, dto, reply } => {
                let result = match stack_txn(&stack, &txn_id) {
                    Ok(txn) => execute_on(txn, dto).await,
                    Err(err) => Err(err),
                };
                let _ = reply.send(result);
            }
            TxnOp::BeginNested {
                parent_txn_id,
                reply,
            } => {
                let result = begin_nested(&mut stack, &parent_txn_id).await;
                let _ = reply.send(result);
            }
            TxnOp::Commit { txn_id, reply } => {
                let result = pop_finish(&mut stack, &txn_id, true).await;
                let empty = stack.is_empty();
                let _ = reply.send(result);
                if empty {
                    return;
                }
            }
            TxnOp::Rollback { txn_id, reply } => {
                let result = pop_finish(&mut stack, &txn_id, false).await;
                let empty = stack.is_empty();
                let _ = reply.send(result);
                if empty {
                    return;
                }
            }
        }
    }
    let _ = rollback_stack(&mut stack).await;
}

/// Returns the savepoint matching `txn_id`, or errors if it is unknown.
fn stack_txn<'a>(
    stack: &'a [(String, DatabaseTransaction)],
    txn_id: &str,
) -> Result<&'a DatabaseTransaction> {
    stack
        .iter()
        .find(|(id, _)| id == txn_id)
        .map(|(_, txn)| txn)
        .ok_or_else(|| format!("unknown txn {txn_id}"))
}

/// Begins a nested savepoint only when `parent_txn_id` is the innermost txn.
async fn begin_nested(
    stack: &mut Vec<(String, DatabaseTransaction)>,
    parent_txn_id: &str,
) -> Result<String> {
    let (pid, parent) = stack
        .pop()
        .ok_or_else(|| format!("unknown parent txn {parent_txn_id}"))?;
    if pid != parent_txn_id {
        let err = format!("parent txn {parent_txn_id} is not innermost (innermost is {pid})");
        stack.push((pid, parent));
        return Err(err);
    }
    let nested = match parent.begin().await {
        Ok(nested) => nested,
        Err(err) => {
            stack.push((pid, parent));
            return Err(err.to_string());
        }
    };
    if bookclerk_library::is_txn_broken() {
        let fault =
            bookclerk_library::take_txn_fault().unwrap_or_else(|| "database begin failed".into());
        let _ = nested.rollback().await;
        stack.push((pid, parent));
        return Err(fault);
    }
    let id = uuid::Uuid::new_v4().to_string();
    stack.push((pid, parent));
    stack.push((id.clone(), nested));
    Ok(id)
}

/// Commits or rolls back the innermost txn; rejects out-of-order ids.
async fn pop_finish(
    stack: &mut Vec<(String, DatabaseTransaction)>,
    txn_id: &str,
    commit: bool,
) -> Result<()> {
    match stack.last() {
        Some((id, _)) if id == txn_id => {}
        Some((id, _)) => {
            return Err(format!("txn {txn_id} is not innermost (innermost is {id})"));
        }
        None => return Err(format!("unknown txn {txn_id}")),
    }
    let (_, txn) = stack.pop().expect("checked last");
    if commit {
        txn.commit().await.map_err(|e| e.to_string())?;
        if let Some(fault) = bookclerk_library::take_txn_fault() {
            return Err(fault);
        }
        Ok(())
    } else {
        txn.rollback().await.map_err(|e| e.to_string())
    }
}

/// Rolls back every remaining savepoint when the worker channel closes.
async fn rollback_stack(stack: &mut Vec<(String, DatabaseTransaction)>) -> Result<()> {
    while let Some((_, txn)) = stack.pop() {
        txn.rollback().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Executes a read-only statement and projects rows into [`QueryResultDto`].
async fn query_on<C: ConnectionTrait>(conn: &C, dto: StatementDto) -> Result<QueryResultDto> {
    let backend = conn.get_database_backend();
    let stmt = statement_from_dto(dto, backend);
    let rows = conn.query_all_raw(stmt).await.map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(row_to_dto(&row));
    }
    Ok(QueryResultDto { rows: out })
}

/// Page size for a guest query; `limit == 0` means [`MAX_LIST_PAGE`].
fn query_page_size(limit: u32) -> usize {
    if limit == 0 {
        MAX_LIST_PAGE as usize
    } else {
        (limit as usize).min(MAX_LIST_PAGE as usize).max(1)
    }
}

/// Wraps a `SELECT`/`WITH` statement so the engine returns at most `fetch` rows.
fn wrap_select_for_page(sql: &str, fetch: usize, offset: usize) -> Result<String> {
    let sql = sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return Err("empty query".into());
    }
    let is_select = sql
        .get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("select"));
    let is_with = sql
        .get(..4)
        .is_some_and(|head| head.eq_ignore_ascii_case("with"));
    if !is_select && !is_with {
        return Err("paged queries require a SELECT or WITH statement".into());
    }
    require_read_only_page_sql(sql)?;
    Ok(format!(
        "SELECT * FROM ({sql}) AS _bookclerk_page LIMIT {fetch} OFFSET {offset}"
    ))
}

/// Rejects data-modifying CTEs and locking clauses so a paged query cannot mutate.
///
/// Literals, quoted identifiers, and comments are stripped first so
/// `SELECT 'DELETE'` is not treated as DML. Postgres cannot `SET TRANSACTION
/// READ ONLY` around `CREATE TEMP TABLE`, so this scan is the statement-shape
/// guard; side-effecting `SELECT` functions are not blocked by PostgreSQL.
fn require_read_only_page_sql(sql: &str) -> Result<()> {
    let upper = sql_code_without_literals_and_comments(sql).to_ascii_uppercase();
    for kw in ["INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE"] {
        if sql_contains_keyword(&upper, kw) {
            return Err(format!("paged queries must be read-only; found {kw}"));
        }
    }
    if sql_contains_keyword(&upper, "FOR UPDATE") || sql_contains_keyword(&upper, "FOR SHARE") {
        return Err("paged queries must be read-only; found FOR UPDATE/SHARE".into());
    }
    Ok(())
}

/// SQL with string/identifier literals and comments replaced by spaces.
fn sql_code_without_literals_and_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(b' ');
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = i.saturating_add(2).min(bytes.len());
            out.push(b' ');
            continue;
        }
        if bytes[i] == b'$' {
            if let Some((body_at, tag)) = parse_dollar_quote_tag(bytes, i) {
                i = body_at;
                while i + tag.len() <= bytes.len() && &bytes[i..i + tag.len()] != tag.as_slice() {
                    i += 1;
                }
                i = i.saturating_add(tag.len()).min(bytes.len());
                out.push(b' ');
                continue;
            }
        }
        if bytes[i] == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(b' ');
            continue;
        }
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(b' ');
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// `$tag$` opener at `start`, returning the index after the opener and the tag bytes.
fn parse_dollar_quote_tag(bytes: &[u8], start: usize) -> Option<(usize, Vec<u8>)> {
    let mut i = start + 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'$' {
        Some((i + 1, bytes[start..=i].to_vec()))
    } else {
        None
    }
}

/// True when `needle` appears in `haystack` as a SQL keyword (not `updated_at`).
fn sql_contains_keyword(haystack: &str, needle: &str) -> bool {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..].starts_with(needle) {
            let before_ok = i == 0 || !sql_ident_byte(hay[i - 1]);
            let after = i + needle.len();
            let after_ok = after == hay.len() || !sql_ident_byte(hay[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Bytes that continue a SQL identifier (`updated_at` must not match `UPDATE`).
fn sql_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Maximum numeric query cursor accepted by [`guest_query_page`].
///
/// Keeps `OFFSET` in a domain that cannot overflow `usize` when adding a page.
const MAX_QUERY_CURSOR: u64 = 1_000_000_000;

/// Parses an opaque numeric cursor, rejecting empty-invalid and overflowing values.
fn parse_query_cursor(cursor: &str) -> Result<usize> {
    let trimmed = cursor.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let parsed = trimmed
        .parse::<u64>()
        .map_err(|_| format!("invalid query cursor `{cursor}`"))?;
    if parsed > MAX_QUERY_CURSOR {
        return Err(format!("invalid query cursor `{cursor}`"));
    }
    usize::try_from(parsed).map_err(|_| format!("invalid query cursor `{cursor}`"))
}

/// Accumulates one JSON-encoded row into a page, stopping at [`MAX_SCALAR_BYTES`].
fn push_bounded_row(
    rows: &mut Vec<ProxyRowDto>,
    encoded: &mut usize,
    row: ProxyRowDto,
) -> Result<()> {
    let piece = serde_json::to_string(&row).map_err(|e| e.to_string())?;
    let extra = piece.len() + usize::from(!rows.is_empty());
    if encoded.saturating_add(extra).saturating_add(1) > MAX_SCALAR_BYTES as usize {
        return Err(format!("JSON result would exceed {MAX_SCALAR_BYTES}"));
    }
    *encoded = encoded.saturating_add(extra);
    rows.push(row);
    Ok(())
}

/// Temp table + ordinal column sharing one UUID suffix (caller-safe names).
struct PostgresTempPage {
    /// `CREATE TEMP TABLE` name (`_bookclerk_page_{uuid}`).
    table: String,
    /// Adapter-private order column (`_bookclerk_page_ord_{uuid}`).
    ord_col: String,
}

/// Unique Postgres temp table that materializes one wrapped page (user SQL once).
fn postgres_temp_page() -> PostgresTempPage {
    let id = uuid::Uuid::new_v4().simple();
    PostgresTempPage {
        table: format!("_bookclerk_page_{id}"),
        ord_col: format!("_bookclerk_page_ord_{id}"),
    }
}

/// `CREATE TEMP TABLE … ON COMMIT DROP AS` so the caller's statement runs once.
fn postgres_create_temp_page_sql(table: &str, ord_col: &str, wrapped_page_sql: &str) -> String {
    format!(
        "CREATE TEMP TABLE {table} ON COMMIT DROP AS SELECT *, ROW_NUMBER() OVER () AS {ord_col} FROM ({wrapped_page_sql}) AS _bookclerk_src"
    )
}

/// Size probe over an already-materialized temp table (integers, not payloads).
fn postgres_temp_size_sql(table: &str) -> String {
    format!("SELECT COALESCE(MAX(pg_column_size({table}.*)), 0) FROM {table}")
}

/// Ordered data fetch from the materialized temp table.
fn postgres_temp_select_sql(table: &str, ord_col: &str) -> String {
    format!("SELECT * FROM {table} ORDER BY {ord_col}")
}

/// Drops the page temp table after size check + stream (or on error).
fn postgres_drop_temp_sql(table: &str) -> String {
    format!("DROP TABLE IF EXISTS {table}")
}

/// How Postgres page materialization pins a physical session.
enum PostgresPageIsolation {
    /// Autocommit: `BEGIN` (pinned connection) + temp table + `COMMIT`.
    BeginPinned,
    /// Guest txn worker already holds a `DatabaseTransaction` (same connection).
    Pinned,
}

/// Reads the integer from [`postgres_temp_size_sql`] (int or bigint).
fn postgres_probe_max_bytes(row: &sea_orm::QueryResult) -> Result<u64> {
    if let Ok(v) = row.try_get_by_index::<i64>(0) {
        return Ok(u64::try_from(v).unwrap_or(0));
    }
    if let Ok(v) = row.try_get_by_index::<i32>(0) {
        return Ok(u64::try_from(v).unwrap_or(0));
    }
    Err("postgres size probe did not return an integer".into())
}

/// Materializes the wrapped page once on a pinned Postgres session.
///
/// Autocommit pages open a pinned transaction so CREATE/probe/stream/drop
/// cannot hop between pool connections. Huge *server-side* expressions can
/// still stress the engine during `CREATE TEMP TABLE AS`; the client bound is
/// the protocol/decode cap.
async fn postgres_query_page_once<C>(
    conn: &C,
    dto: StatementDto,
    fetch: usize,
    isolation: PostgresPageIsolation,
) -> Result<Vec<ProxyRowDto>>
where
    C: ConnectionTrait + StreamTrait + TransactionTrait,
    <C as TransactionTrait>::Transaction: ConnectionTrait + StreamTrait + TransactionSession,
{
    match isolation {
        PostgresPageIsolation::BeginPinned => {
            let txn = conn.begin().await.map_err(|e| e.to_string())?;
            let fetched = postgres_run_temp_page(&txn, dto, fetch).await;
            match fetched {
                Ok(rows) => {
                    txn.commit().await.map_err(|e| e.to_string())?;
                    Ok(rows)
                }
                Err(err) => {
                    let _ = txn.rollback().await;
                    Err(err)
                }
            }
        }
        PostgresPageIsolation::Pinned => postgres_run_temp_page(conn, dto, fetch).await,
    }
}

/// CREATE TEMP → size probe → ordered stream → DROP on one connection.
async fn postgres_run_temp_page<C>(
    conn: &C,
    dto: StatementDto,
    fetch: usize,
) -> Result<Vec<ProxyRowDto>>
where
    C: ConnectionTrait + StreamTrait,
{
    let names = postgres_temp_page();
    let txn_id = dto.txn_id.clone();
    let mut create = dto;
    create.sql = postgres_create_temp_page_sql(&names.table, &names.ord_col, &create.sql);
    conn.execute_raw(statement_from_dto(create, DbBackend::Postgres))
        .await
        .map_err(|e| e.to_string())?;

    let fetched = async {
        let probe = StatementDto {
            sql: postgres_temp_size_sql(&names.table),
            values: Vec::new(),
            txn_id: txn_id.clone(),
        };
        if let Some(row) = conn
            .query_one_raw(statement_from_dto(probe, DbBackend::Postgres))
            .await
            .map_err(|e| e.to_string())?
        {
            let max = postgres_probe_max_bytes(&row)?;
            if max > u64::from(MAX_SCALAR_BYTES) {
                return Err(format!(
                    "postgres row is {max} bytes; exceeds {MAX_SCALAR_BYTES}"
                ));
            }
        }
        let select = StatementDto {
            sql: postgres_temp_select_sql(&names.table, &names.ord_col),
            values: Vec::new(),
            txn_id: txn_id.clone(),
        };
        let mut rows =
            stream_rows_bounded(conn, statement_from_dto(select, DbBackend::Postgres), fetch)
                .await?;
        for row in &mut rows {
            row.values.remove(&names.ord_col);
        }
        Ok(rows)
    }
    .await;

    let drop = StatementDto {
        sql: postgres_drop_temp_sql(&names.table),
        values: Vec::new(),
        txn_id,
    };
    let _ = conn
        .execute_raw(statement_from_dto(drop, DbBackend::Postgres))
        .await;
    fetched
}

/// Streams at most `fetch` rows from a statement that already includes `LIMIT`.
async fn stream_rows_bounded<C: ConnectionTrait + StreamTrait>(
    conn: &C,
    stmt: sea_orm::Statement,
    fetch: usize,
) -> Result<Vec<ProxyRowDto>> {
    let stream = conn.stream_raw(stmt).await.map_err(|e| e.to_string())?;
    futures::pin_mut!(stream);
    let mut rows = Vec::new();
    let mut encoded = 1usize;
    while let Some(row) = stream.try_next().await.map_err(|e| e.to_string())? {
        if rows.len() >= fetch {
            return Err(format!("database adapter fetched more than {fetch} rows"));
        }
        push_bounded_row(&mut rows, &mut encoded, row_to_dto(&row))?;
    }
    Ok(rows)
}

/// Fetches at most `fetch` rows from a statement that already includes `LIMIT`.
///
/// Proxy engines (SQLite/D1) materialize the wrapped page in one round-trip;
/// per-cell caps in those adapters reject oversized scalars while decoding.
async fn fetch_page_all_raw<C: ConnectionTrait>(
    conn: &C,
    stmt: sea_orm::Statement,
    fetch: usize,
) -> Result<Vec<ProxyRowDto>> {
    let fetched = conn.query_all_raw(stmt).await.map_err(|e| e.to_string())?;
    if fetched.len() > fetch {
        return Err(format!(
            "database adapter fetched {} rows; budget is {fetch}",
            fetched.len()
        ));
    }
    let mut rows = Vec::new();
    let mut encoded = 1usize;
    for row in fetched {
        push_bounded_row(&mut rows, &mut encoded, row_to_dto(&row))?;
    }
    Ok(rows)
}

/// Fetches one bounded page, failing if the adapter materializes more than `limit + 1` rows.
async fn query_page_on<C>(
    conn: &C,
    dto: StatementDto,
    cursor: &str,
    limit: u32,
    postgres_isolation: PostgresPageIsolation,
) -> Result<QueryPage>
where
    C: ConnectionTrait + StreamTrait + TransactionTrait,
    <C as TransactionTrait>::Transaction: ConnectionTrait + StreamTrait + TransactionSession,
{
    let offset = parse_query_cursor(cursor)?;
    let page = query_page_size(limit);
    let fetch = page.saturating_add(1);
    let backend = ConnectionTrait::get_database_backend(conn);
    let mut paged = dto;
    paged.sql = wrap_select_for_page(&paged.sql, fetch, offset)?;
    let rows = match backend {
        DbBackend::Postgres => {
            postgres_query_page_once(conn, paged, fetch, postgres_isolation).await?
        }
        _ => {
            let stmt = statement_from_dto(paged, backend);
            fetch_page_all_raw(conn, stmt, fetch).await?
        }
    };
    if rows.len() > fetch {
        return Err(format!(
            "database adapter fetched {} rows; budget is {fetch}",
            rows.len()
        ));
    }
    let has_more = rows.len() > page;
    let page_rows = if has_more {
        &rows[..page]
    } else {
        rows.as_slice()
    };
    let rows_json = serde_json::to_string(page_rows).map_err(|e| e.to_string())?;
    if rows_json.len() > MAX_SCALAR_BYTES as usize {
        return Err(format!(
            "JSON result {} bytes exceeds {MAX_SCALAR_BYTES}",
            rows_json.len()
        ));
    }
    let next_cursor = if has_more {
        Some(
            offset
                .checked_add(page)
                .ok_or_else(|| format!("invalid query cursor `{cursor}`"))?
                .to_string(),
        )
    } else {
        None
    };
    Ok(QueryPage {
        rows_json,
        next_cursor,
    })
}

/// Executes a mutating statement and returns last-insert id plus rows-affected.
async fn execute_on<C: ConnectionTrait>(conn: &C, dto: StatementDto) -> Result<ExecResultDto> {
    let backend = conn.get_database_backend();
    let stmt = statement_from_dto(dto, backend);
    let result = conn.execute_raw(stmt).await.map_err(|e| e.to_string())?;
    Ok(ExecResultDto {
        last_insert_id: result.last_insert_id(),
        rows_affected: result.rows_affected(),
    })
}

/// Convert a SeaORM query row into the RPC DTO (also used by integration tests).
///
/// # Arguments
///
/// * `row` - SeaORM query row to project into an RPC DTO.
///
/// # Returns
///
/// `ProxyRowDto` result.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
#[must_use]
pub fn row_to_dto(row: &sea_orm::QueryResult) -> ProxyRowDto {
    let proxy = from_query_result_to_proxy_row(row);
    proxy_rows_to_dto(vec![proxy])
        .into_iter()
        .next()
        .expect("proxy_rows_to_dto preserves one row")
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::StatementDto;
    use sea_orm::{DbBackend, Statement};
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    /// Serializes tests that mutate the process-wide SeaORM session.
    static SESSION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn stmt(sql: &str) -> StatementDto {
        StatementDto {
            sql: sql.into(),
            values: Vec::new(),
            txn_id: None,
        }
    }

    #[test]
    fn wrap_select_for_page_limits_and_offsets() {
        let sql = wrap_select_for_page("SELECT id FROM t", 11, 10).unwrap();
        assert!(sql.contains("LIMIT 11"));
        assert!(sql.contains("OFFSET 10"));
        assert!(sql.contains("SELECT id FROM t"));
        assert!(wrap_select_for_page("INSERT INTO t VALUES (1)", 2, 0).is_err());
        assert!(
            sql.matches("LIMIT ").count() == 1 && sql.matches("OFFSET ").count() == 1,
            "page wrap must be a single LIMIT+1 statement, got {sql}"
        );
        assert!(sql.contains("AS _bookclerk_page"));
        assert!(wrap_select_for_page(
            "WITH x AS (DELETE FROM t RETURNING id) SELECT * FROM x",
            2,
            0
        )
        .is_err());
        assert!(wrap_select_for_page("SELECT id FROM t FOR UPDATE", 2, 0).is_err());
        assert!(wrap_select_for_page("SELECT updated_at FROM t", 2, 0).is_ok());
        assert!(wrap_select_for_page("SELECT random() AS r", 2, 0).is_ok());
        assert!(wrap_select_for_page("SELECT 'DELETE' AS x", 2, 0).is_ok());
        assert!(wrap_select_for_page("SELECT /* DELETE */ 1 AS x", 2, 0).is_ok());
        assert!(wrap_select_for_page("SELECT 1 AS x -- DELETE\n", 2, 0).is_ok());
        assert!(wrap_select_for_page(r#"SELECT "delete" FROM t"#, 2, 0).is_ok());
        assert!(wrap_select_for_page("SELECT $$DELETE$$ AS x", 2, 0).is_ok());
    }

    #[test]
    fn postgres_temp_page_sql_runs_user_statement_once() {
        let wrapped = wrap_select_for_page("SELECT random() AS r, v FROM t", 11, 0).unwrap();
        let table = "_bookclerk_page_abc";
        let ord = "_bookclerk_page_ord_abc";
        let create = postgres_create_temp_page_sql(table, ord, &wrapped);
        let probe = postgres_temp_size_sql(table);
        let select = postgres_temp_select_sql(table, ord);
        assert!(create.starts_with(
            "CREATE TEMP TABLE _bookclerk_page_abc ON COMMIT DROP AS SELECT *, ROW_NUMBER() OVER () AS _bookclerk_page_ord_abc FROM ("
        ));
        assert!(
            !create.contains("AS _bookclerk_page_ord FROM"),
            "ordinal must not be the fixed caller-colliding name: {create}"
        );
        assert!(create.contains(&wrapped), "{create}");
        assert!(
            !probe.contains("SELECT random()"),
            "size probe must not replay caller SQL: {probe}"
        );
        assert!(
            !select.contains("SELECT random()"),
            "data fetch must not replay caller SQL: {select}"
        );
        assert_eq!(
            probe,
            "SELECT COALESCE(MAX(pg_column_size(_bookclerk_page_abc.*)), 0) FROM _bookclerk_page_abc"
        );
        assert_eq!(
            select,
            "SELECT * FROM _bookclerk_page_abc ORDER BY _bookclerk_page_ord_abc"
        );
        assert_eq!(
            postgres_drop_temp_sql(table),
            "DROP TABLE IF EXISTS _bookclerk_page_abc"
        );
    }

    #[test]
    fn postgres_temp_page_ordinal_reuses_table_uuid() {
        let page = postgres_temp_page();
        let suffix = page
            .table
            .strip_prefix("_bookclerk_page_")
            .expect("table name prefix");
        assert_eq!(page.ord_col, format!("_bookclerk_page_ord_{suffix}"));
        assert_ne!(page.ord_col, "_bookclerk_page_ord");
        let other = postgres_temp_page();
        assert_ne!(page.table, other.table);
        assert_ne!(page.ord_col, other.ord_col);
    }

    #[tokio::test]
    async fn query_page_fetches_at_most_limit_plus_one() {
        let _lock = SESSION_LOCK.lock().await;
        set_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
        .await;
        guest_execute(stmt(
            "CREATE TABLE query_page_budget (id INTEGER PRIMARY KEY, v TEXT)",
        ))
        .await
        .unwrap();
        for i in 0..40 {
            guest_execute(stmt(&format!(
                "INSERT INTO query_page_budget (id, v) VALUES ({i}, 'x')"
            )))
            .await
            .unwrap();
        }
        let page = guest_query_page(stmt("SELECT id FROM query_page_budget ORDER BY id"), "", 5)
            .await
            .unwrap();
        let ids: Vec<serde_json::Value> = serde_json::from_str(&page.rows_json).unwrap();
        assert_eq!(ids.len(), 5);
        assert_eq!(page.next_cursor.as_deref(), Some("5"));

        let unpaged = guest_query(stmt("SELECT id FROM query_page_budget ORDER BY id"))
            .await
            .unwrap();
        assert_eq!(unpaged.rows.len(), 40);

        let wrapped =
            wrap_select_for_page("SELECT id FROM query_page_budget ORDER BY id", 6, 0).unwrap();
        let limited = guest_query(stmt(&wrapped)).await.unwrap();
        assert!(
            limited.rows.len() <= 6,
            "adapter fetched {} rows under LIMIT 6",
            limited.rows.len()
        );
    }

    #[tokio::test]
    async fn query_page_rejects_row_larger_than_scalar_cap() {
        let _lock = SESSION_LOCK.lock().await;
        set_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
        .await;
        guest_execute(stmt(
            "CREATE TABLE query_page_huge (id INTEGER PRIMARY KEY, v TEXT)",
        ))
        .await
        .unwrap();
        let big = "x".repeat(MAX_SCALAR_BYTES as usize + 32);
        guest_execute(stmt(&format!(
            "INSERT INTO query_page_huge (id, v) VALUES (1, '{big}')"
        )))
        .await
        .unwrap();
        let err = guest_query_page(stmt("SELECT id, v FROM query_page_huge"), "", 5)
            .await
            .unwrap_err();
        assert!(
            err.contains(&MAX_SCALAR_BYTES.to_string()) || err.contains("exceeds"),
            "expected scalar-cap error while reading the oversized row, got {err}"
        );
    }

    #[tokio::test]
    async fn query_page_rejects_overflowing_cursors() {
        let _lock = SESSION_LOCK.lock().await;
        set_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
        .await;
        guest_execute(stmt(
            "CREATE TABLE query_page_cursor (id INTEGER PRIMARY KEY)",
        ))
        .await
        .unwrap();
        for cursor in [
            usize::MAX.to_string(),
            (usize::MAX - 1).to_string(),
            u64::MAX.to_string(),
            "nope".to_string(),
        ] {
            let err = guest_query_page(stmt("SELECT id FROM query_page_cursor"), &cursor, 5)
                .await
                .unwrap_err();
            assert!(
                err.contains("invalid query cursor"),
                "cursor {cursor} should be invalid, got {err}"
            );
        }
        let page = guest_query_page(
            stmt("SELECT id FROM query_page_cursor"),
            &MAX_QUERY_CURSOR.to_string(),
            5,
        )
        .await
        .unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&page.rows_json).unwrap();
        assert!(rows.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn query_page_rejects_data_modifying_cte_without_mutating() {
        let _lock = SESSION_LOCK.lock().await;
        set_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
        .await;
        guest_execute(stmt("CREATE TABLE query_page_dml (id INTEGER PRIMARY KEY)"))
            .await
            .unwrap();
        guest_execute(stmt("INSERT INTO query_page_dml (id) VALUES (1)"))
            .await
            .unwrap();
        let err = guest_query_page(
            stmt("WITH gone AS (DELETE FROM query_page_dml RETURNING id) SELECT * FROM gone"),
            "",
            5,
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("read-only") || err.contains("DELETE"),
            "expected read-only rejection, got {err}"
        );
        let left = guest_query(stmt("SELECT id FROM query_page_dml"))
            .await
            .unwrap();
        assert_eq!(left.rows.len(), 1, "DELETE CTE must not run");
    }

    #[tokio::test]
    async fn query_page_allows_volatile_read_only_expression() {
        let _lock = SESSION_LOCK.lock().await;
        set_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
        .await;
        let page = guest_query_page(stmt("SELECT random() AS r"), "", 1)
            .await
            .unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&page.rows_json).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn query_page_allows_delete_keyword_inside_literal() {
        let _lock = SESSION_LOCK.lock().await;
        set_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
        .await;
        let page = guest_query_page(stmt("SELECT 'DELETE' AS x"), "", 1)
            .await
            .unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&page.rows_json).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["values"]["x"], "DELETE");
    }

    fn postgres_test_url() -> String {
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap_or_else(|_| {
            panic!(
                "BOOKCLERK_TEST_POSTGRES_URL is required to run postgres page tests \
                 (CI sets BOOKCLERK_REQUIRE_POSTGRES_TESTS=1)"
            )
        });
        assert!(
            !url.trim().is_empty(),
            "BOOKCLERK_TEST_POSTGRES_URL must not be empty"
        );
        url
    }

    fn postgres_url_with_db(url: &str, db_name: &str) -> String {
        let trimmed = url.trim();
        let (base, query) = match trimmed.split_once('?') {
            Some((b, q)) => (b, Some(q)),
            None => (trimmed, None),
        };
        let slash = base
            .rfind('/')
            .expect("BOOKCLERK_TEST_POSTGRES_URL must include a database path");
        let head = &base[..slash];
        match query {
            Some(q) => format!("{head}/{db_name}?{q}"),
            None => format!("{head}/{db_name}"),
        }
    }

    async fn postgres_test_pool() -> sea_orm::DatabaseConnection {
        let url = postgres_test_url();
        let db_name = format!("page_{}", uuid::Uuid::new_v4().simple());
        let admin = sea_orm::Database::connect(url.as_str())
            .await
            .expect("connect to BOOKCLERK_TEST_POSTGRES_URL");
        let backend = admin.get_database_backend();
        admin
            .execute_raw(sea_orm::Statement::from_string(
                backend,
                format!("CREATE DATABASE {db_name}"),
            ))
            .await
            .expect("create disposable postgres database");
        let mut opt = sea_orm::ConnectOptions::new(postgres_url_with_db(&url, &db_name));
        opt.max_connections(8);
        opt.min_connections(4);
        sea_orm::Database::connect(opt)
            .await
            .expect("connect to disposable postgres database")
    }

    async fn postgres_exec(db: &sea_orm::DatabaseConnection, sql: &str) {
        db.execute_raw(Statement::from_string(DbBackend::Postgres, sql.to_string()))
            .await
            .unwrap_or_else(|err| panic!("postgres setup `{sql}` failed: {err}"));
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
    async fn postgres_page_pins_one_session_with_busy_pool() {
        let _lock = SESSION_LOCK.lock().await;
        let db = postgres_test_pool().await;
        postgres_exec(
            &db,
            "CREATE TABLE query_page_pool (id INTEGER PRIMARY KEY, v TEXT)",
        )
        .await;
        for i in 0..8 {
            postgres_exec(
                &db,
                &format!("INSERT INTO query_page_pool (id, v) VALUES ({i}, 'x')"),
            )
            .await;
        }
        set_connection(db.clone()).await;
        let mut sleeps = Vec::new();
        for _ in 0..3 {
            let db = db.clone();
            sleeps.push(tokio::spawn(async move {
                db.execute_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SELECT pg_sleep(1.5)",
                ))
                .await
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let page = guest_query_page(stmt("SELECT id FROM query_page_pool ORDER BY id"), "", 3)
            .await
            .unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&page.rows_json).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(page.next_cursor.as_deref(), Some("3"));
        for row in &rows {
            let keys: Vec<&str> = row["values"]
                .as_object()
                .expect("values object")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, vec!["id"], "adapter ordinal must not leak: {row}");
        }
        for handle in sleeps {
            handle.await.unwrap().expect("pg_sleep hold query");
        }
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
    async fn postgres_page_literals_and_write_function_once() {
        let _lock = SESSION_LOCK.lock().await;
        let db = postgres_test_pool().await;
        postgres_exec(&db, "CREATE TABLE query_page_ro (id INTEGER PRIMARY KEY)").await;
        postgres_exec(
            &db,
            "CREATE FUNCTION query_page_ro_touch() RETURNS INTEGER LANGUAGE plpgsql AS $$ BEGIN INSERT INTO query_page_ro VALUES (1); RETURN 1; END; $$",
        )
        .await;
        set_connection(db).await;
        let literal = guest_query_page(stmt("SELECT 'DELETE' AS x"), "", 1)
            .await
            .unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&literal.rows_json).unwrap();
        assert_eq!(rows[0]["values"]["x"], "DELETE");
        let commented = guest_query_page(stmt("SELECT /* DELETE */ 1 AS n"), "", 1)
            .await
            .unwrap();
        let commented_rows: Vec<serde_json::Value> =
            serde_json::from_str(&commented.rows_json).unwrap();
        assert_eq!(commented_rows.len(), 1);
        let dml_err = guest_query_page(
            stmt("WITH gone AS (DELETE FROM query_page_ro RETURNING id) SELECT * FROM gone"),
            "",
            5,
        )
        .await
        .unwrap_err();
        assert!(
            dml_err.contains("read-only") || dml_err.contains("DELETE"),
            "expected DML CTE rejection, got {dml_err}"
        );
        guest_query_page(stmt("SELECT query_page_ro_touch() AS n"), "", 1)
            .await
            .unwrap();
        let left = guest_query(stmt("SELECT id FROM query_page_ro"))
            .await
            .unwrap();
        assert_eq!(
            left.rows.len(),
            1,
            "write function must run exactly once during temp-table materialize, got {:?}",
            left.rows
        );
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
    async fn postgres_page_preserves_caller_ordinal_column() {
        let _lock = SESSION_LOCK.lock().await;
        let db = postgres_test_pool().await;
        postgres_exec(
            &db,
            "CREATE TABLE query_page_ord_col (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
        )
        .await;
        for (id, label) in [(1, "a"), (2, "b"), (3, "c")] {
            postgres_exec(
                &db,
                &format!("INSERT INTO query_page_ord_col (id, label) VALUES ({id}, '{label}')"),
            )
            .await;
        }
        set_connection(db).await;
        let page = guest_query_page(
            stmt("SELECT id AS _bookclerk_page_ord, label FROM query_page_ord_col ORDER BY id"),
            "",
            2,
        )
        .await
        .unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&page.rows_json).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(page.next_cursor.as_deref(), Some("2"));
        assert_eq!(rows[0]["values"]["_bookclerk_page_ord"], 1);
        assert_eq!(rows[0]["values"]["label"], "a");
        assert_eq!(rows[1]["values"]["_bookclerk_page_ord"], 2);
        assert_eq!(rows[1]["values"]["label"], "b");
        for row in &rows {
            let keys: Vec<&str> = row["values"]
                .as_object()
                .expect("values object")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                vec!["_bookclerk_page_ord", "label"],
                "caller ordinal must be preserved and adapter ordinal stripped: {row}"
            );
        }
    }
}
