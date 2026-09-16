//! Guest-typed durable receipt finalize (same-transaction payload capture).
//!
//! Host wrap lives in `bookclerk-library`; adapters run
//! [`guest_receipt_finalize_stmts`] in the same atomic transaction before COMMIT.

#![allow(clippy::missing_docs_in_private_items)]

use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, DbValue, ExecuteReply, StatementResult,
    TypedDbStatement,
};
use sea_orm::DbErr;

/// Maximum UTF-8 bytes stored in `db_atomic_receipts.payload` for guest replay.
pub const GUEST_TYPED_REPLAY_PAYLOAD_MAX_BYTES: usize = 65536;

/// Error text when a receipt committed but the caller-visible payload was lost.
pub const GUEST_RECEIPT_RESULT_LOST: &str =
    "guest receipt committed without payload; original response lost";

/// In-flight guest-receipt claim (D1 commits the stub INSERT before DDL).
pub const GUEST_RECEIPT_STATUS_CLAIMED: &str = "claimed";

/// Finalized guest-receipt row; payload is the durable replay body.
pub const GUEST_RECEIPT_STATUS_OK: &str = "ok";

/// Claimed-owner guest SQL finished on D1 (prune + ungated guest in one HTTP).
///
/// Resume remaining guest work only for [`GUEST_RECEIPT_STATUS_CLAIMED`]. An
/// `applied` row means DML already ran; skip guest SQL and finalize (or
/// surface result-lost when payload is still empty).
pub const GUEST_RECEIPT_STATUS_APPLIED: &str = "applied";

/// Write-predicate spliced onto guest DML by the host receipt wrap.
///
/// D1 claimed-owner batches strip this after the stub INSERT commits, because
/// the claim row would otherwise make the predicate false on first execution.
pub const GUEST_RECEIPT_WRITE_GATE: &str =
    "NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)";

/// True when `err` is [`GUEST_RECEIPT_RESULT_LOST`].
#[must_use]
pub fn is_guest_receipt_result_lost(err: &DbErr) -> bool {
    err.to_string().contains(GUEST_RECEIPT_RESULT_LOST)
}

/// Prune + prior-select prefix ahead of the guest statements.
pub const GUEST_RECEIPT_WRAP_PREFIX: usize = 2;

/// Same-batch receipt stub INSERT after gated guest statements.
pub const GUEST_RECEIPT_STUB_SUFFIX: usize = 1;

/// True when `sql` is the guest-receipt claim stub (`INSERT … WHERE NOT EXISTS`).
///
/// D1 commits each HTTP batch immediately, so the adapter sends this statement
/// first and only runs ungated guest DDL after `rows_affected = 1`.
#[must_use]
pub fn is_guest_receipt_stub_insert(sql: &str) -> bool {
    let t = sql.to_ascii_lowercase();
    t.contains("insert into db_atomic_receipts") && t.contains("where not exists")
}

/// Removes the **host-appended** [`GUEST_RECEIPT_WRITE_GATE`] predicate.
///
/// The wrap splices the gate into a code span (before `RETURNING`). A plugin
/// string or comment may contain the same text; only the last code-span match
/// is removed. Returns `sql` unchanged when that gate is absent. The caller
/// must drop the trailing `operation_id` bind the wrap appended for the gate.
#[must_use]
pub fn strip_guest_receipt_write_gate(sql: &str) -> String {
    let Some(idx) = crate::lower::find_last_in_code(sql, GUEST_RECEIPT_WRITE_GATE) else {
        return sql.to_string();
    };
    let before = sql[..idx].trim_end();
    let after = sql[idx + GUEST_RECEIPT_WRITE_GATE.len()..].trim_start();
    let before = strip_trailing_where_or_and(before);
    if after.is_empty() {
        before.to_string()
    } else {
        format!("{before} {after}")
    }
}

/// Drops a trailing top-level `WHERE` or `AND` that preceded the write gate.
fn strip_trailing_where_or_and(sql: &str) -> &str {
    let trimmed = sql.trim_end();
    for kw in ["WHERE", "AND"] {
        if trimmed.len() >= kw.len() {
            let start = trimmed.len() - kw.len();
            if trimmed[start..].eq_ignore_ascii_case(kw)
                && (start == 0
                    || (!trimmed.as_bytes()[start - 1].is_ascii_alphanumeric()
                        && trimmed.as_bytes()[start - 1] != b'_'))
            {
                return trimmed[..start].trim_end();
            }
        }
    }
    trimmed
}

/// Host UPDATE that marks a claimed stub as applied after ungated guest SQL.
#[must_use]
pub fn guest_receipt_applied_stmt(operation_id: &str) -> TypedDbStatement {
    typed_exec(
        "UPDATE db_atomic_receipts SET status = 'applied' \
         WHERE operation_id = ? AND status = 'claimed'",
        vec![DbValue::Text(operation_id.into())],
    )
}

/// True when the prior-receipt SELECT returned a row.
#[must_use]
pub fn prior_receipt_exists(stmt: &StatementResult) -> bool {
    receipt_hash_from(stmt).is_some()
}

/// True when `stmt` is an in-flight [`GUEST_RECEIPT_STATUS_CLAIMED`] receipt.
#[must_use]
pub fn prior_receipt_is_claimed(stmt: &StatementResult) -> bool {
    receipt_status_from(stmt).as_deref() == Some(GUEST_RECEIPT_STATUS_CLAIMED)
}

/// Reads `request_hash` from a prior-receipt select result, if any row exists.
#[must_use]
pub fn receipt_hash_from(stmt: &StatementResult) -> Option<String> {
    let row = stmt.rows.first()?;
    let idx = stmt
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("request_hash"))
        .or(Some(1))?;
    match row.values.get(idx)? {
        DbValue::Text(s) => Some(s.clone()),
        _ => None,
    }
}

/// True when a claimed prior receipt belongs to `guest_hash` (resume ungated DDL).
#[must_use]
pub fn prior_receipt_should_resume_guest(stmt: &StatementResult, guest_hash: &str) -> bool {
    prior_receipt_is_claimed(stmt) && receipt_hash_from(stmt).as_deref() == Some(guest_hash)
}

/// True when wrap prefix results show a completed prior receipt (skip guest SQL).
///
/// An in-flight [`GUEST_RECEIPT_STATUS_CLAIMED`] row with the same hash must
/// **not** skip remaining work: D1 commits the stub INSERT before ungated
/// DDL, so a crash between those HTTP batches has to re-run idempotent DDL
/// and then finalize. A claimed row with a **different** hash still skips
/// (unwrap reports conflict).
#[must_use]
pub fn should_skip_remaining_guest_work(
    results: &[StatementResult],
    total: usize,
    guest_hash: &str,
) -> bool {
    if results.len() != GUEST_RECEIPT_WRAP_PREFIX || total <= GUEST_RECEIPT_WRAP_PREFIX {
        return false;
    }
    let Some(prior) = results.get(1) else {
        return false;
    };
    prior_receipt_exists(prior) && !prior_receipt_should_resume_guest(prior, guest_hash)
}

/// Pads skipped guest + stub results so the wrap shape stays intact.
pub fn pad_skipped_guest_results(results: &mut Vec<StatementResult>, total: usize) {
    while results.len() < total {
        results.push(StatementResult::from_affected(0));
    }
}

/// Builds finalize statements to persist a guest reply before COMMIT.
///
/// Returns an empty list when the prior receipt row already carries payload.
///
/// # Errors
///
/// Returns [`DbErr`] when the envelope is malformed or the payload exceeds
/// [`GUEST_TYPED_REPLAY_PAYLOAD_MAX_BYTES`].
pub fn guest_receipt_finalize_stmts(
    partial: &ExecuteReply,
    guest_len: usize,
    guest_hash: &str,
) -> Result<Vec<TypedDbStatement>, DbErr> {
    let expected = GUEST_RECEIPT_WRAP_PREFIX
        .saturating_add(guest_len)
        .saturating_add(GUEST_RECEIPT_STUB_SUFFIX);
    if partial.statements.len() != expected {
        return Err(DbErr::Custom(format!(
            "guest atomic receipt wrap returned {} statements; expected {expected}",
            partial.statements.len()
        )));
    }
    if let Some(prior) = partial.statements.get(1) {
        if let Some(prior_hash) = receipt_hash_from(prior) {
            if prior_hash != guest_hash {
                // Guest statements must already have been skipped; unwrap
                // reports conflict. Do not emit finalize SQL.
                return Ok(Vec::new());
            }
            if receipt_payload_text(prior).is_some() {
                return Ok(Vec::new());
            }
            if is_gated_guest_replay(partial, prior)? {
                return Err(DbErr::Custom(GUEST_RECEIPT_RESULT_LOST.into()));
            }
            if prior_receipt_is_claimed(prior) && stub_rows_affected(partial) == 0 {
                // In-flight claim with no payload: a DML lost-reply retry
                // must not stash gated zeros. D1 resumes ungated DDL via
                // [`prior_receipt_should_resume_guest`] instead.
                return Ok(Vec::new());
            }
            let guest_reply = guest_slice_reply(partial, guest_len)?;
            let payload = encode_guest_replay_payload(&guest_reply)?;
            return Ok(vec![finalize_claimed_payload(
                &payload,
                &partial.operation_id,
            )]);
        }
    }
    let guest_reply = guest_slice_reply(partial, guest_len)?;
    let payload = encode_guest_replay_payload(&guest_reply)?;
    Ok(vec![finalize_claimed_payload(
        &payload,
        &partial.operation_id,
    )])
}

/// Host UPDATE that promotes a claimed stub to a durable `ok` payload.
fn finalize_claimed_payload(payload: &str, operation_id: &str) -> TypedDbStatement {
    typed_exec(
        "UPDATE db_atomic_receipts SET payload = ?, status = 'ok' \
         WHERE operation_id = ? AND status IN ('claimed', 'applied')",
        vec![
            DbValue::Text(payload.into()),
            DbValue::Text(operation_id.into()),
        ],
    )
}

/// True when a prior receipt row exists but the stub INSERT was gated off (replay after commit).
///
/// An in-flight [`GUEST_RECEIPT_STATUS_CLAIMED`] row is recoverable (resume DDL
/// then finalize) and must not be reported as result-lost.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when the wrapped reply is missing the receipt stub suffix.
fn is_gated_guest_replay(partial: &ExecuteReply, prior: &StatementResult) -> Result<bool, DbErr> {
    if prior.rows.is_empty() {
        return Ok(false);
    }
    if prior_receipt_is_claimed(prior) {
        return Ok(false);
    }
    if receipt_payload_text(prior).is_some() {
        return Ok(false);
    }
    let stub = partial
        .statements
        .last()
        .ok_or_else(|| DbErr::Custom("guest receipt wrap missing stub suffix".into()))?;
    Ok(stub.rows_affected == 0)
}

/// `rows_affected` from the wrap's receipt stub INSERT, or 0 when missing.
fn stub_rows_affected(partial: &ExecuteReply) -> u64 {
    partial
        .statements
        .last()
        .map(|s| s.rows_affected)
        .unwrap_or(0)
}

/// Guest statement results from a wrapped partial reply.
///
/// # Errors
///
/// Never returns an error; the `Result` preserves the surrounding receipt API.
fn guest_slice_reply(partial: &ExecuteReply, guest_len: usize) -> Result<ExecuteReply, DbErr> {
    Ok(ExecuteReply {
        operation_id: partial.operation_id.clone(),
        statements: partial
            .statements
            .iter()
            .skip(GUEST_RECEIPT_WRAP_PREFIX)
            .take(guest_len)
            .cloned()
            .collect(),
        timing: partial.timing.clone(),
    })
}

/// Serializes guest statement results for durable replay.
///
/// # Errors
///
/// Returns when JSON serialization fails or the payload exceeds
/// [`GUEST_TYPED_REPLAY_PAYLOAD_MAX_BYTES`].
fn encode_guest_replay_payload(reply: &ExecuteReply) -> Result<String, DbErr> {
    let payload = GuestReplayPayload {
        operation_id: reply.operation_id.clone(),
        statements: reply.statements.clone(),
        timing: reply.timing.clone(),
    };
    let text = serde_json::to_string(&payload)
        .map_err(|err| DbErr::Custom(format!("guest replay payload encode failed: {err}")))?;
    if text.len() > GUEST_TYPED_REPLAY_PAYLOAD_MAX_BYTES {
        return Err(DbErr::Custom(format!(
            "guest replay payload is {} bytes; max is {}",
            text.len(),
            GUEST_TYPED_REPLAY_PAYLOAD_MAX_BYTES
        )));
    }
    Ok(text)
}

/// JSON envelope stored in `db_atomic_receipts.payload` for guest replay.
#[derive(serde::Serialize, serde::Deserialize)]
struct GuestReplayPayload {
    operation_id: String,
    statements: Vec<StatementResult>,
    timing: bookclerk_plugin_abi::DbTiming,
}

/// Reads `payload` from a prior-receipt select row, if present.
fn receipt_payload_text(prior: &StatementResult) -> Option<String> {
    let row = prior.rows.first()?;
    let idx = prior
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("payload"))
        .or(Some(3))?;
    match row.values.get(idx)? {
        DbValue::Text(s) if !s.is_empty() => Some(s.clone()),
        DbValue::Null(_) => None,
        _ => None,
    }
}

/// Reads `status` from a prior-receipt select row, if present.
fn receipt_status_from(prior: &StatementResult) -> Option<String> {
    let row = prior.rows.first()?;
    let idx = prior
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("status"))
        .or(Some(2))?;
    match row.values.get(idx)? {
        DbValue::Text(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Host-authored execute statement used in the receipt envelope.
fn typed_exec(sql: &str, parameters: Vec<DbValue>) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.into(),
        parameters,
        kind: DbPlanStatementKind::Execute,
        max_rows: 0,
        result_selection: DbResultSelection::AffectedRows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::{DbColumn, DbRow, DbTiming, DbType};

    fn prior_receipt_row(payload: &str) -> StatementResult {
        prior_receipt_row_with_status(GUEST_RECEIPT_STATUS_CLAIMED, payload)
    }

    fn prior_receipt_row_with_status(status: &str, payload: &str) -> StatementResult {
        StatementResult {
            rows: vec![DbRow {
                values: vec![
                    DbValue::Text("guest-op".into()),
                    DbValue::Text("a".repeat(64)),
                    DbValue::Text(status.into()),
                    DbValue::Text(payload.into()),
                    DbValue::Text("2026-01-01T00:00:00Z".into()),
                ],
            }],
            columns: vec![
                DbColumn {
                    name: "operation_id".into(),
                    db_type: DbType::Text,
                },
                DbColumn {
                    name: "request_hash".into(),
                    db_type: DbType::Text,
                },
                DbColumn {
                    name: "status".into(),
                    db_type: DbType::Text,
                },
                DbColumn {
                    name: "payload".into(),
                    db_type: DbType::Text,
                },
                DbColumn {
                    name: "created_at".into(),
                    db_type: DbType::Text,
                },
            ],
            rows_affected: 0,
        }
    }

    fn wrapped_partial(guest_rows_affected: u64, stub_rows_affected: u64) -> ExecuteReply {
        ExecuteReply {
            operation_id: "guest-op".into(),
            statements: vec![
                StatementResult::from_affected(0),
                prior_receipt_row(""),
                StatementResult::from_affected(guest_rows_affected),
                StatementResult::from_affected(stub_rows_affected),
            ],
            timing: DbTiming::default(),
        }
    }

    #[test]
    fn finalize_persists_payload_on_first_commit() {
        let partial = wrapped_partial(1, 1);
        let stmts = guest_receipt_finalize_stmts(&partial, 1, &"a".repeat(64)).expect("finalize");
        assert_eq!(stmts.len(), 1);
        assert!(
            stmts[0].sql.contains("SET payload = ?, status = 'ok'"),
            "{}",
            stmts[0].sql
        );
        assert!(
            stmts[0]
                .sql
                .contains("AND status IN ('claimed', 'applied')"),
            "{}",
            stmts[0].sql
        );
    }

    #[test]
    fn finalize_skips_sql_on_hash_mismatch() {
        let mut partial = wrapped_partial(1, 1);
        partial.statements[1] = prior_receipt_row("{\"operationId\":\"guest-op\"}");
        let stmts = guest_receipt_finalize_stmts(&partial, 1, &"b".repeat(64)).expect("mismatch");
        assert!(stmts.is_empty());
    }

    #[test]
    fn finalize_skips_when_payload_already_present() {
        let mut partial = wrapped_partial(1, 1);
        partial.statements[1] = prior_receipt_row("{\"operationId\":\"guest-op\"}");
        let stmts = guest_receipt_finalize_stmts(&partial, 1, &"a".repeat(64)).expect("finalize");
        assert!(stmts.is_empty());
    }

    #[test]
    fn finalize_rejects_gated_replay_without_payload() {
        let mut partial = wrapped_partial(0, 0);
        partial.statements[1] = prior_receipt_row_with_status(GUEST_RECEIPT_STATUS_OK, "");
        let err = guest_receipt_finalize_stmts(&partial, 1, &"a".repeat(64)).unwrap_err();
        assert!(err.to_string().contains("original response lost"), "{err}");
    }

    #[test]
    fn finalize_persists_claimed_empty_payload() {
        let partial = wrapped_partial(1, 1);
        let stmts = guest_receipt_finalize_stmts(&partial, 1, &"a".repeat(64)).expect("claimed");
        assert_eq!(stmts.len(), 1);
        assert!(
            stmts[0]
                .sql
                .contains("AND status IN ('claimed', 'applied')"),
            "{}",
            stmts[0].sql
        );
    }

    #[test]
    fn finalize_does_not_persist_gated_claimed_retry() {
        let partial = wrapped_partial(0, 0);
        let stmts = guest_receipt_finalize_stmts(&partial, 1, &"a".repeat(64)).expect("in-flight");
        assert!(stmts.is_empty());
    }

    #[test]
    fn skip_remaining_guest_work_only_after_completed_prior() {
        let hash = "a".repeat(64);
        let empty_prefix = vec![
            StatementResult::from_affected(0),
            StatementResult::from_affected(0),
        ];
        assert!(!should_skip_remaining_guest_work(&empty_prefix, 4, &hash));
        let completed = vec![
            StatementResult::from_affected(0),
            prior_receipt_row_with_status(
                GUEST_RECEIPT_STATUS_OK,
                "{\"operationId\":\"guest-op\"}",
            ),
        ];
        assert!(should_skip_remaining_guest_work(&completed, 4, &hash));
        assert!(!should_skip_remaining_guest_work(&completed, 2, &hash));
        let claimed_same = vec![StatementResult::from_affected(0), prior_receipt_row("")];
        assert!(
            !should_skip_remaining_guest_work(&claimed_same, 4, &hash),
            "claimed + same hash must resume guest DDL"
        );
        assert!(prior_receipt_should_resume_guest(&claimed_same[1], &hash));
        let applied_same = vec![
            StatementResult::from_affected(0),
            prior_receipt_row_with_status(GUEST_RECEIPT_STATUS_APPLIED, ""),
        ];
        assert!(
            should_skip_remaining_guest_work(&applied_same, 4, &hash),
            "applied must not resume guest DML"
        );
        assert!(!prior_receipt_should_resume_guest(&applied_same[1], &hash));
        assert!(should_skip_remaining_guest_work(
            &claimed_same,
            4,
            &"b".repeat(64)
        ));
    }

    #[test]
    fn stub_insert_is_the_gated_receipt_claim() {
        assert!(is_guest_receipt_stub_insert(
            "INSERT INTO db_atomic_receipts (\
                operation_id, operation_kind, request_hash, status, payload, created_at, expires_at\
             ) SELECT ?, ?, ?, 'claimed', '', ?, ? \
               WHERE NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)"
        ));
        assert!(!is_guest_receipt_stub_insert(
            "UPDATE db_atomic_receipts SET payload = ? WHERE operation_id = ?"
        ));
        assert!(!is_guest_receipt_stub_insert(
            "SELECT operation_id FROM db_atomic_receipts WHERE operation_id = ?"
        ));
    }

    #[test]
    fn strip_write_gate_restores_insert_select_and_keeps_returning() {
        let gated =
            format!("INSERT INTO counters (id, n) SELECT ?, ? WHERE {GUEST_RECEIPT_WRITE_GATE}");
        assert_eq!(
            strip_guest_receipt_write_gate(&gated),
            "INSERT INTO counters (id, n) SELECT ?, ?"
        );
        let returning = format!(
            "INSERT INTO counters (id, n) SELECT ?, ? WHERE {GUEST_RECEIPT_WRITE_GATE} RETURNING id"
        );
        assert_eq!(
            strip_guest_receipt_write_gate(&returning),
            "INSERT INTO counters (id, n) SELECT ?, ? RETURNING id"
        );
        let update =
            format!("UPDATE counters SET n = n + 1 WHERE id = ? AND {GUEST_RECEIPT_WRITE_GATE}");
        assert_eq!(
            strip_guest_receipt_write_gate(&update),
            "UPDATE counters SET n = n + 1 WHERE id = ?"
        );
        assert_eq!(
            strip_guest_receipt_write_gate("INSERT INTO t (id) SELECT 1"),
            "INSERT INTO t (id) SELECT 1"
        );
        let gate = GUEST_RECEIPT_WRITE_GATE;
        let literal_then_gate =
            format!("INSERT INTO gated_notes (id, body) SELECT 1, '{gate}' WHERE {gate}");
        assert_eq!(
            strip_guest_receipt_write_gate(&literal_then_gate),
            format!("INSERT INTO gated_notes (id, body) SELECT 1, '{gate}'")
        );
        let comment_then_gate =
            format!("INSERT INTO gated_notes (id, body) -- {gate}\nSELECT 1, 'x' WHERE {gate}");
        assert_eq!(
            strip_guest_receipt_write_gate(&comment_then_gate),
            format!("INSERT INTO gated_notes (id, body) -- {gate}\nSELECT 1, 'x'")
        );
        let both = format!(
            "INSERT INTO gated_notes (id, body)\n-- {gate}\nSELECT 1, '{gate}' WHERE {gate}"
        );
        assert_eq!(
            strip_guest_receipt_write_gate(&both),
            format!("INSERT INTO gated_notes (id, body)\n-- {gate}\nSELECT 1, '{gate}'")
        );
    }

    #[test]
    fn finalize_rejects_applied_empty_payload_as_result_lost() {
        let mut partial = wrapped_partial(0, 0);
        partial.statements[1] = prior_receipt_row_with_status(GUEST_RECEIPT_STATUS_APPLIED, "");
        let err = guest_receipt_finalize_stmts(&partial, 1, &"a".repeat(64)).unwrap_err();
        assert!(err.to_string().contains("original response lost"), "{err}");
    }
}
