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

use bookclerk_plugin_sdk::{
    proxy_rows_to_dto, statement_from_dto, DbAtomicRequest, DbAtomicResult, ExecResultDto,
    ProxyRowDto, QueryResultDto, StatementDto,
};
use sea_orm::{
    from_query_result_to_proxy_row, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    TransactionTrait,
};
use tokio::sync::{mpsc, oneshot, Mutex, OwnedMutexGuard};

type Result<T> = std::result::Result<T, String>;

struct Session {
    conn: Option<DatabaseConnection>,
    /// Every live txn id (root and nested) routes to its worker.
    routes: HashMap<String, mpsc::Sender<TxnOp>>,
}

enum TxnOp {
    Query {
        txn_id: String,
        dto: StatementDto,
        reply: oneshot::Sender<Result<QueryResultDto>>,
    },
    Execute {
        txn_id: String,
        dto: StatementDto,
        reply: oneshot::Sender<Result<ExecResultDto>>,
    },
    BeginNested {
        parent_txn_id: String,
        reply: oneshot::Sender<Result<String>>,
    },
    Commit {
        txn_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Rollback {
        txn_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

static SESSION: LazyLock<Mutex<Session>> = LazyLock::new(|| {
    Mutex::new(Session {
        conn: None,
        routes: HashMap::new(),
    })
});

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

async fn route(txn_id: &str) -> Result<mpsc::Sender<TxnOp>> {
    SESSION
        .lock()
        .await
        .routes
        .get(txn_id)
        .cloned()
        .ok_or_else(|| format!("unknown txn {txn_id}"))
}

async fn connection() -> Result<DatabaseConnection> {
    SESSION
        .lock()
        .await
        .conn
        .clone()
        .ok_or_else(|| "database not connected — call db.connect first".into())
}

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

async fn rollback_stack(stack: &mut Vec<(String, DatabaseTransaction)>) -> Result<()> {
    while let Some((_, txn)) = stack.pop() {
        txn.rollback().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

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
#[must_use]
pub fn row_to_dto(row: &sea_orm::QueryResult) -> ProxyRowDto {
    let proxy = from_query_result_to_proxy_row(row);
    proxy_rows_to_dto(vec![proxy])
        .into_iter()
        .next()
        .expect("proxy_rows_to_dto preserves one row")
}
