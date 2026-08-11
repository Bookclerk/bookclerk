//! [`ProxyDatabaseTrait`] adapter over an external database plugin process.
//!
//! The host mediates credentials and forwards SeaORM proxy calls over JSON-RPC.
//! Engine connect / migrate / proxy quirks live entirely in the database guest.
//! There is no in-process fallback.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::{resolve_d1_api_token, resolve_postgres_url, Config, DatabasePluginKind};
use bookclerk_plugin_sdk::{
    exec_result_from_dto, methods, proxy_rows_from_dto, statement_to_dto, DbConnectParams,
    DbConnectResult, ExecResultDto, QueryResultDto,
};
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement,
};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::jail::plugin_data_dir;
use crate::rpc::PluginClient;
use crate::{PluginError, Result as PluginResult};

/// External database backend spawned for `[database].plugin`.
#[derive(Clone)]
pub struct ExternalDatabase {
    client: Arc<PluginClient>,
    plugin_id: String,
    plugin_data_dir: std::path::PathBuf,
}

impl ExternalDatabase {
    /// Spawn and handshake a database plugin (connection happens later).
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> PluginResult<Self> {
        let table = crate::settings_table(config, plugin);
        let config_json = toml_to_json(&toml::Value::Table(table));
        let client = Arc::new(PluginClient::spawn(plugin, config, config_json).await?);
        Ok(Self {
            client,
            plugin_id: plugin.manifest.id.clone(),
            plugin_data_dir: plugin_data_dir(config, &plugin.manifest.id)?,
        })
    }

    /// Open the library connection through the guest (`db.connect` + optional fd pass).
    pub async fn connect(&self, config: &Config) -> Result<DatabaseConnection, DbErr> {
        let params = connect_params(config, &self.plugin_id, &self.plugin_data_dir, &self.client)?;
        let value = serde_json::to_value(&params).map_err(|err| DbErr::Custom(err.to_string()))?;

        let connect_result: DbConnectResult = if self.plugin_id.eq_ignore_ascii_case("sqlite") {
            let path = config.database.sqlite_path(&config.paths().files_dir);
            let raw = if self.client.has_side_channel() || self.client.has_acl_grants() {
                self.client
                    .call_raw_with_db_file(methods::DB_CONNECT, value, &path)
                    .await
                    .map_err(map_rpc_err)?
            } else {
                self.client
                    .call_raw(methods::DB_CONNECT, value)
                    .await
                    .map_err(map_rpc_err)?
            };
            serde_json::from_value(raw)
                .map_err(|err| DbErr::Custom(format!("db.connect result: {err}")))?
        } else {
            self.client
                .call(methods::DB_CONNECT, value)
                .await
                .map_err(map_rpc_err)?
        };

        self.client
            .call::<Value>(methods::DB_PING, Value::Null)
            .await
            .map_err(map_rpc_err)?;

        let backend = dialect_to_backend(&connect_result.dialect)?;
        let proxy: Arc<Box<dyn ProxyDatabaseTrait>> = Arc::new(Box::new(RpcDatabaseProxy {
            client: self.client.clone(),
        }));
        Database::connect_proxy(backend, proxy).await
    }
}

/// Long-lived external database plugin for the active `[database].plugin`.
#[derive(Default, Clone)]
pub struct DatabaseRegistry {
    active: Option<Arc<ExternalDatabase>>,
}

impl DatabaseRegistry {
    /// Currently active external database guest for `[database].plugin`.
    #[must_use]
    pub fn active(&self) -> Option<Arc<ExternalDatabase>> {
        self.active.clone()
    }
}

/// Discover and spawn the external database plugin matching `[database].plugin`.
///
/// Guests are required: when the matching database plugin is missing or fails to
/// start, [`open_library_store`] returns an error (no in-process engine).
pub async fn load_external_database(config: &Config) -> PluginResult<DatabaseRegistry> {
    let mut registry = DatabaseRegistry::default();
    let active = config.database.plugin.trim().to_ascii_lowercase();
    for plugin in crate::discover_plugins(config)? {
        if plugin.manifest.kind != crate::PluginKind::Database {
            continue;
        }
        if plugin.manifest.id.to_ascii_lowercase() != active {
            continue;
        }
        match ExternalDatabase::spawn(&plugin, config).await {
            Ok(db) => {
                tracing::info!(
                    id = %plugin.manifest.id,
                    path = %plugin.command.display(),
                    "loaded external database plugin"
                );
                registry.active = Some(Arc::new(db));
            }
            Err(err) => {
                return Err(PluginError::Other(anyhow::anyhow!(
                    "failed to start database plugin `{active}`: {err}"
                )));
            }
        }
        break;
    }
    if registry.active.is_none() {
        return Err(PluginError::Other(anyhow::anyhow!(
            "database plugin `{active}` not found — stage plugins/{} (see docs/database.md)",
            active
        )));
    }
    Ok(registry)
}

/// Open [`bookclerk_library::LibraryStore`] via the external database guest (required).
pub async fn open_library_store(
    config: &Config,
    registry: &DatabaseRegistry,
) -> bookclerk_library::Result<bookclerk_library::LibraryStore> {
    let ext = registry.active().ok_or_else(|| {
        bookclerk_library::LibraryError::Other(anyhow::anyhow!(
            "no active database plugin — stage and enable [database].plugin"
        ))
    })?;
    let db = ext
        .connect(config)
        .await
        .map_err(bookclerk_library::LibraryError::Orm)?;
    let store = bookclerk_library::LibraryStore::from_connection(db);
    store.ensure_users_bridged().await?;
    Ok(store)
}

/// Open the library for a specific `[database].plugin` id (ignoring the active config value).
pub async fn open_library_store_for_plugin(
    config: &Config,
    plugin_id: &str,
) -> bookclerk_library::Result<bookclerk_library::LibraryStore> {
    let mut cfg = config.clone();
    cfg.database.plugin = plugin_id.trim().to_string();
    let registry = load_external_database(&cfg)
        .await
        .map_err(|err| bookclerk_library::LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
    open_library_store(&cfg, &registry).await
}

/// Copy library data from one database plugin backend to another.
pub async fn migrate_database_plugin(
    config: &Config,
    from_plugin: &str,
    to_plugin: &str,
    opts: &bookclerk_library::BackendMigrateOptions,
) -> bookclerk_library::Result<bookclerk_library::BackendMigrateSummary> {
    if from_plugin.eq_ignore_ascii_case(to_plugin) {
        return Err(bookclerk_library::LibraryError::Other(anyhow::anyhow!(
            "source and destination database plugins are both `{from_plugin}`"
        )));
    }
    let source = open_library_store_for_plugin(config, from_plugin).await?;
    let dest = open_library_store_for_plugin(config, to_plugin).await?;
    bookclerk_library::migrate_library_backend(source.db(), dest.db(), opts).await
}

#[derive(Clone)]
struct RpcDatabaseProxy {
    client: Arc<PluginClient>,
}

impl std::fmt::Debug for RpcDatabaseProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcDatabaseProxy").finish_non_exhaustive()
    }
}

#[async_trait]
impl ProxyDatabaseTrait for RpcDatabaseProxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        let dto = statement_to_dto(&statement);
        let result: QueryResultDto = self
            .client
            .call(
                methods::DB_QUERY,
                serde_json::to_value(dto).map_err(map_json_err)?,
            )
            .await
            .map_err(map_rpc_err)?;
        Ok(proxy_rows_from_dto(result.rows))
    }

    async fn execute(&self, statement: Statement) -> std::result::Result<ProxyExecResult, DbErr> {
        let dto = statement_to_dto(&statement);
        let result: ExecResultDto = self
            .client
            .call(
                methods::DB_EXECUTE,
                serde_json::to_value(dto).map_err(map_json_err)?,
            )
            .await
            .map_err(map_rpc_err)?;
        Ok(exec_result_from_dto(result))
    }

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        self.client
            .call::<Value>(methods::DB_PING, Value::Null)
            .await
            .map_err(map_rpc_err)?;
        Ok(())
    }
}

fn connect_params(
    config: &Config,
    plugin_id: &str,
    plugin_data_dir: &Path,
    client: &PluginClient,
) -> Result<DbConnectParams, DbErr> {
    let data_dir = plugin_data_dir.display().to_string();
    match DatabasePluginKind::parse(plugin_id) {
        Some(DatabasePluginKind::Sqlite) => {
            let path = config.database.sqlite_path(&config.paths().files_dir);
            Ok(DbConnectParams::Sqlite {
                plugin_data_dir: data_dir,
                sqlite_path: Some(path.display().to_string()),
            })
        }
        Some(DatabasePluginKind::D1) => {
            client
                .require_binding("secrets")
                .map_err(|err| DbErr::Custom(err.to_string()))?;
            Ok(DbConnectParams::D1 {
                plugin_data_dir: data_dir,
                account_id: config.database.d1.account_id.clone(),
                database_id: config.database.d1.database_id.clone(),
                api_base: config.database.d1.api_base.clone(),
                api_token: resolve_d1_api_token().map_err(map_config_err)?,
            })
        }
        Some(DatabasePluginKind::Postgres) => {
            client
                .require_binding("secrets")
                .map_err(|err| DbErr::Custom(err.to_string()))?;
            Ok(DbConnectParams::Postgres {
                plugin_data_dir: data_dir,
                url: resolve_postgres_url(config).map_err(map_config_err)?,
            })
        }
        None => Err(DbErr::Custom(format!(
            "unknown database plugin `{plugin_id}`"
        ))),
    }
}

fn dialect_to_backend(dialect: &str) -> Result<DbBackend, DbErr> {
    match dialect.trim().to_ascii_lowercase().as_str() {
        "sqlite" => Ok(DbBackend::Sqlite),
        "postgres" | "postgresql" => Ok(DbBackend::Postgres),
        other => Err(DbErr::Custom(format!(
            "database plugin returned unknown dialect `{other}`"
        ))),
    }
}

fn map_rpc_err(err: crate::PluginError) -> DbErr {
    DbErr::Custom(err.to_string())
}

fn map_json_err(err: serde_json::Error) -> DbErr {
    DbErr::Custom(format!("serialize database RPC params: {err}"))
}

fn map_config_err(err: bookclerk_config::ConfigError) -> DbErr {
    DbErr::Custom(err.to_string())
}

fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_dialects() {
        assert_eq!(dialect_to_backend("sqlite").unwrap(), DbBackend::Sqlite);
        assert_eq!(dialect_to_backend("postgres").unwrap(), DbBackend::Postgres);
    }
}
