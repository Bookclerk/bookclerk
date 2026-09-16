//! Cloudflare D1 database plugin guest.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_db_guest::set_connection;
use bookclerk_plugin_abi::db::{connect_params_from_context, DbConnectParams};
use bookclerk_plugin_abi::{AdapterTransaction, HostAdapterDatabaseSession};
use bookclerk_plugin_abi::{GuestReceiptPersist, HostExecuteEnvelope};
use bookclerk_plugin_sdk::{
    serve, DbBootstrap, DbCapabilities, ExecuteReply, ExecuteRequest, PluginError, PluginMetadata,
};
use bookclerk_plugin_sdk::{
    AdapterDatabaseSession, Database, DatabaseContext, PluginDescribe, PluginRoot, ScalarLimits,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};

fn describe_metadata() -> Result<String, PluginError> {
    bookclerk_plugin_sdk::encode_json(PluginMetadata {
        api_version: PRODUCT_API_VERSION,
        id: "d1".into(),
        kind: "database".into(),
        display_name: Some("Cloudflare D1".into()),
        capabilities: vec!["health".into(), "diagnose".into()],
        sort_key: Some(5),
        ..PluginMetadata::default()
    })
}

async fn connect_from_context(ctx: &DatabaseContext) -> Result<(), PluginError> {
    let params = connect_params_from_context(ctx)?;
    let DbConnectParams::D1 {
        account_id,
        database_id,
        api_base,
        api_token,
        ..
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

/// Builds a dedicated per-binding proxy when the context targets a named
/// plugin database binding, resolving (and provisioning) the D1 database by
/// name.
async fn binding_from_context(
    ctx: &DatabaseContext,
) -> Result<Option<bookclerk_plugin_database_d1::D1Proxy>, PluginError> {
    let Ok(DbConnectParams::D1 {
        account_id,
        api_base,
        api_token,
        binding: Some(binding),
        database_name,
        provision,
        ..
    }) = connect_params_from_context(ctx)
    else {
        return Ok(None);
    };
    let name = database_name.ok_or_else(|| {
        PluginError::invalid_params(format!(
            "database binding `{binding}` open is missing its databaseName"
        ))
    })?;
    let database_id = if provision {
        bookclerk_plugin_database_d1::ensure_database(&api_base, &account_id, &api_token, &name)
            .await
            .map_err(|e| PluginError::internal(format!("database binding `{binding}`: {e}")))?
    } else {
        bookclerk_plugin_database_d1::lookup_database(&api_base, &account_id, &api_token, &name)
            .await
            .map_err(|e| PluginError::internal(format!("database binding `{binding}`: {e}")))?
    };
    Ok(Some(bookclerk_plugin_database_d1::D1Proxy::new(
        api_base,
        account_id,
        database_id,
        api_token,
    )))
}

/// Cloudflare D1 database guest.
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
        })
    }

    async fn database(&self, context: DatabaseContext) -> Result<Box<dyn Database>, PluginError> {
        if let Some(proxy) = binding_from_context(&context).await? {
            return Ok(Box::new(D1Database {
                dedicated: Some(proxy),
            }));
        }
        connect_from_context(&context).await?;
        Ok(Box::new(D1Database { dedicated: None }))
    }
}

/// D1 database factory: shared library proxy, or a dedicated per-binding
/// proxy (its own Cloudflare D1 database) for named plugin bindings.
struct D1Database {
    dedicated: Option<bookclerk_plugin_database_d1::D1Proxy>,
}

#[async_trait(?Send)]
impl Database for D1Database {
    async fn open_session(&self) -> Result<Box<dyn AdapterDatabaseSession>, PluginError> {
        Ok(Box::new(D1Session {
            dedicated: self.dedicated.clone(),
        }))
    }

    fn host_session(&self) -> Option<Box<dyn HostAdapterDatabaseSession>> {
        if let Some(proxy) = &self.dedicated {
            return Some(Box::new(D1DedicatedHostSession {
                proxy: proxy.clone(),
            }));
        }
        Some(Box::new(D1HostSession))
    }
}

struct D1DedicatedHostSession {
    proxy: bookclerk_plugin_database_d1::D1Proxy,
}

#[async_trait(?Send)]
impl HostAdapterDatabaseSession for D1DedicatedHostSession {
    async fn begin(&self) -> Result<Box<dyn AdapterTransaction>, PluginError> {
        Err(PluginError::unsupported(
            "D1 does not support interactive transactions",
        ))
    }

    async fn execute_envelope(
        &self,
        envelope: HostExecuteEnvelope,
    ) -> Result<ExecuteReply, PluginError> {
        self.proxy
            .run_typed_atomic(&envelope.request, envelope.guest_receipt, &envelope.proofs)
            .await
            .map_err(bookclerk_plugin_database_d1::atomic::plugin_error_from_d1)
    }
}

struct D1HostSession;

#[async_trait(?Send)]
impl HostAdapterDatabaseSession for D1HostSession {
    async fn begin(&self) -> Result<Box<dyn AdapterTransaction>, PluginError> {
        Err(PluginError::unsupported(
            "D1 does not support interactive transactions",
        ))
    }

    async fn execute_envelope(
        &self,
        envelope: HostExecuteEnvelope,
    ) -> Result<ExecuteReply, PluginError> {
        let proxy = bookclerk_plugin_database_d1::shared_proxy()
            .ok_or_else(|| PluginError::internal("d1 guest is not connected"))?;
        proxy
            .run_typed_atomic(&envelope.request, envelope.guest_receipt, &envelope.proofs)
            .await
            .map_err(bookclerk_plugin_database_d1::atomic::plugin_error_from_d1)
    }
}

struct D1Session {
    dedicated: Option<bookclerk_plugin_database_d1::D1Proxy>,
}

impl D1Session {
    fn proxy(&self) -> Result<bookclerk_plugin_database_d1::D1Proxy, PluginError> {
        if let Some(proxy) = &self.dedicated {
            return Ok(proxy.clone());
        }
        bookclerk_plugin_database_d1::shared_proxy()
            .ok_or_else(|| PluginError::internal("d1 guest is not connected"))
    }
}

#[async_trait(?Send)]
impl AdapterDatabaseSession for D1Session {
    async fn capabilities(&self) -> Result<DbCapabilities, PluginError> {
        Ok(DbCapabilities::advertised_d1())
    }

    async fn bootstrap(&self) -> Result<DbBootstrap, PluginError> {
        Ok(DbBootstrap::sqlite())
    }

    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteReply, PluginError> {
        self.proxy()?
            .run_typed_atomic(&request, GuestReceiptPersist::default(), &[])
            .await
            .map_err(bookclerk_plugin_database_d1::atomic::plugin_error_from_d1)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(D1Root).await?;
    Ok(())
}
