//! Host→adapter boundary: canonical SQL crosses RPC without physical lowering.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bookclerk_library::TypedAtomicExec;
use bookclerk_plugin_abi::{
    statement_sql_hash, AdapterExecuteRequest, CanonicalExecuteRequest, DbCapabilities,
    DbPlanStatementKind, DbResultSelection, DbValue, ExecuteReply, ExecuteRequest,
    PluginError as AbiPluginError, TypedDbStatement,
};
use sea_orm::{DatabaseBackend, Statement, Value};

struct RecordingAdapter {
    seen_sql: Arc<Mutex<Vec<String>>>,
    inner: SessionSqlite,
}

struct SessionSqlite {
    db: sea_orm::DatabaseConnection,
}

#[async_trait]
impl TypedAtomicExec for RecordingAdapter {
    async fn execute_typed(
        &self,
        envelope: AdapterExecuteRequest,
    ) -> std::result::Result<ExecuteReply, AbiPluginError> {
        envelope
            .require_proofs()
            .map_err(|err| AbiPluginError::invalid_params(err.to_string()))?;
        self.seen_sql.lock().expect("seen sql").extend(
            envelope
                .request
                .statements
                .iter()
                .map(|stmt| stmt.sql.clone()),
        );
        self.inner.execute_typed(envelope).await
    }
}

#[async_trait]
impl TypedAtomicExec for SessionSqlite {
    async fn execute_typed(
        &self,
        envelope: AdapterExecuteRequest,
    ) -> std::result::Result<ExecuteReply, AbiPluginError> {
        let req = envelope.request.clone();
        let reply = bookclerk_db_exec::execute_typed_envelope(
            bookclerk_db_exec::PhysicalEngine::sqlite(),
            &self.db,
            &envelope,
            "sqlite_txn",
            bookclerk_db_exec::ExecCaps::from_capabilities(&DbCapabilities::advertised_sqlite()),
            bookclerk_db_exec::AtomicSession::from_deadline(None)
                .with_type_env(bookclerk_library::migrations::host_sql_type_env()),
        )
        .await
        .map_err(|err| AbiPluginError::unavailable(err.to_string()))?;
        bookclerk_library::validate_execute_reply(
            &req,
            &reply,
            &DbCapabilities::advertised_sqlite(),
        )
        .map_err(|err| AbiPluginError::unavailable(err.to_string()))?;
        Ok(reply)
    }
}

fn like_req() -> ExecuteRequest {
    ExecuteRequest {
        operation_id: "like-boundary".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: "SELECT slot_key FROM db_serialization_slots WHERE slot_key LIKE ?".into(),
            parameters: vec![DbValue::Text("rowcap-%".into())],
            kind: DbPlanStatementKind::Select,
            max_rows: 0,
            result_selection: DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    }
}

#[test]
fn seaorm_proxy_statement_keeps_like_and_question_marks() {
    let sql = "SELECT slot_key FROM db_serialization_slots WHERE slot_key LIKE ?";
    let stmt =
        Statement::from_sql_and_values(DatabaseBackend::Sqlite, sql, [Value::from("rowcap-%")]);
    assert_eq!(stmt.db_backend, DatabaseBackend::Sqlite);
    assert!(stmt.sql.contains("LIKE"), "{}", stmt.sql);
    assert!(!stmt.sql.contains("GLOB"), "{}", stmt.sql);
    assert!(stmt.sql.contains('?'), "{}", stmt.sql);
    assert!(!stmt.sql.contains("$1"), "{}", stmt.sql);
}

#[tokio::test]
async fn host_like_stays_canonical_until_adapter_lowering() {
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .expect("mem db");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let adapter = RecordingAdapter {
        seen_sql: Arc::clone(&seen),
        inner: SessionSqlite { db },
    };

    let mut req = like_req();
    bookclerk_library::authorize_typed_request(&mut req, &DbCapabilities::advertised_sqlite())
        .expect("host admission");
    let canonical_sql = req.statements[0].sql.clone();
    assert!(
        canonical_sql.contains("LIKE"),
        "host admission must not GLOB-lower: {canonical_sql}"
    );
    assert!(
        !canonical_sql.contains("GLOB"),
        "GLOB must not appear on the host/RPC side: {canonical_sql}"
    );
    assert!(canonical_sql.contains('?'), "{canonical_sql}");
    assert!(!canonical_sql.contains("$1"), "{canonical_sql}");

    let proofs = bookclerk_db_exec::stamp_host_proofs(
        &req,
        &bookclerk_library::migrations::host_sql_type_env(),
    )
    .expect("proofs");
    let envelope = CanonicalExecuteRequest::from_desugared(req)
        .bind_proofs(proofs)
        .expect("bind");
    envelope.require_proofs().expect("trust boundary");
    assert_eq!(
        envelope.proofs[0].statement_hash,
        statement_sql_hash(envelope.request.statements[0].sql.trim())
    );

    adapter
        .execute_typed(envelope.clone())
        .await
        .expect("adapter execute");
    let seen_sql = seen.lock().expect("seen").clone();
    assert_eq!(seen_sql.len(), 1);
    assert_eq!(seen_sql[0], envelope.request.statements[0].sql);
    assert!(seen_sql[0].contains("LIKE"), "{}", seen_sql[0]);
    assert!(!seen_sql[0].contains("GLOB"), "{}", seen_sql[0]);
    assert!(seen_sql[0].contains('?'), "{}", seen_sql[0]);
    assert!(!seen_sql[0].contains("$1"), "{}", seen_sql[0]);

    let pg =
        bookclerk_db_exec::lower_canonical_sql(sea_orm::DatabaseBackend::Postgres, &seen_sql[0]);
    assert!(pg.contains("$1"), "postgres adapter owns $n: {pg}");
    assert!(pg.contains("LIKE"), "postgres keeps LIKE: {pg}");
    assert!(!pg.contains("GLOB"), "GLOB is sqlite-family only: {pg}");
    assert!(
        !pg.contains('?') || pg.contains("$1"),
        "physical $n appears only inside postgres lowering: {pg}"
    );

    let sqlite =
        bookclerk_db_exec::lower_canonical_sql(sea_orm::DatabaseBackend::Sqlite, &seen_sql[0]);
    assert!(sqlite.contains("GLOB"), "{sqlite}");
    assert!(!sqlite.contains("$1"), "{sqlite}");
}

#[derive(Debug)]
struct RecordingProxy {
    seen_sql: Arc<Mutex<Vec<String>>>,
    stamped_sql: Arc<Mutex<Vec<String>>>,
}

/// Same frontend as production `RpcDatabaseProxy::query`: SeaORM sqlite-shaped
/// `Statement` → typed request → host authorize → proof stamp.
///
/// The production `PluginSession` / Cap'n path is
/// `production_rpc_proxy_keeps_like_through_*_guest` in
/// `bookclerk-plugin-host` lib tests.
fn proxy_statement_to_typed(
    statement: &Statement,
    kind: DbPlanStatementKind,
) -> std::result::Result<TypedDbStatement, sea_orm::DbErr> {
    let parameters = match &statement.values {
        Some(values) => values
            .0
            .iter()
            .map(bookclerk_db_exec::db_value_from_sea)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sea_orm::DbErr::Custom)?,
        None => Vec::new(),
    };
    Ok(TypedDbStatement {
        sql: statement.sql.clone(),
        parameters,
        kind,
        max_rows: 0,
        result_selection: DbResultSelection::Rows,
    })
}

#[async_trait]
impl sea_orm::ProxyDatabaseTrait for RecordingProxy {
    async fn query(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<Vec<sea_orm::ProxyRow>, sea_orm::DbErr> {
        self.seen_sql
            .lock()
            .expect("seen sql")
            .push(statement.sql.clone());
        let typed = proxy_statement_to_typed(
            &statement,
            bookclerk_library::proxy_read_kind(&statement.sql),
        )?;
        let mut req = ExecuteRequest {
            operation_id: "proxy-query".into(),
            request_hash: String::new(),
            statements: vec![typed],
            deadline_unix_ms: 0,
        };
        bookclerk_library::authorize_typed_request(&mut req, &DbCapabilities::advertised_sqlite())
            .map_err(|err| sea_orm::DbErr::Custom(err.to_string()))?;
        let proofs = bookclerk_db_exec::stamp_host_proofs(
            &req,
            &bookclerk_library::migrations::host_sql_type_env(),
        )
        .map_err(|err| sea_orm::DbErr::Custom(err.to_string()))?;
        let envelope = CanonicalExecuteRequest::from_desugared(req)
            .bind_proofs(proofs)
            .map_err(|err| sea_orm::DbErr::Custom(err.to_string()))?;
        envelope
            .require_proofs()
            .map_err(|err| sea_orm::DbErr::Custom(err.to_string()))?;
        self.stamped_sql
            .lock()
            .expect("stamped sql")
            .push(envelope.request.statements[0].sql.clone());
        Ok(Vec::new())
    }

    async fn execute(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<sea_orm::ProxyExecResult, sea_orm::DbErr> {
        self.seen_sql
            .lock()
            .expect("seen sql")
            .push(statement.sql.clone());
        Ok(sea_orm::ProxyExecResult {
            last_insert_id: 0,
            rows_affected: 0,
        })
    }

    async fn ping(&self) -> std::result::Result<(), sea_orm::DbErr> {
        Ok(())
    }
}

#[tokio::test]
async fn seaorm_rpc_proxy_frontend_keeps_like_and_question_marks() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stamped = Arc::new(Mutex::new(Vec::new()));
    let proxy: Arc<Box<dyn sea_orm::ProxyDatabaseTrait>> = Arc::new(Box::new(RecordingProxy {
        seen_sql: Arc::clone(&seen),
        stamped_sql: Arc::clone(&stamped),
    }));
    let db = sea_orm::Database::connect_proxy(sea_orm::DatabaseBackend::Sqlite, proxy)
        .await
        .expect("proxy");
    sea_orm::ConnectionTrait::query_all_raw(
        &db,
        Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT slot_key FROM db_serialization_slots WHERE slot_key LIKE ?",
            [Value::from("rowcap-%")],
        ),
    )
    .await
    .expect("proxy query");

    let seen_sql = seen
        .lock()
        .expect("seen")
        .iter()
        .filter(|sql| sql.contains("LIKE") || sql.contains("GLOB"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(seen_sql.len(), 1, "{seen_sql:?}");
    assert!(seen_sql[0].contains("LIKE"), "{}", seen_sql[0]);
    assert!(!seen_sql[0].contains("GLOB"), "{}", seen_sql[0]);
    assert!(seen_sql[0].contains('?'), "{}", seen_sql[0]);
    assert!(!seen_sql[0].contains("$1"), "{}", seen_sql[0]);

    let boundary = stamped.lock().expect("stamped").clone();
    assert_eq!(boundary.len(), 1, "{boundary:?}");
    assert!(boundary[0].contains("LIKE"), "{}", boundary[0]);
    assert!(!boundary[0].contains("GLOB"), "{}", boundary[0]);
    assert!(boundary[0].contains('?'), "{}", boundary[0]);
    assert!(!boundary[0].contains("$1"), "{}", boundary[0]);

    let pg = bookclerk_db_exec::lower_canonical_sql(DatabaseBackend::Postgres, &boundary[0]);
    assert!(pg.contains("$1"), "{pg}");
    assert!(pg.contains("LIKE"), "{pg}");
    assert!(!pg.contains("GLOB"), "{pg}");
}

#[tokio::test]
async fn adapter_execute_fails_closed_without_proofs() {
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .expect("mem db");
    let adapter = SessionSqlite { db };
    let envelope = AdapterExecuteRequest::new(like_req(), Default::default());
    let err = adapter
        .execute_typed(envelope)
        .await
        .expect_err("missing proofs must fail closed");
    assert!(err.to_string().to_lowercase().contains("proof"), "{err}");
}
