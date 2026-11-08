//! Trust-boundary validation for typed [`ExecuteReply`] envelopes from guests.

use bookclerk_plugin_abi::{
    encoded_execute_reply_bytes, encoded_statement_result_bytes, DbCapabilities, DbResultSelection,
    DbValue, ExecuteReply, ExecuteRequest, LimitExceeded,
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
/// Returns [`LibraryError::Unavailable`] when the echo, statement count or
/// result selection shape does not match the request, and
/// [`LibraryError::LimitExceeded`] when a statement result or the whole
/// reply exceeds a negotiated cap (`maxResultRows`, `maxCellBytes`,
/// `maxResultBytes`, `maxAtomicResultBytes`). A reply read as a stream is
/// validated with [`validate_streamed_execute_reply`] instead.
pub fn validate_execute_reply(
    req: &ExecuteRequest,
    reply: &ExecuteReply,
    caps: &DbCapabilities,
) -> Result<()> {
    validate_execute_reply_read(req, reply, caps, ReplyRead::Inline)
}

/// [`validate_execute_reply`] for a reply reassembled from
/// `executeStreamed` frames.
///
/// `caps` are the streamed caps ([`DbCapabilities::streamed`]), so the
/// byte budgets are the streamed budget and an exceeded one is reported
/// as `maxStreamedResultBytes` whatever its size, which keeps a caller
/// from retrying the same read in inline pieces.
///
/// # Errors
///
/// As [`validate_execute_reply`].
pub fn validate_streamed_execute_reply(
    req: &ExecuteRequest,
    reply: &ExecuteReply,
    caps: &DbCapabilities,
) -> Result<()> {
    validate_execute_reply_read(req, reply, caps, ReplyRead::Streamed)
}

/// How a reply crossed the wire, which names the byte budget it is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplyRead {
    /// One `ExecuteReply` scalar held to `maxResultBytes` / `maxAtomicResultBytes`.
    Inline,
    /// Frames reassembled from `executeStreamed`, held to `maxStreamedResultBytes`.
    Streamed,
}

impl ReplyRead {
    /// Name of the whole-reply budget a reply of this kind exceeded.
    fn whole_reply_limit_name(self) -> &'static str {
        match self {
            ReplyRead::Inline => "maxAtomicResultBytes",
            ReplyRead::Streamed => "maxStreamedResultBytes",
        }
    }

    /// Name of the per-statement byte budget a statement result of this
    /// kind exceeded.
    fn statement_bytes_limit_name(self) -> &'static str {
        match self {
            ReplyRead::Inline => "maxResultBytes",
            ReplyRead::Streamed => "maxStreamedResultBytes",
        }
    }
}

/// Shared body of the inline and streamed validators.
///
/// # Errors
///
/// As [`validate_execute_reply`].
fn validate_execute_reply_read(
    req: &ExecuteRequest,
    reply: &ExecuteReply,
    caps: &DbCapabilities,
    read: ReplyRead,
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
        validate_statement_result(
            i,
            req_stmt.result_selection,
            req_stmt.max_rows,
            stmt,
            caps,
            read,
        )?;
    }

    if let Some(cap) = atomic_result_cap_bytes(caps) {
        let bytes = encoded_execute_reply_bytes(reply)
            .map_err(|err| LibraryError::Unavailable(err.to_string()))?
            .len();
        if bytes > cap {
            return Err(LibraryError::LimitExceeded(LimitExceeded::new(
                read.whole_reply_limit_name(),
                u64::try_from(cap).unwrap_or(u64::MAX),
                u64::try_from(bytes).unwrap_or(u64::MAX),
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
    read: ReplyRead,
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
                    return Err(LibraryError::LimitExceeded(LimitExceeded::new(
                        "maxResultRows",
                        u64::from(cap),
                        u64::from(n_rows),
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
                    return Err(LibraryError::LimitExceeded(LimitExceeded::new(
                        "maxCellBytes",
                        u64::from(caps.max_cell_bytes),
                        u64::try_from(n).unwrap_or(u64::MAX),
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
            return Err(LibraryError::LimitExceeded(LimitExceeded::new(
                read.statement_bytes_limit_name(),
                u64::from(caps.max_result_bytes),
                u64::try_from(bytes).unwrap_or(u64::MAX),
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
///
/// Inline caps are admitted at or below [`bookclerk_plugin_abi::MAX_SCALAR_BYTES`]
/// (an inline reply is one RPC scalar), so the budget is not clamped here:
/// the caps of a streamed read ([`DbCapabilities::streamed`]) carry the
/// larger streamed budget, which a reassembled reply may legitimately reach.
fn atomic_result_cap_bytes(caps: &DbCapabilities) -> Option<usize> {
    if caps.max_atomic_result_bytes == 0 {
        return None;
    }
    Some(usize::try_from(caps.max_atomic_result_bytes).unwrap_or(usize::MAX))
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
        assert_eq!(
            err.limit(),
            Some(LimitExceeded::new("maxResultRows", 1, 3)),
            "{err}"
        );
        assert!(matches!(err, LibraryError::LimitExceeded(_)), "{err}");
    }

    #[test]
    fn validate_execute_reply_uses_min_of_statement_and_adapter_row_caps() {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_result_rows = 10;
        let req = rows_request(1);
        let reply = rows_reply("op-1", 2);
        let err = validate_execute_reply(&req, &reply, &caps).unwrap_err();
        assert_eq!(
            err.limit(),
            Some(LimitExceeded::new("maxResultRows", 1, 2)),
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
        assert!(matches!(err, LibraryError::LimitExceeded(_)), "{err}");
        assert_eq!(
            err.limit().map(|l| l.limit),
            Some("maxCellBytes".into()),
            "{err}"
        );
        assert!(err.to_string().contains("maxCellBytes"), "{err}");
    }

    #[test]
    fn validate_execute_reply_rejects_oversized_aggregate_reply() {
        // Two statements that each fit `maxResultBytes` but together exceed
        // `maxAtomicResultBytes` are a typed limit error, not a retryable
        // `unavailable`.
        let mut req = rows_request(1);
        req.statements.push(req.statements[0].clone());
        let mut reply = rows_reply("op-1", 1);
        reply.statements[0].rows[0].values[0] = DbValue::Text("x".repeat(100));
        reply.statements.push(reply.statements[0].clone());
        let mut caps = tiny_caps();
        caps.max_cell_bytes = 0;
        caps.max_result_bytes = 256;
        caps.max_atomic_result_bytes = 160;
        let err = validate_execute_reply(&req, &reply, &caps).unwrap_err();
        assert!(matches!(err, LibraryError::LimitExceeded(_)), "{err}");
        let limit = err.limit().expect("typed limit");
        assert_eq!(limit.limit, "maxAtomicResultBytes");
        assert_eq!(limit.cap, 160);
        assert!(limit.used > 160, "{limit:?}");
        assert!(err.to_string().contains("maxAtomicResultBytes"), "{err}");
    }

    /// `rows` rows of one `cell_bytes` text cell each: large results that
    /// stay under the 256 KiB cell cap.
    fn wide_reply(rows: usize, cell_bytes: usize) -> ExecuteReply {
        let mut reply = rows_reply("op-1", rows);
        for row in &mut reply.statements[0].rows {
            row.values[0] = DbValue::Text("x".repeat(cell_bytes));
        }
        reply.statements[0].columns[0].db_type = DbType::Text;
        reply
    }

    /// A reply reassembled from a stream may be larger than one RPC scalar;
    /// the streamed caps must accept it up to the streamed budget.
    #[test]
    fn validate_execute_reply_accepts_a_streamed_reply_above_one_scalar() {
        let scalar = usize::try_from(bookclerk_plugin_abi::MAX_SCALAR_BYTES).unwrap();
        let req = rows_request(0);
        let reply = wide_reply(64, scalar / 32);
        let inline = DbCapabilities::advertised_sqlite();
        let err = validate_execute_reply(&req, &reply, &inline).unwrap_err();
        let limit = err.limit().expect("typed limit");
        assert_eq!(limit.limit, "maxResultBytes");
        assert_eq!(limit.cap, u64::from(inline.max_result_bytes));
        let streamed = inline.streamed().expect("first-party sqlite streams");
        validate_streamed_execute_reply(&req, &reply, &streamed)
            .unwrap_or_else(|err| panic!("streamed reply above one scalar: {err}"));
    }

    /// Past the streamed budget the reply is still refused, under the
    /// streamed limit's name so no caller retries it in inline pieces.
    #[test]
    fn validate_execute_reply_names_the_streamed_budget_when_exceeded() {
        let scalar = usize::try_from(bookclerk_plugin_abi::MAX_SCALAR_BYTES).unwrap();
        let req = rows_request(0);
        let reply = wide_reply(64, scalar / 32);
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_streamed_result_bytes = bookclerk_plugin_abi::MAX_SCALAR_BYTES + 1;
        let streamed = caps.streamed().expect("streams");
        let err = validate_streamed_execute_reply(&req, &reply, &streamed).unwrap_err();
        let limit = err.limit().expect("typed limit");
        assert_eq!(limit.limit, "maxStreamedResultBytes");
        assert_eq!(
            limit.cap,
            u64::from(bookclerk_plugin_abi::MAX_SCALAR_BYTES) + 1
        );
        assert!(limit.used > limit.cap, "{limit:?}");
    }

    /// An isolate adapter's streamed budget is clamped to one scalar; a
    /// reply past it is still named after the streamed budget, because the
    /// name follows how the reply was read, not the size of its cap.
    #[test]
    fn a_streamed_budget_within_one_scalar_is_still_named_streamed() {
        let scalar = usize::try_from(bookclerk_plugin_abi::MAX_SCALAR_BYTES).unwrap();
        let req = rows_request(0);
        let reply = wide_reply(64, scalar / 32);
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_streamed_result_bytes = bookclerk_plugin_abi::MAX_SCALAR_BYTES;
        let streamed = caps.streamed().expect("streams");
        let err = validate_streamed_execute_reply(&req, &reply, &streamed).unwrap_err();
        let limit = err.limit().expect("typed limit");
        assert_eq!(limit.limit, "maxStreamedResultBytes");
        assert_eq!(limit.cap, u64::from(bookclerk_plugin_abi::MAX_SCALAR_BYTES));
        let inline_err = validate_execute_reply(&req, &reply, &streamed).unwrap_err();
        assert_eq!(
            inline_err.limit().expect("typed limit").limit,
            "maxResultBytes",
            "the same caps read inline keep the inline names"
        );
    }

    /// Each statement fits its own streamed budget but together they pass
    /// the whole-reply budget, which is named as the streamed budget too.
    #[test]
    fn validate_execute_reply_names_the_streamed_budget_for_the_whole_reply() {
        let scalar = usize::try_from(bookclerk_plugin_abi::MAX_SCALAR_BYTES).unwrap();
        let mut req = rows_request(0);
        req.statements.push(req.statements[0].clone());
        let half = wide_reply(16, scalar / 32);
        let mut reply = half.clone();
        reply.statements.push(half.statements[0].clone());
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_streamed_result_bytes = bookclerk_plugin_abi::MAX_SCALAR_BYTES + 1;
        let streamed = caps.streamed().expect("streams");
        let err = validate_streamed_execute_reply(&req, &reply, &streamed).unwrap_err();
        let limit = err.limit().expect("typed limit");
        assert_eq!(limit.limit, "maxStreamedResultBytes");
        assert_eq!(
            limit.cap,
            u64::from(bookclerk_plugin_abi::MAX_SCALAR_BYTES) + 1
        );
        assert!(limit.used > limit.cap, "{limit:?}");
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
        assert!(matches!(err, LibraryError::LimitExceeded(_)), "{err}");
        assert_eq!(
            err.limit().map(|l| (l.limit, l.cap)),
            Some(("maxResultBytes".into(), 32)),
            "{err}"
        );
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
