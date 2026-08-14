//! Cloudflare D1 database plugin guest.

use async_trait::async_trait;
use bookclerk_db_guest::{guest_execute, guest_ping, guest_query, set_connection};
use bookclerk_plugin_sdk::{
    BookclerkPlugin, BookclerkPluginGuest, DbAtomicRequest, DbAtomicResult, DbBeginParams,
    DbBeginResult, DbConnectParams, DbConnectResult, DbTxnParams, DiagnoseResult, ExecResultDto,
    HandshakeParams, HandshakeResult, HealthResult, PluginError, QueryResultDto, StatementDto,
    PLUGIN_API_VERSION,
};

/// Cloudflare D1 database guest; handshake advertises query/execute/atomic RPCs.
struct D1Plugin;

#[async_trait]
impl BookclerkPlugin for D1Plugin {
    async fn handshake(&self, _params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        Ok(HandshakeResult {
            api_version: PLUGIN_API_VERSION,
            id: "d1".into(),
            kind: "database".into(),
            display_name: Some("Cloudflare D1".into()),
            capabilities: vec![
                "health".into(),
                "diagnose".into(),
                "dbConnect".into(),
                "dbPing".into(),
                "dbQuery".into(),
                "dbExecute".into(),
                "dbAtomic".into(),
            ],
            sort_key: Some(5),
            ..HandshakeResult::default()
        })
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            id: Some("d1".into()),
            enabled: Some(true),
            detail: Some("d1 database plugin ready".into()),
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult {
            lines: vec!["d1 database plugin diagnose: ok".into()],
        })
    }

    async fn db_connect(&self, params: DbConnectParams) -> Result<DbConnectResult, PluginError> {
        let DbConnectParams::D1 {
            plugin_data_dir: _,
            account_id,
            database_id,
            api_base,
            api_token,
        } = params
        else {
            return Err(PluginError::invalid_params(
                "d1 guest received non-d1 dbConnect params",
            ));
        };
        let db = bookclerk_plugin_database_d1::open(api_base, account_id, database_id, api_token)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        set_connection(db).await;
        Ok(DbConnectResult::d1())
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

    async fn db_begin(&self, _params: DbBeginParams) -> Result<DbBeginResult, PluginError> {
        Err(PluginError::unsupported(
            "D1 does not support interactive transactions; each HTTP request commits immediately. \
             Atomic library operations use dbAtomic (one HTTP batch / one SQL transaction)",
        ))
    }

    async fn db_commit(&self, _params: DbTxnParams) -> Result<(), PluginError> {
        Err(PluginError::unsupported(
            "D1 does not support interactive transactions",
        ))
    }

    async fn db_rollback(&self, _params: DbTxnParams) -> Result<(), PluginError> {
        Err(PluginError::unsupported(
            "D1 does not support interactive transactions",
        ))
    }

    async fn db_atomic(&self, params: DbAtomicRequest) -> Result<DbAtomicResult, PluginError> {
        let proxy = bookclerk_plugin_database_d1::shared_proxy()
            .ok_or_else(|| PluginError::internal("d1 guest is not connected"))?;
        proxy
            .run_atomic(params)
            .await
            .map_err(bookclerk_plugin_database_d1::atomic::plugin_error_from_d1)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(D1Plugin).await?;
    Ok(())
}
