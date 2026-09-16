//! Database factory for the platform SQLite guest.

#![allow(
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

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
    AdapterDatabaseSession, Database, DatabaseContext, PluginDescribe, PluginRoot, ScalarLimits,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{
    AdapterExecuteRequest, DbBootstrap, DbCapabilities, DbIdentityHighWater, ExecuteReply,
    PluginError,
};
use sea_orm::DatabaseConnection;

use crate::ID;

type Result<T> = std::result::Result<T, PluginError>;

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
        if let Some(binding) = binding_open(&context) {
            let conn = open_binding_connection(&binding).await?;
            return Ok(Box::new(SqliteDatabase {
                dedicated: Some(conn),
            }));
        }
        connect_from_context(&context).await?;
        Ok(Box::new(SqliteDatabase { dedicated: None }))
    }
}

/// Per-binding open parsed from host-private connect params, if any.
struct BindingOpen {
    /// Binding name (used in operator-facing errors only).
    binding: String,
    /// Dedicated database file for this binding.
    sqlite_path: String,
    /// When false, refuse to create a missing file (backup capture).
    provision: bool,
}

/// Returns the binding-open parameters when the context targets a named
/// plugin database binding.
fn binding_open(ctx: &DatabaseContext) -> Option<BindingOpen> {
    let DbConnectParams::Sqlite {
        sqlite_path,
        binding: Some(binding),
        provision,
        ..
    } = connect_params_from_context(ctx).ok()?
    else {
        return None;
    };
    Some(BindingOpen {
        binding,
        sqlite_path: sqlite_path.unwrap_or_default(),
        provision,
    })
}

/// Opens the dedicated per-binding database file, creating parent dirs.
///
/// The spawn `BOOKCLERK_SQLITE_PATH` override never applies here: bindings
/// are isolated files, not the shared library database.
async fn open_binding_connection(open: &BindingOpen) -> Result<DatabaseConnection> {
    if open.sqlite_path.is_empty() {
        return Err(PluginError::invalid_params(format!(
            "database binding `{}` open is missing sqlitePath",
            open.binding
        )));
    }
    let path = std::path::Path::new(&open.sqlite_path);
    if !open.provision && !path.is_file() {
        return Err(PluginError::invalid_params(format!(
            "database binding `{}` file does not exist (lookup-only; will not provision)",
            open.binding
        )));
    }
    if open.provision {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                PluginError::internal(format!(
                    "database binding `{}` directory: {e}",
                    open.binding
                ))
            })?;
        }
    }
    crate::open(path)
        .await
        .map_err(|e| PluginError::internal(format!("database binding `{}`: {e}", open.binding)))
}

async fn connect_from_context(ctx: &DatabaseContext) -> Result<()> {
    let path = std::env::var("BOOKCLERK_SQLITE_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            connect_params_from_context(ctx).ok().and_then(|params| {
                let DbConnectParams::Sqlite { sqlite_path, .. } = params else {
                    return None;
                };
                sqlite_path
            })
        })
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
    if path.is_empty() {
        return Err(PluginError::internal(
            "sqlite database path missing (BOOKCLERK_SQLITE_PATH or context sqlitePath)",
        ));
    }
    let db = crate::open(std::path::Path::new(&path))
        .await
        .map_err(|e| PluginError::internal(e.to_string()))?;
    set_connection(db).await;
    Ok(())
}

/// SQLite database factory: shared library connection, or a dedicated
/// per-binding connection for named plugin database bindings.
struct SqliteDatabase {
    /// Dedicated connection for a binding open; `None` = shared library.
    dedicated: Option<DatabaseConnection>,
}

#[async_trait(?Send)]
impl Database for SqliteDatabase {
    async fn open_session(&self) -> Result<Box<dyn AdapterDatabaseSession>> {
        Ok(Box::new(SqliteSession {
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

struct SqliteSession {
    /// Dedicated connection for a binding session; `None` = shared library.
    dedicated: Option<DatabaseConnection>,
}

#[async_trait(?Send)]
impl AdapterDatabaseSession for SqliteSession {
    async fn capabilities(&self) -> Result<DbCapabilities> {
        match &self.dedicated {
            Some(conn) => Ok(capabilities_for(conn)),
            None => guest_capabilities().await,
        }
    }

    async fn bootstrap(&self) -> Result<DbBootstrap> {
        match &self.dedicated {
            Some(conn) => Ok(bootstrap_for(conn)),
            None => guest_bootstrap().await,
        }
    }

    async fn execute(&self, request: AdapterExecuteRequest) -> Result<ExecuteReply> {
        match &self.dedicated {
            Some(conn) => guest_execute_request_on(conn, request).await,
            None => guest_execute_request(request).await,
        }
    }

    async fn export_identity(&self) -> Result<Vec<DbIdentityHighWater>> {
        match &self.dedicated {
            Some(conn) => guest_export_identity_on(conn)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_export_identity()
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn import_identity(&self, rows: &[DbIdentityHighWater]) -> Result<()> {
        match &self.dedicated {
            Some(conn) => guest_import_identity_on(conn, rows)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_import_identity(rows)
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn list_user_relations(&self) -> Result<Vec<String>> {
        match &self.dedicated {
            Some(conn) => guest_list_user_relations_on(conn)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_list_user_relations()
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn prepare_unit_restore(&self) -> Result<()> {
        match &self.dedicated {
            Some(conn) => guest_prepare_unit_restore_on(conn)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_prepare_unit_restore()
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn drop_user_relations(&self, names: &[String]) -> Result<()> {
        match &self.dedicated {
            Some(conn) => guest_drop_user_relations_on(conn, names)
                .await
                .map_err(plugin_error_from_engine),
            None => guest_drop_user_relations(names)
                .await
                .map_err(plugin_error_from_engine),
        }
    }

    async fn assert_restore_constraints(&self) -> Result<()> {
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
