//! Backend-parameterized SQL-contract vectors shared by sqlite, postgres, and D1.

use std::future::Future;

use super::host_ir::{DbAtomicPlan, DbAtomicRequest, DbPlanExecResult, DbPlanStatement};
use bookclerk_db_exec::ExecCaps;
use bookclerk_plugin_abi::{DbCapabilities, DbPlanStatementKind};
use sea_orm::DatabaseConnection;
use serde_json::Value as JsonValue;

use super::{compile_named_request, interpret_exec};
use crate::atomic_ops::{atomic_status, DbAtomicParams};

/// Injected `maxResultRows` for conn-vector row-cap cases (sqlite / postgres).
pub const CONTRACT_VECTOR_ROW_CAP: u32 = 5;

/// One ~160 KiB TEXT cell without embedding the payload in statement text.
///
/// D1 `maxPayloadBytes` and the 100 KiB physical SQL limit cannot admit a
/// 150 KiB literal. Doubling CTE: `10 * 2^14 = 163840` bytes. Two such
/// statements exceed [`bookclerk_plugin_abi::FIRST_PARTY_MAX_RESULT_BYTES`].
pub(super) const LARGE_RESULT_PAD_SQL: &str = "WITH RECURSIVE t(n, s) AS (\
 SELECT 1, 'aaaaaaaaaa' \
 UNION ALL \
 SELECT n + 1, s || s FROM t WHERE n < 15\
) SELECT s AS pad FROM t WHERE n = 15";

/// Runs the contract suite on a SeaORM connection (sqlite / postgres).
///
/// # Panics
///
/// Panics when a vector fails.
pub async fn run_conn_vectors(db: &DatabaseConnection, connect: DbCapabilities, timing: &str) {
    let db = db.clone();
    let timing = timing.to_string();
    let connect_for_run = connect.clone();
    run_contract_vectors(connect, CONTRACT_VECTOR_ROW_CAP, move |req, cap| {
        let db = db.clone();
        let timing = timing.clone();
        let connect = connect_for_run.clone();
        async move {
            let plan = req.plan.expect("vector plan");
            let mut caps = ExecCaps::from_capabilities(&connect);
            if cap > 0 {
                caps.max_result_rows = cap;
            }
            bookclerk_db_exec::execute_statements_on(&db, &plan, &req.operation_id, &timing, caps)
                .await
                .map_err(|e| e.to_string())
        }
    })
    .await;
}

/// Runs the identical contract suite through a guest atomic callback.
///
/// `advertised_cap` is the adapter's `maxResultRows`. The harness does not wrap
/// SQL or post-reject oversized results.
///
/// # Panics
///
/// Panics when a vector fails.
pub async fn run_request_vectors<F, Fut, E>(
    connect: DbCapabilities,
    advertised_cap: u32,
    mut run: F,
) where
    F: FnMut(DbAtomicRequest) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, E>>,
    E: std::fmt::Display,
{
    run_contract_vectors(connect, advertised_cap, move |req, _cap| {
        let fut = run(req);
        async move { fut.await.map_err(|e| e.to_string()) }
    })
    .await;
}

/// Hash-conflict, rollback, row-cap, RETURNING-cap, replay, and unique vectors.
///
/// `run` receives `(request, max_result_rows)` where `0` means unlimited.
/// `row_cap` is the advertised or test-injected result bound used to seed
/// overflow cases (`row_cap + 1` rows).
///
/// # Panics
///
/// Panics when a vector fails.
pub async fn run_contract_vectors<F, Fut>(_connect: DbCapabilities, row_cap: u32, mut run: F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    commit_and_replay(&mut run).await;
    hash_conflict(&mut run).await;
    password_replay_and_hash_conflict(&mut run).await;
    unique_generic_insert_fails(&mut run).await;
    failed_statement_rolls_back(&mut run).await;
    row_cap_select(&mut run, row_cap).await;
    returning_insert_cap(&mut run, row_cap).await;
    returning_update_cap(&mut run, row_cap).await;
    returning_delete_cap(&mut run, row_cap).await;
    rows_affected_by_kind(&mut run).await;
    values_returning_cap(&mut run, row_cap).await;
    aggregate_scalar_cap(&mut run).await;
    wide_numeric_row_cap(&mut run).await;
}

/// Enqueue once, then replay the same `operationId`.
async fn commit_and_replay<F, Fut>(run: &mut F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
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
    let first = run(compiled.clone().into_request("vec-enq"), 0)
        .await
        .unwrap_or_else(|e| panic!("first atomic: {e}"));
    let interpreted = interpret_exec(&compiled.plan, &first, &compiled.expected_hash);
    assert_eq!(interpreted.status, atomic_status::OK);
    let replay = run(compiled.clone().into_request("vec-enq"), 0)
        .await
        .unwrap_or_else(|e| panic!("replay: {e}"));
    let replayed = interpret_exec(&compiled.plan, &replay, &compiled.expected_hash);
    assert!(replayed.replayed, "same operationId must replay");
}

/// Same `operationId` with a different request hash is an idempotency conflict.
async fn hash_conflict<F, Fut>(run: &mut F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
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
    let exec = run(first.clone().into_request("vec-conflict"), 0)
        .await
        .unwrap_or_else(|e| panic!("conflict seed: {e}"));
    assert_eq!(
        interpret_exec(&first.plan, &exec, &first.expected_hash).status,
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
    let snapshot = job_payloads(&mut *run).await;
    let replay = run(other.clone().into_request("vec-conflict"), 0)
        .await
        .unwrap_or_else(|e| panic!("conflict other plan: {e}"));
    let result = interpret_exec(&other.plan, &replay, &other.expected_hash);
    assert_eq!(result.status, atomic_status::IDEMPOTENCY_CONFLICT);
    let after = job_payloads(&mut *run).await;
    assert_eq!(
        snapshot, after,
        "mismatched-hash plan must not mutate domain state"
    );
}

/// Snapshot of `jobs.payload` used to prove a mismatched-hash plan did not mutate.
async fn job_payloads<F, Fut>(run: &mut F) -> Vec<JsonValue>
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let exec = run(
        request(
            "vec-conflict-snap",
            select_plan("SELECT payload FROM jobs ORDER BY id"),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("job snapshot: {e}"));
    exec.statements
        .first()
        .map(|s| s.rows.clone())
        .unwrap_or_default()
}

/// Outcome-first mutating op: exact replay and mismatched hash leave the user row unchanged.
async fn password_replay_and_hash_conflict<F, Fut>(run: &mut F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    run(
        request(
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
    let exec = run(first.clone().into_request("vec-pw"), 0)
        .await
        .unwrap_or_else(|e| panic!("password first: {e}"));
    assert_eq!(
        interpret_exec(&first.plan, &exec, &first.expected_hash).status,
        atomic_status::OK
    );
    let snapshot = user_password_row(&mut *run).await;
    let replay = run(first.clone().into_request("vec-pw"), 0)
        .await
        .unwrap_or_else(|e| panic!("password replay: {e}"));
    assert!(
        interpret_exec(&first.plan, &replay, &first.expected_hash).replayed,
        "exact password replay must replay"
    );
    assert_eq!(
        snapshot,
        user_password_row(&mut *run).await,
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
    let conflict = run(other.clone().into_request("vec-pw"), 0)
        .await
        .unwrap_or_else(|e| panic!("password mismatch: {e}"));
    assert_eq!(
        interpret_exec(&other.plan, &conflict, &other.expected_hash).status,
        atomic_status::IDEMPOTENCY_CONFLICT
    );
    assert_eq!(
        snapshot,
        user_password_row(&mut *run).await,
        "mismatched hash must leave the user row unchanged"
    );
}

/// Snapshot of user `9001` password bytes used by replay / hash-conflict checks.
async fn user_password_row<F, Fut>(run: &mut F) -> Vec<JsonValue>
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let exec = run(
        request(
            "vec-pw-snap",
            select_plan("SELECT password_hash, security_version FROM users WHERE id = 9001"),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("user snapshot: {e}"));
    exec.statements
        .first()
        .map(|s| s.rows.clone())
        .unwrap_or_default()
}

/// Duplicate primary-key inserts fail closed (engine unique / 23505).
async fn unique_generic_insert_fails<F, Fut>(run: &mut F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let err = run(request("vec-dup", dup_slot_plan("vec-dup")), 0)
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
async fn failed_statement_rolls_back<F, Fut>(run: &mut F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    assert!(run(request("vec-rb", dup_slot_plan("rb-vec")), 0)
        .await
        .is_err());
    let check = run(
        request(
            "vec-rb-check",
            select_plan("SELECT slot_key FROM db_serialization_slots WHERE slot_key = 'rb-vec'"),
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
async fn row_cap_select<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        request(
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
        request(
            "vec-cap-ins",
            exec_plan_owned(vec![recursive_insert("vec_rowcap", "x", n)]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rowcap insert: {e}"));
    let err = run(
        request("vec-cap-sel", select_plan("SELECT x FROM vec_rowcap")),
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
async fn returning_insert_cap<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        request(
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
    let before = table_fingerprint(&mut *run, "vec-ret-ins-before", "vec_ret_ins").await;
    let err = run(request("vec-ret-ins", returning_plan(&sql)), row_cap)
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
    let after = table_fingerprint(&mut *run, "vec-ret-ins-after", "vec_ret_ins").await;
    assert_eq!(
        before, after,
        "failed INSERT RETURNING must not leave rows: before={before:?} after={after:?}"
    );
}

/// `UPDATE … RETURNING` honors the row cap without a subquery rewrite.
async fn returning_update_cap<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        request(
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
    let before = table_fingerprint(&mut *run, "vec-ret-upd-before", "vec_ret_upd").await;
    let err = run(
        request(
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
    let after = table_fingerprint(&mut *run, "vec-ret-upd-after", "vec_ret_upd").await;
    assert_eq!(
        before, after,
        "failed UPDATE RETURNING must leave the table unchanged"
    );
}

/// `DELETE … RETURNING` honors the row cap without a subquery rewrite.
async fn returning_delete_cap<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        request(
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
    let before = table_fingerprint(&mut *run, "vec-ret-del-before", "vec_ret_del").await;
    let err = run(
        request(
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
    let after = table_fingerprint(&mut *run, "vec-ret-del-after", "vec_ret_del").await;
    assert_eq!(
        before, after,
        "failed DELETE RETURNING must leave the table unchanged"
    );
}

/// Multi-tuple `INSERT … VALUES (),( ) … RETURNING` is not a 1-row proof.
async fn values_returning_cap<F, Fut>(run: &mut F, row_cap: u32)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let n = row_cap.saturating_add(1);
    run(
        request(
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
    let before = table_fingerprint(&mut *run, "vec-val-before", "vec_val_ins").await;
    let err = run(request("vec-val-ins", returning_plan(&sql)), row_cap)
        .await
        .expect_err("multi-tuple VALUES RETURNING must fail");
    assert!(
        err.to_lowercase().contains("maxresultrows")
            || err.to_lowercase().contains("proven")
            || err.to_lowercase().contains("values"),
        "VALUES RETURNING cap must fail closed: {err}"
    );
    let after = table_fingerprint(&mut *run, "vec-val-after", "vec_val_ins").await;
    assert_eq!(
        before, after,
        "failed VALUES RETURNING must not leave rows: before={before:?} after={after:?}"
    );
}

/// Two individually-valid row-producing statements whose aggregate exceeds the RPC scalar.
async fn aggregate_scalar_cap<F, Fut>(run: &mut F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    // Two ~160 KiB cells exceed FIRST_PARTY_MAX_RESULT_BYTES (256 KiB aggregate).
    // SQL stays small so D1 payload / physical statement caps still admit it.
    let pad = LARGE_RESULT_PAD_SQL;
    run(
        request(
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
    let before = table_fingerprint(&mut *run, "vec-agg-before", "vec_agg").await;
    let err = run(
        request(
            "vec-agg",
            DbAtomicPlan {
                statements: vec![
                    DbPlanStatement::new(pad, vec![], DbPlanStatementKind::Select),
                    DbPlanStatement::new(pad, vec![], DbPlanStatementKind::Select),
                ],
                outcome_index: 0,
                payload_index: None,
                prior_receipt_index: None,
                receipt_select_index: None,
            },
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
    let after = table_fingerprint(&mut *run, "vec-agg-after", "vec_agg").await;
    assert_eq!(before, after, "aggregate overflow must not write rows");
}

/// One result whose encoded JSON exceeds `maxResultBytes` with tiny numeric cells.
async fn wide_numeric_row_cap<F, Fut>(run: &mut F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let pad = "x".repeat(50);
    let cols: Vec<String> = (0..40).map(|i| format!("t.i AS c{i:02}_{pad}")).collect();
    let sql = format!(
        "WITH RECURSIVE t(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM t WHERE i < 120) \
         SELECT {} FROM t",
        cols.join(", ")
    );
    let err = run(request("vec-wide", select_plan(&sql)), 0)
        .await
        .expect_err("wide numeric result must exceed maxResultBytes");
    assert!(
        err.to_lowercase().contains("maxresultbytes")
            || err.to_lowercase().contains("maxatomicresultbytes")
            || err.to_lowercase().contains("body")
            || err.to_lowercase().contains("exceeds"),
        "wide-row JSON budget must fail closed: {err}"
    );
}

/// `Select` reports `rowsAffected = 0`; `Returning` reports returned/affected rows.
async fn rows_affected_by_kind<F, Fut>(run: &mut F)
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    run(
        request(
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
        request(
            "vec-aff-ins",
            exec_plan_owned(vec![recursive_insert("vec_aff", "id", 2)]),
        ),
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("rowsAffected insert: {e}"));
    let sel = run(
        request(
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
        request(
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
async fn table_fingerprint<F, Fut>(run: &mut F, operation_id: &str, table: &str) -> Vec<JsonValue>
where
    F: FnMut(DbAtomicRequest, u32) -> Fut,
    Fut: Future<Output = Result<DbPlanExecResult, String>>,
{
    let exec = run(
        request(
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
        .map(|s| s.rows.clone())
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
fn request(operation_id: &str, plan: DbAtomicPlan) -> DbAtomicRequest {
    DbAtomicRequest {
        operation_id: operation_id.into(),
        request_hash: None,
        plan: Some(plan),
        deadline_unix_ms: None,
    }
}

/// Single Select statement plan.
fn select_plan(sql: &str) -> DbAtomicPlan {
    stmt_plan(sql, DbPlanStatementKind::Select)
}

/// Single DML `RETURNING` statement plan (`maxRows = 0`, unproven).
fn returning_plan(sql: &str) -> DbAtomicPlan {
    stmt_plan(sql, DbPlanStatementKind::Returning)
}

/// Proven 1-row DML `RETURNING` (host-IR `maxRows = 1`).
fn returning_plan_proven(sql: &str) -> DbAtomicPlan {
    let mut plan = returning_plan(sql);
    plan.statements[0].max_rows = 1;
    plan
}

/// Single-statement plan with an explicit wire `kind`.
fn stmt_plan(sql: &str, kind: DbPlanStatementKind) -> DbAtomicPlan {
    DbAtomicPlan {
        statements: vec![DbPlanStatement::new(sql, vec![], kind)],
        outcome_index: 0,
        payload_index: None,
        prior_receipt_index: None,
        receipt_select_index: None,
    }
}

/// Execute-only plan from borrowed SQL strings.
fn exec_plan(sqls: &[&str]) -> DbAtomicPlan {
    exec_plan_owned(sqls.iter().map(|s| (*s).to_string()).collect())
}

/// Execute-only plan from owned SQL strings.
fn exec_plan_owned(sqls: Vec<String>) -> DbAtomicPlan {
    DbAtomicPlan {
        statements: sqls
            .into_iter()
            .map(|sql| DbPlanStatement::new(sql, vec![], DbPlanStatementKind::Execute))
            .collect(),
        outcome_index: 0,
        payload_index: None,
        prior_receipt_index: None,
        receipt_select_index: None,
    }
}

/// Two inserts of the same `db_serialization_slots` key.
fn dup_slot_plan(key: &str) -> DbAtomicPlan {
    DbAtomicPlan {
        statements: vec![
            DbPlanStatement::new(
                format!("INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('{key}', 0)"),
                vec![],
                DbPlanStatementKind::Execute,
            ),
            DbPlanStatement::new(
                format!("INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('{key}', 1)"),
                vec![],
                DbPlanStatementKind::Execute,
            ),
        ],
        outcome_index: 0,
        payload_index: None,
        prior_receipt_index: None,
        receipt_select_index: None,
    }
}
