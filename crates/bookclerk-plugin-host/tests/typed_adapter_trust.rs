//! Trust-boundary tests for typed adapter replies at the plugin-host layer.

use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_library::{compile_named_request, DbAtomicParams, TypedAtomicExec};
use bookclerk_plugin_abi::{
    AdapterExecuteRequest, DbCapabilities, DbColumn, DbPlanStatementKind, DbResultSelection, DbRow,
    DbType, DbValue, ExecuteReply, ExecuteRequest, PluginError as AbiPluginError, PluginErrorCode,
    StatementResult, TypedDbStatement,
};

struct SessionTypedAdapter {
    db: sea_orm::DatabaseConnection,
}

#[async_trait]
impl TypedAtomicExec for SessionTypedAdapter {
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

struct MaliciousAdapter {
    reply: ExecuteReply,
}

#[async_trait]
impl TypedAtomicExec for MaliciousAdapter {
    async fn execute_typed(
        &self,
        _envelope: AdapterExecuteRequest,
    ) -> std::result::Result<ExecuteReply, AbiPluginError> {
        Ok(self.reply.clone())
    }
}

fn rows_reply(operation_id: &str, row_count: usize) -> ExecuteReply {
    let rows = (0..row_count)
        .map(|i| DbRow {
            values: vec![DbValue::Int64(i64::try_from(i).unwrap_or(0))],
        })
        .collect();
    ExecuteReply {
        operation_id: operation_id.into(),
        statements: vec![StatementResult {
            rows,
            columns: vec![DbColumn {
                name: "id".into(),
                db_type: DbType::Int64,
            }],
            rows_affected: 0,
        }],
        timing: Default::default(),
    }
}

#[tokio::test]
async fn external_adapter_replays_named_atomic_after_commit() {
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .expect("mem db");
    let adapter = Arc::new(SessionTypedAdapter { db });
    let compiled = compile_named_request(
        "host-replay-op",
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
    .expect("compile");
    let typed = compiled.clone().into_typed_request("host-replay-op");
    let env = bookclerk_library::migrations::host_sql_type_env();
    let envelope = bookclerk_db_exec::stamp_adapter_execute(typed, &env).expect("stamp proofs");
    let first = adapter
        .execute_typed(envelope.clone())
        .await
        .expect("first");
    let replay = adapter.execute_typed(envelope).await.expect("replay");
    assert_eq!(first.operation_id, replay.operation_id);
    assert_eq!(
        first.statements[0].rows_affected,
        replay.statements[0].rows_affected
    );
    assert_eq!(
        first.statements[0].rows.len(),
        replay.statements[0].rows.len(),
        "lost-response replay must return the same receipt envelope"
    );
}

#[tokio::test]
async fn malicious_adapter_wrong_operation_id_rejected_by_host() {
    let store = bookclerk_library::LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("mem db"),
    )
    .with_db_capabilities(DbCapabilities::advertised_sqlite())
    .with_typed_exec(Arc::new(MaliciousAdapter {
        reply: rows_reply("other-op", 1),
    }));
    let req = ExecuteRequest {
        operation_id: "op-1".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: "SELECT 1".into(),
            parameters: vec![],
            kind: DbPlanStatementKind::Select,
            max_rows: 1,
            result_selection: DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    };
    let err = store
        .execute_guest_atomic(
            req,
            &bookclerk_library::GuestSqlPolicy::allow_tables(["books"]),
        )
        .await
        .expect_err("malicious reply must fail validation");
    assert!(
        err.to_string().to_lowercase().contains("operationid")
            || err.code == PluginErrorCode::Unavailable,
        "{err}"
    );
}

#[tokio::test]
async fn malicious_adapter_statement_count_mismatch_rejected_by_host() {
    let mut reply = rows_reply("op-1", 1);
    reply.statements.push(reply.statements[0].clone());
    let store = bookclerk_library::LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("mem db"),
    )
    .with_db_capabilities(DbCapabilities::advertised_sqlite())
    .with_typed_exec(Arc::new(MaliciousAdapter { reply }));
    let req = ExecuteRequest {
        operation_id: "op-1".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: "SELECT 1".into(),
            parameters: vec![],
            kind: DbPlanStatementKind::Select,
            max_rows: 1,
            result_selection: DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    };
    let err = store
        .execute_guest_atomic(
            req,
            &bookclerk_library::GuestSqlPolicy::allow_tables(["books"]),
        )
        .await
        .expect_err("statement count mismatch must fail");
    assert!(
        err.to_string().to_lowercase().contains("statements")
            || err.code == PluginErrorCode::Unavailable,
        "{err}"
    );
}
