//! Platform SQLite database plugin guest.

use async_trait::async_trait;
use bookclerk_db_guest::{guest_execute, guest_ping, guest_query, set_connection};
use bookclerk_plugin_sdk::{
    upload_file_path, BookclerkPlugin, BookclerkPluginGuest, DbConnectParams, DbConnectResult,
    DiagnoseResult, ExecResultDto, HandshakeParams, HandshakeResult, HealthResult, PluginError,
    QueryResultDto, StatementDto, PLUGIN_API_VERSION,
};

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(SqlitePlugin).await?;
    Ok(())
}
