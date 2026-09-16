//! Host-internal SQL plan IR and bridges to the typed plugin ABI.
//!
//! Planner types live in [`bookclerk_db_exec::host_ir`]; this module re-exports
//! them for library callers and converts between host IR and
//! [`bookclerk_plugin_abi::ExecuteRequest`] / [`ExecuteReply`].

#![allow(clippy::missing_docs_in_private_items)]

pub use bookclerk_db_exec::host_ir::{
    sea_null, sea_null_kind, DbAtomicPlan, DbAtomicRequest, DbAtomicTiming, DbPlanExecResult,
    DbPlanStatement, DbPlanStmtExecResult, DB_ATOMIC_SENTINEL, DB_CAPABILITIES_SENTINEL,
    SEA_NULL_KEY,
};

use bookclerk_db_exec::{db_value_from_json, db_value_to_json};
use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, DbTiming, ExecuteReply, ExecuteRequest,
    StatementResult, TypedDbStatement,
};

/// Converts a host IR statement onto the typed wire.
///
/// # Errors
///
/// Returns when a bind is outside the universal [`bookclerk_plugin_abi::DbValue`] domain.
pub fn typed_statement_from_plan(stmt: &DbPlanStatement) -> Result<TypedDbStatement, String> {
    let parameters = stmt
        .binds
        .iter()
        .map(db_value_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TypedDbStatement {
        sql: stmt.sql.clone(),
        parameters,
        kind: stmt.kind,
        max_rows: stmt.max_rows,
        result_selection: selection_for_kind(stmt.kind),
    })
}

/// JSON-bind plan statement used by in-process executors.
#[must_use]
pub fn plan_statement_from_typed(stmt: &TypedDbStatement) -> DbPlanStatement {
    DbPlanStatement {
        sql: stmt.sql.clone(),
        binds: stmt.parameters.iter().map(db_value_to_json).collect(),
        kind: stmt.kind,
        max_rows: stmt.max_rows,
    }
}

fn selection_for_kind(kind: DbPlanStatementKind) -> DbResultSelection {
    match kind {
        DbPlanStatementKind::Execute => DbResultSelection::AffectedRows,
        _ => DbResultSelection::Rows,
    }
}

/// Typed request from a host [`DbAtomicRequest`].
///
/// # Errors
///
/// Returns when the plan is missing, empty, or a bind is not a [`bookclerk_plugin_abi::DbValue`].
pub fn execute_request_from_atomic(req: &DbAtomicRequest) -> Result<ExecuteRequest, String> {
    let plan = req
        .plan
        .as_ref()
        .ok_or_else(|| "executeAtomic requires a host-authored plan".to_string())?;
    if plan.statements.is_empty() {
        return Err("executeAtomic statements must be non-empty".into());
    }
    let statements = plan
        .statements
        .iter()
        .map(typed_statement_from_plan)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ExecuteRequest {
        operation_id: req.operation_id.clone(),
        request_hash: req.request_hash.clone().unwrap_or_default(),
        statements,
        deadline_unix_ms: req.deadline_unix_ms.unwrap_or(0),
    })
}

/// Host IR envelope from a typed [`ExecuteRequest`].
///
/// # Errors
///
/// Returns when the statement list is empty.
pub fn atomic_from_execute_request(req: ExecuteRequest) -> Result<DbAtomicRequest, String> {
    if req.statements.is_empty() {
        return Err("executeAtomic statements must be non-empty".into());
    }
    let plan = DbAtomicPlan {
        statements: req
            .statements
            .iter()
            .map(plan_statement_from_typed)
            .collect(),
        outcome_index: 0,
        payload_index: None,
        prior_receipt_index: None,
        receipt_select_index: None,
    };
    Ok(DbAtomicRequest {
        operation_id: req.operation_id,
        request_hash: if req.request_hash.is_empty() {
            None
        } else {
            Some(req.request_hash)
        },
        plan: Some(plan),
        deadline_unix_ms: (req.deadline_unix_ms > 0).then_some(req.deadline_unix_ms),
    })
}

/// Host IR plan result from a typed [`ExecuteReply`].
#[must_use]
pub fn plan_exec_from_execute_reply(reply: ExecuteReply) -> DbPlanExecResult {
    DbPlanExecResult {
        operation_id: reply.operation_id,
        statements: reply
            .statements
            .iter()
            .map(statement_result_to_plan_stmt)
            .collect(),
        timing: Some(db_atomic_timing_from_reply(&reply.timing)),
    }
}

fn statement_result_to_plan_stmt(stmt: &StatementResult) -> DbPlanStmtExecResult {
    DbPlanStmtExecResult {
        rows: typed_rows_to_json(&stmt.columns, &stmt.rows),
        rows_affected: stmt.rows_affected,
    }
}

fn db_atomic_timing_from_reply(t: &DbTiming) -> DbAtomicTiming {
    DbAtomicTiming {
        attempt_elapsed_us: t.attempt_elapsed_us,
        db_execution_us: (t.db_execution_us > 0).then_some(t.db_execution_us),
        db_timing_source: (!t.db_timing_source.is_empty()).then_some(t.db_timing_source.clone()),
    }
}

fn typed_rows_to_json(
    columns: &[bookclerk_plugin_abi::DbColumn],
    rows: &[bookclerk_plugin_abi::DbRow],
) -> Vec<serde_json::Value> {
    rows.iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (col, val) in columns.iter().zip(row.values.iter()) {
                obj.insert(col.name.clone(), db_value_to_json(val));
            }
            serde_json::Value::Object(obj)
        })
        .collect()
}
