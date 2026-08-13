//! Guest session transactions: begin/execute/rollback over the shared SeaORM session.

use std::sync::LazyLock;

use bookclerk_db_guest::{
    guest_begin, guest_commit, guest_execute, guest_query, guest_rollback, set_connection,
};
use bookclerk_plugin_sdk::{QueryResultDto, StatementDto};
use tokio::sync::Mutex;

static SESSION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn stmt(sql: &str, txn_id: Option<String>) -> StatementDto {
    StatementDto {
        sql: sql.into(),
        values: Vec::new(),
        txn_id,
    }
}

fn row_count(result: &QueryResultDto) -> usize {
    result.rows.len()
}

#[tokio::test]
async fn guest_rollback_discards_insert() {
    let _lock = SESSION_LOCK.lock().await;
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .unwrap();
    set_connection(db).await;

    let txn = guest_begin(None).await.unwrap();
    guest_execute(stmt(
        "CREATE TABLE rpc_txn_test (id INTEGER PRIMARY KEY, v TEXT)",
        Some(txn.clone()),
    ))
    .await
    .unwrap();
    guest_execute(stmt(
        "INSERT INTO rpc_txn_test (id, v) VALUES (1, 'x')",
        Some(txn.clone()),
    ))
    .await
    .unwrap();
    let inside = guest_query(stmt("SELECT id FROM rpc_txn_test", Some(txn.clone())))
        .await
        .unwrap();
    assert_eq!(row_count(&inside), 1);

    guest_rollback(txn).await.unwrap();

    let master = guest_query(stmt(
        "SELECT name FROM sqlite_master WHERE name = 'rpc_txn_test'",
        None,
    ))
    .await
    .unwrap();
    assert_eq!(row_count(&master), 0);
}

#[tokio::test]
async fn guest_commit_persists_insert() {
    let _lock = SESSION_LOCK.lock().await;
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .unwrap();
    set_connection(db).await;

    let txn = guest_begin(None).await.unwrap();
    guest_execute(stmt(
        "CREATE TABLE rpc_txn_commit (id INTEGER PRIMARY KEY)",
        Some(txn.clone()),
    ))
    .await
    .unwrap();
    guest_commit(txn).await.unwrap();

    let master = guest_query(stmt(
        "SELECT name FROM sqlite_master WHERE name = 'rpc_txn_commit'",
        None,
    ))
    .await
    .unwrap();
    assert_eq!(row_count(&master), 1);
}

#[tokio::test]
async fn guest_nested_rollback_keeps_outer_writes() {
    let _lock = SESSION_LOCK.lock().await;
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .unwrap();
    set_connection(db).await;

    let outer = guest_begin(None).await.unwrap();
    guest_execute(stmt(
        "CREATE TABLE rpc_txn_nested (id INTEGER PRIMARY KEY, v TEXT)",
        Some(outer.clone()),
    ))
    .await
    .unwrap();
    guest_execute(stmt(
        "INSERT INTO rpc_txn_nested (id, v) VALUES (1, 'outer')",
        Some(outer.clone()),
    ))
    .await
    .unwrap();

    let inner = guest_begin(Some(outer.clone())).await.unwrap();
    guest_execute(stmt(
        "INSERT INTO rpc_txn_nested (id, v) VALUES (2, 'inner')",
        Some(inner.clone()),
    ))
    .await
    .unwrap();
    guest_rollback(inner).await.unwrap();

    let rows = guest_query(stmt(
        "SELECT id FROM rpc_txn_nested ORDER BY id",
        Some(outer.clone()),
    ))
    .await
    .unwrap();
    assert_eq!(row_count(&rows), 1);

    guest_commit(outer).await.unwrap();
    let persisted = guest_query(stmt("SELECT id FROM rpc_txn_nested", None))
        .await
        .unwrap();
    assert_eq!(row_count(&persisted), 1);
}

#[tokio::test]
async fn injected_guest_begin_failure_does_not_open_txn() {
    let _lock = SESSION_LOCK.lock().await;
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .unwrap();
    set_connection(db).await;

    bookclerk_library::inject_begin_failures(1);
    let err = guest_begin(None).await.unwrap_err();
    assert!(err.contains("begin failed"), "{err}");
}

#[tokio::test]
async fn injected_guest_commit_failure_rolls_back() {
    let _lock = SESSION_LOCK.lock().await;
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .unwrap();
    set_connection(db).await;

    let txn = guest_begin(None).await.unwrap();
    guest_execute(stmt(
        "CREATE TABLE rpc_txn_commit_fail (id INTEGER PRIMARY KEY)",
        Some(txn.clone()),
    ))
    .await
    .unwrap();
    bookclerk_library::inject_commit_failures(1);
    let err = guest_commit(txn).await.unwrap_err();
    assert!(err.contains("commit failed"), "{err}");

    let master = guest_query(stmt(
        "SELECT name FROM sqlite_master WHERE name = 'rpc_txn_commit_fail'",
        None,
    ))
    .await
    .unwrap();
    assert_eq!(row_count(&master), 0);
}
