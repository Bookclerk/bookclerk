//! Guest-side database plugin: holds one SeaORM connection per process.

use bookclerk_plugin_sdk::{
    proxy_rows_to_dto, statement_from_dto, upload_file_path, DbConnectParams, DbConnectResult,
    ExecResultDto, ProxyRowDto, QueryResultDto, StatementDto,
};
use sea_orm::{from_query_result_to_proxy_row, ConnectionTrait, DatabaseConnection};
use tokio::sync::Mutex;

type Result<T> = std::result::Result<T, String>;

static DB: Mutex<Option<DatabaseConnection>> = Mutex::const_new(None);

/// Open the engine matching `params` and return the SeaORM dialect for the host.
pub async fn guest_connect(params: DbConnectParams) -> Result<DbConnectResult> {
    let (conn, dialect) = match params {
        DbConnectParams::Sqlite {
            plugin_data_dir: _,
            sqlite_path,
        } => {
            let path = upload_file_path(sqlite_path.as_deref()).map_err(|e| e.to_string())?;
            let db = crate::sqlite::open(path.as_ref())
                .await
                .map_err(|e| e.to_string())?;
            (db, DbConnectResult::sqlite())
        }
        DbConnectParams::D1 {
            plugin_data_dir: _,
            account_id,
            database_id,
            api_base,
            api_token,
        } => {
            let db = crate::d1::open(api_base, account_id, database_id, api_token)
                .await
                .map_err(|e| e.to_string())?;
            (db, DbConnectResult::sqlite())
        }
        DbConnectParams::Postgres {
            plugin_data_dir: _,
            url,
        } => {
            let db = crate::postgres::open(&url)
                .await
                .map_err(|e| e.to_string())?;
            (db, DbConnectResult::postgres())
        }
    };
    *DB.lock().await = Some(conn);
    Ok(dialect)
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
        out.push(row_to_dto(&row));
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

/// Convert a SeaORM query row into the JSON-RPC DTO (also used by integration tests).
#[must_use]
pub fn row_to_dto(row: &sea_orm::QueryResult) -> ProxyRowDto {
    // Prefer SeaORM's typed ProxyRow map. Decoding via `try_get::<Option<i64>>`
    // first is wrong: TEXT columns succeed as `Ok(None)` and get serialized as
    // JSON null, which the host then fails to decode (e.g. missing `uuid`).
    let proxy = from_query_result_to_proxy_row(row);
    proxy_rows_to_dto(vec![proxy])
        .into_iter()
        .next()
        .expect("proxy_rows_to_dto preserves one row")
}
