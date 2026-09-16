//! Run a generic [`DbAtomicPlan`] on a SeaORM connection (one native transaction).

#![allow(clippy::missing_docs_in_private_items)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::host_ir::{DbAtomicPlan, DbAtomicTiming, DbPlanExecResult, DbPlanStmtExecResult};
use bookclerk_plugin_abi::{
    apply_schema_action_to_env, apply_schema_sql_to_env, catalog_companions_for_action,
    sql_host_bookkeeping_type_env, statement_is_ddl, typecheck_execute_request_proofs,
    DbCapabilities, DbPlanStatementKind, DbResultSelection, ExecuteRequest, ResolvedStatement,
    SchemaAction, SqlTypeEnv, TypedDbStatement,
};
use futures::TryStreamExt;
use sea_orm::{
    from_query_result_to_proxy_row, ConnectionTrait, DatabaseConnection, DbErr, QueryResult,
    Statement, StreamTrait, TransactionTrait, Value,
};
use serde_json::Value as JsonValue;

use crate::lower_canonical_sql_typed;
use crate::proxy_txn::{
    consume_atomic_interrupt, consume_commit_injection, is_txn_broken, note_query_row,
    record_query_rows_seen, take_txn_fault, with_exec_budget, AtomicInterruptKind,
    AtomicInterruptPhase, ExecBudget,
};
use crate::typed::{load_physical_sql_type_env, load_sql_type_env};

/// Session-level cancel / deadline for one atomic attempt (not hashed).
#[derive(Clone, Default)]
pub struct AtomicSession {
    /// Host RPC cancel flag, when the guest shares a process with the host.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Guest-visible deadline (`deadlineUnixMs` on the wire).
    pub deadline_unix_ms: Option<u64>,
    /// Extra catalog snapshot merged before typing host-authored SQL
    /// (canonical host schema). Empty for plugin-binding execute.
    pub type_env: SqlTypeEnv,
}

impl AtomicSession {
    /// Builds session control from a wire `deadlineUnixMs`.
    #[must_use]
    pub fn from_deadline(deadline_unix_ms: Option<u64>) -> Self {
        Self {
            cancel: None,
            deadline_unix_ms,
            type_env: SqlTypeEnv::new(),
        }
    }

    /// Attaches a host cancel flag (job fence) to this session.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Option<Arc<AtomicBool>>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Merges `env` (typically the canonical host schema) into this session so
    /// host DML is typed against library tables, not the plugin catalog.
    #[must_use]
    pub fn with_type_env(mut self, env: SqlTypeEnv) -> Self {
        self.type_env = env;
        self
    }

    /// Checks cancel / deadline / test inject at `phase`.
    ///
    /// # Errors
    ///
    /// Returns when the session is cancelled, past deadline, or a test inject fires.
    pub(crate) fn check(&self, phase: AtomicInterruptPhase) -> Result<(), DbErr> {
        if let Some(kind) = consume_atomic_interrupt(phase) {
            return Err(interrupt_err(phase, kind));
        }
        let cancelled = self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst));
        let expired = self.deadline_unix_ms.is_some_and(|ms| unix_now_ms() >= ms);
        if cancelled {
            return Err(interrupt_err(phase, AtomicInterruptKind::Cancel));
        }
        if expired {
            return Err(interrupt_err(phase, AtomicInterruptKind::Deadline));
        }
        Ok(())
    }
}

/// Runs adapter-private catalog + identity companions after a statement.
///
/// # Errors
///
/// Returns [`DbErr`] when a companion statement fails to execute.
async fn apply_exec_identity_companions(
    txn: &impl ConnectionTrait,
    backend: sea_orm::DatabaseBackend,
    canonical: &str,
    action: &SchemaAction,
) -> Result<(), DbErr> {
    let mut companions = catalog_companions_for_action(canonical, Some(action));
    if backend == sea_orm::DatabaseBackend::Postgres {
        match action {
            SchemaAction::Create { noop: true, .. } | SchemaAction::None => {}
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

fn plan_as_typed_request(plan: &DbAtomicPlan, operation_id: &str) -> ExecuteRequest {
    ExecuteRequest {
        operation_id: operation_id.to_string(),
        request_hash: String::new(),
        deadline_unix_ms: 0,
        statements: plan
            .statements
            .iter()
            .map(|stmt| TypedDbStatement {
                sql: stmt.sql.clone(),
                parameters: Vec::new(),
                kind: stmt.kind,
                max_rows: stmt.max_rows,
                result_selection: DbResultSelection::Rows,
            })
            .collect(),
    }
}

/// Host plans may include already-lowered schema companions (`PRAGMA`,
/// `CREATE FUNCTION`, …). Those get a hash-bound empty proof. Canonical DML
/// and queries are typed against the merged host schema, in statement order.
///
/// # Errors
///
/// Returns [`DbErr`] when a canonical statement fails SQL v1 typecheck.
fn proofs_for_host_plan(
    req: &ExecuteRequest,
    env: &SqlTypeEnv,
) -> Result<Vec<ResolvedStatement>, DbErr> {
    let mut working = env.clone();
    let mut proofs = Vec::with_capacity(req.statements.len());
    for stmt in &req.statements {
        let sql = stmt.sql.trim();
        if host_adapter_private_sql(sql) || statement_is_ddl(sql) {
            apply_schema_sql_to_env(&mut working, sql);
            proofs.push(ResolvedStatement::bound_empty(sql));
            continue;
        }
        let one = ExecuteRequest {
            operation_id: req.operation_id.clone(),
            request_hash: req.request_hash.clone(),
            deadline_unix_ms: req.deadline_unix_ms,
            statements: vec![stmt.clone()],
        };
        let mut typed = typecheck_execute_request_proofs(&one, &working)
            .map_err(|err| DbErr::Custom(err.to_string()))?;
        proofs.push(typed.pop().ok_or_else(|| {
            DbErr::Custom("host SQL typecheck returned no proof for a statement".into())
        })?);
    }
    Ok(proofs)
}

fn host_adapter_private_sql(sql: &str) -> bool {
    let t = sql.trim();
    let u = t.to_ascii_uppercase();
    crate::is_host_schema_version_marker(t)
        || u.starts_with("PRAGMA ")
        || u.starts_with("SET LOCAL ")
        || u.starts_with("CREATE OR REPLACE FUNCTION")
        || u.starts_with("CREATE FUNCTION")
        || u.starts_with("CREATE TRIGGER")
        || u.starts_with("DROP FUNCTION")
        || u.starts_with("DROP TRIGGER")
        || u.starts_with("ALTER TABLE")
        || u.starts_with("DO $")
}

/// Per-statement result bounds. Row/byte `0` means unlimited at execute time.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecCaps {
    /// Maximum rows a statement may return (`0` = unlimited).
    pub max_result_rows: u32,
    /// Maximum JSON bytes of one statement's rows (`0` = unlimited).
    pub max_result_bytes: u32,
    /// Maximum UTF-8 / blob bytes of one cell (`0` = unlimited).
    pub max_cell_bytes: u32,
    /// Maximum JSON bytes of the whole encoded [`DbPlanExecResult`] (`0` = unlimited).
    pub max_atomic_result_bytes: u32,
}

impl From<u32> for ExecCaps {
    fn from(max_result_rows: u32) -> Self {
        Self {
            max_result_rows,
            max_result_bytes: 0,
            max_cell_bytes: 0,
            max_atomic_result_bytes: 0,
        }
    }
}

impl ExecCaps {
    /// Copies negotiated capability limits into the executor.
    #[must_use]
    pub fn from_capabilities(caps: &DbCapabilities) -> Self {
        Self {
            max_result_rows: caps.max_result_rows,
            max_result_bytes: caps.max_result_bytes,
            max_cell_bytes: caps.max_cell_bytes,
            max_atomic_result_bytes: caps.max_atomic_result_bytes,
        }
    }
}

/// Executes `plan` as one transaction and returns generic statement results.
///
/// `max_result_rows` of `0` (when passing a bare `u32`) means unlimited rows;
/// byte caps default to unlimited in that form. Pass
/// [`ExecCaps::from_capabilities`] to enforce negotiated result/cell budgets.
/// A statement that yields more
/// than the cap fails the plan (the transaction is rolled back) rather than
/// truncating the result.
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails or the session is interrupted.
pub async fn execute_statements_on(
    db: &DatabaseConnection,
    plan: &DbAtomicPlan,
    operation_id: &str,
    timing_source: &str,
    caps: impl Into<ExecCaps>,
) -> Result<DbPlanExecResult, DbErr> {
    execute_statements_on_session(
        db,
        plan,
        operation_id,
        timing_source,
        caps,
        AtomicSession::default(),
    )
    .await
}

/// [`execute_statements_on`] with session cancel / deadline checks.
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails or the session is interrupted.
pub async fn execute_statements_on_session(
    db: &DatabaseConnection,
    plan: &DbAtomicPlan,
    operation_id: &str,
    timing_source: &str,
    caps: impl Into<ExecCaps>,
    session: AtomicSession,
) -> Result<DbPlanExecResult, DbErr> {
    let caps = caps.into();
    session.check(AtomicInterruptPhase::BeforeBegin)?;
    let budget = ExecBudget::new(session.deadline_unix_ms, caps.max_result_rows);
    let seen_budget = Arc::clone(&budget);
    let result = with_exec_budget(Arc::clone(&budget), || {
        execute_statements_body(db, plan, operation_id, timing_source, caps, session)
    })
    .await;
    record_query_rows_seen(seen_budget.rows_seen());
    result
}

/// Body of [`execute_statements_on_session`] after the request budget is armed.
///
/// # Errors
///
/// Returns [`DbErr`] when a statement fails, a result budget is exceeded, or the session is interrupted.
async fn execute_statements_body(
    db: &DatabaseConnection,
    plan: &DbAtomicPlan,
    operation_id: &str,
    timing_source: &str,
    caps: ExecCaps,
    session: AtomicSession,
) -> Result<DbPlanExecResult, DbErr> {
    let started = Instant::now();
    let backend = ConnectionTrait::get_database_backend(db);
    let sql_started = Instant::now();
    // Snapshot in autocommit so SQLite BEGIN IMMEDIATE does not hold a reserved
    // lock across paged catalog/physical loads (concurrent writers wait on
    // busy_timeout). Physical tables first so ad-hoc host test tables (created
    // via raw SQL) are visible. Canonical host schema and the plugin catalog
    // overwrite overlapping names; plugin typed execute never calls this loader.
    let mut env = load_physical_sql_type_env(db).await?;
    env.merge(&load_sql_type_env(db).await?);
    env.merge(&sql_host_bookkeeping_type_env());
    env.merge(&session.type_env);
    let type_req = plan_as_typed_request(plan, operation_id);
    let proofs = proofs_for_host_plan(&type_req, &env)?;
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
    let mut statements = Vec::with_capacity(plan.statements.len());
    let mut used_atomic = atomic_result_envelope_len(operation_id);
    for (stmt, proof) in plan.statements.iter().zip(proofs.iter()) {
        if let Err(err) = session.check(AtomicInterruptPhase::BetweenStatements) {
            let _ = txn.rollback().await;
            let _ = take_txn_fault();
            return Err(err);
        }
        let values: Vec<Value> = stmt.binds.iter().map(json_to_sea).collect();
        let canonical = stmt.sql.clone();
        let sql = if bookclerk_plugin_abi::statement_is_ddl(&canonical) {
            canonical.clone()
        } else {
            lower_canonical_sql_typed(backend, canonical.trim(), Some(proof))
                .map_err(|err| DbErr::Custom(err.to_string()))?
        };
        let sql = if stmt.kind.wrap_select_limit() {
            cap_query_sql(&sql, caps.max_result_rows)
        } else {
            sql
        };
        let sea_stmt = Statement::from_sql_and_values(backend, &sql, values);
        let stmt_result = if stmt.kind.collects_rows() {
            let json_rows = match collect_capped_query_rows(&txn, sea_stmt, caps).await {
                Ok(rows) => rows,
                Err(err) => {
                    let _ = txn.rollback().await;
                    let _ = take_txn_fault();
                    return Err(err);
                }
            };
            let rows_affected = rows_affected_for_kind(stmt.kind, json_rows.len());
            DbPlanStmtExecResult {
                rows: json_rows,
                rows_affected,
            }
        } else {
            let exec = match txn.execute_raw(sea_stmt).await {
                Ok(exec) => exec,
                Err(err) => {
                    let _ = txn.rollback().await;
                    let _ = take_txn_fault();
                    return Err(err);
                }
            };
            DbPlanStmtExecResult {
                rows: Vec::new(),
                rows_affected: exec.rows_affected(),
            }
        };
        if let Err(err) =
            apply_exec_identity_companions(&txn, backend, &canonical, &proof.schema_action).await
        {
            let _ = txn.rollback().await;
            let _ = take_txn_fault();
            return Err(err);
        }
        apply_schema_action_to_env(&mut env, &proof.schema_action);
        if let Err(err) = note_atomic_stmt_bytes(
            &mut used_atomic,
            statements.len(),
            &stmt_result,
            caps.max_atomic_result_bytes,
        ) {
            let _ = txn.rollback().await;
            let _ = take_txn_fault();
            return Err(err);
        }
        statements.push(stmt_result);
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
    let result = DbPlanExecResult {
        operation_id: operation_id.to_string(),
        statements,
        timing: Some(DbAtomicTiming {
            attempt_elapsed_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            db_execution_us: Some(db_execution_us),
            db_timing_source: Some(timing_source.to_string()),
        }),
    };
    if let Err(err) = note_atomic_result_bytes(&result, caps.max_atomic_result_bytes) {
        let _ = txn.rollback().await;
        let _ = take_txn_fault();
        return Err(err);
    }
    txn.commit().await.map_err(|err| {
        let _ = take_txn_fault();
        DbErr::Custom(format!("database commit failed: {err}"))
    })?;
    if let Some(fault) = take_txn_fault() {
        return Err(DbErr::Custom(fault));
    }
    Ok(result)
}

/// Maps a session interrupt onto a guest-classifiable [`DbErr`].
fn interrupt_err(phase: AtomicInterruptPhase, kind: AtomicInterruptKind) -> DbErr {
    let around_commit = matches!(phase, AtomicInterruptPhase::AroundCommit);
    let msg = match (around_commit, kind) {
        (true, _) => "database commit failed: session interrupt at commit",
        (false, AtomicInterruptKind::Cancel) => "cancelled: atomic session cancelled",
        (false, AtomicInterruptKind::Deadline) => "deadline_exceeded: atomic deadline elapsed",
    };
    DbErr::Custom(msg.into())
}

/// Current unix time in milliseconds.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Remaining milliseconds until `deadline_unix_ms` (`None` if unlimited).
pub(crate) fn remaining_deadline_ms(deadline_unix_ms: Option<u64>) -> Option<u64> {
    let dl = deadline_unix_ms?;
    Some(dl.saturating_sub(unix_now_ms()).max(1))
}

/// Wraps a host-tagged [`bookclerk_plugin_abi::DbPlanStatementKind::Select`] so the engine stops
/// after `cap + 1` rows. Callers must not pass DML `RETURNING` SQL.
#[must_use]
pub fn cap_query_sql(sql: &str, max_result_rows: u32) -> String {
    if max_result_rows == 0 {
        return sql.to_string();
    }
    let n = u64::from(max_result_rows) + 1;
    let inner = sql.trim().trim_end_matches(';');
    format!("SELECT * FROM ({inner}) AS _bc_cap LIMIT {n}")
}

/// Collects at most `max_result_rows + 1` engine rows (no JSON conversion).
///
/// PostgreSQL is streamed. SQLite goes through the rusqlite proxy, which stops
/// the cursor at the same cap (and records positional column metadata).
///
/// # Errors
///
/// Returns when the engine stream/query fails.
pub(crate) async fn collect_capped_query_results(
    txn: &sea_orm::DatabaseTransaction,
    stmt: Statement,
    max_result_rows: u32,
) -> Result<Vec<QueryResult>, DbErr> {
    let stop_after = row_stop_after(max_result_rows);
    if ConnectionTrait::get_database_backend(txn) == sea_orm::DatabaseBackend::Postgres {
        let stream = txn.stream_raw(stmt).await?;
        futures::pin_mut!(stream);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await? {
            let _ = note_query_row();
            rows.push(row);
            if rows.len() >= stop_after {
                break;
            }
        }
        return Ok(rows);
    }
    let rows = txn.query_all_raw(stmt).await?;
    Ok(rows.into_iter().take(stop_after).collect())
}

/// Collects at most `max_result_rows + 1` rows, checking cell/result bytes first.
///
/// # Errors
///
/// Returns [`DbErr`] when the engine query fails or a result budget is exceeded.
async fn collect_capped_query_rows(
    txn: &sea_orm::DatabaseTransaction,
    stmt: Statement,
    caps: ExecCaps,
) -> Result<Vec<JsonValue>, DbErr> {
    let rows = collect_capped_query_results(txn, stmt, caps.max_result_rows).await?;
    let mut json_rows = Vec::new();
    let mut used = 0usize;
    for row in rows {
        push_capped_json_row(&mut json_rows, &mut used, query_row_to_json(row)?, caps)?;
    }
    json_rows_respecting_cap(json_rows, caps.max_result_rows)
}

/// Uniform `rowsAffected` by host-authored kind.
pub(crate) fn rows_affected_for_kind(kind: DbPlanStatementKind, returned_rows: usize) -> u64 {
    match kind {
        DbPlanStatementKind::Select => 0,
        DbPlanStatementKind::Returning => u64::try_from(returned_rows).unwrap_or(u64::MAX),
        DbPlanStatementKind::Execute => 0,
    }
}

/// Fetch one extra row past a positive cap so overflow can fail closed.
fn row_stop_after(max_result_rows: u32) -> usize {
    if max_result_rows == 0 {
        usize::MAX
    } else {
        usize::try_from(max_result_rows)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    }
}

/// Converts one engine row, enforcing cell then statement byte budgets.
///
/// # Errors
///
/// Returns [`DbErr`] when a cell or statement byte budget is exceeded.
fn push_capped_json_row(
    json_rows: &mut Vec<JsonValue>,
    used: &mut usize,
    json: JsonValue,
    caps: ExecCaps,
) -> Result<(), DbErr> {
    reject_row_cell_bytes(&json, caps.max_cell_bytes)?;
    note_result_row_bytes(&json, used, caps.max_result_bytes)?;
    json_rows.push(json);
    Ok(())
}

/// JSON bytes of an empty `DbPlanExecResult` envelope (`statements: []`).
fn atomic_result_envelope_len(operation_id: &str) -> usize {
    let empty = DbPlanExecResult {
        operation_id: operation_id.to_string(),
        statements: Vec::new(),
        timing: None,
    };
    serde_json::to_vec(&empty)
        .map(|b| b.len())
        .unwrap_or(usize::MAX)
}

/// Adds one encoded statement result (plus array comma) toward the aggregate cap.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when the running aggregate would exceed `max_atomic_result_bytes`.
fn note_atomic_stmt_bytes(
    used: &mut usize,
    stmt_index: usize,
    stmt: &DbPlanStmtExecResult,
    max_atomic_result_bytes: u32,
) -> Result<(), DbErr> {
    if max_atomic_result_bytes == 0 {
        return Ok(());
    }
    let encoded = serde_json::to_vec(stmt)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    let extra = if stmt_index == 0 {
        encoded
    } else {
        encoded.saturating_add(1)
    };
    *used = used.saturating_add(extra);
    let cap = usize::try_from(max_atomic_result_bytes).unwrap_or(usize::MAX);
    if *used > cap {
        return Err(DbErr::Custom(format!(
            "atomic result is {used} bytes; maxAtomicResultBytes is {max_atomic_result_bytes}"
        )));
    }
    Ok(())
}

/// Exact encoded-size check of the finished result (including timing).
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when the encoded result exceeds `max_atomic_result_bytes`.
fn note_atomic_result_bytes(
    result: &DbPlanExecResult,
    max_atomic_result_bytes: u32,
) -> Result<(), DbErr> {
    if max_atomic_result_bytes == 0 {
        return Ok(());
    }
    let used = serde_json::to_vec(result)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    let cap = usize::try_from(max_atomic_result_bytes).unwrap_or(usize::MAX);
    if used > cap {
        return Err(DbErr::Custom(format!(
            "atomic result is {used} bytes; maxAtomicResultBytes is {max_atomic_result_bytes}"
        )));
    }
    Ok(())
}

/// Maps a SeaORM query row onto the generic JSON result object.
///
/// # Errors
///
/// Returns [`DbErr`] when the row cannot be converted to JSON.
fn query_row_to_json(row: QueryResult) -> Result<JsonValue, DbErr> {
    let proxy = from_query_result_to_proxy_row(&row);
    let mut map = serde_json::Map::new();
    for (name, value) in proxy.values {
        map.insert(name, sea_value_to_json(&value));
    }
    Ok(JsonValue::Object(map))
}

/// UTF-8 / JSON length of one cell used for `maxCellBytes`.
pub fn json_cell_utf8_len(v: &JsonValue) -> usize {
    match v {
        JsonValue::String(s) => s.len(),
        JsonValue::Array(_) | JsonValue::Object(_) => v.to_string().len(),
        _ => 0,
    }
}

/// Fails when any string/blob cell in `row` exceeds `max_cell_bytes`.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when a cell exceeds `max_cell_bytes`.
fn reject_row_cell_bytes(row: &JsonValue, max_cell_bytes: u32) -> Result<(), DbErr> {
    if max_cell_bytes == 0 {
        return Ok(());
    }
    let cap = usize::try_from(max_cell_bytes).unwrap_or(usize::MAX);
    let JsonValue::Object(map) = row else {
        let n = json_cell_utf8_len(row);
        if n > cap {
            return Err(DbErr::Custom(format!(
                "result cell is {n} bytes; maxCellBytes is {max_cell_bytes}"
            )));
        }
        return Ok(());
    };
    for (name, v) in map {
        let n = json_cell_utf8_len(v);
        if n > cap {
            return Err(DbErr::Custom(format!(
                "column `{name}` is {n} bytes; maxCellBytes is {max_cell_bytes}"
            )));
        }
    }
    Ok(())
}

/// Accumulates JSON bytes of `row` and fails when the statement budget is exceeded.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when the running total would exceed `max_result_bytes`.
fn note_result_row_bytes(
    row: &JsonValue,
    used: &mut usize,
    max_result_bytes: u32,
) -> Result<(), DbErr> {
    note_encoded_result_bytes(used, row.to_string().len(), max_result_bytes)
}

/// Adds `extra` encoded JSON bytes toward `max_result_bytes`.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when the running total would exceed the cap.
pub fn note_encoded_result_bytes(
    used: &mut usize,
    extra: usize,
    max_result_bytes: u32,
) -> Result<(), DbErr> {
    if max_result_bytes == 0 {
        return Ok(());
    }
    *used = used.saturating_add(extra);
    let cap = usize::try_from(max_result_bytes).unwrap_or(usize::MAX);
    if *used > cap {
        return Err(DbErr::Custom(format!(
            "query result is {used} bytes; maxResultBytes is {max_result_bytes}"
        )));
    }
    Ok(())
}

/// Fails closed when `rows` exceeds a positive cap (the extra row was fetched
/// only to detect overflow).
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when `rows` exceeds `max_result_rows`.
fn json_rows_respecting_cap(
    json_rows: Vec<JsonValue>,
    max_result_rows: u32,
) -> Result<Vec<JsonValue>, DbErr> {
    if exceeds_result_row_cap(json_rows.len(), max_result_rows) {
        return Err(DbErr::Custom(format!(
            "query returned {} rows; maxResultRows is {max_result_rows}",
            json_rows.len()
        )));
    }
    Ok(json_rows)
}

/// Fails closed when `n` exceeds a positive `max_result_rows` cap.
pub(crate) fn exceeds_result_row_cap(n: usize, max_result_rows: u32) -> bool {
    if max_result_rows == 0 {
        return false;
    }
    let cap = usize::try_from(max_result_rows).unwrap_or(usize::MAX);
    n > cap
}

/// Maps a JSON bind onto a SeaORM [`Value`], decoding `b64:` strings as blobs.
fn json_to_sea(v: &JsonValue) -> Value {
    if let Some(kind) = crate::host_ir::sea_null_kind(v) {
        return match kind {
            "Bytes" => Value::Bytes(None),
            "BigInt" | "Int" | "TinyInt" | "SmallInt" | "TinyUnsigned" | "SmallUnsigned"
            | "Unsigned" | "BigUnsigned" => Value::BigInt(None),
            "Bool" => Value::Bool(None),
            "Double" | "Float" => Value::Double(None),
            _ => Value::String(None),
        };
    }
    match v {
        JsonValue::Null => Value::String(None),
        JsonValue::Bool(b) => Value::Bool(Some(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::BigInt(Some(i))
            } else if let Some(u) = n.as_u64() {
                Value::BigInt(Some(i64::try_from(u).unwrap_or(i64::MAX)))
            } else {
                Value::Double(n.as_f64())
            }
        }
        JsonValue::String(s) => {
            if let Some(bytes) = crate::b64_string_to_bytes(s) {
                Value::Bytes(Some(bytes))
            } else {
                Value::String(Some(s.clone()))
            }
        }
        other => Value::String(Some(other.to_string())),
    }
}

/// Maps a SeaORM cell onto JSON for plan interpretation.
#[must_use]
pub fn sea_value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Bool(Some(b)) => JsonValue::Bool(*b),
        Value::TinyInt(Some(n)) => JsonValue::from(*n),
        Value::SmallInt(Some(n)) => JsonValue::from(*n),
        Value::Int(Some(n)) => JsonValue::from(*n),
        Value::BigInt(Some(n)) => JsonValue::from(*n),
        Value::TinyUnsigned(Some(n)) => JsonValue::from(*n),
        Value::SmallUnsigned(Some(n)) => JsonValue::from(*n),
        Value::Unsigned(Some(n)) => JsonValue::from(*n),
        Value::BigUnsigned(Some(n)) => JsonValue::from(*n),
        Value::Float(Some(n)) => JsonValue::from(f64::from(*n)),
        Value::Double(Some(n)) => JsonValue::from(*n),
        Value::String(Some(s)) => JsonValue::String(s.clone()),
        Value::Char(Some(c)) => JsonValue::String(c.to_string()),
        Value::Bytes(Some(b)) => JsonValue::String(crate::bytes_to_b64_string(b)),
        _ => JsonValue::Null,
    }
}

/// Encoded JSON length of a SeaORM proxy row (keys, numbers, punctuation).
#[must_use]
pub fn encoded_proxy_row_len<'a>(
    values: impl IntoIterator<Item = (&'a String, &'a Value)>,
) -> usize {
    let mut map = serde_json::Map::new();
    for (name, value) in values {
        map.insert(name.clone(), sea_value_to_json(value));
    }
    JsonValue::Object(map).to_string().len()
}

#[cfg(test)]
mod tests {
    use super::{cap_query_sql, json_to_sea};
    use crate::host_ir::{DbPlanExecResult, DbPlanStmtExecResult};
    use bookclerk_plugin_abi::DbPlanStatementKind;
    use sea_orm::Value;
    use serde_json::json;

    #[test]
    fn typed_null_bytes_is_bytea_null() {
        assert!(matches!(
            json_to_sea(&json!({ "$sea_null": "Bytes" })),
            Value::Bytes(None)
        ));
    }

    #[test]
    fn typed_null_bigint_is_integer_null() {
        assert!(matches!(
            json_to_sea(&json!({ "$sea_null": "BigInt" })),
            Value::BigInt(None)
        ));
    }

    #[test]
    fn b64_string_decodes_as_bytes() {
        assert!(matches!(
            json_to_sea(&json!("b64:AA==")),
            Value::Bytes(Some(b)) if b.as_slice() == [0]
        ));
    }

    #[test]
    fn cap_query_sql_wraps_readonly_select() {
        let sql = cap_query_sql("SELECT x FROM t", 5);
        assert!(sql.contains("LIMIT 6"), "{sql}");
        assert!(sql.contains("AS _bc_cap"), "{sql}");
    }

    #[test]
    fn only_select_kind_requests_limit_wrap() {
        assert!(DbPlanStatementKind::Select.wrap_select_limit());
        assert!(!DbPlanStatementKind::Returning.wrap_select_limit());
        assert!(!DbPlanStatementKind::Execute.wrap_select_limit());
    }

    #[test]
    fn encoded_proxy_row_counts_keys_and_numbers() {
        use super::{encoded_proxy_row_len, sea_value_to_json};
        let alias = format!("c00_{}", "x".repeat(40));
        let values = [(alias.clone(), Value::BigInt(Some(1)))];
        let nbytes = encoded_proxy_row_len(values.iter().map(|(k, v)| (k, v)));
        let mut map = serde_json::Map::new();
        map.insert(alias.clone(), sea_value_to_json(&values[0].1));
        assert_eq!(nbytes, serde_json::Value::Object(map).to_string().len());
        assert!(
            nbytes > alias.len(),
            "JSON punctuation and the numeric cell must count: {nbytes} vs alias {}",
            alias.len()
        );
    }

    #[test]
    fn incremental_atomic_budget_matches_full_serialize_without_timing() {
        use super::{atomic_result_envelope_len, note_atomic_stmt_bytes};
        let result = DbPlanExecResult {
            operation_id: "op".into(),
            statements: vec![
                DbPlanStmtExecResult {
                    rows: vec![serde_json::json!({"a": 1})],
                    rows_affected: 0,
                },
                DbPlanStmtExecResult {
                    rows: Vec::new(),
                    rows_affected: 1,
                },
            ],
            timing: None,
        };
        let mut used = atomic_result_envelope_len("op");
        for (i, stmt) in result.statements.iter().enumerate() {
            note_atomic_stmt_bytes(&mut used, i, stmt, u32::MAX).unwrap();
        }
        let exact = serde_json::to_vec(&result).unwrap().len();
        assert_eq!(used, exact);
    }
}
