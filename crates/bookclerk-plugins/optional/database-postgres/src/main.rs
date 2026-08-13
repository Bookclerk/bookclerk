//! PostgreSQL database plugin guest.

use async_trait::async_trait;
use bookclerk_db_guest::{
    guest_begin, guest_commit, guest_execute, guest_ping, guest_query, guest_rollback,
    set_connection,
};
use bookclerk_plugin_sdk::{
    BookclerkPlugin, BookclerkPluginGuest, DbBeginParams, DbBeginResult, DbConnectParams,
    DbConnectResult, DbTxnParams, DiagnoseResult, ExecResultDto, HandshakeParams, HandshakeResult,
    HealthResult, PluginError, QueryResultDto, StatementDto, PLUGIN_API_VERSION,
};

struct PostgresPlugin;

#[async_trait]
impl BookclerkPlugin for PostgresPlugin {
    async fn handshake(&self, _params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        Ok(HandshakeResult {
            api_version: PLUGIN_API_VERSION,
            id: "postgres".into(),
            kind: "database".into(),
            display_name: Some("PostgreSQL".into()),
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
            ],
            sort_key: Some(5),
            ..HandshakeResult::default()
        })
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            id: Some("postgres".into()),
            enabled: Some(true),
            detail: Some("postgres database plugin ready".into()),
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult {
            lines: vec!["postgres database plugin diagnose: ok".into()],
        })
    }

    async fn db_connect(&self, params: DbConnectParams) -> Result<DbConnectResult, PluginError> {
        let DbConnectParams::Postgres {
            plugin_data_dir: _,
            url,
        } = params
        else {
            return Err(PluginError::invalid_params(
                "postgres guest received non-postgres dbConnect params",
            ));
        };
        let db = bookclerk_plugin_database_postgres::open(&url)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        set_connection(db).await;
        Ok(DbConnectResult::postgres())
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(PostgresPlugin).await?;
    Ok(())
}
