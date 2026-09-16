//! Trust-boundary validation for typed [`ExecuteReply`] envelopes from guests.

use bookclerk_plugin_abi::{
    encoded_execute_reply_bytes, encoded_statement_result_bytes, DbCapabilities, DbResultSelection,
    DbValue, ExecuteReply, ExecuteRequest,
};

use crate::error::{LibraryError, Result};

/// Rejects a guest [`ExecuteReply`] that does not echo the sent request or
/// exceeds negotiated caps.
///
/// A mismatch after the guest reports success is treated as
/// [`LibraryError::Unavailable`] so callers retry the same `operationId`
/// rather than interpreting a truncated or mismatched envelope.
///
/// # Errors
///
/// Returns [`LibraryError::Unavailable`] when the echo, statement count,
/// result selection shape, or byte/row bounds do not match the request.
pub fn validate_execute_reply(
    req: &ExecuteRequest,
    reply: &ExecuteReply,
    caps: &DbCapabilities,
) -> Result<()> {
    reply
        .validate_positional()
        .map_err(LibraryError::Unavailable)?;

    if reply.operation_id != req.operation_id {
        return Err(LibraryError::Unavailable(format!(
            "execute reply operationId {:?} does not echo {}",
            reply.operation_id, req.operation_id
        )));
    }

    if reply.statements.len() != req.statements.len() {
        return Err(LibraryError::Unavailable(format!(
            "execute reply has {} statements; request has {}",
            reply.statements.len(),
            req.statements.len()
        )));
    }

    for (i, (req_stmt, stmt)) in req.statements.iter().zip(&reply.statements).enumerate() {
        validate_statement_result(i, req_stmt.result_selection, req_stmt.max_rows, stmt, caps)?;
    }

    if let Some(cap) = atomic_result_cap_bytes(caps) {
        let bytes = encoded_execute_reply_bytes(reply)
            .map_err(|err| LibraryError::Unavailable(err.to_string()))?
            .len();
        if bytes > cap {
            return Err(LibraryError::Unavailable(format!(
                "execute reply is {bytes} bytes; guest maxAtomicResultBytes is {cap}"
            )));
        }
    }

    Ok(())
}

/// Validates one statement result against the request's selection and negotiated caps.
fn validate_statement_result(
    index: usize,
    selection: DbResultSelection,
    stmt_max_rows: u32,
    stmt: &bookclerk_plugin_abi::StatementResult,
    caps: &DbCapabilities,
) -> Result<()> {
    match selection {
        DbResultSelection::AffectedRows => {
            if !stmt.rows.is_empty() {
                return Err(LibraryError::Unavailable(format!(
                    "execute reply statement {index} returned {} rows for affectedRows selection",
                    stmt.rows.len()
                )));
            }
            if !stmt.columns.is_empty() {
                return Err(LibraryError::Unavailable(format!(
                    "execute reply statement {index} returned columns for affectedRows selection"
                )));
            }
        }
        DbResultSelection::Discard => {
            if !stmt.rows.is_empty() {
                return Err(LibraryError::Unavailable(format!(
                    "execute reply statement {index} returned {} rows for discard selection",
                    stmt.rows.len()
                )));
            }
            if !stmt.columns.is_empty() {
                return Err(LibraryError::Unavailable(format!(
                    "execute reply statement {index} returned columns for discard selection"
                )));
            }
            return Ok(());
        }
        DbResultSelection::Rows => {
            if let Some(cap) = effective_row_cap(stmt_max_rows, caps.max_result_rows) {
                let n_rows = u32::try_from(stmt.rows.len()).unwrap_or(u32::MAX);
                if n_rows > cap {
                    return Err(LibraryError::Unavailable(format!(
                        "execute reply statement {index} returned {n_rows} rows; limit is {cap}"
                    )));
                }
            }
        }
    }

    if caps.max_cell_bytes > 0 && !stmt.rows.is_empty() {
        let cap = usize::try_from(caps.max_cell_bytes).unwrap_or(usize::MAX);
        for row in &stmt.rows {
            for cell in &row.values {
                let n = db_value_cell_len(cell);
                if n > cap {
                    return Err(LibraryError::Unavailable(format!(
                        "execute reply statement {index} cell exceeds guest maxCellBytes {}",
                        caps.max_cell_bytes
                    )));
                }
            }
        }
    }

    if caps.max_result_bytes > 0 {
        let bytes = encoded_statement_result_bytes(stmt)
            .map_err(|err| LibraryError::Unavailable(err.to_string()))?
            .len();
        let cap = usize::try_from(caps.max_result_bytes).unwrap_or(usize::MAX);
        if bytes > cap {
            return Err(LibraryError::Unavailable(format!(
                "execute reply statement {index} encoded result is {bytes} bytes; guest maxResultBytes is {}",
                caps.max_result_bytes
            )));
        }
    }

    Ok(())
}

/// Effective row upper bound from statement and negotiated caps (`None` = unlimited).
fn effective_row_cap(stmt_max_rows: u32, caps_max_result_rows: u32) -> Option<u32> {
    match (stmt_max_rows, caps_max_result_rows) {
        (0, 0) => None,
        (a, 0) => Some(a),
        (0, b) => Some(b),
        (a, b) => Some(a.min(b)),
    }
}

/// Encoded whole-reply byte budget from negotiated caps (`None` = unlimited).
fn atomic_result_cap_bytes(caps: &DbCapabilities) -> Option<usize> {
    if caps.max_atomic_result_bytes == 0 {
        return None;
    }
    Some(
        usize::try_from(
            caps.max_atomic_result_bytes
                .min(bookclerk_plugin_abi::MAX_SCALAR_BYTES),
        )
        .unwrap_or(usize::MAX),
    )
}

/// UTF-8 / byte length counted toward `maxCellBytes` for text and blob cells.
fn db_value_cell_len(v: &DbValue) -> usize {
    match v {
        DbValue::Text(s) => s.len(),
        DbValue::Bytes(b) => b.len(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_plugin_abi::{
        DbColumn, DbPlanStatementKind, DbResultSelection, DbRow, DbType, ExecuteRequest,
        StatementResult, TypedDbStatement,
    };

    use super::*;

    fn tiny_caps() -> DbCapabilities {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_result_rows = 2;
        caps.max_result_bytes = 256;
        caps.max_cell_bytes = 16;
        caps.max_atomic_result_bytes = 512;
        caps
    }

    fn rows_request(max_rows: u32) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: "op-1".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT id FROM books".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        }
    }

    fn rows_reply(operation_id: &str, row_count: usize) -> ExecuteReply {
        let rows = (0..row_count)
            .map(|i| DbRow {
                values: vec![DbValue::Int64(i64::try_from(i).unwrap_or(0))],
            })
            .collect();
        ExecuteReply {
            operation_id: operation_id.into(),
            statements: vec![StatementResult {
                rows,
                columns: vec![DbColumn {
                    name: "id".into(),
                    db_type: DbType::Int64,
                }],
                rows_affected: 0,
            }],
            timing: Default::default(),
        }
    }

    #[test]
    fn validate_execute_reply_accepts_matching_rows() {
        let req = rows_request(2);
        let reply = rows_reply("op-1", 2);
        validate_execute_reply(&req, &reply, &tiny_caps()).unwrap();
    }

    #[test]
    fn validate_execute_reply_rejects_wrong_operation_id() {
        let req = rows_request(2);
        let reply = rows_reply("other", 1);
        let err = validate_execute_reply(&req, &reply, &tiny_caps()).unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("operationId"), "{err}");
    }

    #[test]
    fn validate_execute_reply_rejects_wrong_statement_count() {
        let req = rows_request(1);
        let mut reply = rows_reply("op-1", 1);
        reply.statements.push(reply.statements[0].clone());
        let err = validate_execute_reply(&req, &reply, &tiny_caps()).unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("statements"), "{err}");
    }

    #[test]
    fn validate_execute_reply_rejects_too_many_rows() {
        let req = rows_request(1);
        let reply = rows_reply("op-1", 3);
        let err = validate_execute_reply(&req, &reply, &tiny_caps()).unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("rows"), "{err}");
    }

    #[test]
    fn validate_execute_reply_uses_min_of_statement_and_adapter_row_caps() {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_result_rows = 10;
        let req = rows_request(1);
        let reply = rows_reply("op-1", 2);
        let err = validate_execute_reply(&req, &reply, &caps).unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(
            err.to_string().contains("rows"),
            "stmt maxRows=1 must bind below adapter max_result_rows=10: {err}"
        );
    }

    #[test]
    fn validate_execute_reply_rejects_affected_rows_with_rows() {
        let req = ExecuteRequest {
            operation_id: "op-1".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "UPDATE books SET title = ?".into(),
                parameters: vec![DbValue::Text("x".into())],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let reply = rows_reply("op-1", 1);
        let err = validate_execute_reply(&req, &reply, &tiny_caps()).unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("affectedRows"), "{err}");
    }

    #[test]
    fn validate_execute_reply_rejects_oversized_cell() {
        let req = rows_request(1);
        let mut reply = rows_reply("op-1", 1);
        reply.statements[0].rows[0].values[0] = DbValue::Text("x".repeat(32));
        let err = validate_execute_reply(&req, &reply, &tiny_caps()).unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("maxCellBytes"), "{err}");
    }

    #[test]
    fn validate_execute_reply_rejects_oversized_statement_result() {
        let req = rows_request(1);
        let mut reply = rows_reply("op-1", 1);
        reply.statements[0].rows[0].values[0] = DbValue::Text("x".repeat(200));
        let mut caps = tiny_caps();
        caps.max_cell_bytes = 0;
        caps.max_result_bytes = 32;
        let err = validate_execute_reply(&req, &reply, &caps).unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("maxResultBytes"), "{err}");
    }

    #[test]
    fn validate_execute_reply_rejects_text_nul_without_fake_size() {
        let req = rows_request(1);
        let mut reply = rows_reply("op-1", 1);
        reply.statements[0].columns[0].db_type = DbType::Text;
        reply.statements[0].rows[0].values[0] = DbValue::Text("a\0b".into());
        let err = validate_execute_reply(&req, &reply, &tiny_caps()).unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("U+0000"), "{msg}");
        assert!(
            !msg.contains("18446744073709551615"),
            "encode failure must not look like usize::MAX oversized: {msg}"
        );
    }
}
