//! Typed adapter conformance (`ExecuteRequest` / `ExecuteReply`).
//!
//! Shared vectors run on three first-party adapters (differential suite):
//! - SQLite in-process: [`run_typed_conn_vectors`] and
//!   `bookclerk-plugin-database-sqlite` `typed_conformance` (guest execute)
//! - PostgreSQL: [`run_typed_conn_vectors`] when `BOOKCLERK_TEST_POSTGRES_URL`
//!   is set (`typed_shared_vectors_on_postgres`)
//! - D1 HTTP mock: `database-d1` `executing_mock_typed_shared_vectors`
//!
//! Compare typed `DbValue` cells and Bookclerk error classes, not engine
//! strings. Admission N/N+1 for advertised caps lives in `sql_plan::limits_tests`.
//!
//! [`run_typed_request_vectors`] is the #178 admission path for database
//! plugins; the vector list itself lives at the bottom of this module.

#![allow(clippy::missing_docs_in_private_items)]

use std::future::Future;

use bookclerk_plugin_abi::{
    apply_schema_sql_to_env, AdapterExecuteRequest, DbCapabilities, DbPlanStatementKind,
    DbResultSelection, DbValue, ExecuteReply, ExecuteRequest, SqlTypeEnv, TypedDbStatement,
};
use sea_orm::DatabaseConnection;
use serde_json::Value as JsonValue;

use super::{
    compile_named_request, interpret_typed_exec, validate_execute_reply, CONTRACT_VECTOR_ROW_CAP,
};
use crate::atomic_ops::{atomic_status, DbAtomicParams};

/// Runs the shared contract suite through a typed `ExecuteRequest` callback.
///
/// `advertised_cap` is the adapter's negotiated `maxResultRows`. The harness does
/// not wrap SQL or post-reject oversized results.
///
/// # Panics
///
/// Panics when a vector fails.
pub async fn run_typed_request_vectors<F, Fut, E>(
    connect: DbCapabilities,
    advertised_cap: u32,
    mut run: F,
) where
    F: FnMut(ExecuteRequest) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, E>>,
    E: std::fmt::Display,
{
    run_typed_contract_vectors(connect, advertised_cap, move |typed, _cap| {
        let fut = run(typed);
        async move { fut.await.map_err(|err| err.to_string()) }
    })
    .await;
}

/// Desugars and stamps proofs for one vector, recording DDL in `catalog`.
///
/// Adapter-boundary tests (guest execute) must stamp against the host catalog
/// plus tables created by earlier vectors.
///
/// # Errors
///
/// Returns when typecheck fails.
pub fn stamp_typed_vector(
    req: ExecuteRequest,
    catalog: &mut SqlTypeEnv,
) -> Result<AdapterExecuteRequest, String> {
    let envelope = bookclerk_db_exec::stamp_adapter_execute(req.clone(), catalog)
        .map_err(|err| err.to_string())?;
    for stmt in &envelope.request.statements {
        apply_schema_sql_to_env(catalog, &stmt.sql);
    }
    Ok(envelope)
}

/// Runs the shared contract suite through the host stamp + adapter execute path.
///
/// # Panics
///
/// Panics when a vector fails.
pub async fn run_typed_conn_vectors(db: &DatabaseConnection, connect: DbCapabilities) {
    let db = db.clone();
    let connect_for_run = connect.clone();
    let mut catalog = crate::migrations::host_sql_type_env();
    run_typed_contract_vectors(connect, CONTRACT_VECTOR_ROW_CAP, move |typed, cap| {
        let stamped = stamp_typed_vector(typed, &mut catalog);
        let db = db.clone();
        let connect = connect_for_run.clone();
        async move {
            let envelope = stamped?;
            let mut caps = bookclerk_db_exec::ExecCaps::from_capabilities(&connect);
            if cap > 0 {
                caps.max_result_rows = cap;
            }
            let reply = bookclerk_db_exec::execute_typed_envelope_on_connection(
                &db,
                &envelope,
                caps,
                bookclerk_db_exec::AtomicSession::from_deadline(None)
                    .with_type_env(crate::migrations::host_sql_type_env()),
            )
            .await
            .map_err(|err| err.to_string())?;
            validate_execute_reply(&envelope.request, &reply, &connect)
                .map_err(|err| err.to_string())?;
            Ok(reply)
        }
    })
    .await;
}

/// Native typed contract suite (`ExecuteRequest` → `ExecuteReply`).
///
/// Every reply is validated against its request before the vector's own
/// assertions run.
pub async fn run_typed_contract_vectors<F, Fut>(connect: DbCapabilities, row_cap: u32, mut run: F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let connect_for_validate = connect.clone();
    run_raw_typed_vectors(connect, row_cap, move |typed, cap| {
        let connect = connect_for_validate.clone();
        let typed_for_validate = typed.clone();
        let fut = run(typed, cap);
        async move {
            let reply = fut.await?;
            validate_typed_reply(&connect, &typed_for_validate, &reply)?;
            Ok(reply)
        }
    })
    .await;
}

/// Asserts a typed reply echoes the request before host interpretation.
fn validate_typed_reply(
    connect: &DbCapabilities,
    req: &ExecuteRequest,
    reply: &ExecuteReply,
) -> Result<(), String> {
    reply.validate_positional().map_err(|err| err.to_string())?;
    if reply.operation_id != req.operation_id {
        return Err(format!(
            "execute reply operationId {:?} does not echo {}",
            reply.operation_id, req.operation_id
        ));
    }
    if reply.statements.len() != req.statements.len() {
        return Err(format!(
            "execute reply has {} statements; request has {}",
            reply.statements.len(),
            req.statements.len()
        ));
    }
    validate_execute_reply(req, reply, connect).map_err(|err| err.to_string())
}

/// One ~160 KiB TEXT cell without embedding the payload in statement text.
///
/// D1 `maxPayloadBytes` and the 100 KiB physical SQL limit cannot admit a
/// 150 KiB literal. Doubling CTE: `10 * 2^14 = 163840` bytes. Two such
/// statements exceed [`bookclerk_plugin_abi::FIRST_PARTY_MAX_RESULT_BYTES`].
const LARGE_RESULT_PAD_SQL: &str = "WITH RECURSIVE t(n, s) AS (\
 SELECT 1, 'aaaaaaaaaa' \
 UNION ALL \
 SELECT n + 1, s || s FROM t WHERE n < 15\
) SELECT s AS pad FROM t WHERE n = 15";

/// Runs the raw vector list without reply validation; see
/// [`run_typed_contract_vectors`] for the validating public entry point.
async fn run_raw_typed_vectors<F, Fut>(_connect: DbCapabilities, row_cap: u32, mut run: F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    typed_commit_and_replay(&mut run).await;
    typed_hash_conflict(&mut run).await;
    typed_password_replay_and_hash_conflict(&mut run).await;
    typed_unique_generic_insert_fails(&mut run).await;
    typed_failed_statement_rolls_back(&mut run).await;
    typed_row_cap_select(&mut run, row_cap).await;
    typed_returning_insert_cap(&mut run, row_cap).await;
    typed_returning_update_cap(&mut run, row_cap).await;
    typed_returning_delete_cap(&mut run, row_cap).await;
    typed_rows_affected_by_kind(&mut run).await;
    typed_values_returning_cap(&mut run, row_cap).await;
    typed_aggregate_scalar_cap(&mut run).await;
    typed_wide_numeric_row_cap(&mut run).await;
    typed_universal_value_matrix(&mut run).await;
}

/// Universal `DbValue` round-trip through declared table columns.
///
/// Identical-observable-value contract (#178) for every admitted adapter:
/// `Int64` (including `i64::MIN` / `i64::MAX`), finite `Float64`, UTF-8
/// `Text` (including adversarial `b64:`-prefixed text), `Bytes`, `Boolean`,
/// and typed SQL NULL all round-trip to the **same** `DbValue` variant on
/// every adapter. Engine storage affinity must not leak: adapters normalize
/// results from declared column metadata (SQLite decltype, PostgreSQL type
/// info, D1 `pragma_table_info`), so a declared `BOOLEAN` column reads back
/// [`bookclerk_plugin_abi::DbValue::Boolean`] and a NULL in a declared
/// `BIGINT` column reads back `Null(Int64)` everywhere.
async fn typed_universal_value_matrix<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    use bookclerk_plugin_abi::{DbResultSelection, DbValue, TypedDbStatement};

    run(
        typed_request(
            "vec-vals-setup",
            exec_plan(&[
                // BYTEA is a valid declared type on PostgreSQL and carries
                // Bytes affinity metadata on SQLite-family adapters.
                "CREATE TABLE IF NOT EXISTS vec_vals (id BIGINT PRIMARY KEY, i BIGINT, \
                 f DOUBLE PRECISION, t TEXT, y BYTEA, b BOOLEAN, n BIGINT)",
                "DELETE FROM vec_vals",
            ]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("value matrix setup: {e}"));

    let rows: [(&str, i64, i64, bool, &str); 2] = [
        ("vec-vals-min", 1, i64::MIN, true, "café 日本語"),
        ("vec-vals-max", 2, i64::MAX, false, "b64:AAAA"),
    ];
    let bytes = vec![0u8, 127, 255, 42];
    for (op, id, int, boolean, text) in rows {
        let insert = ExecuteRequest {
            operation_id: op.into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO vec_vals (id, i, f, t, y, b, n) VALUES (?, ?, ?, ?, ?, ?, ?)"
                    .into(),
                parameters: vec![
                    DbValue::Int64(id),
                    DbValue::Int64(int),
                    DbValue::Float64(1.25),
                    DbValue::Text(text.into()),
                    DbValue::Bytes(bytes.clone()),
                    DbValue::Boolean(boolean),
                    DbValue::Null(bookclerk_plugin_abi::DbType::Int64),
                ],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let reply = run(insert, 0)
            .await
            .unwrap_or_else(|e| panic!("{op} insert: {e}"));
        assert_eq!(reply.statements[0].rows_affected, 1, "{op}");
    }
    for (op, id, int, boolean, text) in rows {
        let select = ExecuteRequest {
            operation_id: format!("{op}-sel"),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT i, f, t, y, b, n FROM vec_vals WHERE id = ?".into(),
                parameters: vec![DbValue::Int64(id)],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let reply = run(select, 0)
            .await
            .unwrap_or_else(|e| panic!("{op} select: {e}"));
        let values = &reply.statements[0].rows[0].values;
        assert_eq!(values[0], DbValue::Int64(int), "{op} int64");
        assert_eq!(values[1], DbValue::Float64(1.25), "{op} float64");
        assert_eq!(values[2], DbValue::Text(text.into()), "{op} text");
        assert_eq!(values[3], DbValue::Bytes(bytes.clone()), "{op} bytes");
        assert_eq!(
            values[4],
            DbValue::Boolean(boolean),
            "{op} declared BOOLEAN column must read back Boolean on every adapter"
        );
        assert_eq!(
            values[5],
            DbValue::Null(bookclerk_plugin_abi::DbType::Int64),
            "{op} NULL in a declared BIGINT column must read back Null(int64) on every adapter"
        );
    }
}

async fn typed_commit_and_replay<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let compiled = compile_named_request(
        "vec-enq",
        &DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"a"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
        "2024-06-01T00:00:00Z",
    )
    .expect("compile enqueue");
    let first = run(compiled.clone().into_typed_request("vec-enq"), 0)
        .await
        .unwrap_or_else(|e| panic!("first atomic: {e}"));
    let interpreted = interpret_typed_exec(&compiled, &first, &compiled.expected_hash);
    assert_eq!(interpreted.status, atomic_status::OK);
    let replay = run(compiled.clone().into_typed_request("vec-enq"), 0)
        .await
        .unwrap_or_else(|e| panic!("replay: {e}"));
    let replayed = interpret_typed_exec(&compiled, &replay, &compiled.expected_hash);
    assert!(replayed.replayed, "same operationId must replay");
}

/// Same `operationId` with a different request hash is an idempotency conflict.
async fn typed_hash_conflict<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let first = compile_named_request(
        "vec-conflict",
        &DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"a"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
        "2024-06-01T00:00:00Z",
    )
    .unwrap();
    let exec = run(first.clone().into_typed_request("vec-conflict"), 0)
        .await
        .unwrap_or_else(|e| panic!("conflict seed: {e}"));
    assert_eq!(
        interpret_typed_exec(&first, &exec, &first.expected_hash).status,
        atomic_status::OK
    );
    let other = compile_named_request(
        "vec-conflict",
        &DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"other"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
        "2024-06-01T00:00:00Z",
    )
    .unwrap();
    let snapshot = typed_job_payloads(&mut *run).await;
    let replay = run(other.clone().into_typed_request("vec-conflict"), 0)
        .await
        .unwrap_or_else(|e| panic!("conflict other plan: {e}"));
    let result = interpret_typed_exec(&other, &replay, &other.expected_hash);
    assert_eq!(result.status, atomic_status::IDEMPOTENCY_CONFLICT);
    let after = typed_job_payloads(&mut *run).await;
    assert_eq!(
        snapshot, after,
        "mismatched-hash plan must not mutate domain state"
    );
}

/// Snapshot of `jobs.payload` used to prove a mismatched-hash plan did not mutate.
async fn typed_job_payloads<F, Fut>(run: &mut F) -> Vec<JsonValue>
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let exec = run(
        typed_request(
            "vec-conflict-snap",
            select_plan("SELECT payload FROM jobs ORDER BY id"),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("job snapshot: {e}"));
    exec.statements
        .first()
        .map(|s| {
            s.rows
                .iter()
                .map(|row| serde_json::Value::Array(row.values.iter().map(cell_to_json).collect()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Outcome-first mutating op: exact replay and mismatched hash leave the user row unchanged.
async fn typed_password_replay_and_hash_conflict<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    run(
        typed_request(
            "vec-pw-seed",
            exec_plan(&["INSERT INTO users (id, role, status, display_name, security_version, created_at, updated_at) \
                 VALUES (9001, 'member', 'active', 'vec-pw', 0, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("password user seed: {e}"));
    let first = compile_named_request(
        "vec-pw",
        &DbAtomicParams::SetUserPasswordHash {
            user_id: 9001,
            password_hash: Some("hash-one".into()),
        },
        "2024-06-01T00:00:00Z",
    )
    .unwrap();
    let exec = run(first.clone().into_typed_request("vec-pw"), 0)
        .await
        .unwrap_or_else(|e| panic!("password first: {e}"));
    assert_eq!(
        interpret_typed_exec(&first, &exec, &first.expected_hash).status,
        atomic_status::OK
    );
    let snapshot = typed_user_password_row(&mut *run).await;
    let replay = run(first.clone().into_typed_request("vec-pw"), 0)
        .await
        .unwrap_or_else(|e| panic!("password replay: {e}"));
    assert!(
        interpret_typed_exec(&first, &replay, &first.expected_hash).replayed,
        "exact password replay must replay"
    );
    assert_eq!(
        snapshot,
        typed_user_password_row(&mut *run).await,
        "exact replay must not bump security_version or rewrite the hash"
    );
    let other = compile_named_request(
        "vec-pw",
        &DbAtomicParams::SetUserPasswordHash {
            user_id: 9001,
            password_hash: Some("hash-two".into()),
        },
        "2024-06-01T00:00:00Z",
    )
    .unwrap();
    let conflict = run(other.clone().into_typed_request("vec-pw"), 0)
        .await
        .unwrap_or_else(|e| panic!("password mismatch: {e}"));
    assert_eq!(
        interpret_typed_exec(&other, &conflict, &other.expected_hash).status,
        atomic_status::IDEMPOTENCY_CONFLICT
    );
    assert_eq!(
        snapshot,
        typed_user_password_row(&mut *run).await,
        "mismatched hash must leave the user row unchanged"
    );
}

/// Snapshot of user `9001` password bytes used by replay / hash-conflict checks.
async fn typed_user_password_row<F, Fut>(run: &mut F) -> Vec<JsonValue>
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let exec = run(
        typed_request(
            "vec-pw-snap",
            select_plan("SELECT password_hash, security_version FROM users WHERE id = 9001"),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("user snapshot: {e}"));
    exec.statements
        .first()
        .map(|s| {
            s.rows
                .iter()
                .map(|row| serde_json::Value::Array(row.values.iter().map(cell_to_json).collect()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Duplicate primary-key inserts fail closed (engine unique / 23505).
async fn typed_unique_generic_insert_fails<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let err = run(typed_request("vec-dup", dup_slot_plan("vec-dup")), 0)
        .await
        .err()
        .unwrap_or_else(|| panic!("duplicate insert must fail"));
    let msg = err.to_lowercase();
    assert!(
        msg.contains("unique")
            || msg.contains("constraint")
            || msg.contains("23505")
            || msg.contains("conflict"),
        "{err}"
    );
}

/// A later failing statement rolls back earlier inserts in the same plan.
async fn typed_failed_statement_rolls_back<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    assert!(run(typed_request("vec-rb", dup_slot_plan("rb-vec")), 0)
        .await
        .is_err());
    let check = run(
        typed_request(
            "vec-rb-check",
            select_plan("SELECT slot_key FROM bookclerk_slots WHERE slot_key = 'rb-vec'"),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rollback check: {e}"));
    assert!(
        check.statements[0].rows.is_empty(),
        "failed plan must not leave the first insert: {:?}",
        check.statements[0].rows
    );
}

/// Read-only SELECT stops after `maxResultRows`.
async fn typed_row_cap_select<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        typed_request(
            "vec-cap-setup",
            exec_plan(&[
                "CREATE TABLE IF NOT EXISTS vec_rowcap (x INTEGER)",
                "DELETE FROM vec_rowcap",
            ]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rowcap setup: {e}"));
    run(
        typed_request(
            "vec-cap-ins",
            exec_plan_owned(vec![recursive_insert("vec_rowcap", "x", n)]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rowcap insert: {e}"));
    let err = run(
        typed_request("vec-cap-sel", select_plan("SELECT x FROM vec_rowcap")),
        row_cap,
    )
    .await
    .expect_err("over-cap SELECT must fail");
    assert!(
        err.to_lowercase().contains("maxresultrows"),
        "row cap must fail closed: {err}"
    );
}

/// `INSERT … RETURNING` is not rewritten as a subquery and still honors the cap.
async fn typed_returning_insert_cap<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        typed_request(
            "vec-ret-ins-setup",
            exec_plan(&[
                "CREATE TABLE IF NOT EXISTS vec_ret_ins (id INTEGER PRIMARY KEY)",
                "DELETE FROM vec_ret_ins",
            ]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("returning insert setup: {e}"));
    let sql = recursive_insert_returning("vec_ret_ins", "id", n);
    let before = typed_table_fingerprint(&mut *run, "vec-ret-ins-before", "vec_ret_ins").await;
    let err = run(typed_request("vec-ret-ins", returning_plan(&sql)), row_cap)
        .await
        .expect_err("capped INSERT RETURNING must fail");
    assert!(
        !err.to_lowercase().contains("syntax"),
        "INSERT RETURNING must not be wrapped as a subquery: {err}"
    );
    assert!(
        err.to_lowercase().contains("maxresultrows"),
        "INSERT RETURNING cap must fail closed: {err}"
    );
    let after = typed_table_fingerprint(&mut *run, "vec-ret-ins-after", "vec_ret_ins").await;
    assert_eq!(
        before, after,
        "failed INSERT RETURNING must not leave rows: before={before:?} after={after:?}"
    );
}

/// `UPDATE … RETURNING` honors the row cap without a subquery rewrite.
async fn typed_returning_update_cap<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        typed_request(
            "vec-ret-upd-setup",
            exec_plan_owned(vec![
                "CREATE TABLE IF NOT EXISTS vec_ret_upd (id INTEGER PRIMARY KEY)".into(),
                "DELETE FROM vec_ret_upd".into(),
                recursive_insert("vec_ret_upd", "id", n),
            ]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("returning update setup: {e}"));
    let before = typed_table_fingerprint(&mut *run, "vec-ret-upd-before", "vec_ret_upd").await;
    let err = run(
        typed_request(
            "vec-ret-upd",
            returning_plan("UPDATE vec_ret_upd SET id = id RETURNING id"),
        ),
        row_cap,
    )
    .await
    .expect_err("capped UPDATE RETURNING must fail");
    assert!(
        !err.to_lowercase().contains("syntax"),
        "UPDATE RETURNING must not be wrapped as a subquery: {err}"
    );
    assert!(
        err.to_lowercase().contains("maxresultrows"),
        "UPDATE RETURNING cap must fail closed: {err}"
    );
    let after = typed_table_fingerprint(&mut *run, "vec-ret-upd-after", "vec_ret_upd").await;
    assert_eq!(
        before, after,
        "failed UPDATE RETURNING must leave the table unchanged"
    );
}

/// `DELETE … RETURNING` honors the row cap without a subquery rewrite.
async fn typed_returning_delete_cap<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        typed_request(
            "vec-ret-del-setup",
            exec_plan_owned(vec![
                "CREATE TABLE IF NOT EXISTS vec_ret_del (id INTEGER PRIMARY KEY)".into(),
                "DELETE FROM vec_ret_del".into(),
                recursive_insert("vec_ret_del", "id", n),
            ]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("returning delete setup: {e}"));
    let before = typed_table_fingerprint(&mut *run, "vec-ret-del-before", "vec_ret_del").await;
    let err = run(
        typed_request(
            "vec-ret-del",
            returning_plan("DELETE FROM vec_ret_del RETURNING id"),
        ),
        row_cap,
    )
    .await
    .expect_err("capped DELETE RETURNING must fail");
    assert!(
        !err.to_lowercase().contains("syntax"),
        "DELETE RETURNING must not be wrapped as a subquery: {err}"
    );
    assert!(
        err.to_lowercase().contains("maxresultrows"),
        "DELETE RETURNING cap must fail closed: {err}"
    );
    let after = typed_table_fingerprint(&mut *run, "vec-ret-del-after", "vec_ret_del").await;
    assert_eq!(
        before, after,
        "failed DELETE RETURNING must leave the table unchanged"
    );
}

/// Multi-tuple `INSERT … VALUES (),( ) … RETURNING` is not a 1-row proof.
async fn typed_values_returning_cap<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        typed_request(
            "vec-val-setup",
            exec_plan(&[
                "CREATE TABLE IF NOT EXISTS vec_val_ins (id INTEGER PRIMARY KEY)",
                "DELETE FROM vec_val_ins",
            ]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("values returning setup: {e}"));
    let tuples = (0..n)
        .map(|i| format!("({i})"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("INSERT INTO vec_val_ins (id) VALUES {tuples} RETURNING id");
    let before = typed_table_fingerprint(&mut *run, "vec-val-before", "vec_val_ins").await;
    let err = run(typed_request("vec-val-ins", returning_plan(&sql)), row_cap)
        .await
        .expect_err("multi-tuple VALUES RETURNING must fail");
    assert!(
        err.to_lowercase().contains("maxresultrows")
            || err.to_lowercase().contains("proven")
            || err.to_lowercase().contains("values"),
        "VALUES RETURNING cap must fail closed: {err}"
    );
    let after = typed_table_fingerprint(&mut *run, "vec-val-after", "vec_val_ins").await;
    assert_eq!(
        before, after,
        "failed VALUES RETURNING must not leave rows: before={before:?} after={after:?}"
    );
}

/// Two individually-valid row-producing statements whose aggregate exceeds the RPC scalar.
async fn typed_aggregate_scalar_cap<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    // Two ~160 KiB cells exceed FIRST_PARTY_MAX_RESULT_BYTES (256 KiB aggregate).
    // SQL stays small so D1 payload / physical statement caps still admit it.
    let pad = LARGE_RESULT_PAD_SQL;
    run(
        typed_request(
            "vec-agg-setup",
            exec_plan(&[
                "CREATE TABLE IF NOT EXISTS vec_agg (id INTEGER PRIMARY KEY)",
                "DELETE FROM vec_agg",
            ]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("aggregate setup: {e}"));
    let before = typed_table_fingerprint(&mut *run, "vec-agg-before", "vec_agg").await;
    let err = run(
        typed_request(
            "vec-agg",
            vec![
                typed_stmt(pad.to_string(), DbPlanStatementKind::Select),
                typed_stmt(pad.to_string(), DbPlanStatementKind::Select),
            ],
        ),
        0,
    )
    .await
    .expect_err("aggregate result must exceed maxAtomicResultBytes");
    assert!(
        err.to_lowercase().contains("maxatomicresultbytes")
            || err.to_lowercase().contains("maxresultbytes")
            || err.to_lowercase().contains("body"),
        "aggregate scalar cap must fail closed: {err}"
    );
    let after = typed_table_fingerprint(&mut *run, "vec-agg-after", "vec_agg").await;
    assert_eq!(before, after, "aggregate overflow must not write rows");
}

/// One result whose Cap'n-encoded [`StatementResult`] exceeds `maxResultBytes`.
///
/// Tiny integer cells stay compact on the typed wire, so this uses repeated
/// TEXT cells. The SQL text itself stays well under `maxPayloadBytes`.
async fn typed_wide_numeric_row_cap<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let pad = "x".repeat(3_000);
    let sql = format!(
        "WITH RECURSIVE t(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM t WHERE i < 100) \
         SELECT '{pad}' AS pad FROM t"
    );
    let err = run(typed_request("vec-wide", select_plan(&sql)), 0)
        .await
        .expect_err("wide result must exceed maxResultBytes");
    assert!(
        err.to_lowercase().contains("maxresultbytes")
            || err.to_lowercase().contains("maxatomicresultbytes")
            || err.to_lowercase().contains("body")
            || err.to_lowercase().contains("exceeds"),
        "result-byte budget must fail closed: {err}"
    );
}

/// `Select` reports `rowsAffected = 0`; `Returning` reports returned/affected rows.
async fn typed_rows_affected_by_kind<F, Fut>(run: &mut F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    run(
        typed_request(
            "vec-aff-setup",
            exec_plan(&[
                "CREATE TABLE IF NOT EXISTS vec_aff (id INTEGER PRIMARY KEY)",
                "DELETE FROM vec_aff",
            ]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rowsAffected setup: {e}"));
    run(
        typed_request(
            "vec-aff-ins",
            exec_plan_owned(vec![recursive_insert("vec_aff", "id", 2)]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rowsAffected insert: {e}"));
    let sel = run(
        typed_request(
            "vec-aff-sel",
            select_plan("SELECT id FROM vec_aff ORDER BY id"),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rowsAffected select: {e}"));
    assert_eq!(
        sel.statements[0].rows.len(),
        2,
        "{:?}",
        sel.statements[0].rows
    );
    assert_eq!(
        sel.statements[0].rows_affected, 0,
        "Select rowsAffected must be 0: {:?}",
        sel.statements[0]
    );
    let ins = run(
        typed_request(
            "vec-aff-ret",
            returning_plan_proven("INSERT INTO vec_aff (id) VALUES (99) RETURNING id"),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rowsAffected returning: {e}"));
    assert_eq!(ins.statements[0].rows.len(), 1);
    assert_eq!(
        ins.statements[0].rows_affected, 1,
        "Returning rowsAffected must match returned rows: {:?}",
        ins.statements[0]
    );
}

/// Uncapped one-row `SELECT COUNT/SUM` proving RETURNING overflow did not mutate.
async fn typed_table_fingerprint<F, Fut>(
    run: &mut F,
    operation_id: &str,
    table: &str,
) -> Vec<JsonValue>
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let exec = run(
        typed_request(
            operation_id,
            select_plan(&format!(
                "SELECT COUNT(*) AS n, COALESCE(SUM(id), 0) AS s FROM {table}"
            )),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("{operation_id}: {e}"));
    exec.statements
        .first()
        .map(|s| {
            s.rows
                .iter()
                .map(|row| serde_json::Value::Array(row.values.iter().map(cell_to_json).collect()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// `WITH RECURSIVE` prefix producing `n` rows (`0 .. n-1`) in `t(col)`.
fn recursive_cte(col: &str, n: u32) -> String {
    let last = n.saturating_sub(1);
    format!(
        "WITH RECURSIVE t({col}) AS (SELECT 0 UNION ALL SELECT {col}+1 FROM t WHERE {col} < {last})"
    )
}

/// Portable `WITH … INSERT INTO table SELECT` of `n` rows.
fn recursive_insert(table: &str, col: &str, n: u32) -> String {
    format!(
        "{} INSERT INTO {table} ({col}) SELECT {col} FROM t",
        recursive_cte(col, n)
    )
}

/// Portable `WITH … INSERT … RETURNING` of `n` rows.
fn recursive_insert_returning(table: &str, col: &str, n: u32) -> String {
    format!(
        "{} INSERT INTO {table} ({col}) SELECT {col} FROM t RETURNING {col}",
        recursive_cte(col, n)
    )
}

/// Envelope for a vector plan with no request hash.
fn typed_request(operation_id: &str, statements: Vec<TypedDbStatement>) -> ExecuteRequest {
    ExecuteRequest {
        operation_id: operation_id.into(),
        request_hash: String::new(),
        statements,
        deadline_unix_ms: 0,
    }
}

fn selection_for_kind(kind: DbPlanStatementKind) -> DbResultSelection {
    match kind {
        DbPlanStatementKind::Execute => DbResultSelection::AffectedRows,
        _ => DbResultSelection::Rows,
    }
}

fn typed_stmt(sql: impl Into<String>, kind: DbPlanStatementKind) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.into(),
        parameters: Vec::new(),
        kind,
        max_rows: 0,
        result_selection: selection_for_kind(kind),
    }
}

/// Single Select statement plan.
fn select_plan(sql: &str) -> Vec<TypedDbStatement> {
    stmt_plan(sql, DbPlanStatementKind::Select)
}

/// Single DML `RETURNING` statement plan (`maxRows = 0`, unproven).
fn returning_plan(sql: &str) -> Vec<TypedDbStatement> {
    stmt_plan(sql, DbPlanStatementKind::Returning)
}

/// Proven 1-row DML `RETURNING` (host-IR `maxRows = 1`).
fn returning_plan_proven(sql: &str) -> Vec<TypedDbStatement> {
    let mut plan = returning_plan(sql);
    plan[0].max_rows = 1;
    plan
}

/// Single-statement plan with an explicit wire `kind`.
fn stmt_plan(sql: &str, kind: DbPlanStatementKind) -> Vec<TypedDbStatement> {
    vec![typed_stmt(sql, kind)]
}

/// Execute-only plan from borrowed SQL strings.
fn exec_plan(sqls: &[&str]) -> Vec<TypedDbStatement> {
    exec_plan_owned(sqls.iter().map(|s| (*s).to_string()).collect())
}

/// Execute-only plan from owned SQL strings.
fn exec_plan_owned(sqls: Vec<String>) -> Vec<TypedDbStatement> {
    sqls.into_iter()
        .map(|sql| typed_stmt(sql, DbPlanStatementKind::Execute))
        .collect()
}

/// Two inserts of the same `bookclerk_slots` key.
fn dup_slot_plan(key: &str) -> Vec<TypedDbStatement> {
    vec![
        typed_stmt(
            format!("INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('{key}', 0)"),
            DbPlanStatementKind::Execute,
        ),
        typed_stmt(
            format!("INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('{key}', 1)"),
            DbPlanStatementKind::Execute,
        ),
    ]
}

/// Projects one typed cell to JSON for vector assertions (blobs as `b64:`).
fn cell_to_json(v: &DbValue) -> JsonValue {
    match v {
        DbValue::Null(_) => JsonValue::Null,
        DbValue::Boolean(b) => JsonValue::Bool(*b),
        DbValue::Int64(n) => serde_json::json!(*n),
        DbValue::Float64(n) => serde_json::json!(*n),
        DbValue::Text(s) => JsonValue::String(s.clone()),
        DbValue::Bytes(b) => JsonValue::String(bookclerk_db_exec::bytes_to_b64_string(b)),
    }
}

#[cfg(test)]
mod typed_value_matrix {
    use super::*;
    use bookclerk_db_exec::{AtomicSession, ExecCaps};
    use bookclerk_plugin_abi::{
        DbPlanStatementKind, DbResultSelection, DbType, DbValue, ExecuteRequest, TypedDbStatement,
    };

    async fn mem_db() -> sea_orm::DatabaseConnection {
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("in-memory sqlite")
    }

    fn sqlite_caps() -> ExecCaps {
        ExecCaps::from_capabilities(&DbCapabilities::advertised_sqlite())
    }

    fn select_param(op: &str, sql: &str, param: DbValue) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: op.into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters: vec![param],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        }
    }

    async fn roundtrip(db: &DatabaseConnection, req: &ExecuteRequest) -> DbValue {
        let reply = bookclerk_db_exec::execute_typed_on_session(
            db,
            req,
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect("typed execute");
        reply.statements[0].rows[0].values[0].clone()
    }

    #[tokio::test]
    async fn int64_min_max_roundtrip_via_select_param() {
        let db = mem_db().await;
        for (label, value) in [
            ("min", DbValue::Int64(i64::MIN)),
            ("max", DbValue::Int64(i64::MAX)),
        ] {
            let got = roundtrip(&db, &select_param(label, "SELECT ? AS v", value.clone())).await;
            assert_eq!(got, value, "{label}");
        }
    }

    #[tokio::test]
    async fn text_utf8_roundtrip_rejects_embedded_nul() {
        let db = mem_db().await;
        let utf8 = DbValue::Text("café 日本語".into());
        let got = roundtrip(&db, &select_param("utf8", "SELECT ? AS v", utf8.clone())).await;
        assert_eq!(got, utf8);

        let nul = DbValue::Text("before\u{0}after".into());
        let err = bookclerk_db_exec::execute_typed_on_session(
            &db,
            &select_param("nul", "SELECT ? AS v", nul),
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect_err("TEXT U+0000 must fail closed");
        assert!(
            err.to_string().contains("U+0000"),
            "expected portable TEXT reject, got {err}"
        );

        let blob = DbValue::Bytes(b"before\0after".to_vec());
        let got = roundtrip(&db, &select_param("blob", "SELECT ? AS v", blob.clone())).await;
        assert_eq!(got, blob, "BYTES may contain 0x00");
    }

    #[test]
    fn text_nul_rejected_by_typed_decoder() {
        use bookclerk_db_exec::{db_value_from_sea, db_value_to_sea};
        use sea_orm::Value as SeaValue;
        let err = db_value_from_sea(&SeaValue::String(Some("a\0b".into()))).unwrap_err();
        assert!(
            err.contains("U+0000"),
            "expected TEXT NUL rejection, got {err}"
        );
        let blob = db_value_from_sea(&db_value_to_sea(&DbValue::Bytes(vec![0, 1]))).unwrap();
        assert_eq!(blob, DbValue::Bytes(vec![0, 1]));
    }

    #[tokio::test]
    async fn bytes_roundtrip_via_select_param() {
        let db = mem_db().await;
        let bytes = DbValue::Bytes(vec![0, 127, 255, 42]);
        let got = roundtrip(&db, &select_param("bytes", "SELECT ? AS v", bytes.clone())).await;
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn typed_null_carries_expected_type() {
        let db = mem_db().await;
        let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                "CREATE TABLE IF NOT EXISTS typed_null_probe (v INTEGER)",
            ),
        )
        .await
        .unwrap();
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                "INSERT INTO typed_null_probe (v) VALUES (NULL)",
            ),
        )
        .await
        .unwrap();
        let req = ExecuteRequest {
            operation_id: "null-int".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT v FROM typed_null_probe".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let mut catalog = crate::migrations::host_sql_type_env();
        bookclerk_plugin_abi::apply_schema_sql_to_env(
            &mut catalog,
            "CREATE TABLE typed_null_probe (v INTEGER)",
        );
        let reply = bookclerk_db_exec::execute_typed_on_session(
            &db,
            &req,
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None).with_type_env(catalog),
        )
        .await
        .expect("typed execute");
        assert_eq!(reply.statements[0].columns[0].db_type, DbType::Int64);
        assert!(matches!(
            reply.statements[0].rows[0].values[0],
            DbValue::Null(DbType::Int64)
        ));
    }

    #[tokio::test]
    async fn finite_float_roundtrip_via_select_param() {
        let db = mem_db().await;
        let finite = DbValue::Float64(1.25);
        let got = roundtrip(
            &db,
            &select_param("finite", "SELECT ? AS v", finite.clone()),
        )
        .await;
        assert_eq!(got, finite);
    }

    #[test]
    fn non_finite_float_rejected_by_typed_decoder() {
        use bookclerk_db_exec::db_value_from_sea;
        use sea_orm::Value as SeaValue;
        for value in [
            SeaValue::Double(Some(f64::NAN)),
            SeaValue::Double(Some(f64::INFINITY)),
        ] {
            let err = db_value_from_sea(&value).unwrap_err();
            assert!(
                err.contains("finite"),
                "expected non-finite rejection for {value:?}, got {err}"
            );
        }
    }

    #[tokio::test]
    async fn failed_statement_rolls_back_first_insert() {
        let db = mem_db().await;
        let req = ExecuteRequest {
            operation_id: "typed-rb".into(),
            request_hash: String::new(),
            statements: vec![
                TypedDbStatement {
                    sql: "INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('typed-rb', 0)"
                        .into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
                TypedDbStatement {
                    sql: "INSERT INTO bookclerk_slots (slot_key, bump) VALUES ('typed-rb', 1)"
                        .into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
            ],
            deadline_unix_ms: 0,
        };
        let err = bookclerk_db_exec::execute_typed_on_session(
            &db,
            &req,
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect_err("duplicate key must fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique") || msg.contains("constraint"),
            "{err}"
        );

        let check = ExecuteRequest {
            operation_id: "typed-rb-check".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT slot_key FROM bookclerk_slots WHERE slot_key = 'typed-rb'".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let reply = bookclerk_db_exec::execute_typed_on_session(
            &db,
            &check,
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect("rollback check");
        assert!(
            reply.statements[0].rows.is_empty(),
            "failed atomic batch must roll back the first insert"
        );
    }
}
