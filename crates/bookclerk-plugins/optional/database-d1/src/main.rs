//! Cloudflare D1 database plugin guest.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_db_guest::{guest_execute, guest_query_page, set_connection};
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
            "d1 database context is missing connect params",
        ));
    } else {
        serde_json::from_str(&ctx.json)
            .map_err(|err| PluginError::invalid_params(err.to_string()))?
    };
    let DbConnectParams::D1 {
        plugin_data_dir: _,
        account_id,
        database_id,
        api_base,
        api_token,
    } = params
    else {
        return Err(PluginError::invalid_params(
            "d1 guest received non-d1 database context",
        ));
    };
    let db = bookclerk_plugin_database_d1::open(api_base, account_id, database_id, api_token)
        .await
        .map_err(|e| PluginError::internal(e.to_string()))?;
    set_connection(db).await;
    Ok(())
}

/// Cloudflare D1 database guest; `describe` advertises query/execute/atomic RPCs.
struct D1Root;

#[async_trait(?Send)]
impl PluginRoot for D1Root {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "d1".into(),
            kind: "database".into(),
            display_name: Some("Cloudflare D1".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            supported_roles: vec!["database".into()],
            metadata_json: describe_metadata()?,
            ..PluginDescribe::default()
        })
    }

    async fn database(&self, context: DatabaseContext) -> Result<Box<dyn Database>, PluginError> {
        connect_from_context(&context).await?;
        Ok(Box::new(D1Database))
    }
}

struct D1Database;

#[async_trait(?Send)]
impl Database for D1Database {
    async fn open_session(&self) -> Result<Box<dyn DatabaseSession>, PluginError> {
        Ok(Box::new(D1Session))
    }
}

struct D1Session;

#[async_trait(?Send)]
impl DatabaseSession for D1Session {
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
            let proxy = bookclerk_plugin_database_d1::shared_proxy()
                .ok_or_else(|| PluginError::internal("d1 guest is not connected"))?;
            let result = proxy
                .run_atomic(req)
                .await
                .map_err(bookclerk_plugin_database_d1::atomic::plugin_error_from_d1)?;
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
        Err(PluginError::unsupported(
            "D1 does not support interactive transactions; each HTTP request commits immediately. \
             Atomic library operations use bookclerk.atomic (one HTTP batch / one SQL transaction)",
        ))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(D1Root).await?;
    Ok(())
}
