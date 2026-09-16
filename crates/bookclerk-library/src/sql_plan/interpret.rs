//! Decode generic atomic-plan statement results into [`DbAtomicResult`].

use super::host_ir::{DbAtomicPlan, DbPlanExecResult, DbPlanStmtExecResult};
use super::CompiledAtomic;
use bookclerk_plugin_abi::{DbCapabilities, ExecuteReply};
use serde_json::Value as JsonValue;

use crate::atomic_ops::{atomic_status, DbAtomicResult};
use crate::error::{LibraryError, Result};

/// Rows produced by one plan statement.
#[derive(Debug, Clone, Default)]
pub struct PlanStmtResult {
    /// Result-set rows (empty for DML).
    pub rows: Vec<JsonValue>,
    /// Engine-reported rows affected.
    pub rows_affected: u64,
}

impl From<&DbPlanStmtExecResult> for PlanStmtResult {
    fn from(stmt: &DbPlanStmtExecResult) -> Self {
        Self {
            rows: stmt.rows.clone(),
            rows_affected: stmt.rows_affected,
        }
    }
}

/// Maps a guest [`DbPlanExecResult`] onto a host [`DbAtomicResult`].
#[must_use]
pub fn interpret_exec(
    plan: &DbAtomicPlan,
    exec: &DbPlanExecResult,
    expected_hash: &str,
) -> DbAtomicResult {
    let results: Vec<PlanStmtResult> = exec.statements.iter().map(PlanStmtResult::from).collect();
    let mut result = interpret_plan(plan, &results, expected_hash);
    result.operation_id = exec.operation_id.clone();
    if result.timing.is_none() {
        result.timing = exec.timing.clone();
    }
    result
}

/// Maps a typed [`ExecuteReply`] onto a host [`DbAtomicResult`].
#[must_use]
pub fn interpret_typed_exec(
    compiled: &CompiledAtomic,
    reply: &ExecuteReply,
    expected_hash: &str,
) -> DbAtomicResult {
    let exec = super::host_ir::plan_exec_from_execute_reply(reply.clone());
    interpret_exec(&compiled.plan, &exec, expected_hash)
}

/// Rejects a guest atomic result that does not match the sent plan or caps.
///
/// A mismatch after the guest reports success is treated as
/// [`LibraryError::Unavailable`] so the caller retries the same `operationId`
/// rather than interpreting a truncated envelope as `empty` / `notFound`.
///
/// # Errors
///
/// Returns [`LibraryError::Unavailable`] when the echo, statement count, or
/// per-statement row/byte bounds do not match the request.
pub fn validate_exec_result(
    plan: &DbAtomicPlan,
    exec: &DbPlanExecResult,
    caps: &DbCapabilities,
    operation_id: &str,
) -> Result<()> {
    if exec.operation_id != operation_id {
        return Err(LibraryError::Unavailable(format!(
            "atomic result operationId {:?} does not echo {operation_id}",
            exec.operation_id
        )));
    }
    if exec.statements.len() != plan.statements.len() {
        return Err(LibraryError::Unavailable(format!(
            "atomic result has {} statements; plan has {}",
            exec.statements.len(),
            plan.statements.len()
        )));
    }
    for (i, stmt) in exec.statements.iter().enumerate() {
        let n_rows = u32::try_from(stmt.rows.len()).unwrap_or(u32::MAX);
        if caps.max_result_rows > 0 && n_rows > caps.max_result_rows {
            return Err(LibraryError::Unavailable(format!(
                "atomic result statement {i} returned {n_rows} rows; guest maxResultRows is {}",
                caps.max_result_rows
            )));
        }
        if caps.max_result_bytes > 0 {
            let bytes = serde_json::to_vec(&stmt.rows).map(|b| b.len()).unwrap_or(0);
            let cap = usize::try_from(caps.max_result_bytes).unwrap_or(usize::MAX);
            if bytes > cap {
                return Err(LibraryError::Unavailable(format!(
                    "atomic result statement {i} encoded rows are {bytes} bytes; guest maxResultBytes is {}",
                    caps.max_result_bytes
                )));
            }
        }
        if caps.max_cell_bytes > 0 {
            let cap = usize::try_from(caps.max_cell_bytes).unwrap_or(usize::MAX);
            for row in &stmt.rows {
                let over = match row {
                    serde_json::Value::Object(map) => map
                        .values()
                        .any(|cell| bookclerk_db_exec::json_cell_utf8_len(cell) > cap),
                    other => bookclerk_db_exec::json_cell_utf8_len(other) > cap,
                };
                if over {
                    return Err(LibraryError::Unavailable(format!(
                        "atomic result statement {i} cell exceeds guest maxCellBytes {}",
                        caps.max_cell_bytes
                    )));
                }
            }
        }
    }
    if caps.max_atomic_result_bytes > 0 {
        let bytes = serde_json::to_vec(exec).map(|b| b.len()).unwrap_or(0);
        let cap = usize::try_from(
            caps.max_atomic_result_bytes
                .min(bookclerk_plugin_abi::MAX_SCALAR_BYTES),
        )
        .unwrap_or(usize::MAX);
        if bytes > cap {
            return Err(LibraryError::Unavailable(format!(
                "atomic result is {bytes} bytes; guest maxAtomicResultBytes is {cap}"
            )));
        }
    }
    Ok(())
}

/// Maps statement rows onto a [`DbAtomicResult`], preferring receipt rows.
#[must_use]
pub fn interpret_plan(
    plan: &DbAtomicPlan,
    results: &[PlanStmtResult],
    expected_hash: &str,
) -> DbAtomicResult {
    if let Some(idx) = plan.prior_receipt_index {
        let idx = idx as usize;
        if let Some(row) = results.get(idx).and_then(|r| r.rows.first()) {
            return interpret_receipt(Some(row), expected_hash, true);
        }
    }
    if let Some(idx) = plan.receipt_select_index {
        let idx = idx as usize;
        return interpret_receipt(
            results.get(idx).and_then(|r| r.rows.first()),
            expected_hash,
            false,
        );
    }
    let Some(outcome) = results
        .get(plan.outcome_index as usize)
        .and_then(|r| r.rows.first())
    else {
        return DbAtomicResult::with_status(atomic_status::CLAIM_INVALID);
    };
    let status = outcome
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or(atomic_status::CLAIM_INVALID);
    if status != atomic_status::OK {
        return DbAtomicResult::with_status(status);
    }
    let Some(payload_index) = plan.payload_index else {
        return DbAtomicResult::ok_unit();
    };
    let Some(row) = results
        .get(payload_index as usize)
        .and_then(|r| r.rows.first())
    else {
        return DbAtomicResult::with_status(atomic_status::NOT_FOUND);
    };
    if let Some(payload) = row.get("payload") {
        return match decode_receipt_payload(Some(payload)) {
            Some(value) => DbAtomicResult::ok(value),
            None => DbAtomicResult::ok_unit(),
        };
    }
    DbAtomicResult::ok(row.clone())
}

/// Decodes a receipt row, flagging an idempotency conflict when `request_hash` differs.
fn interpret_receipt(
    row: Option<&JsonValue>,
    expected_hash: &str,
    replayed: bool,
) -> DbAtomicResult {
    let Some(row) = row else {
        return DbAtomicResult::with_status(atomic_status::EMPTY);
    };
    let stored_hash = row
        .get("request_hash")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if stored_hash != expected_hash {
        return DbAtomicResult::with_status(atomic_status::IDEMPOTENCY_CONFLICT);
    }
    let status = row
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or(atomic_status::EMPTY);
    let created_at = row
        .get("created_at")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let mut result = if status == atomic_status::OK {
        match decode_receipt_payload(row.get("payload")) {
            Some(payload) => DbAtomicResult::ok(payload),
            None => DbAtomicResult::ok_unit(),
        }
    } else {
        DbAtomicResult::with_status(status)
    };
    result.replayed = replayed;
    result.receipt_created_at = created_at;
    result
}

/// Parses a receipt `payload` cell, accepting JSON objects or a JSON-encoded string.
fn decode_receipt_payload(value: Option<&JsonValue>) -> Option<JsonValue> {
    match value {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::String(s)) => serde_json::from_str(s)
            .ok()
            .or_else(|| Some(JsonValue::String(s.clone()))),
        Some(other) => Some(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_exec_result, DbAtomicPlan, DbPlanExecResult, DbPlanStmtExecResult};
    use crate::sql_plan::DbPlanStatement;
    use crate::LibraryError;
    use bookclerk_plugin_abi::{DbCapabilities, DbPlanStatementKind};

    fn one_stmt_plan() -> DbAtomicPlan {
        DbAtomicPlan {
            statements: vec![DbPlanStatement {
                sql: "SELECT 1".into(),
                binds: vec![],
                kind: DbPlanStatementKind::Returning,
                max_rows: 0,
            }],
            outcome_index: 0,
            payload_index: None,
            prior_receipt_index: None,
            receipt_select_index: None,
        }
    }

    fn exec(id: &str, statements: Vec<DbPlanStmtExecResult>) -> DbPlanExecResult {
        DbPlanExecResult {
            operation_id: id.into(),
            statements,
            timing: None,
        }
    }

    #[test]
    fn validate_exec_rejects_wrong_operation_id() {
        let err = validate_exec_result(
            &one_stmt_plan(),
            &exec(
                "other",
                vec![DbPlanStmtExecResult {
                    rows: vec![],
                    rows_affected: 0,
                }],
            ),
            &DbCapabilities::advertised_sqlite(),
            "wanted",
        )
        .unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("operationId"), "{err}");
    }

    #[test]
    fn validate_exec_rejects_short_statement_list() {
        let err = validate_exec_result(
            &one_stmt_plan(),
            &exec("wanted", vec![]),
            &DbCapabilities::advertised_sqlite(),
            "wanted",
        )
        .unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("statements"), "{err}");
    }

    #[test]
    fn validate_exec_rejects_over_cap_rows() {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_result_rows = 1;
        let err = validate_exec_result(
            &one_stmt_plan(),
            &exec(
                "wanted",
                vec![DbPlanStmtExecResult {
                    rows: vec![serde_json::json!({"a":1}), serde_json::json!({"a":2})],
                    rows_affected: 2,
                }],
            ),
            &caps,
            "wanted",
        )
        .unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("maxResultRows"), "{err}");
    }
}
