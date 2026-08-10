//! Per-process SeaORM connection for a database plugin guest.

use bookclerk_plugin_sdk::{
    proxy_rows_to_dto, statement_from_dto, ExecResultDto, ProxyRowDto, QueryResultDto, StatementDto,
};
use sea_orm::{from_query_result_to_proxy_row, ConnectionTrait, DatabaseConnection};
use tokio::sync::Mutex;

type Result<T> = std::result::Result<T, String>;

static DB: Mutex<Option<DatabaseConnection>> = Mutex::const_new(None);

/// Store the opened engine connection for subsequent ping/query/execute calls.
pub async fn set_connection(conn: DatabaseConnection) {
    *DB.lock().await = Some(conn);
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

/// Convert a SeaORM query row into the RPC DTO (also used by integration tests).
#[must_use]
pub fn row_to_dto(row: &sea_orm::QueryResult) -> ProxyRowDto {
    let proxy = from_query_result_to_proxy_row(row);
    proxy_rows_to_dto(vec![proxy])
        .into_iter()
        .next()
        .expect("proxy_rows_to_dto preserves one row")
}
