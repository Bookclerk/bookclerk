//! PostgreSQL database plugin guest.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_db_guest::{
    bootstrap_for, capabilities_for, guest_assert_restore_constraints,
    guest_assert_restore_constraints_on, guest_bootstrap, guest_capabilities,
    guest_drop_user_relations, guest_drop_user_relations_on, guest_execute_request,
    guest_execute_request_on, guest_export_identity, guest_export_identity_on,
    guest_import_identity, guest_import_identity_on, guest_list_user_relations,
    guest_list_user_relations_on, guest_prepare_unit_restore, guest_prepare_unit_restore_on,
    host_session, host_session_on, set_connection,
};
use bookclerk_plugin_abi::db::{connect_params_from_context, DbConnectParams};
use bookclerk_plugin_abi::HostAdapterDatabaseSession;
use bookclerk_plugin_sdk::database_adapter::plugin_error_from_engine;
use bookclerk_plugin_sdk::{
    serve, AdapterDatabaseSession, Database, DatabaseContext, PluginDescribe, PluginRoot,
    ScalarLimits, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{
    AdapterExecuteRequest, DbBootstrap, DbCapabilities, DbIdentityHighWater, ExecuteReply,
    PluginError, PluginMetadata,
};

fn describe_metadata() -> Result<String, PluginError> {
    bookclerk_plugin_sdk::encode_json(PluginMetadata {
        api_version: PRODUCT_API_VERSION,
        id: "postgres".into(),
        kind: "database".into(),
        display_name: Some("PostgreSQL".into()),
        capabilities: vec!["health".into(), "diagnose".into()],
        sort_key: Some(5),
        ..PluginMetadata::default()
    })
}

async fn connect_from_context(ctx: &DatabaseContext) -> Result<(), PluginError> {
    let params = connect_params_from_context(ctx)?;
    let DbConnectParams::Postgres { url, .. } = params else {
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

/// Opens a dedicated per-binding connection when the context targets a named
/// plugin database binding.
async fn binding_from_context(
    ctx: &DatabaseContext,
) -> Result<Option<sea_orm::DatabaseConnection>, PluginError> {
    let Ok(DbConnectParams::Postgres {
        url,
        binding: Some(binding),
        database,
        provision,
        ..
    }) = connect_params_from_context(ctx)
    else {
        return Ok(None);
    };
    let database = database.ok_or_else(|| {
        PluginError::invalid_params(format!(
            "database binding `{binding}` open is missing its database name"
        ))
    })?;
    let db = if provision {
        bookclerk_plugin_database_postgres::open_binding(&url, &database).await
    } else {
        bookclerk_plugin_database_postgres::open_binding_existing(&url, &database).await
    }
    .map_err(|e| PluginError::internal(format!("database binding `{binding}`: {e}")))?;
    Ok(Some(db))
}

/// Database guest that opens a Postgres URL and serves typed adapter RPC.
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
        })
    }

    async fn database(&self, context: DatabaseContext) -> Result<Box<dyn Database>, PluginError> {
        if let Some(conn) = binding_from_context(&context).await? {
            return Ok(Box::new(PostgresDatabase {
                dedicated: Some(conn),
            }));
        }
        connect_from_context(&context).await?;
        Ok(Box::new(PostgresDatabase { dedicated: None }))
    }
}

/// Postgres database factory: shared library pool, or a dedicated database
/// connection for named plugin database bindings.
struct PostgresDatabase {
    dedicated: Option<sea_orm::DatabaseConnection>,
}

#[async_trait(?Send)]
impl Database for PostgresDatabase {
    async fn open_session(&self) -> Result<Box<dyn AdapterDatabaseSession>, PluginError> {
        Ok(Box::new(PostgresSession {
            dedicated: self.dedicated.clone(),
        }))
    }

    fn host_session(&self) -> Option<Box<dyn HostAdapterDatabaseSession>> {
        if let Some(conn) = &self.dedicated {
            return Some(Box::new(host_session_on(conn.clone())));
        }
        Some(Box::new(host_session()))
    }
}

struct PostgresSession {
    dedicated: Option<sea_orm::DatabaseConnection>,
}

#[async_trait(?Send)]
impl AdapterDatabaseSession for PostgresSession {
    async fn capabilities(&self) -> Result<DbCapabilities, PluginError> {
        match &self.dedicated {
            Some(conn) => Ok(capabilities_for(conn)),
            None => guest_capabilities().await,
        }
    }

    async fn bootstrap(&self) -> Result<DbBootstrap, PluginError> {
        match &self.dedicated {
            Some(conn) => Ok(bootstrap_for(conn)),
            None => guest_bootstrap().await,
        }
    }

    async fn execute(&self, request: AdapterExecuteRequest) -> Result<ExecuteReply, PluginError> {
        match &self.dedicated {
            Some(conn) => guest_execute_request_on(conn, request).await,
            None => guest_execute_request(request).await,
        }
    }

    async fn export_identity(&self) -> Result<Vec<DbIdentityHighWater>, PluginError> {
        match &self.dedicated {
            Some(conn) => guest_export_identity_on(conn)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_export_identity()
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn import_identity(&self, rows: &[DbIdentityHighWater]) -> Result<(), PluginError> {
        match &self.dedicated {
            Some(conn) => guest_import_identity_on(conn, rows)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_import_identity(rows)
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn list_user_relations(&self) -> Result<Vec<String>, PluginError> {
        match &self.dedicated {
            Some(conn) => guest_list_user_relations_on(conn)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_list_user_relations()
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn prepare_unit_restore(&self) -> Result<(), PluginError> {
        match &self.dedicated {
            Some(conn) => guest_prepare_unit_restore_on(conn)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_prepare_unit_restore()
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn drop_user_relations(&self, names: &[String]) -> Result<(), PluginError> {
        match &self.dedicated {
            Some(conn) => guest_drop_user_relations_on(conn, names)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_drop_user_relations(names)
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn assert_restore_constraints(&self) -> Result<(), PluginError> {
        match &self.dedicated {
            Some(conn) => guest_assert_restore_constraints_on(conn)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_assert_restore_constraints()
                .await
                .map_err(plugin_error_from_engine),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(PostgresRoot).await?;
    Ok(())
}
