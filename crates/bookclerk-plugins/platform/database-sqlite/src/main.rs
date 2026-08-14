//! Platform SQLite database plugin guest.

use async_trait::async_trait;
use bookclerk_db_guest::{
    guest_atomic, guest_begin, guest_commit, guest_execute, guest_ping, guest_query,
    guest_rollback, set_connection,
};
use bookclerk_plugin_sdk::{
    upload_file_path, BookclerkPlugin, BookclerkPluginGuest, DbAtomicRequest, DbAtomicResult,
    DbBeginParams, DbBeginResult, DbConnectParams, DbConnectResult, DbTxnParams, DiagnoseResult,
    ExecResultDto, HandshakeParams, HandshakeResult, HealthResult, PluginError, QueryResultDto,
    StatementDto, PLUGIN_API_VERSION,
};

/// External SQLite database guest (`kind = database`, id `sqlite`).
struct SqlitePlugin;

#[async_trait]
impl BookclerkPlugin for SqlitePlugin {
    async fn handshake(&self, _params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        Ok(HandshakeResult {
            api_version: PLUGIN_API_VERSION,
            id: "sqlite".into(),
            kind: "database".into(),
            display_name: Some("SQLite".into()),
            capabilities: vec![
                "health".into(),
                "diagnose".into(),
                "dbConnect".into(),
                "dbPing".into(),
                "dbQuery".into(),
                "dbExecute".into(),
                "dbBegin".into(),
                "dbCommit".into(),
                "dbRollback".into(),
                "dbAtomic".into(),
            ],
            sort_key: Some(5),
            ..HandshakeResult::default()
        })
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            id: Some("sqlite".into()),
            enabled: Some(true),
            detail: Some("sqlite database plugin ready".into()),
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult {
            lines: vec!["sqlite database plugin diagnose: ok".into()],
        })
    }

    async fn db_connect(&self, params: DbConnectParams) -> Result<DbConnectResult, PluginError> {
        let DbConnectParams::Sqlite {
            plugin_data_dir: _,
            sqlite_path,
        } = params
        else {
            return Err(PluginError::invalid_params(
                "sqlite guest received non-sqlite dbConnect params",
            ));
        };
        let path = upload_file_path(sqlite_path.as_deref())
            .map_err(|e| PluginError::internal(e.to_string()))?;
        let db = bookclerk_plugin_database_sqlite::open(path.as_ref())
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        set_connection(db).await;
        Ok(DbConnectResult::sqlite())
    }

    async fn db_ping(&self) -> Result<(), PluginError> {
        guest_ping().await.map_err(PluginError::internal)
    }

    async fn db_query(&self, params: StatementDto) -> Result<QueryResultDto, PluginError> {
        guest_query(params).await.map_err(PluginError::internal)
    }

    async fn db_execute(&self, params: StatementDto) -> Result<ExecResultDto, PluginError> {
        guest_execute(params).await.map_err(PluginError::internal)
    }

    async fn db_begin(&self, params: DbBeginParams) -> Result<DbBeginResult, PluginError> {
        let txn_id = guest_begin(params.parent_txn_id)
            .await
            .map_err(PluginError::internal)?;
        Ok(DbBeginResult { txn_id })
    }

    async fn db_commit(&self, params: DbTxnParams) -> Result<(), PluginError> {
        guest_commit(params.txn_id)
            .await
            .map_err(PluginError::internal)
    }

    async fn db_rollback(&self, params: DbTxnParams) -> Result<(), PluginError> {
        guest_rollback(params.txn_id)
            .await
            .map_err(PluginError::internal)
    }

    async fn db_atomic(&self, params: DbAtomicRequest) -> Result<DbAtomicResult, PluginError> {
        guest_atomic(params).await.map_err(PluginError::internal)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(SqlitePlugin).await?;
    Ok(())
}
