//! ABI v2 database factory for the platform SQLite guest.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_db_guest::{
    guest_atomic, guest_begin, guest_commit, guest_execute, guest_query_page, guest_rollback,
    set_connection,
};
use bookclerk_plugin_sdk::v2::{
    Database, DatabaseContext, DatabaseSession, ExecResult, PluginDescribe, PluginRoot, QueryPage,
    ScalarLimits, Statement, Transaction, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{upload_file_path, DbAtomicRequest, PluginError, StatementDto};

use crate::ID;

type Result<T> = std::result::Result<T, PluginError>;

fn map_guest(err: String) -> PluginError {
    if err.contains("invalid query cursor") {
        PluginError::invalid_cursor(err)
    } else {
        PluginError::internal(err)
    }
}

fn to_dto(statement: &Statement, txn_id: Option<String>) -> StatementDto {
    StatementDto {
        sql: statement.sql.clone(),
        values: serde_json::from_str(&statement.values_json).unwrap_or_default(),
        txn_id,
    }
}

fn exec_from_dto(dto: bookclerk_plugin_sdk::ExecResultDto) -> ExecResult {
    ExecResult {
        last_insert_id: i64::try_from(dto.last_insert_id).unwrap_or(i64::MAX),
        rows_affected: dto.rows_affected,
    }
}

/// Root capability for the platform SQLite database guest.
pub struct SqliteRoot;

#[async_trait(?Send)]
impl PluginRoot for SqliteRoot {
    async fn describe(&self) -> Result<PluginDescribe> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: ID.into(),
            kind: "database".into(),
            display_name: Some("SQLite".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            supported_roles: vec!["database".into()],
            ..PluginDescribe::default()
        })
    }

    async fn database(&self, context: DatabaseContext) -> Result<Box<dyn Database>> {
        connect_from_context(&context).await?;
        Ok(Box::new(SqliteDatabase))
    }
}

async fn connect_from_context(ctx: &DatabaseContext) -> Result<()> {
    let path = std::env::var("BOOKCLERK_SQLITE_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            serde_json::from_str::<serde_json::Value>(&ctx.json)
                .ok()
                .and_then(|v| {
                    v.get("sqlitePath")
                        .and_then(|p| p.as_str())
                        .map(str::to_string)
                })
        })
        .unwrap_or_default();
    let path = upload_file_path(if path.is_empty() {
        None
    } else {
        Some(path.as_str())
    })
    .map_err(|e| PluginError::internal(e.to_string()))?;
    let db = crate::open(path.as_ref())
        .await
        .map_err(|e| PluginError::internal(e.to_string()))?;
    set_connection(db).await;
    Ok(())
}

struct SqliteDatabase;

#[async_trait(?Send)]
impl Database for SqliteDatabase {
    async fn open_session(&self) -> Result<Box<dyn DatabaseSession>> {
        Ok(Box::new(SqliteSession))
    }
}

struct SqliteSession;

#[async_trait(?Send)]
impl DatabaseSession for SqliteSession {
    async fn execute(&self, statement: Statement) -> Result<ExecResult> {
        if statement.sql == "bookclerk.atomic" {
            return Err(PluginError::unsupported(
                "bookclerk.atomic is a query, not execute",
            ));
        }
        let dto = guest_execute(to_dto(&statement, None))
            .await
            .map_err(map_guest)?;
        Ok(exec_from_dto(dto))
    }

    async fn query(&self, statement: Statement, cursor: &str, limit: u32) -> Result<QueryPage> {
        if statement.sql == "bookclerk.atomic" {
            let req: DbAtomicRequest = serde_json::from_str(&statement.values_json)
                .map_err(|e| PluginError::invalid_params(e.to_string()))?;
            let result = guest_atomic(req).await.map_err(map_guest)?;
            return Ok(QueryPage {
                rows_json: bookclerk_plugin_sdk::encode_json(result)?,
                next_cursor: None,
            });
        }
        let page = guest_query_page(to_dto(&statement, None), cursor, limit)
            .await
            .map_err(map_guest)?;
        Ok(page)
    }

    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        let txn_id = guest_begin(None).await.map_err(map_guest)?;
        Ok(Box::new(SqliteTxn { txn_id }))
    }
}

struct SqliteTxn {
    txn_id: String,
}

#[async_trait(?Send)]
impl Transaction for SqliteTxn {
    async fn execute(&self, statement: Statement) -> Result<ExecResult> {
        let dto = guest_execute(to_dto(&statement, Some(self.txn_id.clone())))
            .await
            .map_err(map_guest)?;
        Ok(exec_from_dto(dto))
    }

    async fn query(&self, statement: Statement, cursor: &str, limit: u32) -> Result<QueryPage> {
        let page = guest_query_page(to_dto(&statement, Some(self.txn_id.clone())), cursor, limit)
            .await
            .map_err(map_guest)?;
        Ok(page)
    }

    async fn commit(&self) -> Result<()> {
        guest_commit(self.txn_id.clone()).await.map_err(map_guest)
    }

    async fn rollback(&self) -> Result<()> {
        guest_rollback(self.txn_id.clone()).await.map_err(map_guest)
    }
}
