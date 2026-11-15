//! The typed executor remembers a SQLite file's catalog snapshot between
//! batches; a schema change made by any connection must still be seen by
//! the next batch.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bookclerk_plugin_abi::{
    DbCapabilities, DbPlanStatementKind, DbResultSelection, DbValue, ExecuteRequest, SqlTypeEnv,
    TypedDbStatement,
};

/// A host execute request of one `SELECT` returning up to 10 rows.
fn select(sql: &str) -> ExecuteRequest {
    ExecuteRequest {
        operation_id: "memo".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.into(),
            parameters: Vec::<DbValue>::new(),
            kind: DbPlanStatementKind::Select,
            max_rows: 10,
            result_selection: DbResultSelection::Rows,
        }],
        ..Default::default()
    }
}

/// Runs `sql` as a host batch typed against `env`.
async fn run(
    db: &sea_orm::DatabaseConnection,
    env: &SqlTypeEnv,
    sql: &str,
) -> Result<bookclerk_plugin_abi::ExecuteReply, sea_orm::DbErr> {
    let envelope = bookclerk_db_exec::stamp_adapter_execute(select(sql), env)?;
    let caps = bookclerk_db_exec::ExecCaps::from_capabilities(&DbCapabilities::advertised_sqlite());
    let session = bookclerk_db_exec::AtomicSession::from_deadline(None)
        .with_type_env(bookclerk_plugin_abi::sql_host_bookkeeping_type_env());
    bookclerk_db_exec::execute_typed_envelope_on_connection(db, &envelope, caps, session).await
}

#[tokio::test]
async fn a_table_created_by_another_connection_is_seen_by_the_next_batch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(
        bookclerk_plugin_database_sqlite::open_store(&path)
            .await
            .unwrap(),
    );
    let db = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
    let host = bookclerk_library::migrations::host_sql_type_env();
    // Remembers the snapshot of the library as it is.
    run(&db, &host, "SELECT id FROM works LIMIT 1")
        .await
        .unwrap();

    let ddl = "CREATE TABLE memo_probe (id TEXT NOT NULL PRIMARY KEY, label TEXT NOT NULL)";
    let other = rusqlite::Connection::open(&path).unwrap();
    other.execute_batch(ddl).unwrap();
    other
        .execute(
            "INSERT INTO memo_probe (id, label) VALUES ('a', 'first')",
            [],
        )
        .unwrap();
    drop(other);

    let mut env = host.clone();
    env.merge(&bookclerk_plugin_abi::sql_type_env_from_canonical_statements([ddl]));
    let reply = run(&db, &env, "SELECT label FROM memo_probe WHERE id = 'a'")
        .await
        .unwrap();
    assert_eq!(reply.statements[0].rows.len(), 1, "{reply:?}");
}

/// The snapshot key `db` reads now.
async fn key(db: &sea_orm::DatabaseConnection) -> (String, String) {
    bookclerk_db_exec::sqlite_snapshot_key(db)
        .await
        .expect("a library file has a snapshot key")
}

#[tokio::test]
async fn the_snapshot_key_changes_with_the_schema_and_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(
        bookclerk_plugin_database_sqlite::open_store(&path)
            .await
            .unwrap(),
    );
    let db = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
    let (file, before) = key(&db).await;
    assert!(file.ends_with("library.db"), "{file}");
    assert_eq!(
        key(&db).await.1,
        before,
        "an unchanged library keeps its key"
    );

    // Rows that are not the catalog leave it alone.
    let other = rusqlite::Connection::open(&path).unwrap();
    other
        .execute_batch("UPDATE works SET title = title WHERE 0")
        .unwrap();
    assert_eq!(key(&db).await.1, before);

    // DDL from another connection changes it.
    other
        .execute_batch("CREATE TABLE memo_probe (id TEXT NOT NULL PRIMARY KEY)")
        .unwrap();
    let created = key(&db).await.1;
    assert_ne!(created, before, "a new table changes the key");

    // So does a ledger row edited without DDL.
    other
        .execute_batch(
            "UPDATE bookclerk_sql_schema SET fingerprint = 'not-the-ledger' \
             WHERE table_name = 'works'",
        )
        .unwrap();
    let tampered = key(&db).await.1;
    assert_ne!(tampered, created, "a ledger edit changes the key");

    // And a catalog row removed without DDL.
    other
        .execute_batch(
            "DELETE FROM bookclerk_sql_catalog WHERE table_name = 'works' AND column_name = 'title'",
        )
        .unwrap();
    assert_ne!(
        key(&db).await.1,
        tampered,
        "a catalog row count change changes the key"
    );
}

#[tokio::test]
async fn an_in_memory_database_is_never_remembered() {
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .unwrap();
    assert_eq!(bookclerk_db_exec::sqlite_snapshot_key(&db).await, None);
}
