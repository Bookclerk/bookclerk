//! Guest-side database plugin: holds one SeaORM connection per process.

use std::collections::BTreeMap;

use bookclerk_config::Config;
use bookclerk_library::{apply_pending_migrations, connect_postgres, connect_sqlite, D1Proxy};
use bookclerk_plugin_sdk::{
    statement_from_dto, upload_file_path, DbConnectParams, ExecResultDto, ProxyRowDto,
    QueryResultDto, StatementDto,
};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend};
use tokio::sync::Mutex;

type Result<T> = std::result::Result<T, String>;

static DB: Mutex<Option<DatabaseConnection>> = Mutex::const_new(None);

pub async fn guest_connect(params: DbConnectParams) -> Result<()> {
    let backend = params.backend.to_ascii_lowercase();
    let conn = match backend.as_str() {
        "sqlite" => {
            let path =
                upload_file_path(params.sqlite_path.as_deref()).map_err(|e| e.to_string())?;
            connect_sqlite(path.as_ref())
                .await
                .map_err(|e| e.to_string())?
        }
        "d1" => {
            let account_id = params
                .account_id
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "d1 connect requires account_id".to_string())?;
            let database_id = params
                .database_id
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "d1 connect requires database_id".to_string())?;
            let token = params
                .d1_api_token
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "d1 connect requires api token from host".to_string())?;
            let api_base = params
                .api_base
                .unwrap_or_else(|| "https://api.cloudflare.com/client/v4".into());
            let proxy = D1Proxy::new(api_base, account_id, database_id, token);
            let db =
                Database::connect_proxy(DbBackend::Sqlite, std::sync::Arc::new(Box::new(proxy)))
                    .await
                    .map_err(|e| e.to_string())?;
            apply_pending_migrations(&db)
                .await
                .map_err(|e| e.to_string())?;
            db
        }
        "postgres" => {
            let url = params
                .postgres_url
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "postgres connect requires url from host".to_string())?;
            let mut config = Config::default();
            config.database.plugin = "postgres".into();
            config.database.postgres.url = Some(url);
            connect_postgres(&config).await.map_err(|e| e.to_string())?
        }
        other => return Err(format!("unsupported database backend `{other}`")),
    };
    *DB.lock().await = Some(conn);
    Ok(())
}

pub async fn guest_ping() -> Result<()> {
    let conn = connection().await?;
    conn.ping().await.map_err(|e| e.to_string())
}

pub async fn guest_query(dto: StatementDto) -> Result<QueryResultDto> {
    let conn = connection().await?;
    let backend = conn.get_database_backend();
    let stmt = statement_from_dto(dto, backend);
    let rows = conn.query_all_raw(stmt).await.map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(row_to_dto(&row)?);
    }
    Ok(QueryResultDto { rows: out })
}

pub async fn guest_execute(dto: StatementDto) -> Result<ExecResultDto> {
    let conn = connection().await?;
    let backend = conn.get_database_backend();
    let stmt = statement_from_dto(dto, backend);
    let result = conn.execute_raw(stmt).await.map_err(|e| e.to_string())?;
    Ok(ExecResultDto {
        last_insert_id: result.last_insert_id(),
        rows_affected: result.rows_affected(),
    })
}

async fn connection() -> Result<DatabaseConnection> {
    DB.lock()
        .await
        .clone()
        .ok_or_else(|| "database not connected — call db.connect first".into())
}

fn row_to_dto(row: &sea_orm::QueryResult) -> Result<ProxyRowDto> {
    let mut values = BTreeMap::new();
    for column in row.column_names() {
        let json = if let Ok(v) = row.try_get::<Option<i64>>("", &column) {
            v.map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null)
        } else if let Ok(v) = row.try_get::<Option<f64>>("", &column) {
            v.map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null)
        } else if let Ok(v) = row.try_get::<Option<String>>("", &column) {
            v.map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null)
        } else if let Ok(v) = row.try_get::<Option<Vec<u8>>>("", &column) {
            v.map(|b| serde_json::Value::String(bookclerk_plugin_sdk::bytes_to_b64_string(&b)))
                .unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Null
        };
        values.insert(column.to_string(), json);
    }
    Ok(ProxyRowDto { values })
}
