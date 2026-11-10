//! Durable `(operationId, requestHash)` receipt wrap for guest-authored typed batches.
//!
//! Host domain plans already gate DML with `bookclerk_receipts`. Guest
//! `executeAtomic` must use the same envelope so a D1 (or any) adapter cannot
//! apply guest mutations twice after an ambiguous commit.

use bookclerk_db_exec::{
    AdapterExecuteRequest, GuestReceiptPersist, GUEST_RECEIPT_WRAP_PREFIX, GUEST_RECEIPT_WRITE_GATE,
};
use bookclerk_plugin_abi::{
    typecheck_execute_request_proofs, CanonicalExecuteRequest, DbCapabilities, DbPlanStatementKind,
    DbResultSelection, DbValue, ExecuteReply, ExecuteRequest, PluginError, SqlTypeEnv,
    TypedDbStatement,
};
use chrono::{Duration, Utc};

use super::named::apply_write_predicate;

/// Prune + prior-select prefix ahead of the guest statements.
const WRAP_PREFIX: usize = GUEST_RECEIPT_WRAP_PREFIX;

/// Host `operationKind` stored on guest-typed receipts.
const GUEST_TYPED_KIND: &str = "guestTyped";

/// Wraps an already-authorized guest batch with prune / prior-select / gated
/// DML. Clears `requestHash` so a later host re-authorize stamps the wrapper,
/// not the original guest SQL, and stamps a host-only finalize hint so
/// adapters persist replay payload before COMMIT.
///
/// # Errors
///
/// Returns when the wrapped SQL fails typecheck. Callers must not dispatch
/// the adapter on this error.
pub(crate) fn wrap_guest_typed_request(
    mut req: ExecuteRequest,
    type_env: &SqlTypeEnv,
) -> Result<AdapterExecuteRequest, PluginError> {
    let now = Utc::now();
    let created = now.to_rfc3339();
    let operation_id = req.operation_id.clone();
    let request_hash = req.request_hash.clone();
    let guest_len = u32::try_from(req.statements.len()).unwrap_or(u32::MAX);

    let prune = typed_exec(
        "DELETE FROM bookclerk_receipts WHERE expires_at <= ? AND operation_id != ?",
        vec![
            DbValue::Text(created.clone()),
            DbValue::Text(operation_id.clone()),
        ],
    );
    let select = typed_query(
        "SELECT operation_id, request_hash, status, payload, created_at \
         FROM bookclerk_receipts WHERE operation_id = ?",
        vec![DbValue::Text(operation_id.clone())],
    );
    let mut gated = Vec::with_capacity(req.statements.len());
    for mut stmt in req.statements {
        // DDL takes no WHERE predicate. Binding DDL is host-proven
        // idempotent (`IF [NOT] EXISTS`; `ALTER` / `CREATE TABLE AS` refused).
        // Same-token different-hash retries skip remaining guest work after
        // the prior-receipt SELECT so a changed CREATE/DROP cannot run.
        if is_write(stmt.kind) && !bookclerk_plugin_abi::statement_is_ddl(&stmt.sql) {
            stmt.sql = apply_write_predicate(&stmt.sql, stmt.kind, GUEST_RECEIPT_WRITE_GATE);
            stmt.parameters.push(DbValue::Text(operation_id.clone()));
        }
        gated.push(stmt);
    }

    let mut statements = Vec::with_capacity(WRAP_PREFIX + gated.len() + 1);
    statements.push(prune);
    statements.push(select);
    statements.extend(gated);
    let expires = (now + Duration::hours(24)).to_rfc3339();
    statements.push(typed_exec(
        "INSERT INTO bookclerk_receipts (\
            operation_id, operation_kind, request_hash, status, payload, created_at, expires_at\
         ) SELECT ?, ?, ?, 'claimed', '', ?, ? \
           WHERE NOT EXISTS (SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?)",
        vec![
            DbValue::Text(operation_id.clone()),
            DbValue::Text(GUEST_TYPED_KIND.into()),
            DbValue::Text(request_hash.clone()),
            DbValue::Text(created),
            DbValue::Text(expires),
            DbValue::Text(operation_id),
        ],
    ));

    req.statements = statements;
    req.request_hash.clear();
    let mut env = receipt_wrap_type_env();
    env.merge(type_env);
    let proofs = typecheck_execute_request_proofs(&req, &env)?;
    CanonicalExecuteRequest::from_desugared(req)
        .with_guest_receipt(GuestReceiptPersist {
            guest_statement_len: guest_len,
            guest_request_hash: request_hash,
        })
        .bind_proofs(proofs)
}

/// Bookkeeping tables present when typing receipt-wrapper SQL.
fn receipt_wrap_type_env() -> SqlTypeEnv {
    bookclerk_plugin_abi::sql_host_bookkeeping_type_env()
}

/// Interprets a wrapped guest reply: replay from stored payload, else strip wrapper rows.
///
/// Reconstructed replay payloads are re-validated against `guest_req` and
/// `caps` so a lying adapter cannot return a wrap-shaped prior-SELECT whose
/// JSON body skips guest row/cell/byte caps.
///
/// A matching prior row with empty payload is unavailable, except a `claimed`
/// row whose guest slice has this-attempt results (SELECT ran before finalize).
/// A prior row holding the read-only marker
/// ([`bookclerk_db_exec::GUEST_READ_ONLY_REPLAY_PAYLOAD`]) means the batch
/// only read, so this attempt's own rows are returned: nothing was gated and
/// the reads ran again.
///
/// # Errors
///
/// Returns [`PluginError::conflict`] when a prior receipt exists with a
/// different hash, [`PluginError::unavailable`] when a stored payload fails
/// guest reply validation, or [`PluginError::internal`] when the envelope is
/// malformed.
pub(crate) fn unwrap_guest_typed_reply(
    mut reply: ExecuteReply,
    guest_req: &ExecuteRequest,
    caps: &DbCapabilities,
) -> Result<ExecuteReply, PluginError> {
    let guest_len = guest_req.statements.len();
    let guest_hash = guest_req.request_hash.as_str();
    let expected = WRAP_PREFIX
        .saturating_add(guest_len)
        .saturating_add(bookclerk_db_exec::GUEST_RECEIPT_STUB_SUFFIX);
    if reply.statements.len() != expected {
        return Err(PluginError::internal(format!(
            "guest atomic receipt wrap returned {} statements; expected {expected}",
            reply.statements.len()
        )));
    }
    if let Some(prior) = reply.statements.get(1) {
        if let Some(prior_hash) = receipt_hash_from(prior) {
            if prior_hash != guest_hash {
                return Err(PluginError::conflict(
                    "executeAtomic operationId was already committed with a different requestHash",
                ));
            }
            if receipt_payload_text(prior).is_some() {
                if let Some(replayed) = decode_guest_replay_payload(prior)? {
                    crate::validate_execute_reply(guest_req, &replayed, caps)
                        .map_err(|err| PluginError::unavailable(err.to_string()))?;
                    return Ok(replayed);
                }
            } else if !bookclerk_db_exec::prior_receipt_is_claimed(prior)
                || guest_slice_is_gated_noop(&reply, guest_len)
            {
                // `ok`/`applied` with no payload is result-lost. A `claimed`
                // row with empty payload is the pre-finalize SELECT: drain
                // only when this attempt re-ran remaining guest work. Gated
                // DML zeros (lost HTTP after the stub committed) stay
                // unavailable so callers cannot treat a no-op as success.
                return Err(PluginError::unavailable(
                    "executeAtomic committed with an empty guest receipt payload; retry after finalize",
                ));
            }
        }
    }
    reply.statements = reply
        .statements
        .drain(WRAP_PREFIX..WRAP_PREFIX + guest_len)
        .collect();
    Ok(reply)
}

/// True when the guest slice is empty rows and zero `rows_affected` (gated retry).
fn guest_slice_is_gated_noop(reply: &ExecuteReply, guest_len: usize) -> bool {
    reply
        .statements
        .iter()
        .skip(WRAP_PREFIX)
        .take(guest_len)
        .all(|stmt| stmt.rows.is_empty() && stmt.rows_affected == 0)
}

/// Reconstructs a guest [`ExecuteReply`] from a prior-receipt select row.
///
/// Returns `None` for a missing payload and for the read-only marker, whose
/// batch re-ran its reads in this attempt.
fn decode_guest_replay_payload(
    prior: &bookclerk_plugin_abi::StatementResult,
) -> Result<Option<ExecuteReply>, PluginError> {
    let Some(text) = receipt_payload_text(prior) else {
        return Ok(None);
    };
    if text == bookclerk_db_exec::GUEST_READ_ONLY_REPLAY_PAYLOAD {
        return Ok(None);
    }
    let payload: GuestReplayPayload = serde_json::from_str(&text).map_err(|err| {
        PluginError::internal(format!("guest replay payload is not valid JSON: {err}"))
    })?;
    Ok(Some(ExecuteReply {
        operation_id: payload.operation_id,
        statements: payload.statements,
        timing: payload.timing,
    }))
}

/// JSON envelope stored in `bookclerk_receipts.payload` for guest replay.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::missing_docs_in_private_items)]
struct GuestReplayPayload {
    operation_id: String,
    statements: Vec<bookclerk_plugin_abi::StatementResult>,
    timing: bookclerk_plugin_abi::DbTiming,
}

/// Reads `payload` from a prior-receipt select row, if present.
fn receipt_payload_text(prior: &bookclerk_plugin_abi::StatementResult) -> Option<String> {
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

/// True for DML kinds that must be receipt-gated.
fn is_write(kind: DbPlanStatementKind) -> bool {
    matches!(
        kind,
        DbPlanStatementKind::Execute | DbPlanStatementKind::Returning
    )
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

/// Host-authored select used to read a prior receipt row.
fn typed_query(sql: &str, parameters: Vec<DbValue>) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.into(),
        parameters,
        kind: DbPlanStatementKind::Select,
        max_rows: 1,
        result_selection: DbResultSelection::Rows,
    }
}

/// Reads `request_hash` from a prior-receipt select result, if any row exists.
fn receipt_hash_from(stmt: &bookclerk_plugin_abi::StatementResult) -> Option<String> {
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

#[cfg(test)]
mod replay_finalize {
    use super::*;
    use bookclerk_plugin_abi::{
        sql_type_env_from_canonical_ddl, DbPlanStatementKind, DbResultSelection, ExecuteRequest,
        TypedDbStatement,
    };

    fn slots_env() -> SqlTypeEnv {
        sql_type_env_from_canonical_ddl(
            "CREATE TABLE bookclerk_slots (slot_key TEXT NOT NULL, bump INTEGER NOT NULL)",
        )
    }

    /// A read-only guest batch larger than the receipt replay payload cap
    /// commits, and a replay of the same token re-runs the read instead of
    /// failing on a payload it never stored.
    #[tokio::test]
    async fn read_only_guest_batch_is_not_bounded_by_the_replay_payload() {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("mem db");
        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
        let body = "x".repeat(bookclerk_db_exec::GUEST_TYPED_REPLAY_PAYLOAD_MAX_BYTES + 1);
        let hash = "c".repeat(64);
        let read = |hash: &str| ExecuteRequest {
            operation_id: "guest-read-op".into(),
            request_hash: hash.into(),
            statements: vec![
                TypedDbStatement {
                    sql: "SELECT ? AS body".into(),
                    parameters: vec![DbValue::Text(body.clone())],
                    kind: DbPlanStatementKind::Select,
                    max_rows: 1,
                    result_selection: DbResultSelection::Rows,
                },
                TypedDbStatement {
                    sql: "SELECT bump FROM bookclerk_slots WHERE slot_key = 'guest-read'".into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Select,
                    max_rows: 1,
                    result_selection: DbResultSelection::Rows,
                },
            ],
            deadline_unix_ms: 0,
        };
        let set_bump = |bump: i64| {
            use sea_orm::ConnectionTrait;
            let db = db.clone();
            async move {
                db.execute_unprepared("DELETE FROM bookclerk_slots WHERE slot_key = 'guest-read'")
                    .await
                    .expect("clear the slot the read observes");
                db.execute_unprepared(&format!(
                    "INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('guest-read', {bump})"
                ))
                .await
                .expect("store the slot the read observes");
            }
        };
        set_bump(1).await;
        let run = |req: ExecuteRequest| {
            let db = db.clone();
            let caps = caps.clone();
            async move {
                let guest_req = req.clone();
                let wrapped = wrap_guest_typed_request(req, &slots_env()).expect("wrap");
                let reply = bookclerk_db_exec::execute_typed_envelope(
                    bookclerk_db_exec::PhysicalEngine::sqlite(),
                    &db,
                    &wrapped,
                    "sqlite_txn",
                    bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
                    bookclerk_db_exec::AtomicSession::from_deadline(None)
                        .with_type_env(crate::migrations::host_sql_type_env()),
                )
                .await
                .map_err(|err| err.to_string())?;
                unwrap_guest_typed_reply(reply, &guest_req, &caps).map_err(|err| err.to_string())
            }
        };
        let first = run(read(&hash)).await.expect("read-only batch commits");
        assert_eq!(first.statements.len(), 2);
        assert_eq!(
            first.statements[0].rows[0].values[0],
            DbValue::Text(body.clone())
        );
        assert_eq!(first.statements[1].rows[0].values[0], DbValue::Int64(1));
        set_bump(2).await;
        let replay = run(read(&hash)).await.expect("replay re-runs the read");
        assert_eq!(
            replay.statements[0].rows[0].values[0],
            DbValue::Text(body.clone())
        );
        assert_eq!(
            replay.statements[1].rows[0].values[0],
            DbValue::Int64(2),
            "the replay re-ran the read against the stored row instead of serving cached rows"
        );
        let conflict = run(read(&"d".repeat(64))).await.unwrap_err();
        assert!(conflict.contains("different requestHash"), "{conflict}");
    }

    #[tokio::test]
    async fn guest_receipt_finalize_enables_replay_after_commit() {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("mem db");
        let guest_hash = "a".repeat(64);
        let req = ExecuteRequest {
            operation_id: "guest-replay-op".into(),
            request_hash: guest_hash.clone(),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('guest-replay', 1)"
                    .into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let guest_req = req.clone();
        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
        let wrapped = wrap_guest_typed_request(req, &slots_env()).expect("wrap");
        assert!(!wrapped.guest_receipt.is_absent());
        let reply = bookclerk_db_exec::execute_typed_envelope(
            bookclerk_db_exec::PhysicalEngine::sqlite(),
            &db,
            &wrapped,
            "sqlite_txn",
            bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
            bookclerk_db_exec::AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect("first execute");
        let guest = unwrap_guest_typed_reply(reply, &guest_req, &caps).expect("unwrap");
        assert_eq!(guest.statements[0].rows_affected, 1);

        let replay_req = ExecuteRequest {
            operation_id: "guest-replay-op".into(),
            request_hash: guest_hash.clone(),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('guest-replay', 99)"
                    .into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let replay_wrapped =
            wrap_guest_typed_request(replay_req.clone(), &slots_env()).expect("wrap replay");
        let replay = bookclerk_db_exec::execute_typed_envelope(
            bookclerk_db_exec::PhysicalEngine::sqlite(),
            &db,
            &replay_wrapped,
            "sqlite_txn",
            bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
            bookclerk_db_exec::AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect("replay execute");
        let replayed = unwrap_guest_typed_reply(replay, &replay_req, &caps).expect("replay");
        assert_eq!(replayed.statements[0].rows_affected, 1);
    }

    #[tokio::test]
    async fn nested_txn_wrap_finalizes_payload_before_outer_commit() {
        use sea_orm::{ConnectionTrait, Statement, TransactionTrait};

        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("mem db");
        let guest_hash = "b".repeat(64);
        let req = ExecuteRequest {
            operation_id: "guest-nested-op".into(),
            request_hash: guest_hash.clone(),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('guest-nested', 1)"
                    .into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let guest_req = req.clone();
        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
        let wrapped = wrap_guest_typed_request(req, &slots_env()).expect("wrap");
        assert!(!wrapped.guest_receipt.is_absent());
        // The engine-side executor runs inside a transaction it opened.
        #[allow(clippy::disallowed_methods)]
        let txn = db.begin().await.expect("begin");
        let reply = bookclerk_db_exec::execute_typed_on_txn_envelope(
            bookclerk_db_exec::PhysicalEngine::sqlite(),
            &txn,
            &wrapped,
            "sqlite_txn",
            bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
            bookclerk_db_exec::AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
            Some(&db),
        )
        .await
        .expect("nested wrap");
        let guest = unwrap_guest_typed_reply(reply, &guest_req, &caps).expect("unwrap");
        assert_eq!(guest.statements[0].rows_affected, 1);
        txn.commit().await.expect("outer commit");

        let rows = db
            .query_all_raw(Statement::from_string(
                db.get_database_backend(),
                "SELECT payload, status FROM bookclerk_receipts \
                 WHERE operation_id = 'guest-nested-op'",
            ))
            .await
            .expect("select receipt");
        assert_eq!(rows.len(), 1);
        let payload: String = rows[0].try_get("", "payload").expect("payload");
        let status: String = rows[0].try_get("", "status").expect("status");
        assert_eq!(status, "ok");
        assert!(
            !payload.is_empty(),
            "nested host execute must persist replay payload before outer commit"
        );

        let replay_req = ExecuteRequest {
            operation_id: "guest-nested-op".into(),
            request_hash: guest_hash.clone(),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('guest-nested', 99)"
                    .into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let replay_wrapped =
            wrap_guest_typed_request(replay_req.clone(), &slots_env()).expect("wrap replay");
        let replay = bookclerk_db_exec::execute_typed_envelope(
            bookclerk_db_exec::PhysicalEngine::sqlite(),
            &db,
            &replay_wrapped,
            "sqlite_txn",
            bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
            bookclerk_db_exec::AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect("replay execute");
        let replayed = unwrap_guest_typed_reply(replay, &replay_req, &caps).expect("replay");
        assert_eq!(replayed.statements[0].rows_affected, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_plan::DbPlanStatementKind;

    fn sqlite_caps() -> DbCapabilities {
        bookclerk_plugin_abi::DbCapabilities::advertised_sqlite()
    }

    fn guest_insert() -> ExecuteRequest {
        ExecuteRequest {
            operation_id: "guest-op".into(),
            request_hash: "a".repeat(64),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO counters (id, n) VALUES (?, ?)".into(),
                parameters: vec![DbValue::Int64(1), DbValue::Int64(1)],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        }
    }

    #[test]
    fn wrap_rewrites_insert_values_and_gates_writes() {
        let env = bookclerk_plugin_abi::sql_type_env_from_canonical_ddl(
            "CREATE TABLE counters (id INTEGER, n INTEGER)",
        );
        let wrapped = wrap_guest_typed_request(guest_insert(), &env).expect("wrap");
        assert!(wrapped.request.request_hash.is_empty());
        assert_eq!(wrapped.request.statements.len(), 4);
        assert!(!wrapped.guest_receipt.is_absent());
        assert!(
            wrapped.request.statements[1]
                .sql
                .contains("bookclerk_receipts"),
            "prior receipt select at index 1"
        );
        let gated = &wrapped.request.statements[2];
        assert!(
            gated.sql.to_ascii_uppercase().contains("SELECT"),
            "INSERT VALUES must become INSERT SELECT: {}",
            gated.sql
        );
        assert!(
            !gated.sql.to_ascii_uppercase().contains("VALUES"),
            "{}",
            gated.sql
        );
        assert!(
            gated.sql.contains(GUEST_RECEIPT_WRITE_GATE),
            "{}",
            gated.sql
        );
        assert_eq!(gated.parameters.len(), 3);
        assert_eq!(
            wrapped.proofs.len(),
            wrapped.request.statements.len(),
            "wrap must stamp 1:1 proofs"
        );
    }

    #[test]
    fn wrap_typecheck_failure_does_not_stamp_empty_proofs() {
        let err = match wrap_guest_typed_request(guest_insert(), &SqlTypeEnv::new()) {
            Ok(_) => panic!("wrap must fail closed when guest SQL cannot typecheck"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("unknown table") || err.to_string().contains("counters"),
            "{err}"
        );
    }

    #[test]
    fn wrap_does_not_predicate_idempotent_ddl() {
        let req = ExecuteRequest {
            operation_id: "guest-ddl".into(),
            request_hash: "b".repeat(64),
            statements: vec![TypedDbStatement {
                sql: "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY)".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Discard,
            }],
            deadline_unix_ms: 0,
        };
        let wrapped = wrap_guest_typed_request(req, &SqlTypeEnv::new()).expect("wrap");
        let ddl = &wrapped.request.statements[2];
        assert!(
            !ddl.sql.contains("bookclerk_receipts"),
            "DDL must not grow a write predicate: {}",
            ddl.sql
        );
        assert!(ddl.sql.contains("IF NOT EXISTS"), "{}", ddl.sql);
        let stub = wrapped.request.statements.last().expect("stub");
        assert!(
            stub.sql.contains("'claimed'"),
            "D1 preclaim stub must not look committed: {}",
            stub.sql
        );
    }

    #[test]
    fn wrap_mixed_ddl_dml_still_gates_writes() {
        let req = ExecuteRequest {
            operation_id: "guest-mixed".into(),
            request_hash: "c".repeat(64),
            statements: vec![
                TypedDbStatement {
                    sql: "CREATE TABLE IF NOT EXISTS counters (id INTEGER PRIMARY KEY, n INTEGER)"
                        .into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::Discard,
                },
                TypedDbStatement {
                    sql: "INSERT INTO counters (id, n) VALUES (?, ?)".into(),
                    parameters: vec![DbValue::Int64(1), DbValue::Int64(1)],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
            ],
            deadline_unix_ms: 0,
        };
        let wrapped = wrap_guest_typed_request(req, &SqlTypeEnv::new()).expect("wrap");
        let ddl = &wrapped.request.statements[2];
        let dml = &wrapped.request.statements[3];
        assert!(
            !ddl.sql.contains(GUEST_RECEIPT_WRITE_GATE),
            "DDL must stay ungated: {}",
            ddl.sql
        );
        assert!(
            dml.sql.contains(GUEST_RECEIPT_WRITE_GATE),
            "DML-only and mixed wrap still gate writes; D1 claimed-owner strips: {}",
            dml.sql
        );
        assert_eq!(dml.parameters.len(), 3);
    }

    #[test]
    fn unwrap_replays_stored_guest_reply_on_hash_match() {
        let guest = guest_insert();
        let payload = serde_json::to_string(&GuestReplayPayload {
            operation_id: guest.operation_id.clone(),
            statements: vec![bookclerk_plugin_abi::StatementResult::from_affected(1)],
            timing: bookclerk_plugin_abi::DbTiming::default(),
        })
        .expect("encode");
        let prior = bookclerk_plugin_abi::StatementResult {
            rows: vec![bookclerk_plugin_abi::DbRow {
                values: vec![
                    DbValue::Text("guest-op".into()),
                    DbValue::Text("a".repeat(64)),
                    DbValue::Text("ok".into()),
                    DbValue::Text(payload),
                    DbValue::Text("2026-01-01T00:00:00Z".into()),
                ],
            }],
            columns: vec![
                bookclerk_plugin_abi::DbColumn {
                    name: "operation_id".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "request_hash".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "status".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "payload".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "created_at".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
            ],
            rows_affected: 0,
        };
        let reply = ExecuteReply {
            operation_id: "guest-op".into(),
            statements: vec![
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                prior,
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                bookclerk_plugin_abi::StatementResult::from_affected(0),
            ],
            timing: bookclerk_plugin_abi::DbTiming::default(),
        };
        let replayed = unwrap_guest_typed_reply(reply, &guest, &sqlite_caps()).expect("replay");
        assert_eq!(replayed.statements.len(), 1);
        assert_eq!(replayed.statements[0].rows_affected, 1);
    }

    #[test]
    fn unwrap_rejects_replay_payload_that_fails_guest_reply_validation() {
        let guest = guest_insert();
        let payload = serde_json::to_string(&GuestReplayPayload {
            operation_id: "other-op".into(),
            statements: vec![
                bookclerk_plugin_abi::StatementResult::from_affected(1),
                bookclerk_plugin_abi::StatementResult::from_affected(1),
            ],
            timing: bookclerk_plugin_abi::DbTiming::default(),
        })
        .expect("encode");
        let prior = bookclerk_plugin_abi::StatementResult {
            rows: vec![bookclerk_plugin_abi::DbRow {
                values: vec![
                    DbValue::Text("guest-op".into()),
                    DbValue::Text("a".repeat(64)),
                    DbValue::Text("ok".into()),
                    DbValue::Text(payload),
                    DbValue::Text("2026-01-01T00:00:00Z".into()),
                ],
            }],
            columns: vec![
                bookclerk_plugin_abi::DbColumn {
                    name: "operation_id".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "request_hash".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "status".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "payload".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "created_at".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
            ],
            rows_affected: 0,
        };
        let reply = ExecuteReply {
            operation_id: "guest-op".into(),
            statements: vec![
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                prior,
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                bookclerk_plugin_abi::StatementResult::from_affected(0),
            ],
            timing: bookclerk_plugin_abi::DbTiming::default(),
        };
        let err = unwrap_guest_typed_reply(reply, &guest, &sqlite_caps()).unwrap_err();
        assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Unavailable);
    }

    #[test]
    fn unwrap_conflict_on_hash_mismatch() {
        let prior = bookclerk_plugin_abi::StatementResult {
            rows: vec![bookclerk_plugin_abi::DbRow {
                values: vec![
                    DbValue::Text("guest-op".into()),
                    DbValue::Text("other-hash".into()),
                ],
            }],
            columns: vec![
                bookclerk_plugin_abi::DbColumn {
                    name: "operation_id".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "request_hash".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
            ],
            rows_affected: 0,
        };
        let reply = ExecuteReply {
            operation_id: "guest-op".into(),
            statements: vec![
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                prior,
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                bookclerk_plugin_abi::StatementResult::from_affected(0),
            ],
            timing: bookclerk_plugin_abi::DbTiming::default(),
        };
        let mut guest = guest_insert();
        guest.request_hash = "expected-hash".into();
        let err = unwrap_guest_typed_reply(reply, &guest, &sqlite_caps()).unwrap_err();
        assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Conflict);
    }

    #[test]
    fn unwrap_unavailable_on_ok_empty_payload() {
        let prior = bookclerk_plugin_abi::StatementResult {
            rows: vec![bookclerk_plugin_abi::DbRow {
                values: vec![
                    DbValue::Text("guest-op".into()),
                    DbValue::Text("a".repeat(64)),
                    DbValue::Text("ok".into()),
                    DbValue::Text(String::new()),
                    DbValue::Text("2026-01-01T00:00:00Z".into()),
                ],
            }],
            columns: vec![
                bookclerk_plugin_abi::DbColumn {
                    name: "operation_id".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "request_hash".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "status".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "payload".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "created_at".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
            ],
            rows_affected: 0,
        };
        let reply = ExecuteReply {
            operation_id: "guest-op".into(),
            statements: vec![
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                prior,
                bookclerk_plugin_abi::StatementResult::from_affected(7),
                bookclerk_plugin_abi::StatementResult::from_affected(0),
            ],
            timing: bookclerk_plugin_abi::DbTiming::default(),
        };
        let err = unwrap_guest_typed_reply(reply, &guest_insert(), &sqlite_caps()).unwrap_err();
        assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Unavailable);
    }

    #[test]
    fn unwrap_claimed_empty_payload_uses_guest_slice() {
        let prior = bookclerk_plugin_abi::StatementResult {
            rows: vec![bookclerk_plugin_abi::DbRow {
                values: vec![
                    DbValue::Text("guest-op".into()),
                    DbValue::Text("a".repeat(64)),
                    DbValue::Text("claimed".into()),
                    DbValue::Text(String::new()),
                    DbValue::Text("2026-01-01T00:00:00Z".into()),
                ],
            }],
            columns: vec![
                bookclerk_plugin_abi::DbColumn {
                    name: "operation_id".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "request_hash".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "status".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "payload".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "created_at".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
            ],
            rows_affected: 0,
        };
        let reply = ExecuteReply {
            operation_id: "guest-op".into(),
            statements: vec![
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                prior,
                bookclerk_plugin_abi::StatementResult::from_affected(7),
                bookclerk_plugin_abi::StatementResult::from_affected(0),
            ],
            timing: bookclerk_plugin_abi::DbTiming::default(),
        };
        let guest = unwrap_guest_typed_reply(reply, &guest_insert(), &sqlite_caps())
            .expect("claimed resume");
        assert_eq!(guest.statements.len(), 1);
        assert_eq!(guest.statements[0].rows_affected, 7);
    }

    #[test]
    fn unwrap_claimed_empty_payload_gated_zeros_is_unavailable() {
        let prior = bookclerk_plugin_abi::StatementResult {
            rows: vec![bookclerk_plugin_abi::DbRow {
                values: vec![
                    DbValue::Text("guest-op".into()),
                    DbValue::Text("a".repeat(64)),
                    DbValue::Text("claimed".into()),
                    DbValue::Text(String::new()),
                    DbValue::Text("2026-01-01T00:00:00Z".into()),
                ],
            }],
            columns: vec![
                bookclerk_plugin_abi::DbColumn {
                    name: "operation_id".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "request_hash".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "status".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "payload".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
                bookclerk_plugin_abi::DbColumn {
                    name: "created_at".into(),
                    db_type: bookclerk_plugin_abi::DbType::Text,
                },
            ],
            rows_affected: 0,
        };
        let reply = ExecuteReply {
            operation_id: "guest-op".into(),
            statements: vec![
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                prior,
                bookclerk_plugin_abi::StatementResult::from_affected(0),
                bookclerk_plugin_abi::StatementResult::from_affected(0),
            ],
            timing: bookclerk_plugin_abi::DbTiming::default(),
        };
        let err = unwrap_guest_typed_reply(reply, &guest_insert(), &sqlite_caps()).unwrap_err();
        assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Unavailable);
    }
}

/// The shared vector the TypeScript and Python SDKs check their isolate
/// adapter receipt helpers against.
///
/// It holds one host-wrapped write batch and one read-only batch exactly as
/// [`wrap_guest_typed_request`] builds them, the SQLite lowering of every
/// statement an adapter runs for them (including the finalize `UPDATE`), and
/// a partial reply with the finalize statements
/// [`bookclerk_db_exec::guest_receipt_finalize_stmts`] derives from it, so
/// the SDK helpers persist the same payload the host decodes.
#[cfg(test)]
mod isolate_adapter_fixture {
    use super::*;
    use bookclerk_plugin_abi::{DbColumn, DbRow, DbTiming, DbType, StatementResult};
    use sea_orm::DatabaseBackend;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    /// Fixture path, relative to this crate.
    const FIXTURE: &str = "fixtures/guest_receipt/isolate_adapter.json";

    /// Fixed receipt creation time, so the vector does not change per run.
    const CREATED_AT: &str = "2026-01-01T00:00:00+00:00";

    /// Fixed receipt expiry, far enough out that no test prunes it.
    const EXPIRES_AT: &str = "2999-01-01T00:00:00+00:00";

    /// The guest table both batches use.
    const NOTES_DDL: &str =
        "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)";

    /// One guest statement.
    fn stmt(
        sql: &str,
        parameters: Vec<DbValue>,
        kind: DbPlanStatementKind,
        result_selection: DbResultSelection,
    ) -> TypedDbStatement {
        TypedDbStatement {
            sql: sql.into(),
            parameters,
            kind,
            max_rows: 0,
            result_selection,
        }
    }

    /// Wraps `statements` the way the host does, with fixed timestamps, and
    /// records the SQLite lowering of every wrapped statement in `sqlite`.
    fn wrapped_case(
        operation_id: &str,
        request_hash: &str,
        statements: Vec<TypedDbStatement>,
        sqlite: &mut BTreeMap<String, String>,
    ) -> Value {
        let env = bookclerk_plugin_abi::sql_type_env_from_canonical_ddl(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
        );
        let guest = ExecuteRequest {
            operation_id: operation_id.into(),
            request_hash: request_hash.into(),
            statements,
            deadline_unix_ms: 0,
        };
        let mut wrapped = wrap_guest_typed_request(guest.clone(), &env).expect("wrap");
        let last = wrapped.request.statements.len() - 1;
        wrapped.request.statements[0].parameters[0] = DbValue::Text(CREATED_AT.into());
        wrapped.request.statements[last].parameters[3] = DbValue::Text(CREATED_AT.into());
        wrapped.request.statements[last].parameters[4] = DbValue::Text(EXPIRES_AT.into());
        for (stmt, proof) in wrapped.request.statements.iter().zip(&wrapped.proofs) {
            sqlite.insert(stmt.sql.clone(), lower_sqlite(&stmt.sql, Some(proof)));
        }
        json!({
            "guest": guest,
            "request": {
                "operationId": wrapped.request.operation_id,
                "requestHash": wrapped.request.request_hash,
                "statements": wrapped.request.statements,
                "receipt": {
                    "guestLen": wrapped.guest_receipt.guest_statement_len,
                    "guestHash": wrapped.guest_receipt.guest_request_hash,
                },
            },
        })
    }

    /// SQLite text an adapter runs for canonical `sql`.
    fn lower_sqlite(sql: &str, proof: Option<&bookclerk_plugin_abi::ResolvedStatement>) -> String {
        if bookclerk_plugin_abi::statement_is_ddl(sql) {
            return sql.to_string();
        }
        bookclerk_db_exec::lower_canonical_sql_typed(DatabaseBackend::Sqlite, sql, proof)
            .expect("lower")
    }

    /// A partial wrapped reply whose guest cells cover every `DbValue` kind
    /// and JSON escapes, with the prior-receipt SELECT empty.
    fn partial_reply() -> ExecuteReply {
        let text = |name: &str| DbColumn {
            name: name.into(),
            db_type: DbType::Text,
        };
        let prior = StatementResult {
            rows: Vec::new(),
            columns: [
                "operation_id",
                "request_hash",
                "status",
                "payload",
                "created_at",
            ]
            .into_iter()
            .map(text)
            .collect(),
            rows_affected: 0,
        };
        let guest = StatementResult {
            rows: vec![DbRow {
                values: vec![
                    DbValue::Int64(-9_007_199_254_740_991),
                    DbValue::Text("line\n\"quoted\"\t\\ caf\u{e9} \u{1} \u{1f4da}".into()),
                    DbValue::Float64(1.5),
                    DbValue::Boolean(true),
                    DbValue::Bytes(vec![0, 1, 254, 255]),
                    DbValue::Null(DbType::Text),
                    DbValue::Float64(-0.0),
                ],
            }],
            columns: vec![
                DbColumn {
                    name: "id".into(),
                    db_type: DbType::Int64,
                },
                text("body"),
                DbColumn {
                    name: "score".into(),
                    db_type: DbType::Float64,
                },
                DbColumn {
                    name: "done".into(),
                    db_type: DbType::Bool,
                },
                DbColumn {
                    name: "blob".into(),
                    db_type: DbType::Bytes,
                },
                text("note"),
                DbColumn {
                    name: "signed_zero".into(),
                    db_type: DbType::Float64,
                },
            ],
            rows_affected: 1,
        };
        ExecuteReply {
            operation_id: "fixture-partial".into(),
            statements: vec![
                StatementResult::from_affected(0),
                prior,
                StatementResult::from_affected(0),
                guest,
                StatementResult::from_affected(1),
            ],
            timing: DbTiming {
                attempt_elapsed_us: 0,
                db_execution_us: 0,
                db_timing_source: "sqlite_txn".into(),
            },
        }
    }

    /// Builds the whole vector.
    fn fixture() -> Value {
        let mut sqlite = BTreeMap::new();
        let mut setup = Vec::new();
        for ddl in crate::migrations::binding_bootstrap_statements()
            .iter()
            .filter(|sql| sql.contains("bookclerk_receipts"))
        {
            setup.push(ddl.clone());
        }
        let write = wrapped_case(
            "fixture-write",
            &"a".repeat(64),
            vec![
                stmt(
                    NOTES_DDL,
                    Vec::new(),
                    DbPlanStatementKind::Execute,
                    DbResultSelection::Discard,
                ),
                stmt(
                    "INSERT INTO notes (id, body) VALUES (?, ?) RETURNING id, body",
                    vec![DbValue::Int64(1), DbValue::Text("first".into())],
                    DbPlanStatementKind::Returning,
                    DbResultSelection::Rows,
                ),
            ],
            &mut sqlite,
        );
        let read_only = wrapped_case(
            "fixture-read",
            &"b".repeat(64),
            vec![stmt(
                "SELECT id, body FROM notes ORDER BY id",
                Vec::new(),
                DbPlanStatementKind::Select,
                DbResultSelection::Rows,
            )],
            &mut sqlite,
        );
        let partial = partial_reply();
        let guest_hash = "c".repeat(64);
        let finalize =
            bookclerk_db_exec::guest_receipt_finalize_stmts(&partial, 2, &guest_hash, false)
                .expect("finalize");
        for stmt in &finalize {
            sqlite.insert(stmt.sql.clone(), lower_sqlite(&stmt.sql, None));
        }
        json!({
            "about": "Generated by bookclerk-library `isolate_adapter_receipt_fixture_is_current`; \
                      regenerate with UPDATE_GUEST_RECEIPT_FIXTURE=1, never edit by hand.",
            "replayPayloadMaxBytes": bookclerk_db_exec::GUEST_TYPED_REPLAY_PAYLOAD_MAX_BYTES,
            "readOnlyPayload": bookclerk_db_exec::GUEST_READ_ONLY_REPLAY_PAYLOAD,
            "setup": setup,
            "notesDdl": NOTES_DDL,
            "sqlite": sqlite,
            "write": write,
            "readOnly": read_only,
            "finalize": {
                "partial": partial,
                "guestLen": 2,
                "guestHash": guest_hash,
                "statements": finalize,
            },
        })
    }

    #[test]
    fn isolate_adapter_receipt_fixture_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
        let mut text = serde_json::to_string_pretty(&fixture()).expect("encode");
        text.push('\n');
        // Regenerating writes the vector, then checks it like any other run.
        // One expression, so the regenerate path does not leave an unrun line.
        std::env::var_os("UPDATE_GUEST_RECEIPT_FIXTURE")
            .map(|_| std::fs::write(&path, &text))
            .transpose()
            .expect("write fixture");
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == text,
            "{FIXTURE} is stale; regenerate with UPDATE_GUEST_RECEIPT_FIXTURE=1 cargo test -p bookclerk-library isolate_adapter_receipt_fixture"
        );
    }

    #[test]
    fn the_fixture_payload_is_what_the_host_decodes() {
        let partial = partial_reply();
        let finalize =
            bookclerk_db_exec::guest_receipt_finalize_stmts(&partial, 2, &"c".repeat(64), false)
                .expect("finalize");
        let bound = serde_json::to_value(&finalize[0].parameters[0]).expect("encode");
        let payload = bound["value"]
            .as_str()
            .expect("finalize binds the payload text first");
        let decoded: GuestReplayPayload = serde_json::from_str(payload).expect("decode");
        assert_eq!(decoded.statements, partial.statements[2..4].to_vec());
    }
}
