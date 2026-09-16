//! Guest session transactions: begin/execute/rollback over the shared SeaORM session.

use std::sync::LazyLock;

use bookclerk_db_guest::{
    guest_begin, guest_commit, guest_execute, guest_query, guest_rollback, set_connection,
    GuestStatement,
};
use tokio::sync::Mutex;

static SESSION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn stmt(sql: &str, txn_id: Option<String>) -> GuestStatement {
    GuestStatement {
        sql: sql.into(),
        values: Vec::new(),
        txn_id,
    }
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
    assert_eq!(inside.rows.len(), 1);

    guest_rollback(txn).await.unwrap();

    let master = guest_query(stmt(
        "SELECT name FROM sqlite_master WHERE name = 'rpc_txn_test'",
        None,
    ))
    .await
    .unwrap();
    assert_eq!(master.rows.len(), 0);
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
    assert_eq!(master.rows.len(), 1);
}

#[tokio::test]
async fn guest_nested_savepoint_rollback() {
    let _lock = SESSION_LOCK.lock().await;
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .unwrap();
    set_connection(db).await;

    let root = guest_begin(None).await.unwrap();
    guest_execute(stmt(
        "CREATE TABLE rpc_txn_nested (id INTEGER PRIMARY KEY)",
        Some(root.clone()),
    ))
    .await
    .unwrap();
    guest_execute(stmt(
        "INSERT INTO rpc_txn_nested (id) VALUES (1)",
        Some(root.clone()),
    ))
    .await
    .unwrap();
    let nested = guest_begin(Some(root.clone())).await.unwrap();
    guest_execute(stmt(
        "INSERT INTO rpc_txn_nested (id) VALUES (2)",
        Some(nested.clone()),
    ))
    .await
    .unwrap();
    let rows = guest_query(stmt(
        "SELECT id FROM rpc_txn_nested ORDER BY id",
        Some(nested.clone()),
    ))
    .await
    .unwrap();
    assert_eq!(rows.rows.len(), 2);
    guest_rollback(nested).await.unwrap();
    let persisted = guest_query(stmt("SELECT id FROM rpc_txn_nested", Some(root.clone())))
        .await
        .unwrap();
    assert_eq!(persisted.rows.len(), 1);
    guest_rollback(root).await.unwrap();
}

#[tokio::test]
async fn guest_nested_savepoint_commit() {
    let _lock = SESSION_LOCK.lock().await;
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .unwrap();
    set_connection(db).await;

    let root = guest_begin(None).await.unwrap();
    guest_execute(stmt(
        "CREATE TABLE rpc_txn_nested_commit (id INTEGER PRIMARY KEY)",
        Some(root.clone()),
    ))
    .await
    .unwrap();
    let nested = guest_begin(Some(root.clone())).await.unwrap();
    guest_execute(stmt(
        "INSERT INTO rpc_txn_nested_commit (id) VALUES (1)",
        Some(nested.clone()),
    ))
    .await
    .unwrap();
    guest_commit(nested).await.unwrap();
    guest_commit(root).await.unwrap();

    let master = guest_query(stmt("SELECT id FROM rpc_txn_nested_commit", None))
        .await
        .unwrap();
    assert_eq!(master.rows.len(), 1);
}
