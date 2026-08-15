//! PostgreSQL database plugin guest.

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
use bookclerk_plugin_sdk::{
    serve, DbAtomicRequest, DbConnectParams, HandshakeResult, PluginError, StatementDto,
};

fn describe_metadata() -> Result<String, PluginError> {
    bookclerk_plugin_sdk::encode_json(HandshakeResult {
        api_version: PRODUCT_API_VERSION,
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
            "dbAtomic".into(),
        ],
        sort_key: Some(5),
        ..HandshakeResult::default()
    })
}

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

async fn connect_from_context(ctx: &DatabaseContext) -> Result<(), PluginError> {
    let params: DbConnectParams = if ctx.json.trim().is_empty() {
        return Err(PluginError::invalid_params(
            "postgres database context is missing connect params",
        ));
    } else {
        serde_json::from_str(&ctx.json)
            .map_err(|err| PluginError::invalid_params(err.to_string()))?
    };
    let DbConnectParams::Postgres {
        plugin_data_dir: _,
        url,
    } = params
    else {
        return Err(PluginError::invalid_params(
            "postgres guest received non-postgres database context",
        ));
    };
    let db = bookclerk_plugin_database_postgres::open(&url)
        .await
        .map_err(|e| PluginError::internal(e.to_string()))?;
    set_connection(db).await;
    Ok(())
}

/// Database guest that opens a Postgres URL and serves SeaORM RPC through `bookclerk-db-guest`.
struct PostgresRoot;

#[async_trait(?Send)]
impl PluginRoot for PostgresRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "postgres".into(),
            kind: "database".into(),
            display_name: Some("PostgreSQL".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            supported_roles: vec!["database".into()],
            metadata_json: describe_metadata()?,
            ..PluginDescribe::default()
        })
    }

    async fn database(&self, context: DatabaseContext) -> Result<Box<dyn Database>, PluginError> {
        connect_from_context(&context).await?;
        Ok(Box::new(PostgresDatabase))
    }
}

struct PostgresDatabase;

#[async_trait(?Send)]
impl Database for PostgresDatabase {
    async fn open_session(&self) -> Result<Box<dyn DatabaseSession>, PluginError> {
        Ok(Box::new(PostgresSession))
    }
}

struct PostgresSession;

#[async_trait(?Send)]
impl DatabaseSession for PostgresSession {
    async fn execute(&self, statement: Statement) -> Result<ExecResult, PluginError> {
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

    async fn query(
        &self,
        statement: Statement,
        cursor: &str,
        limit: u32,
    ) -> Result<QueryPage, PluginError> {
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

    async fn begin(&self) -> Result<Box<dyn Transaction>, PluginError> {
        let txn_id = guest_begin(None).await.map_err(map_guest)?;
        Ok(Box::new(PostgresTxn { txn_id }))
    }
}

struct PostgresTxn {
    txn_id: String,
}

#[async_trait(?Send)]
impl Transaction for PostgresTxn {
    async fn execute(&self, statement: Statement) -> Result<ExecResult, PluginError> {
        let dto = guest_execute(to_dto(&statement, Some(self.txn_id.clone())))
            .await
            .map_err(map_guest)?;
        Ok(exec_from_dto(dto))
    }

    async fn query(
        &self,
        statement: Statement,
        cursor: &str,
        limit: u32,
    ) -> Result<QueryPage, PluginError> {
        let page = guest_query_page(to_dto(&statement, Some(self.txn_id.clone())), cursor, limit)
            .await
            .map_err(map_guest)?;
        Ok(page)
    }

    async fn commit(&self) -> Result<(), PluginError> {
        guest_commit(self.txn_id.clone()).await.map_err(map_guest)
    }

    async fn rollback(&self) -> Result<(), PluginError> {
        guest_rollback(self.txn_id.clone()).await.map_err(map_guest)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(PostgresRoot).await?;
    Ok(())
}
