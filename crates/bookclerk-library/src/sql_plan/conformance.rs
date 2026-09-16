//! Shared SQL-plan conformance vectors (SQLite in-process).
//!
//! **Admission (#178):** database plugins must pass
//! [`super::typed_vectors::run_typed_request_vectors`] with a callback that
//! executes native [`ExecuteRequest`] / [`ExecuteReply`] (Cap'n Proto on the wire).

use crate::atomic_ops::{atomic_status, DbAtomicParams};

use super::{
    compile_named_request, execute_compiled_on, execute_typed_on_session, AtomicSession,
    CONTRACT_VECTOR_ROW_CAP,
};
use bookclerk_plugin_abi::DbCapabilities;
use std::path::PathBuf;
use std::process::Command;

/// True when Postgres conformance tests should run (URL present).
///
/// `BOOKCLERK_REQUIRE_POSTGRES_TESTS=1` without a URL is a hard failure so CI
/// cannot skip these cases.
fn postgres_conformance_enabled() -> bool {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if url.is_some() {
        return true;
    }
    assert!(
        std::env::var("BOOKCLERK_REQUIRE_POSTGRES_TESTS")
            .ok()
            .as_deref()
            != Some("1"),
        "BOOKCLERK_TEST_POSTGRES_URL is required when BOOKCLERK_REQUIRE_POSTGRES_TESTS=1"
    );
    false
}

/// Opens an in-memory sqlite library and applies migrations.
async fn mem_db() -> sea_orm::DatabaseConnection {
    bookclerk_plugin_database_sqlite::open_memory()
        .await
        .expect("in-memory sqlite")
}

struct NamedOp {
    id: &'static str,
    params: DbAtomicParams,
}

fn named(id: &'static str, params: DbAtomicParams) -> NamedOp {
    NamedOp { id, params }
}

fn typed_stmt(
    sql: impl Into<String>,
    kind: bookclerk_plugin_abi::DbPlanStatementKind,
) -> bookclerk_plugin_abi::TypedDbStatement {
    use bookclerk_plugin_abi::{DbResultSelection, TypedDbStatement};
    TypedDbStatement {
        sql: sql.into(),
        parameters: vec![],
        kind,
        max_rows: 0,
        result_selection: if kind == bookclerk_plugin_abi::DbPlanStatementKind::Execute {
            DbResultSelection::AffectedRows
        } else {
            DbResultSelection::Rows
        },
    }
}

fn typed_req(
    op: &str,
    statements: Vec<bookclerk_plugin_abi::TypedDbStatement>,
) -> bookclerk_plugin_abi::ExecuteRequest {
    bookclerk_plugin_abi::ExecuteRequest {
        operation_id: op.into(),
        request_hash: String::new(),
        statements,
        deadline_unix_ms: 0,
    }
}

#[tokio::test]
async fn typed_shared_vectors_on_sqlite() {
    let db = mem_db().await;
    super::typed_vectors::run_typed_conn_vectors(
        &db,
        DbCapabilities::advertised_sqlite(),
        "sqlite_txn",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn typed_shared_vectors_on_postgres() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    super::typed_vectors::run_typed_conn_vectors(
        &db,
        DbCapabilities::advertised_postgres(),
        "postgres_txn",
    )
    .await;
}

#[tokio::test]
async fn sqlite_recursive_cte_honors_deadline() {
    let db = mem_db().await;
    let deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
        .saturating_add(80);
    let plan = vec![{
        let mut s = typed_stmt("WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM t WHERE x < 200000000) SELECT COUNT(*) AS n FROM t", bookclerk_plugin_abi::DbPlanStatementKind::Returning);
        s.max_rows = 0;
        s
    }];
    let err = super::execute_typed_on_session(
        &db,
        &typed_req("op-deadline", plan),
        "sqlite_txn",
        0,
        super::AtomicSession::from_deadline(Some(deadline)),
    )
    .await
    .unwrap_err();
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("deadline") || msg.contains("interrupt") || msg.contains("cancel"),
        "long statement must honor deadline, got {err}"
    );
}

#[tokio::test]
async fn sqlite_query_stops_after_cap_plus_one() {
    let db = mem_db().await;
    let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
    sea_orm::ConnectionTrait::execute_raw(
        &db,
        sea_orm::Statement::from_string(
            backend,
            "CREATE TABLE IF NOT EXISTS rowcap_probe (x INTEGER)",
        ),
    )
    .await
    .ok();
    for i in 0..50 {
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_sql_and_values(
                backend,
                "INSERT INTO rowcap_probe (x) VALUES (?)",
                [i.into()],
            ),
        )
        .await
        .unwrap();
    }
    let plan = vec![{
        let mut s = typed_stmt(
            "SELECT x FROM rowcap_probe",
            bookclerk_plugin_abi::DbPlanStatementKind::Returning,
        );
        s.max_rows = 0;
        s
    }];
    let err = super::execute_typed_on(&db, &typed_req("op-early", plan), "sqlite_txn", 5)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("maxResultRows"), "{err}");
    let seen = crate::query_rows_seen();
    assert!(
        seen <= 6,
        "must stop after cap+1 materialized rows, saw {seen}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_attempts_keep_independent_deadlines_and_caps() {
    let db_deadline = mem_db().await;
    let db_cap = mem_db().await;
    let backend = sea_orm::ConnectionTrait::get_database_backend(&db_cap);
    sea_orm::ConnectionTrait::execute_raw(
        &db_cap,
        sea_orm::Statement::from_string(
            backend,
            "CREATE TABLE IF NOT EXISTS rowcap_probe (x INTEGER)",
        ),
    )
    .await
    .ok();
    for i in 0..50 {
        sea_orm::ConnectionTrait::execute_raw(
            &db_cap,
            sea_orm::Statement::from_sql_and_values(
                backend,
                "INSERT INTO rowcap_probe (x) VALUES (?)",
                [i.into()],
            ),
        )
        .await
        .unwrap();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let cte = vec![{
        let mut s = typed_stmt("WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM t WHERE x < 200000000) SELECT COUNT(*) AS n FROM t", bookclerk_plugin_abi::DbPlanStatementKind::Returning);
        s.max_rows = 0;
        s
    }];
    let select = vec![{
        let mut s = typed_stmt(
            "SELECT x FROM rowcap_probe",
            bookclerk_plugin_abi::DbPlanStatementKind::Returning,
        );
        s.max_rows = 0;
        s
    }];
    let deadline_req = typed_req("op-conc-deadline", cte);
    let cap_req = typed_req("op-conc-cap", select);
    let deadline = execute_typed_on_session(
        &db_deadline,
        &deadline_req,
        "sqlite_txn",
        0,
        AtomicSession::from_deadline(Some(now.saturating_add(80))),
    );
    let cap = execute_typed_on_session(
        &db_cap,
        &cap_req,
        "sqlite_txn",
        5,
        AtomicSession::from_deadline(Some(now.saturating_add(60_000))),
    );
    let (deadline, cap) = tokio::join!(
        tokio::time::timeout(std::time::Duration::from_secs(3), deadline),
        cap
    );
    let deadline_err = deadline
        .expect("deadline attempt must not hang if its budget is overwritten")
        .unwrap_err();
    let dmsg = deadline_err.to_string().to_lowercase();
    assert!(
        dmsg.contains("deadline") || dmsg.contains("interrupt") || dmsg.contains("cancel"),
        "long CTE must honor its own deadline, got {deadline_err}"
    );
    let cap_err = cap.unwrap_err();
    assert!(
        cap_err.to_string().contains("maxResultRows"),
        "capped SELECT must honor its own cap, got {cap_err}"
    );
}

#[tokio::test]
async fn plan_commit_inserts_receipt() {
    let db = mem_db().await;
    let now = "2024-06-01T00:00:00Z";
    let req = named(
        "conf-enq",
        DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"a"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
    );
    let compiled = compile_named_request(req.id, &req.params, now).unwrap();
    let result = execute_compiled_on(&db, compiled.clone(), "sqlite_txn")
        .await
        .unwrap();
    assert_eq!(result.status, atomic_status::OK);
    assert!(!result.replayed);
    let replay = execute_compiled_on(&db, compiled.clone(), "sqlite_txn")
        .await
        .unwrap();
    assert!(replay.replayed, "same operationId must replay the receipt");
    assert_eq!(replay.status, atomic_status::OK);
}

#[tokio::test]
async fn plan_hash_conflict_is_idempotency_conflict() {
    let db = mem_db().await;
    let now = "2024-06-01T00:00:00Z";
    let first = named(
        "conf-conflict",
        DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"a"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
    );
    let compiled = compile_named_request(first.id, &first.params, now).unwrap();
    execute_compiled_on(&db, compiled.clone(), "sqlite_txn")
        .await
        .unwrap();
    let second = named(
        "conf-conflict",
        DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"other"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
    );
    let other = compile_named_request(second.id, &second.params, now).unwrap();
    let result = execute_compiled_on(&db, other, "sqlite_txn").await.unwrap();
    assert_eq!(result.status, atomic_status::IDEMPOTENCY_CONFLICT);
}

#[tokio::test]
async fn unique_constraint_on_generic_insert_is_engine_error() {
    let db = mem_db().await;
    let plan = vec![
        {
            let mut s = typed_stmt(
                "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('dup', 0)",
                bookclerk_plugin_abi::DbPlanStatementKind::Execute,
            );
            s.max_rows = 0;
            s
        },
        {
            let mut s = typed_stmt(
                "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('dup', 1)",
                bookclerk_plugin_abi::DbPlanStatementKind::Execute,
            );
            s.max_rows = 0;
            s
        },
    ];
    let err = super::execute_typed_on(&db, &typed_req("op-unique", plan.clone()), "sqlite_txn", 0)
        .await
        .unwrap_err();
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("unique") || msg.contains("constraint"),
        "{err}"
    );
}

#[tokio::test]
async fn failed_statement_rolls_back_earlier_inserts() {
    let db = mem_db().await;
    let plan = vec![
        {
            let mut s = typed_stmt(
                "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('rb', 0)",
                bookclerk_plugin_abi::DbPlanStatementKind::Execute,
            );
            s.max_rows = 0;
            s
        },
        {
            let mut s = typed_stmt(
                "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('rb', 1)",
                bookclerk_plugin_abi::DbPlanStatementKind::Execute,
            );
            s.max_rows = 0;
            s
        },
    ];
    assert!(
        super::execute_typed_on(&db, &typed_req("op-rb", plan.clone()), "sqlite_txn", 0)
            .await
            .is_err()
    );
    let rows: Vec<sea_orm::QueryResult> = sea_orm::ConnectionTrait::query_all_raw(
        &db,
        sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT bump FROM db_serialization_slots WHERE slot_key = 'rb'",
        ),
    )
    .await
    .unwrap();
    assert!(
        rows.is_empty(),
        "unique failure must roll back the first INSERT"
    );
}

#[tokio::test]
async fn conditional_update_zero_rows_is_ok_execute() {
    let db = mem_db().await;
    let plan = vec![
        {
            let mut s = typed_stmt(
                "UPDATE db_serialization_slots SET bump = 1 WHERE slot_key = 'missing'",
                bookclerk_plugin_abi::DbPlanStatementKind::Execute,
            );
            s.max_rows = 0;
            s
        },
        {
            let mut s = typed_stmt(
                "SELECT 'ok' AS status",
                bookclerk_plugin_abi::DbPlanStatementKind::Returning,
            );
            s.max_rows = 0;
            s
        },
    ];
    let reply = super::execute_typed_on(&db, &typed_req("op-cond", plan.clone()), "sqlite_txn", 0)
        .await
        .unwrap();
    let compiled = super::CompiledAtomic {
        request: typed_req("op-cond", plan),
        selection: super::AtomicSelection {
            outcome_index: 1,
            payload_index: None,
            prior_receipt_index: None,
            receipt_select_index: None,
        },
        expected_hash: "hash".into(),
    };
    let result = super::interpret_typed_exec(&compiled, &reply, "hash");
    assert_eq!(result.status, atomic_status::OK);
}

#[tokio::test]
async fn timing_receipt_shape_is_uniform() {
    let db = mem_db().await;
    let now = "2024-06-01T00:00:00Z";
    let req = named(
        "conf-timing",
        DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"a"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
    );
    let compiled = compile_named_request(req.id, &req.params, now).unwrap();
    let result = execute_compiled_on(&db, compiled.clone(), "sqlite_txn")
        .await
        .unwrap();
    let timing = result.timing.expect("timing");
    assert!(timing.attempt_elapsed_us > 0);
    assert_eq!(timing.db_timing_source, "sqlite_txn");
    assert!(timing.db_execution_us > 0);
}

#[tokio::test]
async fn serialization_slot_bump_is_monotonic() {
    let db = mem_db().await;
    crate::sql_plan::lock_serialization_slot(&db, "job-queue")
        .await
        .unwrap();
    crate::sql_plan::lock_serialization_slot(&db, "job-queue")
        .await
        .unwrap();
    let rows = sea_orm::ConnectionTrait::query_all_raw(
        &db,
        sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT bump FROM db_serialization_slots WHERE slot_key = 'job-queue'",
        ),
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    let bump = rows[0].try_get_by_index::<i64>(0).unwrap();
    assert!(bump >= 2, "two locks must bump twice, got {bump}");
}

#[test]
fn postgres_renderer_lowers_canonical_placeholders() {
    let now = "2024-06-01T00:00:00Z";
    let req = named(
        "pg-render",
        DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"pg"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
    );
    let compiled = compile_named_request(req.id, &req.params, now).unwrap();
    let joined = compiled
        .request
        .statements
        .iter()
        .map(|s| s.sql.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains("$1"),
        "host compiler must emit canonical SQL, not $n:\n{joined}"
    );
    let mut lowered_any = false;
    for stmt in &compiled.request.statements {
        if stmt.sql.contains('?') {
            let lowered = bookclerk_db_exec::lower_canonical_to_postgres(&stmt.sql);
            assert!(
                lowered.contains('$'),
                "adapter lowering must produce $n binds:\n{lowered}"
            );
            assert!(
                !lowered.contains('?'),
                "adapter lowering must not leave ? placeholders:\n{lowered}"
            );
            lowered_any = true;
        }
    }
    assert!(lowered_any, "enqueue plan must contain binds:\n{joined}");
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_plan_receipt_replay() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    let now = "2024-06-01T00:00:00Z";
    let req = named(
        "pg-conf-enq",
        DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: r#"{"v":1,"account":"pg"}"#.into(),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
    );
    let compiled = compile_named_request(req.id, &req.params, now).unwrap();
    let first = execute_compiled_on(&db, compiled.clone(), "postgres_txn")
        .await
        .unwrap();
    assert_eq!(first.status, atomic_status::OK);
    let replay = execute_compiled_on(&db, compiled.clone(), "postgres_txn")
        .await
        .unwrap();
    assert!(replay.replayed);
}

/// Disposable Postgres so claim cannot see leftover `jobs` rows.
async fn postgres_migrated_db() -> sea_orm::DatabaseConnection {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").expect("postgres url");
    let db_name = format!("plan_{}", uuid::Uuid::new_v4().as_simple());
    let admin = sea_orm::Database::connect(url.as_str())
        .await
        .expect("connect to BOOKCLERK_TEST_POSTGRES_URL");
    let backend = sea_orm::ConnectionTrait::get_database_backend(&admin);
    sea_orm::ConnectionTrait::execute_raw(
        &admin,
        sea_orm::Statement::from_string(backend, format!("CREATE DATABASE {db_name}")),
    )
    .await
    .expect("create disposable postgres database");
    let (base, query) = match url.split_once('?') {
        Some((base, q)) => (base, Some(q)),
        None => (url.as_str(), None),
    };
    let trimmed = base.trim_end_matches('/');
    let slash = trimmed
        .rfind('/')
        .expect("BOOKCLERK_TEST_POSTGRES_URL must include a database path");
    let db_url = match query {
        Some(q) => format!("{}/{db_name}?{q}", &trimmed[..slash]),
        None => format!("{}/{db_name}", &trimmed[..slash]),
    };
    let db = sea_orm::Database::connect(&db_url)
        .await
        .expect("connect to disposable postgres database");
    crate::apply_host_schema(&db, crate::HostSchemaKind::RowMarker)
        .await
        .expect("host-applied postgres schema");
    db
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_claim_malformed_json_is_quarantined() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
    for (id, payload, priority) in [
        ("bad-json", "{not-json", 10_i64),
        ("good", r#"{"v":1}"#, 0_i64),
    ] {
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_sql_and_values(
                backend,
                "INSERT INTO jobs (\
                    id, kind, state, priority, resource_class, payload, progress, \
                    attempt_count, max_attempts, run_after, lease_owner, lease_expires_at, \
                    dedup_key, error_kind, error_message, cancel_requested, \
                    created_at, updated_at, started_at, finished_at, lease_generation\
                 ) VALUES ($1, 'scan', 'pending', $2, 'network', $3, NULL, 0, 3, \
                    '2020-01-01T00:00:00Z', NULL, NULL, $1, NULL, NULL, 0, \
                    '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z', NULL, NULL, 0)",
                [id.into(), priority.into(), payload.into()],
            ),
        )
        .await
        .unwrap_or_else(|err| panic!("seed job {id}: {err}"));
    }
    let now = "2024-06-01T00:00:00Z";
    let req = named(
        "pg-claim-poison",
        DbAtomicParams::ClaimNextJob {
            resource_class: "network".into(),
            owner: "worker-1".into(),
            lease_secs: 60,
        },
    );
    let compiled = compile_named_request(req.id, &req.params, now).unwrap();
    let result = execute_compiled_on(&db, compiled.clone(), "postgres_txn")
        .await
        .expect("malformed payload must not abort the claim batch");
    assert_eq!(result.status, atomic_status::OK);
    let rows = sea_orm::ConnectionTrait::query_all_raw(
        &db,
        sea_orm::Statement::from_string(
            backend,
            "SELECT id, state, kind, error_kind FROM jobs WHERE id IN ('bad-json', 'good') ORDER BY id",
        ),
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].try_get_by_index::<String>(0).unwrap(), "bad-json");
    assert_eq!(rows[0].try_get_by_index::<String>(1).unwrap(), "failed");
    assert_eq!(rows[0].try_get_by_index::<String>(2).unwrap(), "invalid");
    assert_eq!(
        rows[0].try_get_by_index::<String>(3).unwrap(),
        "invalid_job"
    );
    assert_eq!(rows[1].try_get_by_index::<String>(0).unwrap(), "good");
    assert_eq!(rows[1].try_get_by_index::<String>(1).unwrap(), "running");
}

#[test]
fn architecture_lint_database_plugins() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("scripts/check-db-plugin-isolation.py");
    let status = Command::new("python3")
        .arg(&script)
        .current_dir(&root)
        .status()
        .expect("python3");
    assert!(status.success(), "check-db-plugin-isolation.py failed");
}

fn enqueue_scan(id: &'static str, account: &str) -> NamedOp {
    named(
        id,
        DbAtomicParams::EnqueueJob {
            kind: "scan".into(),
            payload_json: format!(r#"{{"v":1,"account":"{account}"}}"#),
            priority: 0,
            max_attempts: 3,
            max_pending: 10,
            run_after: None,
        },
    )
}

#[tokio::test]
async fn plan_cancel_hook_aborts_before_commit() {
    let db = mem_db().await;
    crate::inject_commit_failures(1);
    let plan = vec![{
        let mut s = typed_stmt(
            "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('cancel', 0)",
            bookclerk_plugin_abi::DbPlanStatementKind::Execute,
        );
        s.max_rows = 0;
        s
    }];
    let err = super::execute_typed_on(&db, &typed_req("op-cancel", plan.clone()), "sqlite_txn", 0)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("commit failed"), "{err}");
    let rows: Vec<sea_orm::QueryResult> = sea_orm::ConnectionTrait::query_all_raw(
        &db,
        sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT bump FROM db_serialization_slots WHERE slot_key = 'cancel'",
        ),
    )
    .await
    .unwrap();
    assert!(rows.is_empty(), "injected commit failure must roll back");
}

#[tokio::test]
async fn execute_caps_collected_rows_at_max_result_rows() {
    let db = mem_db().await;
    for i in 0..5 {
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Sqlite,
                "INSERT INTO db_serialization_slots (slot_key, bump) VALUES (?, 0)",
                [format!("cap-{i}").into()],
            ),
        )
        .await
        .unwrap();
    }
    let plan = vec![{
        let mut s = typed_stmt("SELECT slot_key FROM db_serialization_slots WHERE slot_key LIKE 'cap-%' ORDER BY slot_key", bookclerk_plugin_abi::DbPlanStatementKind::Returning);
        s.max_rows = 0;
        s
    }];
    let exec = super::execute_typed_on(&db, &typed_req("op-cap", plan.clone()), "sqlite_txn", 5)
        .await
        .unwrap();
    assert_eq!(exec.statements[0].rows.len(), 5);
    let err = super::execute_typed_on(&db, &typed_req("op-cap-over", plan), "sqlite_txn", 2)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("maxResultRows"),
        "row cap must fail closed: {err}"
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_plan_commit_inserts_receipt() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    let now = "2024-06-01T00:00:00Z";
    let op = enqueue_scan("pg-conf-enq-2", "pg");
    let compiled = compile_named_request(op.id, &op.params, now).unwrap();
    let first = execute_compiled_on(&db, compiled.clone(), "postgres_txn")
        .await
        .unwrap();
    assert_eq!(first.status, atomic_status::OK);
    let replay = execute_compiled_on(&db, compiled.clone(), "postgres_txn")
        .await
        .unwrap();
    assert!(replay.replayed);
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_plan_exceeds_max_binds_is_rejected() {
    let mut caps = bookclerk_plugin_abi::DbCapabilities::advertised_postgres();
    caps.max_binds = 2;
    let compiled = super::CompiledAtomic {
        request: typed_req(
            "over-binds",
            vec![{
                let mut s = typed_stmt(
                    "SELECT ?, ?, ?",
                    bookclerk_plugin_abi::DbPlanStatementKind::Returning,
                );
                s.parameters = vec![
                    bookclerk_plugin_abi::DbValue::Int64(1),
                    bookclerk_plugin_abi::DbValue::Int64(2),
                    bookclerk_plugin_abi::DbValue::Int64(3),
                ];
                s
            }],
        ),
        selection: super::AtomicSelection::default(),
        expected_hash: String::new(),
    };
    let err = super::validate_plan(&compiled, &caps).unwrap_err();
    assert!(err.to_string().contains("maxBinds"), "{err}");
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_execute_caps_collected_rows() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
    for i in 0..5 {
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_sql_and_values(
                backend,
                "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ($1, 0)",
                [format!("pg-cap-{i}").into()],
            ),
        )
        .await
        .unwrap();
    }
    let plan = vec![{
        let mut s = typed_stmt("SELECT slot_key FROM db_serialization_slots WHERE slot_key LIKE 'pg-cap-%' ORDER BY slot_key", bookclerk_plugin_abi::DbPlanStatementKind::Returning);
        s.max_rows = 0;
        s
    }];
    let exec = super::execute_typed_on(
        &db,
        &typed_req("op-pg-cap", plan.clone()),
        "postgres_txn",
        5,
    )
    .await
    .unwrap();
    assert_eq!(exec.statements[0].rows.len(), 5);
    let err = super::execute_typed_on(&db, &typed_req("op-pg-cap-over", plan), "postgres_txn", 2)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("maxResultRows"),
        "row cap must fail closed: {err}"
    );
}

#[tokio::test]
async fn host_applies_schema_to_unmigrated_sqlite() {
    let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .expect("unmigrated sqlite");
    crate::apply_host_schema(&db, crate::HostSchemaKind::PragmaMarker)
        .await
        .expect("host schema");
    let rows = sea_orm::ConnectionTrait::query_all_raw(
        &db,
        sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT name FROM sqlite_master WHERE type='table' AND name='db_serialization_slots'",
        ),
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
}

fn interrupt_plan(slot: &str) -> Vec<bookclerk_plugin_abi::TypedDbStatement> {
    vec![{
        let mut s = typed_stmt(
            format!("INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('{slot}', 0)"),
            bookclerk_plugin_abi::DbPlanStatementKind::Execute,
        );
        s.max_rows = 0;
        s
    }]
}

async fn slot_missing(db: &sea_orm::DatabaseConnection, key: &str) -> bool {
    let rows = sea_orm::ConnectionTrait::query_all_raw(
        db,
        sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT bump FROM db_serialization_slots WHERE slot_key = ?",
            [key.into()],
        ),
    )
    .await
    .unwrap();
    rows.is_empty()
}

#[tokio::test]
async fn plan_cancel_before_begin_does_not_commit() {
    let db = mem_db().await;
    crate::inject_atomic_interrupt(
        crate::AtomicInterruptPhase::BeforeBegin,
        crate::AtomicInterruptKind::Cancel,
    );
    let err = super::execute_typed_on(
        &db,
        &typed_req("op-c-before", interrupt_plan("c-before")),
        "sqlite_txn",
        0,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
    assert!(slot_missing(&db, "c-before").await);
}

#[tokio::test]
async fn plan_cancel_during_statements_rolls_back() {
    let db = mem_db().await;
    crate::inject_atomic_interrupt(
        crate::AtomicInterruptPhase::BetweenStatements,
        crate::AtomicInterruptKind::Cancel,
    );
    let err = super::execute_typed_on(
        &db,
        &typed_req("op-c-during", interrupt_plan("c-during")),
        "sqlite_txn",
        0,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
    assert!(slot_missing(&db, "c-during").await);
}

#[tokio::test]
async fn plan_cancel_around_commit_is_unavailable() {
    let db = mem_db().await;
    crate::inject_atomic_interrupt(
        crate::AtomicInterruptPhase::AroundCommit,
        crate::AtomicInterruptKind::Cancel,
    );
    let err = super::execute_typed_on(
        &db,
        &typed_req("op-c-commit", interrupt_plan("c-commit")),
        "sqlite_txn",
        0,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("database commit failed"), "{err}");
    assert!(slot_missing(&db, "c-commit").await);
}

#[tokio::test]
async fn plan_deadline_before_begin() {
    let db = mem_db().await;
    crate::inject_atomic_interrupt(
        crate::AtomicInterruptPhase::BeforeBegin,
        crate::AtomicInterruptKind::Deadline,
    );
    let err = super::execute_typed_on(
        &db,
        &typed_req("op-d-before", interrupt_plan("d-before")),
        "sqlite_txn",
        0,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("deadline"), "{err}");
    assert!(slot_missing(&db, "d-before").await);
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_plan_cancel_before_begin() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    crate::inject_atomic_interrupt(
        crate::AtomicInterruptPhase::BeforeBegin,
        crate::AtomicInterruptKind::Cancel,
    );
    let err = super::execute_typed_on(
        &db,
        &typed_req("op-pg-c-before", interrupt_plan("pg-c-before")),
        "postgres_txn",
        0,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_plan_cancel_during_statements() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    crate::inject_atomic_interrupt(
        crate::AtomicInterruptPhase::BetweenStatements,
        crate::AtomicInterruptKind::Cancel,
    );
    let err = super::execute_typed_on(
        &db,
        &typed_req("op-pg-c-during", interrupt_plan("pg-c-during")),
        "postgres_txn",
        0,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_host_applies_schema() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    sea_orm::ConnectionTrait::query_all_raw(
        &db,
        sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 FROM db_serialization_slots LIMIT 1",
        ),
    )
    .await
    .expect("db_serialization_slots exists after host schema");
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_plan_cancel_around_commit_is_unavailable() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    crate::inject_atomic_interrupt(
        crate::AtomicInterruptPhase::AroundCommit,
        crate::AtomicInterruptKind::Cancel,
    );
    let err = super::execute_typed_on(
        &db,
        &typed_req("op-pg-c-commit", interrupt_plan("pg-c-commit")),
        "postgres_txn",
        0,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("database commit failed"), "{err}");
}

fn typed_query(id: &str, sql: &str) -> bookclerk_plugin_abi::ExecuteRequest {
    use bookclerk_plugin_abi::{
        DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
    };
    ExecuteRequest {
        operation_id: id.into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.into(),
            parameters: vec![],
            kind: DbPlanStatementKind::Select,
            max_rows: 0,
            result_selection: DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    }
}

async fn seed_typed_probe(db: &sea_orm::DatabaseConnection, sql_values: &str) {
    let backend = sea_orm::ConnectionTrait::get_database_backend(db);
    sea_orm::ConnectionTrait::execute_raw(
        db,
        sea_orm::Statement::from_string(
            backend,
            "CREATE TABLE IF NOT EXISTS typed_probe (x INTEGER, y TEXT)",
        ),
    )
    .await
    .unwrap();
    sea_orm::ConnectionTrait::execute_raw(db, sea_orm::Statement::from_string(backend, sql_values))
        .await
        .unwrap();
}

#[tokio::test]
async fn typed_sqlite_duplicate_alias_zero_row_and_null_metadata() {
    let db = mem_db().await;
    seed_typed_probe(&db, "INSERT INTO typed_probe (x, y) VALUES (NULL, 'a')").await;
    let dup = typed_query("dup", "SELECT x, x FROM typed_probe");
    let reply = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &dup,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "sqlite_txn",
        bookclerk_db_exec::ExecCaps::from_capabilities(
            &bookclerk_plugin_abi::DbCapabilities::advertised_sqlite(),
        ),
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await;
    match reply {
        Err(err) => assert!(err.to_string().contains("duplicate column"), "{err}"),
        Ok(reply) => {
            assert_eq!(
                reply.statements[0].columns.len(),
                2,
                "duplicate expressions must stay positional, not collapse: {:?}",
                reply.statements[0].columns
            );
            assert_eq!(reply.statements[0].rows[0].values.len(), 2);
        }
    }

    let empty = typed_query("empty", "SELECT x FROM typed_probe WHERE FALSE");
    let reply = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &empty,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "sqlite_txn",
        bookclerk_db_exec::ExecCaps::from_capabilities(
            &bookclerk_plugin_abi::DbCapabilities::advertised_sqlite(),
        ),
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap();
    assert!(reply.statements[0].rows.is_empty());
    assert_eq!(reply.statements[0].columns.len(), 1);
    assert_eq!(reply.statements[0].columns[0].name, "x");

    let nulls = typed_query("nulls", "SELECT x FROM typed_probe");
    let reply = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &nulls,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "sqlite_txn",
        bookclerk_db_exec::ExecCaps::from_capabilities(
            &bookclerk_plugin_abi::DbCapabilities::advertised_sqlite(),
        ),
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap();
    assert_eq!(
        reply.statements[0].columns[0].db_type,
        bookclerk_plugin_abi::DbType::Int64
    );
    assert!(matches!(
        reply.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Null(bookclerk_plugin_abi::DbType::Int64)
    ));
}

#[tokio::test]
async fn typed_sqlite_select_stops_after_cap_plus_one() {
    let db = mem_db().await;
    let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
    sea_orm::ConnectionTrait::execute_raw(
        &db,
        sea_orm::Statement::from_string(
            backend,
            "CREATE TABLE IF NOT EXISTS typed_rowcap (x INTEGER)",
        ),
    )
    .await
    .ok();
    for i in 0..50 {
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_sql_and_values(
                backend,
                "INSERT INTO typed_rowcap (x) VALUES (?)",
                [i.into()],
            ),
        )
        .await
        .unwrap();
    }
    let req = typed_query("cap", "SELECT x FROM typed_rowcap");
    let mut caps = bookclerk_db_exec::ExecCaps::from_capabilities(
        &bookclerk_plugin_abi::DbCapabilities::advertised_sqlite(),
    );
    caps.max_result_rows = CONTRACT_VECTOR_ROW_CAP;
    let err = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &req,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "sqlite_txn",
        caps,
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("maxResultRows"), "{err}");
    let seen = crate::query_rows_seen();
    assert!(
        seen <= 6,
        "must stop after cap+1 materialized rows, saw {seen}"
    );
}

#[tokio::test]
async fn typed_sqlite_statement_max_rows_is_a_proven_bound() {
    let db = mem_db().await;
    seed_typed_probe(
        &db,
        "INSERT INTO typed_probe (x, y) VALUES (1, 'a'), (2, 'b')",
    )
    .await;
    use bookclerk_plugin_abi::{
        DbPlanStatementKind, DbResultSelection, DbValue, ExecuteRequest, TypedDbStatement,
    };
    fn req(sql: &str) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: "first".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        }
    }
    let caps = bookclerk_db_exec::ExecCaps::from_capabilities(
        &bookclerk_plugin_abi::DbCapabilities::advertised_sqlite(),
    );
    let err = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &req("SELECT x FROM typed_probe ORDER BY x"),
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "sqlite_txn",
        caps,
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("maxRows"), "{err}");

    let reply = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &req("SELECT x FROM typed_probe ORDER BY x LIMIT 1"),
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "sqlite_txn",
        caps,
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap();
    assert_eq!(reply.statements[0].rows.len(), 1);
    assert_eq!(reply.statements[0].rows[0].values[0], DbValue::Int64(1));
}

#[tokio::test]
async fn typed_sqlite_per_statement_max_result_bytes() {
    let db = mem_db().await;
    seed_typed_probe(
        &db,
        "INSERT INTO typed_probe (x, y) VALUES (1, 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')",
    )
    .await;
    use bookclerk_plugin_abi::{
        DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
    };
    let req = ExecuteRequest {
        operation_id: "bytes".into(),
        request_hash: String::new(),
        statements: vec![
            TypedDbStatement {
                sql: "SELECT y FROM typed_probe".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            },
            TypedDbStatement {
                sql: "SELECT y FROM typed_probe".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            },
        ],
        deadline_unix_ms: 0,
    };
    let mut caps = bookclerk_db_exec::ExecCaps::from_capabilities(
        &bookclerk_plugin_abi::DbCapabilities::advertised_sqlite(),
    );
    caps.max_result_bytes = 32;
    caps.max_atomic_result_bytes = 1_048_576;
    let err = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &req,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "sqlite_txn",
        caps,
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("maxResultBytes"), "{err}");
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn typed_postgres_duplicate_alias_zero_row_and_null_metadata() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    seed_typed_probe(&db, "INSERT INTO typed_probe (x, y) VALUES (NULL, 'a')").await;
    let dup = typed_query("dup", "SELECT x AS n, x AS n FROM typed_probe");
    let err = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &dup,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "postgres_txn",
        bookclerk_db_exec::ExecCaps::from_capabilities(
            &bookclerk_plugin_abi::DbCapabilities::advertised_postgres(),
        ),
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("duplicate") || err.to_string().to_lowercase().contains("n"),
        "{err}"
    );

    let empty = typed_query("empty", "SELECT x FROM typed_probe WHERE false");
    let reply = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &empty,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "postgres_txn",
        bookclerk_db_exec::ExecCaps::from_capabilities(
            &bookclerk_plugin_abi::DbCapabilities::advertised_postgres(),
        ),
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap();
    assert!(reply.statements[0].rows.is_empty());
    assert_eq!(reply.statements[0].columns.len(), 1);
    assert_eq!(reply.statements[0].columns[0].name, "x");
    assert_eq!(
        reply.statements[0].columns[0].db_type,
        bookclerk_plugin_abi::DbType::Int64
    );

    let nulls = typed_query("nulls", "SELECT x FROM typed_probe");
    let reply = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &nulls,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "postgres_txn",
        bookclerk_db_exec::ExecCaps::from_capabilities(
            &bookclerk_plugin_abi::DbCapabilities::advertised_postgres(),
        ),
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap();
    assert_eq!(
        reply.statements[0].columns[0].db_type,
        bookclerk_plugin_abi::DbType::Int64
    );
    assert!(matches!(
        reply.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Null(bookclerk_plugin_abi::DbType::Int64)
    ));
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn typed_postgres_empty_select_describe_does_not_reexecute() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_migrated_db().await;
    let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
    for sql in [
        "CREATE TABLE typed_counter (n INTEGER NOT NULL)",
        "INSERT INTO typed_counter (n) VALUES (0)",
        "CREATE FUNCTION typed_bump() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN UPDATE typed_counter SET n = n + 1; RETURN 1; END; $$",
        "CREATE VIEW typed_bump_view AS SELECT typed_bump() AS n",
    ] {
        sea_orm::ConnectionTrait::execute_raw(&db, sea_orm::Statement::from_string(backend, sql))
            .await
            .unwrap();
    }
    // `typed_bump()` is not a SQL v1 helper. SELECT the volatile function
    // through a view so the typed statement stays fail-closed on unknown
    // helpers while still detecting a describe-time re-execute.
    let mut type_env = crate::migrations::host_sql_type_env();
    type_env.insert_column(
        "typed_bump_view",
        "n",
        bookclerk_plugin_abi::SqlType::Integer,
    );
    let empty = typed_query("bump", "SELECT n FROM typed_bump_view LIMIT 0");
    let reply = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &empty,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "postgres_txn",
        bookclerk_db_exec::ExecCaps::from_capabilities(
            &bookclerk_plugin_abi::DbCapabilities::advertised_postgres(),
        ),
        bookclerk_db_exec::AtomicSession::from_deadline(None).with_type_env(type_env.clone()),
    )
    .await
    .unwrap();
    assert!(reply.statements[0].rows.is_empty());
    assert_eq!(reply.statements[0].columns[0].name, "n");

    let count = typed_query("count", "SELECT n FROM typed_counter");
    let reply = bookclerk_db_exec::execute_typed_on_session(
        &db,
        &count,
        bookclerk_db_exec::GuestReceiptPersist::default(),
        "postgres_txn",
        bookclerk_db_exec::ExecCaps::from_capabilities(
            &bookclerk_plugin_abi::DbCapabilities::advertised_postgres(),
        ),
        bookclerk_db_exec::AtomicSession::from_deadline(None)
            .with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .unwrap();
    let bookclerk_plugin_abi::DbValue::Int64(n) = reply.statements[0].rows[0].values[0] else {
        panic!(
            "expected int64 counter, got {:?}",
            reply.statements[0].rows[0].values[0]
        );
    };
    assert!(
        n <= 1,
        "zero-row SELECT must execute at most once (counter={n}); describe must not re-run it"
    );
}

/// Disposable Postgres **binding** database (receipt bootstrap only, not the host catalog).
async fn postgres_binding_db() -> sea_orm::DatabaseConnection {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").expect("postgres url");
    let db_name = format!("bind_{}", uuid::Uuid::new_v4().as_simple());
    let admin = sea_orm::Database::connect(url.as_str())
        .await
        .expect("connect to BOOKCLERK_TEST_POSTGRES_URL");
    let backend = sea_orm::ConnectionTrait::get_database_backend(&admin);
    sea_orm::ConnectionTrait::execute_raw(
        &admin,
        sea_orm::Statement::from_string(backend, format!("CREATE DATABASE {db_name}")),
    )
    .await
    .expect("create disposable postgres binding database");
    let (base, query) = match url.split_once('?') {
        Some((base, q)) => (base, Some(q)),
        None => (url.as_str(), None),
    };
    let trimmed = base.trim_end_matches('/');
    let slash = trimmed
        .rfind('/')
        .expect("BOOKCLERK_TEST_POSTGRES_URL must include a database path");
    let db_url = match query {
        Some(q) => format!("{}/{db_name}?{q}", &trimmed[..slash]),
        None => format!("{}/{db_name}", &trimmed[..slash]),
    };
    let db = sea_orm::Database::connect(&db_url)
        .await
        .expect("connect to disposable postgres binding database");
    let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
    for sql in
        bookclerk_db_exec::split_schema_statements(crate::migrations::binding_bootstrap_sql())
    {
        let sql = bookclerk_db_exec::schema_sql_for_backend(backend, &sql);
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(backend, sql.into_owned()),
        )
        .await
        .expect("binding bootstrap DDL");
    }
    db
}

async fn run_postgres_binding(
    db: &sea_orm::DatabaseConnection,
    request: bookclerk_plugin_abi::ExecuteRequest,
) -> Result<bookclerk_plugin_abi::ExecuteReply, bookclerk_plugin_abi::PluginError> {
    let caps = DbCapabilities::advertised_postgres();
    let env = bookclerk_db_exec::load_sql_type_env(db)
        .await
        .expect("load binding catalog");
    let policy = bookclerk_plugin_abi::GuestSqlPolicy::binding_owned().with_sql_types(env);
    let exec_caps = caps.clone();
    super::execute_guest_atomic_with(request, &caps, &policy, |envelope| async move {
        let deadline =
            (envelope.request.deadline_unix_ms > 0).then_some(envelope.request.deadline_unix_ms);
        bookclerk_db_exec::execute_typed_envelope(
            db,
            &envelope,
            "postgres_txn",
            bookclerk_db_exec::ExecCaps::from_capabilities(&exec_caps),
            bookclerk_db_exec::AtomicSession::from_deadline(deadline),
        )
        .await
        .map_err(|err| bookclerk_plugin_abi::PluginError::internal(err.to_string()))
    })
    .await
}

fn binding_stmt(
    sql: &str,
    parameters: Vec<bookclerk_plugin_abi::DbValue>,
) -> bookclerk_plugin_abi::TypedDbStatement {
    let kind = bookclerk_plugin_abi::guest_statement_kind(sql);
    let result_selection = if kind == bookclerk_plugin_abi::DbPlanStatementKind::Select {
        bookclerk_plugin_abi::DbResultSelection::Rows
    } else {
        bookclerk_plugin_abi::DbResultSelection::Discard
    };
    bookclerk_plugin_abi::TypedDbStatement {
        sql: sql.into(),
        parameters,
        kind,
        max_rows: 0,
        result_selection,
    }
}

fn binding_req(
    operation_id: &str,
    statements: Vec<bookclerk_plugin_abi::TypedDbStatement>,
) -> bookclerk_plugin_abi::ExecuteRequest {
    bookclerk_plugin_abi::ExecuteRequest {
        operation_id: operation_id.into(),
        request_hash: String::new(),
        statements,
        deadline_unix_ms: 0,
    }
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_binding_mixed_ddl_dml() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_binding_db().await;
    let mixed = || {
        let ddl = binding_stmt(
            "CREATE TABLE IF NOT EXISTS counters (id INTEGER PRIMARY KEY, n INTEGER)",
            vec![],
        );
        let mut insert = binding_stmt(
            "INSERT INTO counters (id, n) VALUES (?, ?)",
            vec![
                bookclerk_plugin_abi::DbValue::Int64(1),
                bookclerk_plugin_abi::DbValue::Int64(1),
            ],
        );
        insert.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
        binding_req("pg-mixed-once", vec![ddl, insert])
    };
    let first = run_postgres_binding(&db, mixed())
        .await
        .expect("first mixed");
    assert_eq!(first.statements[1].rows_affected, 1);
    let replay = run_postgres_binding(&db, mixed())
        .await
        .expect("replay mixed");
    assert_eq!(replay.statements[1].rows_affected, 1);
    let mut count = binding_stmt("SELECT count(*) FROM counters", vec![]);
    count.max_rows = 8;
    let counted = run_postgres_binding(&db, binding_req("pg-mixed-count", vec![count]))
        .await
        .expect("count");
    let bookclerk_plugin_abi::DbValue::Int64(n) = counted.statements[0].rows[0].values[0] else {
        panic!(
            "expected int64 count, got {:?}",
            counted.statements[0].rows[0].values[0]
        );
    };
    assert_eq!(n, 1, "postgres mixed batch must not double-insert");
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_binding_mixed_ddl_dml_preserves_gate_text_in_literal_and_comment() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_binding_db().await;
    let mixed = || {
        let ddl = binding_stmt(bookclerk_db_exec::sql_v1::MIXED_GATE_LITERAL_DDL, vec![]);
        let mut insert = binding_stmt(
            &bookclerk_db_exec::sql_v1::mixed_gate_literal_insert(),
            vec![],
        );
        insert.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
        binding_req("pg-mixed-gate-lit", vec![ddl, insert])
    };
    let first = run_postgres_binding(&db, mixed())
        .await
        .expect("first mixed gate");
    assert_eq!(first.statements[1].rows_affected, 1);
    let replay = run_postgres_binding(&db, mixed())
        .await
        .expect("replay mixed gate");
    assert_eq!(replay.statements[1].rows_affected, 1);
    let mut select = binding_stmt(bookclerk_db_exec::sql_v1::MIXED_GATE_LITERAL_SELECT, vec![]);
    select.max_rows = 8;
    let mut count = binding_stmt(bookclerk_db_exec::sql_v1::MIXED_GATE_LITERAL_COUNT, vec![]);
    count.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-mixed-gate-sel", vec![select, count]))
        .await
        .expect("select mixed gate");
    assert_eq!(
        reply.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Text(bookclerk_db_exec::GUEST_RECEIPT_WRITE_GATE.into())
    );
    let bookclerk_plugin_abi::DbValue::Int64(n) = reply.statements[1].rows[0].values[0] else {
        panic!(
            "expected int64 count, got {:?}",
            reply.statements[1].rows[0].values[0]
        );
    };
    assert_eq!(n, 1, "postgres mixed gate batch must not double-insert");
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_binding_portable_functions() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_binding_db().await;
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-typed",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_AUTOINCREMENT_BLOB,
                vec![],
            )],
        ),
    )
    .await
    .expect("typed DDL");
    let mut insert = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_INSERT,
        vec![bookclerk_plugin_abi::DbValue::Bytes(
            bookclerk_db_exec::sql_v1::PORTABLE_INSERT_BLOB.to_vec(),
        )],
    );
    insert.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ins-typed", vec![insert]))
        .await
        .expect("typed insert");
    let mut select = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_SELECT, vec![]);
    select.max_rows = 8;
    let mut aggregates = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_AGGREGATE_SELECT, vec![]);
    aggregates.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-sel-typed", vec![select, aggregates]))
        .await
        .expect("portable select");
    if let Some(err) = bookclerk_db_exec::sql_v1::portable_select_mismatch(&reply.statements[0]) {
        panic!("{err}");
    }
    if let Some(err) = bookclerk_db_exec::sql_v1::portable_aggregate_mismatch(&reply.statements[1])
    {
        panic!("{err}");
    }
    let mut blob = binding_stmt("SELECT blob FROM typed", vec![]);
    blob.max_rows = 8;
    let blob_reply = run_postgres_binding(&db, binding_req("pg-sel-blob", vec![blob]))
        .await
        .expect("blob select");
    assert_eq!(
        blob_reply.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Bytes(
            bookclerk_db_exec::sql_v1::PORTABLE_INSERT_BLOB.to_vec()
        )
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_binding_portable_boolean_column() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_binding_db().await;
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-bool",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_BOOLEAN,
                vec![],
            )],
        ),
    )
    .await
    .expect("boolean DDL");
    let mut insert = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_BOOLEAN_INSERT,
        bookclerk_db_exec::sql_v1::portable_boolean_insert_binds(),
    );
    insert.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ins-bool", vec![insert]))
        .await
        .expect("boolean insert");
    let mut select = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_BOOLEAN_SELECT, vec![]);
    select.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-sel-bool", vec![select]))
        .await
        .expect("boolean select");
    if let Some(err) = bookclerk_db_exec::sql_v1::portable_boolean_mismatch(&reply.statements[0]) {
        panic!("{err}");
    }
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_binding_lowercase_ddl_and_insert_or_ignore_returning() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_binding_db().await;
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-lc",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_LOWERCASE,
                vec![],
            )],
        ),
    )
    .await
    .expect("lowercase DDL");
    let mut insert = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_INSERT_OR_IGNORE_RETURNING_LC,
        bookclerk_db_exec::sql_v1::portable_lowercase_insert_binds(),
    );
    insert.result_selection = bookclerk_plugin_abi::DbResultSelection::Rows;
    insert.max_rows = 1;
    let inserted = run_postgres_binding(&db, binding_req("pg-ins-lc", vec![insert]))
        .await
        .expect("lowercase insert returning");
    let bookclerk_plugin_abi::DbValue::Int64(id) = inserted.statements[0].rows[0].values[0] else {
        panic!(
            "expected int64 returning id, got {:?}",
            inserted.statements[0].rows[0].values[0]
        );
    };
    assert!(id >= 1, "returning id {id}");
    let mut select = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_LOWERCASE_SELECT, vec![]);
    select.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-sel-lc", vec![select]))
        .await
        .expect("lowercase select");
    assert_eq!(
        reply.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Bytes(
            bookclerk_db_exec::sql_v1::PORTABLE_INSERT_BLOB.to_vec()
        )
    );
    assert_eq!(
        reply.statements[0].rows[0].values[1],
        bookclerk_plugin_abi::DbValue::Boolean(true)
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_binding_insert_or_ignore_unique_not_null_domain() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_binding_db().await;
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-conflict",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_CONFLICT,
                vec![],
            )],
        ),
    )
    .await
    .expect("conflict DDL");
    let mut first = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_INSERT_OR_IGNORE_UNIQUE,
        vec![],
    );
    first.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    let inserted = run_postgres_binding(&db, binding_req("pg-ins-u1", vec![first.clone()]))
        .await
        .expect("first unique insert");
    assert_eq!(inserted.statements[0].rows_affected, 1);
    let ignored = run_postgres_binding(&db, binding_req("pg-ins-u2", vec![first]))
        .await
        .expect("duplicate unique ignore");
    assert_eq!(ignored.statements[0].rows_affected, 0);
    let mut null_ins = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_INSERT_OR_IGNORE_NOT_NULL,
        vec![],
    );
    null_ins.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    let err = run_postgres_binding(&db, binding_req("pg-ins-nn", vec![null_ins]))
        .await
        .expect_err("NOT NULL must still abort");
    let t = err.to_string().to_ascii_lowercase();
    assert!(
        t.contains("null") || t.contains("constraint") || t.contains("not null"),
        "{err}"
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_binding_semantic_helpers_order_identity_fold() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_binding_db().await;
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-typed",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_AUTOINCREMENT_BLOB,
                vec![],
            )],
        ),
    )
    .await
    .expect("typed DDL");
    let mut insert = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_INSERT,
        vec![bookclerk_plugin_abi::DbValue::Bytes(
            bookclerk_db_exec::sql_v1::PORTABLE_INSERT_BLOB.to_vec(),
        )],
    );
    insert.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ins-typed", vec![insert]))
        .await
        .expect("typed insert");
    let mut mm = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_MIN_MAX_NULL, vec![]);
    mm.max_rows = 8;
    let mut round = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_UNCAST_ROUND, vec![]);
    round.max_rows = 8;
    let mut sum_avg = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_UNCAST_SUM_AVG, vec![]);
    sum_avg.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-sel-sem", vec![mm, round, sum_avg]))
        .await
        .expect("semantic select");
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_min_max_null_mismatch(&reply.statements[0])
    {
        panic!("{err}");
    }
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_uncast_round_mismatch(&reply.statements[1])
    {
        panic!("{err}");
    }
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_uncast_sum_avg_mismatch(&reply.statements[2])
    {
        panic!("{err}");
    }

    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-ord",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_ORDER_NULLS,
                vec![],
            )],
        ),
    )
    .await
    .expect("order DDL");
    for (op, sql) in [
        (
            "pg-i1",
            bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_INSERT_1,
        ),
        (
            "pg-inull",
            bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_INSERT_NULL,
        ),
        (
            "pg-i2",
            bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_INSERT_2,
        ),
    ] {
        let mut ins = binding_stmt(sql, vec![]);
        ins.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
        run_postgres_binding(&db, binding_req(op, vec![ins]))
            .await
            .expect(op);
    }
    let mut asc = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_ASC, vec![]);
    asc.max_rows = 8;
    let mut desc = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_DESC, vec![]);
    desc.max_rows = 8;
    let ordered = run_postgres_binding(&db, binding_req("pg-sel-ord", vec![asc, desc]))
        .await
        .expect("order select");
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_order_nulls_asc_mismatch(&ordered.statements[0])
    {
        panic!("{err}");
    }
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_order_nulls_desc_mismatch(&ordered.statements[1])
    {
        panic!("{err}");
    }

    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-id",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_IDENTITY,
                vec![],
            )],
        ),
    )
    .await
    .expect("identity DDL");
    let mut expl = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_EXPLICIT,
        vec![],
    );
    expl.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ins-ex", vec![expl]))
        .await
        .expect("explicit id");
    let mut omit = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_OMIT,
        vec![],
    );
    omit.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ins-om", vec![omit.clone()]))
        .await
        .expect("omit id");
    let mut mx = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_SELECT_MAX,
        vec![],
    );
    mx.max_rows = 8;
    let max1 = run_postgres_binding(&db, binding_req("pg-sel-max1", vec![mx.clone()]))
        .await
        .expect("max after omit");
    assert_eq!(
        max1.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Int64(101)
    );
    let mut del = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_DELETE_MAX,
        vec![],
    );
    del.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-del-max", vec![del]))
        .await
        .expect("delete max");
    run_postgres_binding(&db, binding_req("pg-ins-om2", vec![omit]))
        .await
        .expect("omit after delete");
    let max2 = run_postgres_binding(&db, binding_req("pg-sel-max2", vec![mx]))
        .await
        .expect("max after reinsert");
    assert_eq!(
        max2.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Int64(102)
    );

    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-fold",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_UNQUOTED_FOLD,
                vec![],
            )],
        ),
    )
    .await
    .expect("fold DDL");
    let mut fins = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_UNQUOTED_FOLD_INSERT,
        vec![],
    );
    fins.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ins-fold", vec![fins]))
        .await
        .expect("fold insert");
    let mut fsel = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_UNQUOTED_FOLD_SELECT,
        vec![],
    );
    fsel.max_rows = 8;
    let folded = run_postgres_binding(&db, binding_req("pg-sel-fold", vec![fsel]))
        .await
        .expect("fold select");
    assert_eq!(
        folded.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Int64(7)
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn postgres_binding_sql_v1_p1_vectors() {
    if !postgres_conformance_enabled() {
        return;
    }
    let db = postgres_binding_db().await;
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-ign",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_IGNORE_SELECT,
                vec![],
            )],
        ),
    )
    .await
    .expect("ign ddl");
    let mut ign = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IGNORE_SELECT,
        vec![bookclerk_plugin_abi::DbValue::Int64(1)],
    );
    ign.result_selection = bookclerk_plugin_abi::DbResultSelection::Rows;
    ign.max_rows = 0;
    run_postgres_binding(&db, binding_req("pg-ign-sel", vec![ign]))
        .await
        .expect("ign select");
    let mut withs = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IGNORE_SELECT_WITH,
        vec![bookclerk_plugin_abi::DbValue::Int64(2)],
    );
    withs.result_selection = bookclerk_plugin_abi::DbResultSelection::Rows;
    withs.max_rows = 0;
    run_postgres_binding(&db, binding_req("pg-ign-with", vec![withs]))
        .await
        .expect("ign with");
    let mut uni = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IGNORE_SELECT_UNION,
        vec![
            bookclerk_plugin_abi::DbValue::Int64(3),
            bookclerk_plugin_abi::DbValue::Int64(4),
        ],
    );
    uni.result_selection = bookclerk_plugin_abi::DbResultSelection::Rows;
    uni.max_rows = 0;
    run_postgres_binding(&db, binding_req("pg-ign-union", vec![uni]))
        .await
        .expect("ign union");
    let mut ord = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IGNORE_SELECT_ORDER_LIMIT,
        vec![bookclerk_plugin_abi::DbValue::Int64(5)],
    );
    ord.result_selection = bookclerk_plugin_abi::DbResultSelection::Rows;
    ord.max_rows = 0;
    run_postgres_binding(&db, binding_req("pg-ign-ord", vec![ord]))
        .await
        .expect("ign order limit");

    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-like",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_LIKE,
                vec![],
            )],
        ),
    )
    .await
    .expect("like ddl");
    let mut like_ins = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_LIKE_INSERT, vec![]);
    like_ins.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-like-ins", vec![like_ins]))
        .await
        .expect("like ins");
    let mut like_sel = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_LIKE_SELECT,
        vec![bookclerk_plugin_abi::DbValue::Text("A".into())],
    );
    like_sel.max_rows = 8;
    let liked = run_postgres_binding(&db, binding_req("pg-like-sel", vec![like_sel]))
        .await
        .expect("like sel");
    if let Some(err) = bookclerk_db_exec::sql_v1::portable_statement_mismatch(
        &liked.statements[0],
        bookclerk_db_exec::sql_v1::portable_like_expects(),
        "like",
    ) {
        panic!("{err}");
    }
    let mut like_na = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_LIKE_NON_ASCII, vec![]);
    like_na.max_rows = 8;
    let na = run_postgres_binding(&db, binding_req("pg-like-na", vec![like_na]))
        .await
        .expect("like na");
    assert_eq!(
        na.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Int64(0)
    );

    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-blobdef",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_BLOB_DEFAULT,
                vec![],
            )],
        ),
    )
    .await
    .expect("blobdef ddl");
    let mut bdi = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_BLOB_DEFAULT_INSERT,
        vec![],
    );
    bdi.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-blobdef-ins", vec![bdi]))
        .await
        .expect("blobdef ins");
    let mut bds = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_BLOB_DEFAULT_SELECT,
        vec![],
    );
    bds.max_rows = 8;
    let blob = run_postgres_binding(&db, binding_req("pg-blobdef-sel", vec![bds]))
        .await
        .expect("blobdef sel");
    assert_eq!(
        blob.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
    );

    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-textord",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_TEXT_ORDER,
                vec![],
            )],
        ),
    )
    .await
    .expect("textord ddl");
    for (op, sql) in [
        (
            "pg-to-b",
            bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_INSERT_B,
        ),
        (
            "pg-to-a",
            bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_INSERT_A,
        ),
        (
            "pg-to-eac",
            bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_INSERT_EACUTE,
        ),
        (
            "pg-to-e",
            bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_INSERT_E,
        ),
    ] {
        let mut ins = binding_stmt(sql, vec![]);
        ins.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
        run_postgres_binding(&db, binding_req(op, vec![ins]))
            .await
            .expect(op);
    }
    let mut tos = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_SELECT,
        vec![],
    );
    tos.max_rows = 8;
    let ordered = run_postgres_binding(&db, binding_req("pg-text-ord", vec![tos]))
        .await
        .expect("text ord");
    if let Some(err) = bookclerk_db_exec::sql_v1::portable_rows_mismatch(
        &ordered.statements[0],
        bookclerk_db_exec::sql_v1::portable_text_order_expects(),
        "text order",
    ) {
        panic!("{err}");
    }
    let mut ops = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_TEXT_OPS, vec![]);
    ops.max_rows = 8;
    let tops = run_postgres_binding(&db, binding_req("pg-text-ops", vec![ops]))
        .await
        .expect("text ops");
    if let Some(err) = bookclerk_db_exec::sql_v1::portable_statement_mismatch(
        &tops.statements[0],
        bookclerk_db_exec::sql_v1::portable_text_ops_expects(),
        "text ops",
    ) {
        panic!("{err}");
    }
    let mut multi = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_TEXT_MULTILINGUAL,
        vec![],
    );
    multi.max_rows = 8;
    let mrows = run_postgres_binding(&db, binding_req("pg-text-multi", vec![multi]))
        .await
        .expect("text multilingual");
    if let Some(err) = bookclerk_db_exec::sql_v1::portable_statement_mismatch(
        &mrows.statements[0],
        bookclerk_db_exec::sql_v1::portable_text_multilingual_expects(),
        "text multilingual",
    ) {
        panic!("{err}");
    }

    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-ident-p1",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_IDENTITY,
                vec![],
            )],
        ),
    )
    .await
    .expect("ident ddl");
    let mut ok = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_EXPLICIT,
        vec![],
    );
    ok.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    let mut dup = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_EXPLICIT,
        vec![],
    );
    dup.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ident-rollback", vec![ok, dup]))
        .await
        .expect_err("unique conflict must abort");
    let mut omit = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_OMIT,
        vec![],
    );
    omit.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ident-omit-rb", vec![omit]))
        .await
        .expect("omit after rollback");
    let mut mx = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_SELECT_MAX,
        vec![],
    );
    mx.max_rows = 8;
    let maxed = run_postgres_binding(&db, binding_req("pg-ident-max-rb", vec![mx]))
        .await
        .expect("max after rollback");
    assert_eq!(
        maxed.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Int64(1)
    );

    run_postgres_binding(
        &db,
        binding_req(
            "pg-ident-drop",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_DROP,
                vec![],
            )],
        ),
    )
    .await
    .expect("drop ident");
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ident-recreate",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_IDENTITY,
                vec![],
            )],
        ),
    )
    .await
    .expect("recreate ident");
    let mut omit2 = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_OMIT,
        vec![],
    );
    omit2.result_selection = bookclerk_plugin_abi::DbResultSelection::AffectedRows;
    run_postgres_binding(&db, binding_req("pg-ident-omit-re", vec![omit2]))
        .await
        .expect("omit after recreate");
    let mut mx2 = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_SELECT_MAX,
        vec![],
    );
    mx2.max_rows = 8;
    let maxed2 = run_postgres_binding(&db, binding_req("pg-ident-max-re", vec![mx2]))
        .await
        .expect("max after recreate");
    assert_eq!(
        maxed2.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Int64(1)
    );

    let err = run_postgres_binding(
        &db,
        binding_req(
            "pg-where-int",
            vec![binding_stmt("SELECT 1 WHERE 1", vec![])],
        ),
    )
    .await
    .expect_err("WHERE 1 is not BOOLEAN");
    assert!(
        err.to_string().contains("BOOLEAN") || err.to_string().contains("invalid"),
        "{err}"
    );
    sea_orm::ConnectionTrait::execute_raw(
        &db,
        sea_orm::Statement::from_string(
            sea_orm::ConnectionTrait::get_database_backend(&db),
            "CREATE TABLE orphaned (n INTEGER)".to_string(),
        ),
    )
    .await
    .expect("physical orphan");
    let err = run_postgres_binding(
        &db,
        binding_req(
            "pg-adopt-orphan",
            vec![binding_stmt(
                "CREATE TABLE IF NOT EXISTS orphaned (n INTEGER)",
                vec![],
            )],
        ),
    )
    .await
    .expect_err("must not adopt orphan physical table");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("physical") || msg.contains("adopt") || msg.contains("catalog"),
        "{err}"
    );
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ddl-typed",
            vec![binding_stmt(
                bookclerk_db_exec::sql_v1::BINDING_DDL_AUTOINCREMENT_BLOB,
                vec![],
            )],
        ),
    )
    .await
    .expect("typed ddl");
    let mut edges = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_RUNTIME_EDGES, vec![]);
    edges.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-runtime-edges", vec![edges]))
        .await
        .expect("runtime edges");
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_runtime_edges_mismatch(&reply.statements[0])
    {
        panic!("{err}");
    }
    let mut overflow = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_INTEGER_OVERFLOW, vec![]);
    overflow.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-integer-overflow", vec![overflow]))
        .await
        .expect("integer overflow");
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_integer_overflow_mismatch(&reply.statements[0])
    {
        panic!("{err}");
    }
    let mut nested = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_NESTED_INTEGER_ARITH,
        vec![],
    );
    nested.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-nested-arith", vec![nested]))
        .await
        .expect("nested integer arith");
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_nested_integer_arith_mismatch(&reply.statements[0])
    {
        panic!("{err}");
    }
    let mut nested_b = binding_stmt(
        "SELECT ? + abs(?)",
        vec![
            bookclerk_plugin_abi::DbValue::Int64(1),
            bookclerk_plugin_abi::DbValue::Int64(-2),
        ],
    );
    nested_b.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-nested-arith-binds", vec![nested_b]))
        .await
        .expect("nested arith binds");
    assert_eq!(
        reply.statements[0].rows[0].values[0],
        bookclerk_plugin_abi::DbValue::Int64(3)
    );
    let mut path_cmp = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_JSON_PATH_LIKE_COMPARE,
        vec![],
    );
    path_cmp.max_rows = 8;
    let reply = run_postgres_binding(
        &db,
        binding_req("pg-json-path-like-compare", vec![path_cmp]),
    )
    .await
    .expect("json-path-like compare");
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_json_path_like_compare_mismatch(&reply.statements[0])
    {
        panic!("{err}");
    }
    let mut div = binding_stmt(bookclerk_db_exec::sql_v1::PORTABLE_DIV_OPERANDS, vec![]);
    div.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-div-operands", vec![div]))
        .await
        .expect("div operands");
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_div_operands_mismatch(&reply.statements[0])
    {
        panic!("{err}");
    }
    run_postgres_binding(
        &db,
        binding_req(
            "pg-div-qual-ddl",
            vec![binding_stmt(
                "CREATE TABLE IF NOT EXISTS divops (n INTEGER)",
                vec![],
            )],
        ),
    )
    .await
    .expect("divops ddl");
    run_postgres_binding(
        &db,
        binding_req(
            "pg-div-qual-ins",
            vec![binding_stmt("INSERT INTO divops (n) VALUES (0)", vec![])],
        ),
    )
    .await
    .expect("divops insert");
    let mut qdiv = binding_stmt(
        "SELECT 10 / abs(n) AS d0, 10 / t.n AS d1, 10 / -n AS d2, 10 / CAST(n AS INTEGER) AS d3, 10 / (n + 0) AS d4 FROM divops t",
        vec![],
    );
    qdiv.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-div-qualified", vec![qdiv]))
        .await
        .expect("qualified div");
    assert!(
        reply.statements[0].rows[0]
            .values
            .iter()
            .all(|v| matches!(v, bookclerk_plugin_abi::DbValue::Null(_))),
        "{:?}",
        reply.statements[0].rows[0].values
    );
    let mut prefixes = binding_stmt(
        bookclerk_db_exec::sql_v1::PORTABLE_TEXT_PREFIX_LITERALS,
        vec![],
    );
    prefixes.max_rows = 8;
    let reply = run_postgres_binding(&db, binding_req("pg-text-prefixes", vec![prefixes]))
        .await
        .expect("text prefixes");
    if let Some(err) =
        bookclerk_db_exec::sql_v1::portable_text_prefix_literals_mismatch(&reply.statements[0])
    {
        panic!("{err}");
    }
    run_postgres_binding(
        &db,
        binding_req(
            "pg-ins-dest-ddl",
            vec![binding_stmt(
                "CREATE TABLE IF NOT EXISTS dest_int (n INTEGER)",
                vec![],
            )],
        ),
    )
    .await
    .expect("dest ddl");
    let err = run_postgres_binding(
        &db,
        binding_req(
            "pg-ins-sel-text",
            vec![binding_stmt("INSERT INTO dest_int(n) SELECT 'x'", vec![])],
        ),
    )
    .await
    .expect_err("INSERT SELECT TEXT into INTEGER");
    assert!(
        err.to_string().contains("incompatible") || err.to_string().contains("invalid"),
        "{err}"
    );
    let err = run_postgres_binding(
        &db,
        binding_req(
            "pg-fp-mismatch",
            vec![binding_stmt(
                "CREATE TABLE IF NOT EXISTS dest_int (n TEXT)",
                vec![],
            )],
        ),
    )
    .await
    .expect_err("CREATE IF NOT EXISTS fingerprint mismatch");
    assert!(
        err.to_string().contains("does not match") || err.to_string().contains("invalid"),
        "{err}"
    );
    let mut rec = binding_stmt(
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
        vec![],
    );
    rec.max_rows = 8;
    let rec_reply = run_postgres_binding(&db, binding_req("pg-recursive-cte", vec![rec]))
        .await
        .expect("recursive CTE");
    assert_eq!(rec_reply.statements[0].rows.len(), 3);
}
