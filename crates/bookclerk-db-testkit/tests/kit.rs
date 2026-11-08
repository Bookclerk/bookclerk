//! The kit behaves like an external database adapter: it refuses `begin()`,
//! enforces the profile on every batch and surfaces injected faults.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used)]

use bookclerk_db_testkit::{fixtures, Profile, TestDatabase, INTERACTIVE_TRANSACTIONS_REFUSED};
use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, DbValue, ExecuteRequest, PluginErrorCode,
    TypedDbStatement, HOST_MIN_STATEMENTS,
};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};

/// An execute statement with `parameters`.
fn exec(sql: &str, parameters: Vec<DbValue>) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.to_string(),
        parameters,
        kind: DbPlanStatementKind::Execute,
        max_rows: 0,
        result_selection: DbResultSelection::AffectedRows,
    }
}

/// A row-returning statement capped at `max_rows`.
fn query(sql: &str, parameters: Vec<DbValue>, max_rows: u32) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.to_string(),
        parameters,
        kind: DbPlanStatementKind::Select,
        max_rows,
        result_selection: DbResultSelection::Rows,
    }
}

/// A batch with an empty hash; the kit stamps it.
fn request(operation_id: &str, statements: Vec<TypedDbStatement>) -> ExecuteRequest {
    ExecuteRequest {
        operation_id: operation_id.to_string(),
        request_hash: String::new(),
        statements,
        deadline_unix_ms: 0,
    }
}

/// Inserts one discover candidate row keyed `key` carrying `json`.
fn insert_candidate(key: &str, json: &str) -> TypedDbStatement {
    exec(
        "INSERT INTO discover_candidates \
         (subject_key, source, candidate_key, seed_scope, candidate_json, fetched_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            DbValue::Text("user:1".into()),
            DbValue::Text("libro".into()),
            DbValue::Text(key.into()),
            DbValue::Text("own".into()),
            DbValue::Text(json.into()),
            DbValue::Text("2026-01-01T00:00:00Z".into()),
        ],
    )
}

/// Candidate keys present for the test subject, through the connection.
async fn candidate_keys(db: &TestDatabase) -> Vec<String> {
    let rows = db
        .connection()
        .query_all_raw(Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT candidate_key FROM discover_candidates ORDER BY candidate_key",
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|row| row.try_get::<String>("", "candidate_key").unwrap())
        .collect()
}

#[tokio::test]
async fn every_profile_opens_and_meets_the_host_minimums() {
    for db in TestDatabase::open_all().await.unwrap() {
        let caps = db.capabilities();
        assert!(
            caps.meets_host_minimums(),
            "{}: {}",
            db.profile().name(),
            caps.capability_failure_reason()
        );
        assert_eq!(Profile::parse(db.profile().name()), Some(db.profile()));
        assert_eq!(db.refused_begins(), 0);
        assert!(db.batch_sizes().is_empty());
    }
    assert_eq!(Profile::parse("sqlite"), None);
}

#[tokio::test]
async fn begin_is_refused_and_the_connection_recovers() {
    let db = TestDatabase::open(Profile::FirstParty).await.unwrap();
    let probe = || Statement::from_string(sea_orm::DbBackend::Sqlite, "SELECT 1 AS one");
    let txn = db.connection().begin().await.unwrap();
    let err = txn
        .query_one_raw(probe())
        .await
        .expect_err("statements inside a refused transaction fail");
    assert!(
        err.to_string().contains(INTERACTIVE_TRANSACTIONS_REFUSED),
        "{err}"
    );
    txn.rollback().await.unwrap();
    assert_eq!(db.refused_begins(), 1);
    // The refusal is not sticky: the connection keeps working.
    db.connection().query_one_raw(probe()).await.unwrap();
    // A dropped transaction clears the fault too.
    let dropped = db.connection().begin().await.unwrap();
    drop(dropped);
    db.connection().query_one_raw(probe()).await.unwrap();
    assert_eq!(db.refused_begins(), 2);
}

#[tokio::test]
async fn batches_run_as_one_transaction_without_an_interactive_begin() {
    for db in TestDatabase::open_all().await.unwrap() {
        let reply = db
            .execute_batch(request(
                "insert-two",
                vec![
                    insert_candidate("a", "{}"),
                    insert_candidate("b", "{}"),
                    query(
                        "SELECT candidate_key FROM discover_candidates WHERE subject_key = ? \
                         ORDER BY candidate_key",
                        vec![DbValue::Text("user:1".into())],
                        1,
                    ),
                ],
            ))
            .await;
        // Two rows do not fit a one-row statement; the whole batch is refused.
        let err = reply.unwrap_err();
        assert_eq!(
            err.code,
            PluginErrorCode::LimitExceeded,
            "{}: {err}",
            db.profile().name()
        );
        assert_eq!(
            err.limit().unwrap().limit,
            "maxResultRows",
            "{}: {err}",
            db.profile().name()
        );
        assert!(
            candidate_keys(&db).await.is_empty(),
            "{}: a refused batch left rows behind",
            db.profile().name()
        );
        let reply = db
            .execute_batch(request(
                "insert-two",
                vec![insert_candidate("a", "{}"), insert_candidate("b", "{}")],
            ))
            .await
            .unwrap();
        assert_eq!(reply.statements.len(), 2);
        assert_eq!(candidate_keys(&db).await, vec!["a", "b"]);
        assert_eq!(db.refused_begins(), 0, "{}", db.profile().name());
        assert_eq!(db.batch_sizes(), vec![3, 2]);
    }
}

#[tokio::test]
async fn the_minimum_profile_refuses_a_long_batch_and_a_wide_cell() {
    let db = TestDatabase::open(Profile::Minimum).await.unwrap();
    let caps = db.capabilities().clone();
    let count = usize::try_from(HOST_MIN_STATEMENTS).unwrap() + 1;
    let statements: Vec<TypedDbStatement> = (0..count)
        .map(|i| insert_candidate(&format!("k{i}"), "{}"))
        .collect();
    let err = db
        .execute_batch(request("too-long", statements))
        .await
        .unwrap_err();
    assert_eq!(err.limit().unwrap().limit, "maxStatements", "{err}");

    // A cell over the cell cap never fits the statement payload either, so
    // the payload cap answers first, before anything reaches the engine.
    let wide = fixtures::description_over_cell_cap(&caps);
    let err = db
        .execute_batch(request("wide-cell", vec![insert_candidate("wide", &wide)]))
        .await
        .unwrap_err();
    assert_eq!(err.limit().unwrap().limit, "maxPayloadBytes", "{err}");
    assert!(candidate_keys(&db).await.is_empty());

    // A description that leaves room for the SQL and the other binds fits.
    let fits = fixtures::description_near_cell_cap(&caps, 512);
    db.execute_batch(request("fits", vec![insert_candidate("fits", &fits)]))
        .await
        .unwrap();
    assert_eq!(candidate_keys(&db).await, vec!["fits"]);

    // Eight rows that each fit the payload cap do not fit a one-row result;
    // one row reads back alone.
    let rows: Vec<TypedDbStatement> = fixtures::rows_over_result_cap(&caps, "r", 8)
        .into_iter()
        .map(|(key, json)| insert_candidate(&key, &json))
        .collect();
    assert_eq!(rows.len(), 8);
    for row in rows {
        db.execute_batch(request("one", vec![row])).await.unwrap();
    }
    let read = |max_rows: u32, prefix: &str| {
        request(
            "read",
            vec![query(
                "SELECT candidate_json FROM discover_candidates \
                 WHERE candidate_key LIKE ? ORDER BY candidate_key",
                vec![DbValue::Text(format!("{prefix}%"))],
                max_rows,
            )],
        )
    };
    let err = db.execute_batch(read(8, "r")).await.unwrap_err();
    assert_eq!(err.limit().unwrap().limit, "maxResultRows", "{err}");
    let one = db.execute_batch(read(1, "fits")).await.unwrap();
    assert_eq!(one.statements[0].rows.len(), 1);
}

#[tokio::test]
async fn a_lost_reply_leaves_the_batch_applied() {
    let db = TestDatabase::open(Profile::D1).await.unwrap();
    db.lose_next_replies(1);
    let err = db
        .execute_batch(request("lost", vec![insert_candidate("a", "{}")]))
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Unavailable, "{err}");
    assert_eq!(candidate_keys(&db).await, vec!["a"]);
    // The next batch answers normally.
    db.execute_batch(request("next", vec![insert_candidate("b", "{}")]))
        .await
        .unwrap();
    assert_eq!(candidate_keys(&db).await, vec!["a", "b"]);
}

#[tokio::test]
async fn lock_contention_refuses_the_batch_before_it_is_applied() {
    let db = TestDatabase::open(Profile::FirstParty).await.unwrap();
    db.lock_next_requests(2);
    for _ in 0..2 {
        let err = db
            .execute_batch(request("locked", vec![insert_candidate("a", "{}")]))
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::Unavailable, "{err}");
        assert!(err.message.contains("SQLITE_BUSY"), "{err}");
    }
    assert!(candidate_keys(&db).await.is_empty());
    db.execute_batch(request("locked", vec![insert_candidate("a", "{}")]))
        .await
        .unwrap();
    assert_eq!(candidate_keys(&db).await, vec!["a"]);
    assert_eq!(db.batch_sizes(), vec![1, 1, 1]);
}

#[tokio::test]
async fn a_result_over_the_byte_cap_is_a_typed_limit_error() {
    let db = TestDatabase::open(Profile::FirstParty).await.unwrap();
    let caps = db.capabilities().clone();
    for (key, json) in fixtures::rows_over_result_cap(&caps, "r", 8) {
        db.execute_batch(request("one", vec![insert_candidate(&key, &json)]))
            .await
            .unwrap();
    }
    let err = db
        .execute_batch(request(
            "read",
            vec![query(
                "SELECT candidate_json FROM discover_candidates ORDER BY candidate_key",
                vec![],
                8,
            )],
        ))
        .await
        .unwrap_err();
    let limit = err.limit().unwrap();
    assert_eq!(limit.limit, "maxResultBytes", "{err}");
    assert_eq!(limit.cap, u64::from(caps.max_result_bytes));
    assert_eq!(candidate_keys(&db).await.len(), 8);
}
