//! [`ProxyDatabaseTrait`] adapter over an external database plugin process.
//!
//! The host mediates credentials, applies schema after connect, and forwards
//! SeaORM proxy calls over JSON-RPC. Engine connect/proxy quirks live in the
//! database guest. There is no in-process fallback.

#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bookclerk_config::{resolve_d1_api_token, resolve_postgres_url, Config, DatabasePluginKind};
use bookclerk_db_exec::db_value_from_sea;
use bookclerk_plugin_abi::{
    catalog_page_statement, database_context_from_params, reserved_catalog_relation_missing,
    sql_catalog_page_rows, AdapterExecuteRequest, DbBootstrap, DbCapabilities, DbConnectParams,
    DbValue, IsolationReq, SqlType, SqlTypeEnv, SQL_CATALOG_TABLE, SQL_SCHEMA_TABLE,
};
use bookclerk_plugin_sdk::GuestDatabase;
use bookclerk_plugin_sdk::PRODUCT_API_VERSION;
use bookclerk_plugin_sdk::{
    proxy_rows_from_typed, DbPlanStatementKind, DbResultSelection, ExecuteReply, ExecuteRequest,
    PluginError as AbiPluginError, TypedDbStatement,
};
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement,
};
use serde_json::Value;
use tokio::task::{try_id, Id as TaskId};

use crate::discover::DiscoveredPlugin;
use crate::jail::plugin_data_dir;
use crate::rpc_session::{PluginSession, OPERATOR_ACCOUNT};
use crate::{PluginError, Result as PluginResult};
use bookclerk_library::{
    atomic_status, binding_bootstrap_plan, binding_bootstrap_type_env, history_from_execute_reply,
    plugin_binding_type_env, plugin_history_session_matches, plugin_journal_select_request,
    prove_plugin_migration_sequence, sql_string_literal, DbAtomicParams, PluginMigrationHistory,
    PluginMigrationSequence, SchemaState, BOOKCLERK_SCHEMA_NAMESPACE, SCHEMA_MIGRATIONS_DDL,
};

#[cfg(test)]
tokio::task_local! {
    /// Stamped Cap'n SQL captured by [`capture_outbound_adapter_sql`].
    static ADAPTER_SQL_CAPTURE: RefCell<Vec<String>>;
}

/// Records stamped adapter SQL when a test has [`capture_outbound_adapter_sql`] in scope.
#[cfg(test)]
fn note_outbound_adapter_sql(req: &AdapterExecuteRequest) {
    let _ = ADAPTER_SQL_CAPTURE.try_with(|slot| {
        slot.borrow_mut()
            .extend(req.request.statements.iter().map(|stmt| stmt.sql.clone()));
    });
}

/// Runs `f` and returns SQL that [`RpcDatabaseProxy`] stamped for Cap'n `execute`.
#[cfg(test)]
pub(super) async fn capture_outbound_adapter_sql<F, Fut, T>(f: F) -> (T, Vec<String>)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    ADAPTER_SQL_CAPTURE
        .scope(RefCell::new(Vec::new()), async {
            let value = f().await;
            let sql = ADAPTER_SQL_CAPTURE.with(|slot| slot.borrow().clone());
            (value, sql)
        })
        .await
}

/// Stamps 1:1 proofs on already-canonical host SQL for adapter execute.
fn stamp_adapter_request(
    req: ExecuteRequest,
    catalog: &SqlTypeEnv,
    isolation: IsolationReq,
) -> std::result::Result<AdapterExecuteRequest, crate::PluginError> {
    let proofs = bookclerk_db_exec::stamp_host_proofs(&req, catalog)
        .map_err(|err| crate::PluginError::from_abi(Some("invalid_params"), err.to_string()))?;
    bookclerk_plugin_abi::CanonicalExecuteRequest::from_desugared(req)
        .with_isolation(isolation)
        .bind_proofs(proofs)
        .map_err(|err| crate::PluginError::from_abi(Some("invalid_params"), err.to_string()))
}

/// [`stamp_adapter_request`] against the host library SQL catalog.
fn stamp_library_adapter_request(
    req: ExecuteRequest,
    isolation: IsolationReq,
) -> std::result::Result<AdapterExecuteRequest, crate::PluginError> {
    stamp_adapter_request(
        req,
        &bookclerk_library::migrations::host_sql_type_env(),
        isolation,
    )
}

/// External database backend spawned for `[database].plugin`.
#[derive(Clone)]
pub struct ExternalDatabase {
    /// Cap'n Proto session (vat holds the database session).
    session: Arc<PluginSession>,
    /// Manifest id (first-party `sqlite` / `d1` / `postgres`, or a
    /// third-party adapter id) used to build the factory context.
    plugin_id: String,
    /// Guest HOME / data directory passed in the factory context.
    plugin_data_dir: std::path::PathBuf,
    /// Granted `[database.<id>]` settings delivered to third-party adapters
    /// via the public `DatabaseAdapterConfig` payload.
    settings_json: Value,
    /// Files dir used for in-place schema snapshots before upgrades.
    files_dir: std::path::PathBuf,
}

impl ExternalDatabase {
    /// Spawn and describe a database plugin (connection happens later).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> PluginResult<Self> {
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(PluginError::message(format!(
                "plugin `{}` api_version {} is not supported",
                plugin.manifest.id, plugin.manifest.api_version
            )));
        }
        let table = crate::settings_table(config, plugin);
        let config_json = toml_to_json(&toml::Value::Table(table));
        let plugin_data_dir = plugin_data_dir(config, &plugin.manifest.id)?;
        let extra_env = match DatabasePluginKind::parse(&plugin.manifest.id) {
            Some(DatabasePluginKind::D1) | Some(DatabasePluginKind::Postgres) => Vec::new(),
            Some(DatabasePluginKind::Sqlite) => {
                let path = config.database.sqlite_path(&config.paths().files_dir);
                vec![(
                    "BOOKCLERK_SQLITE_PATH",
                    std::ffi::OsString::from(path.as_os_str()),
                )]
            }
            None => Vec::new(),
        };
        let session = Arc::new(
            PluginSession::spawn_for_account_with_env(
                plugin,
                config,
                config_json.clone(),
                OPERATOR_ACCOUNT,
                extra_env.as_slice(),
            )
            .await?,
        );
        Ok(Self {
            session,
            plugin_id: plugin.manifest.id.clone(),
            plugin_data_dir,
            settings_json: config_json,
            files_dir: config.paths().files_dir.clone(),
        })
    }

    /// Open the library connection through the guest (`database.openSession`).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn connect(
        &self,
        config: &Config,
    ) -> Result<(DatabaseConnection, DbCapabilities), DbErr> {
        let (db, caps) = self.connect_without_migrate(config).await?;
        self.apply_host_schema(&db, &caps).await?;
        Ok((db, caps))
    }

    /// Open the library connection without applying host schema (CLI migrate / version).
    ///
    /// # Errors
    ///
    /// Returns an error when the guest cannot open a session.
    pub async fn connect_without_migrate(
        &self,
        config: &Config,
    ) -> Result<(DatabaseConnection, DbCapabilities), DbErr> {
        let ctx = connect_context(
            config,
            &self.plugin_id,
            &self.plugin_data_dir,
            &self.session,
            &self.settings_json,
        )?;
        self.session.db_open(ctx).await.map_err(map_rpc_err)?;

        let caps = self.session.db_capabilities().await.map_err(map_rpc_err)?;
        if !caps.meets_host_minimums() {
            return Err(DbErr::Custom(caps.capability_failure_reason()));
        }
        bookclerk_library::require_schema_migrations(&caps)
            .map_err(|err| DbErr::Custom(err.to_string()))?;
        // Fail closed: transport/internal/deadline failures must not be
        // silently replaced with plugin-id-derived metadata. Only a typed
        // `unsupported` (adapter without a bootstrap surface) may fall back
        // to the first-party id inference below.
        let mut bootstrap = match self.session.db_bootstrap().await {
            Ok(bootstrap) => bootstrap,
            Err(crate::PluginError::Abi { code, .. }) if code == "unsupported" => {
                DbBootstrap::default()
            }
            Err(err) => return Err(map_rpc_err(err)),
        };
        apply_bootstrap_metadata(&mut bootstrap, &self.plugin_id);
        let _ = &bootstrap;
        let proxy: Arc<Box<dyn ProxyDatabaseTrait>> = Arc::new(Box::new(RpcDatabaseProxy {
            session: self.session.clone(),
            txn_depth: Arc::new(Mutex::new(HashMap::new())),
            caps: caps.clone(),
            binding: None,
        }));
        let db = Database::connect_proxy(canonical_seaorm_backend(), proxy).await?;
        Ok((db, caps))
    }

    /// Adapter snapshot/identity/restore hooks for the library session.
    #[must_use]
    pub fn library_backup_ops(&self) -> bookclerk_plugin_abi::SharedAdapterBackupOps {
        crate::rpc_session::RpcBackupOps::library(self.session.clone()).shared()
    }

    /// Adapter snapshot/identity/restore hooks for one named binding session.
    #[must_use]
    pub fn binding_backup_ops(
        &self,
        key: impl Into<String>,
    ) -> bookclerk_plugin_abi::SharedAdapterBackupOps {
        crate::rpc_session::RpcBackupOps::binding(self.session.clone(), key).shared()
    }

    /// Reads the guest schema version and applies remaining host-authored DDL.
    async fn apply_host_schema(
        &self,
        db: &DatabaseConnection,
        caps: &DbCapabilities,
    ) -> Result<(), DbErr> {
        bookclerk_library::require_schema_migrations(caps)
            .map_err(|err| DbErr::Custom(err.to_string()))?;
        let session = self.session.clone();
        let caps = caps.clone();
        let backend_at_capture = backup_adapter_id(&self.plugin_id);
        let opts = bookclerk_library::SchemaApplyOptions {
            backup: Some(bookclerk_library::SchemaBackupOpts {
                files_dir: self.files_dir.clone(),
                include_plugin_databases: false,
                consistent_backup_read: caps.supports_consistent_backup_read(),
                backend_at_capture,
                max_result_rows: caps.max_result_rows,
                max_result_bytes: caps.max_result_bytes,
                max_atomic_result_bytes: caps.max_atomic_result_bytes,
                plugin_units: Vec::new(),
                adapter: Some(self.library_backup_ops()),
            }),
        };
        bookclerk_library::apply_host_schema_with_batch_opts(db, opts, move |stmts| {
            let session = session.clone();
            let caps = caps.clone();
            async move { exec_host_ddl_batch(&session, &caps, stmts).await }
        })
        .await
        .map_err(|err| DbErr::Custom(err.to_string()))
    }

    /// Migrates toward `target` using guest `executeAtomic` batches (CLI migrate / downgrade).
    ///
    /// # Errors
    ///
    /// Returns when capability flags are unknown or a schema batch fails.
    pub async fn migrate_to(
        &self,
        db: &DatabaseConnection,
        caps: &DbCapabilities,
        target: i64,
        opts: bookclerk_library::SchemaApplyOptions,
    ) -> Result<bookclerk_library::SchemaWalk, DbErr> {
        bookclerk_library::require_schema_migrations(caps)
            .map_err(|err| DbErr::Custom(err.to_string()))?;
        let session = self.session.clone();
        let caps = caps.clone();
        bookclerk_library::migrate_host_schema_to_with_batch(db, target, opts, move |stmts| {
            let session = session.clone();
            let caps = caps.clone();
            async move { exec_host_ddl_batch(&session, &caps, stmts).await }
        })
        .await
        .map_err(|err| DbErr::Custom(err.to_string()))
    }
}

/// Long-lived external database plugin for the active `[database].plugin`.
#[derive(Default, Clone)]
pub struct DatabaseRegistry {
    /// Spawned guest matching `[database].plugin`, if describe succeeded.
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
///
/// # Errors
///
/// Returns an error when the operation fails.
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
        let files = &config.paths().files_dir;
        let expected = files.join("plugins").join(&active);
        let mut hint = format!(
            "looked under {} (set {} to the directory `cargo dev` uses, \
             typically ./BookclerkFiles, or run `cargo install-platform` / \
             `cargo dev-cli -- daemon token`)",
            expected.display(),
            bookclerk_config::BOOKCLERK_FILES_DIR_ENV
        );
        if let Ok(cwd) = std::env::current_dir() {
            let alt = cwd.join("BookclerkFiles").join("plugins").join(&active);
            if alt.is_dir() && alt != expected {
                hint.push_str(&format!(
                    "; found guest at {} — export {}={}",
                    alt.display(),
                    bookclerk_config::BOOKCLERK_FILES_DIR_ENV,
                    cwd.join("BookclerkFiles").display()
                ));
            }
        }
        return Err(PluginError::Other(anyhow::anyhow!(
            "database plugin `{active}` not found — {hint} (see docs/database.md)"
        )));
    }
    Ok(registry)
}

/// Opens the active database guest without auto-apply and migrates to `target`.
///
/// Uses guest `executeAtomic` batches (the same path as daemon connect).
///
/// # Errors
///
/// Returns when the guest cannot start, connect, or apply schema.
pub async fn migrate_library_schema(
    config: &Config,
    target: i64,
    opts: bookclerk_library::SchemaApplyOptions,
) -> PluginResult<bookclerk_library::SchemaWalk> {
    let registry = load_external_database(config).await?;
    let ext = registry.active().ok_or_else(|| {
        PluginError::message(
            "no active database plugin — stage and enable [database].plugin".to_string(),
        )
    })?;
    let (db, caps) = ext
        .connect_without_migrate(config)
        .await
        .map_err(|err| PluginError::message(err.to_string()))?;
    ext.migrate_to(&db, &caps, target, opts)
        .await
        .map_err(|err| PluginError::message(err.to_string()))
}

/// Open [`bookclerk_library::LibraryStore`] via the external database guest (required).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn open_library_store(
    config: &Config,
    registry: &DatabaseRegistry,
) -> bookclerk_library::Result<bookclerk_library::LibraryStore> {
    let ext = registry.active().ok_or_else(|| {
        bookclerk_library::LibraryError::Other(anyhow::anyhow!(
            "no active database plugin — stage and enable [database].plugin"
        ))
    })?;
    let (db, caps) = ext
        .connect(config)
        .await
        .map_err(bookclerk_library::LibraryError::Orm)?;
    let backend = Arc::new(RpcAtomicBackend {
        session: ext.session.clone(),
        caps: caps.clone(),
    });
    let store = bookclerk_library::LibraryStore::from_connection(db)
        .with_db_capabilities(caps)
        .with_atomic_txn(backend.clone())
        .with_typed_exec(backend);
    store.ensure_users_bridged().await?;
    Ok(store)
}

/// Test helper: a `GuestDatabase` over a library store with an explicit policy.
///
/// Production jobs do not inject the host library; plugins use named bindings.
#[cfg(test)]
#[must_use]
fn granted_job_database(store: bookclerk_library::LibraryStore) -> Arc<dyn GuestDatabase> {
    granted_job_database_with_policy(
        store,
        bookclerk_library::GuestSqlPolicy::allow_tables(["books"])
            .with_sql_types(bookclerk_library::migrations::host_sql_type_env()),
    )
}

/// Like [`granted_job_database`], with an explicit table/column/function policy.
#[cfg(test)]
#[must_use]
fn granted_job_database_with_policy(
    store: bookclerk_library::LibraryStore,
    policy: bookclerk_library::GuestSqlPolicy,
) -> Arc<dyn GuestDatabase> {
    Arc::new(GuestJobDatabase { store, policy })
}

/// Test-only host-library [`GuestDatabase`] (never injected into production jobs).
#[cfg(test)]
struct GuestJobDatabase {
    /// Library used for authorized typed `execute`.
    store: bookclerk_library::LibraryStore,
    /// Host-issued table/column/function allowlist for this job.
    policy: bookclerk_library::GuestSqlPolicy,
}

#[cfg(test)]
#[async_trait(?Send)]
impl GuestDatabase for GuestJobDatabase {
    async fn execute(
        &self,
        request: ExecuteRequest,
    ) -> std::result::Result<ExecuteReply, AbiPluginError> {
        self.store.execute_guest_atomic(request, &self.policy).await
    }
}

/// Caps a guest `deadlineUnixMs` by the host job lease.
///
/// Guest `0` means unlimited and inherits the host deadline. Host `0` means
/// the guest value is unchanged. Otherwise the earlier of the two wins.
#[must_use]
fn capped_binding_deadline(guest_unix_ms: u64, host_unix_ms: u64) -> u64 {
    if guest_unix_ms == 0 {
        host_unix_ms
    } else if host_unix_ms == 0 {
        guest_unix_ms
    } else {
        guest_unix_ms.min(host_unix_ms)
    }
}

/// One provisioned named plugin database binding served by the active adapter.
///
/// The adapter holds an isolated session (own file / database / D1 database);
/// guest SQL is authorized with [`bookclerk_library::GuestSqlPolicy::binding_owned`]
/// and receipt-wrapped against the binding's own `bookclerk_receipts` table.
/// Execute uses the host-private envelope path so receipts finalize, and
/// forwards the job cancel flag plus a capped host lease deadline.
struct BindingGuestDatabase {
    /// Adapter vat session shared with the library connection.
    session: Arc<PluginSession>,
    /// Vat-unique binding session key (`<plugin>/<BINDING>`).
    key: String,
    /// Negotiated adapter capabilities.
    caps: DbCapabilities,
    /// Job fence / cancel flag shared with the destination vat.
    cancel: Arc<AtomicBool>,
    /// Host lease deadline (`deadlineUnixMs`); `0` means unlimited.
    host_deadline_unix_ms: u64,
    /// History digest captured when this session was opened.
    expected_plugin_history_digest: String,
}

#[async_trait(?Send)]
impl GuestDatabase for BindingGuestDatabase {
    async fn execute(
        &self,
        mut request: ExecuteRequest,
    ) -> std::result::Result<ExecuteReply, AbiPluginError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(AbiPluginError::cancelled("fence lost"));
        }
        request.deadline_unix_ms =
            capped_binding_deadline(request.deadline_unix_ms, self.host_deadline_unix_ms);
        let observed = load_binding_plugin_history(
            &self.session,
            &self.key,
            &self.cancel,
            request.deadline_unix_ms,
        )
        .await
        .map_err(host_err_to_abi)?;
        plugin_history_session_matches(&self.expected_plugin_history_digest, &observed)
            .map_err(|err| AbiPluginError::internal(err.to_string()))?;
        let env = load_binding_sql_type_env(
            &self.session,
            &self.key,
            &self.cancel,
            request.deadline_unix_ms,
            self.caps.max_result_rows,
            false,
        )
        .await?;
        let policy = bookclerk_library::GuestSqlPolicy::binding_owned().with_sql_types(env);
        let cancel = Arc::clone(&self.cancel);
        bookclerk_library::execute_guest_atomic_with(request, &self.caps, &policy, |envelope| {
            let session = Arc::clone(&self.session);
            let key = self.key.clone();
            async move {
                session
                    .db_execute_binding_envelope_request(&key, envelope, cancel)
                    .await
                    .map_err(host_err_to_abi)
            }
        })
        .await
    }
}

/// Collision-resistant instance id for one `(owner_plugin_id, binding)` pair.
///
/// Length-prefixed SHA-256 so `("ab_c", "D")` and `("ab", "C_D")` cannot
/// collide. Hosts pass this opaque value to third-party adapters and derive
/// backend-native unit names from a length-bounded prefix of the same digest.
fn binding_instance_id(owner_plugin_id: &str, binding: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(
        u64::try_from(owner_plugin_id.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    hasher.update(owner_plugin_id.as_bytes());
    hasher.update(
        u64::try_from(binding.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    hasher.update(binding.as_bytes());
    hex::encode(hasher.finalize())
}

/// Postgres `CREATE DATABASE` identifier (`pb_` + 32 hex = 35 ≤ 63).
fn postgres_binding_database_name(owner_plugin_id: &str, binding: &str) -> String {
    format!(
        "pb_{}",
        &binding_instance_id(owner_plugin_id, binding)[..32]
    )
}

/// Cloudflare D1 database name (`bookclerk-pb-` + 32 hex).
fn d1_binding_database_name(owner_plugin_id: &str, binding: &str) -> String {
    format!(
        "bookclerk-pb-{}",
        &binding_instance_id(owner_plugin_id, binding)[..32]
    )
}

/// Backend-native unit ref for one `(plugin, binding)` on the active adapter.
pub(crate) fn plugin_binding_unit_ref(
    config: &Config,
    kind: Option<DatabasePluginKind>,
    owner_plugin_id: &str,
    binding: &str,
) -> String {
    match kind {
        Some(DatabasePluginKind::Sqlite) => config
            .paths()
            .files_dir
            .join("plugin-databases")
            .join(owner_plugin_id)
            .join(format!("{binding}.db"))
            .display()
            .to_string(),
        Some(DatabasePluginKind::Postgres) => {
            postgres_binding_database_name(owner_plugin_id, binding)
        }
        Some(DatabasePluginKind::D1) => d1_binding_database_name(owner_plugin_id, binding),
        None => binding_instance_id(owner_plugin_id, binding),
    }
}

/// Adapter id recorded on a backup (diagnostic; restore is capability-driven).
#[must_use]
pub fn backup_adapter_id(plugin_id: &str) -> String {
    match DatabasePluginKind::parse(plugin_id) {
        Some(DatabasePluginKind::Sqlite) => "sqlite".into(),
        Some(DatabasePluginKind::Postgres) => "postgres".into(),
        Some(DatabasePluginKind::D1) => "d1".into(),
        None => plugin_id.to_string(),
    }
}

/// Nested savepoint when a binding transaction is already open; else atomic batch.
fn binding_isolation(on_txn: bool) -> IsolationReq {
    if on_txn {
        IsolationReq::NestedSavepoint
    } else {
        IsolationReq::AtomicBatch
    }
}

/// Typed execute on a named binding session, or on its open transaction.
async fn exec_binding_request(
    session: &PluginSession,
    key: &str,
    stamped: AdapterExecuteRequest,
    cancel: &Arc<AtomicBool>,
    on_txn: bool,
) -> std::result::Result<ExecuteReply, crate::PluginError> {
    if on_txn {
        session
            .db_txn_execute_binding_request(key, stamped, Arc::clone(cancel))
            .await
    } else {
        session
            .db_execute_binding_request(key, stamped, Arc::clone(cancel))
            .await
    }
}

/// Reads the durable binding catalog through the host (guest-denied) path.
async fn load_binding_sql_type_env(
    session: &PluginSession,
    key: &str,
    cancel: &Arc<AtomicBool>,
    deadline_unix_ms: u64,
    max_result_rows: u32,
    on_txn: bool,
) -> std::result::Result<SqlTypeEnv, AbiPluginError> {
    let page = sql_catalog_page_rows(max_result_rows);
    let mut env = SqlTypeEnv::new();
    let mut cursor_table = String::new();
    let mut cursor_ord: i64 = -1;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(AbiPluginError::cancelled("fence lost"));
        }
        let req = ExecuteRequest {
            operation_id: format!("binding-catalog-{key}"),
            request_hash: String::new(),
            deadline_unix_ms,
            statements: vec![TypedDbStatement {
                sql: format!(
                    "SELECT table_name, column_name, sql_type, ordinal, is_identity, default_sql \
                     FROM {SQL_CATALOG_TABLE} \
                     WHERE table_name > ? OR (table_name = ? AND ordinal > ?) \
                     ORDER BY table_name, ordinal LIMIT {page}"
                ),
                parameters: vec![
                    DbValue::Text(cursor_table.clone()),
                    DbValue::Text(cursor_table.clone()),
                    DbValue::Int64(cursor_ord),
                ],
                kind: DbPlanStatementKind::Select,
                max_rows: page,
                result_selection: DbResultSelection::Rows,
            }],
        };
        let stamped = stamp_adapter_request(req, &SqlTypeEnv::new(), binding_isolation(on_txn))
            .map_err(host_err_to_abi)?;
        let reply = match exec_binding_request(session, key, stamped, cancel, on_txn).await {
            Ok(reply) => reply,
            Err(err) => {
                if reserved_catalog_relation_missing(&err.to_string(), SQL_CATALOG_TABLE) {
                    return Ok(SqlTypeEnv::new());
                }
                return Err(host_err_to_abi(err));
            }
        };
        let stmt = catalog_page_statement(&reply)?;
        if stmt.rows.is_empty() {
            break;
        }
        let n = stmt.rows.len();
        for row in &stmt.rows {
            let table = catalog_cell_text(row.values.first());
            let column = catalog_cell_text(row.values.get(1));
            let ty = catalog_cell_text(row.values.get(2));
            if table.is_empty() || column.is_empty() {
                return Err(AbiPluginError::internal(
                    "bookclerk_sql_catalog row is missing table_name or column_name",
                ));
            }
            let Some(sql_ty) = SqlType::from_column_ident(ty.to_ascii_lowercase().as_str()) else {
                return Err(AbiPluginError::internal(format!(
                    "bookclerk_sql_catalog has unknown sql_type {ty}"
                )));
            };
            let ordinal = catalog_cell_i64(row.values.get(3)).ok_or_else(|| {
                AbiPluginError::internal("bookclerk_sql_catalog row is missing ordinal")
            })?;
            env.insert_column(&table, &column, sql_ty);
            cursor_table = table;
            cursor_ord = ordinal;
        }
        if n < usize::try_from(page).unwrap_or(usize::MAX) {
            break;
        }
    }
    load_binding_sql_schema_env(
        session,
        key,
        cancel,
        deadline_unix_ms,
        page,
        on_txn,
        &mut env,
    )
    .await?;
    Ok(env)
}

/// Pages `bookclerk_sql_schema` into `env` (fail closed on malformed rows).
async fn load_binding_sql_schema_env(
    session: &PluginSession,
    key: &str,
    cancel: &Arc<AtomicBool>,
    deadline_unix_ms: u64,
    page: u32,
    on_txn: bool,
    env: &mut SqlTypeEnv,
) -> std::result::Result<(), AbiPluginError> {
    let mut cursor = String::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(AbiPluginError::cancelled("fence lost"));
        }
        let req = ExecuteRequest {
            operation_id: format!("binding-schema-{key}"),
            request_hash: String::new(),
            deadline_unix_ms,
            statements: vec![TypedDbStatement {
                sql: format!(
                    "SELECT table_name, fingerprint, identity_column \
                     FROM {SQL_SCHEMA_TABLE} \
                     WHERE table_name > ? \
                     ORDER BY table_name LIMIT {page}"
                ),
                parameters: vec![DbValue::Text(cursor.clone())],
                kind: DbPlanStatementKind::Select,
                max_rows: page,
                result_selection: DbResultSelection::Rows,
            }],
        };
        let stamped = stamp_adapter_request(req, &SqlTypeEnv::new(), binding_isolation(on_txn))
            .map_err(host_err_to_abi)?;
        let reply = match exec_binding_request(session, key, stamped, cancel, on_txn).await {
            Ok(reply) => reply,
            Err(err) => {
                if reserved_catalog_relation_missing(&err.to_string(), SQL_SCHEMA_TABLE) {
                    return Ok(());
                }
                return Err(host_err_to_abi(err));
            }
        };
        let stmt = catalog_page_statement(&reply)?;
        if stmt.rows.is_empty() {
            break;
        }
        let n = stmt.rows.len();
        for row in &stmt.rows {
            let table = catalog_cell_text(row.values.first());
            let fingerprint = catalog_cell_text(row.values.get(1));
            let identity = catalog_cell_text(row.values.get(2));
            if table.is_empty() || fingerprint.is_empty() {
                return Err(AbiPluginError::internal(
                    "bookclerk_sql_schema row is missing table_name or fingerprint",
                ));
            }
            let cols = env.table_columns(&table).unwrap_or(&[]).to_vec();
            env.insert_table_schema(
                table.clone(),
                cols,
                if identity.is_empty() {
                    None
                } else {
                    Some(identity)
                },
                fingerprint,
            );
            cursor = table;
        }
        if n < usize::try_from(page).unwrap_or(usize::MAX) {
            break;
        }
    }
    Ok(())
}

/// TEXT cell from a catalog row, or empty when the value is missing/non-text.
fn catalog_cell_text(v: Option<&DbValue>) -> String {
    match v {
        Some(DbValue::Text(s)) => s.clone(),
        _ => String::new(),
    }
}

/// INTEGER cell from a catalog row.
fn catalog_cell_i64(v: Option<&DbValue>) -> Option<i64> {
    match v {
        Some(DbValue::Int64(n)) => Some(*n),
        _ => None,
    }
}

/// Host-authored statements for one binding bootstrap apply unit.
fn binding_sql_request(operation_id: String, sqls: Vec<String>) -> ExecuteRequest {
    ExecuteRequest {
        operation_id,
        request_hash: String::new(),
        deadline_unix_ms: 0,
        statements: sqls
            .into_iter()
            .map(|sql| TypedDbStatement {
                sql,
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Discard,
            })
            .collect(),
    }
}

/// Reads binding `bookclerk_schema_migrations` rows into [`SchemaState`].
fn binding_schema_state_from_reply(reply: &ExecuteReply) -> PluginResult<SchemaState> {
    let Some(stmt) = reply.statements.first() else {
        return Ok(SchemaState::Uninitialized);
    };
    if stmt.rows.is_empty() {
        return Ok(SchemaState::Uninitialized);
    }
    let mut unreleased = None;
    let mut frozen: Option<(i64, String)> = None;
    for row in &stmt.rows {
        let version = match row.values.first() {
            Some(DbValue::Int64(n)) => *n,
            Some(DbValue::Text(s)) => s.parse::<i64>().unwrap_or(0),
            _ => {
                return Err(PluginError::message(
                    "binding bookclerk_schema_migrations row is missing version",
                ));
            }
        };
        let state = match row.values.get(1) {
            Some(DbValue::Text(s)) => s.as_str(),
            _ => "",
        };
        let checksum = match row.values.get(2) {
            Some(DbValue::Text(s)) => s.clone(),
            _ => String::new(),
        };
        match state {
            "unreleased" => {
                if unreleased.is_some() {
                    return Err(PluginError::message(
                        "binding bookclerk_schema_migrations has multiple unreleased rows",
                    ));
                }
                unreleased = Some((version, checksum));
            }
            "frozen" => {
                frozen = match frozen {
                    Some((v, _)) if v >= version => frozen,
                    _ => Some((version, checksum)),
                };
            }
            other => {
                return Err(PluginError::message(format!(
                    "unrecognized binding bookclerk_schema_migrations.state `{other}`"
                )));
            }
        }
    }
    if let Some((base_version, checksum)) = unreleased {
        return Ok(SchemaState::Unreleased {
            base_version,
            checksum,
        });
    }
    if let Some((version, checksum)) = frozen {
        return Ok(SchemaState::Frozen { version, checksum });
    }
    Ok(SchemaState::Uninitialized)
}

/// Re-reads the host-private plugin migration journal for stale-session fencing.
async fn load_binding_plugin_history(
    session: &PluginSession,
    key: &str,
    cancel: &Arc<AtomicBool>,
    deadline_unix_ms: u64,
) -> PluginResult<PluginMigrationHistory> {
    if cancel.load(Ordering::SeqCst) {
        return Err(PluginError::message("fence lost"));
    }
    let select =
        plugin_journal_select_request(format!("binding-history-fence-{key}"), deadline_unix_ms);
    let reply = session
        .db_execute_binding_request(
            key,
            stamp_adapter_request(
                select,
                &binding_bootstrap_type_env(),
                IsolationReq::AtomicBatch,
            )?,
            Arc::clone(cancel),
        )
        .await?;
    history_from_execute_reply(&reply).map_err(|err| PluginError::message(err.to_string()))
}

/// RPC-host adapter for [`super::plugin_migration_apply::apply_registered_plugin_migrations`].
struct BindingPluginMigrationHost<'a> {
    /// Live plugin session used to execute binding-scoped typed requests.
    session: &'a PluginSession,
    /// Session key for this binding's database handle.
    key: &'a str,
    /// Plugin id that owns the binding.
    owner: &'a str,
    /// Binding name inside that plugin.
    binding: &'a str,
    /// Proven registration used to stamp `plugin_binding_type_env`.
    registered: &'a PluginMigrationSequence,
}

#[async_trait]
impl super::plugin_migration_apply::PluginMigrationApplyHost for BindingPluginMigrationHost<'_> {
    async fn load_plugin_migration_history(&self) -> PluginResult<PluginMigrationHistory> {
        let cancel = Arc::new(AtomicBool::new(false));
        let select = plugin_journal_select_request(
            format!("binding-plugin-history-{}-{}", self.owner, self.binding),
            0,
        );
        let reply = self
            .session
            .db_execute_binding_request(
                self.key,
                stamp_adapter_request(
                    select,
                    &binding_bootstrap_type_env(),
                    IsolationReq::AtomicBatch,
                )?,
                cancel,
            )
            .await?;
        history_from_execute_reply(&reply).map_err(|err| PluginError::message(err.to_string()))
    }

    async fn execute_plugin_migration_apply(
        &self,
        operation_id: String,
        statements: Vec<String>,
        applied_prefix: usize,
    ) -> PluginResult<()> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.session
            .db_execute_binding_request(
                self.key,
                stamp_adapter_request(
                    super::plugin_migration_apply::plugin_migration_apply_request(
                        operation_id,
                        statements,
                    ),
                    &plugin_binding_type_env(self.registered, applied_prefix),
                    IsolationReq::AtomicBatch,
                )?,
                cancel,
            )
            .await?;
        Ok(())
    }
}

impl ExternalDatabase {
    /// Opens (provisioning on first use) isolated sessions for the named
    /// plugin database bindings of `owner_plugin_id`.
    ///
    /// Backend-native units: SQLite gets a file per binding under
    /// `<files_dir>/plugin-databases/<plugin>/<BINDING>.db`, PostgreSQL a
    /// dedicated database (`pb_` + 32 hex of the `(plugin, binding)` digest),
    /// and D1 its own database (`bookclerk-pb-` + the same 32 hex). Third-party
    /// adapters advertising `pluginDatabases` receive `binding` plus a
    /// collision-resistant `instanceId` on the public
    /// [`bookclerk_plugin_abi::DatabaseAdapterConfig`]. Each provisioned unit
    /// is recorded in the `plugin_databases` registry (an existing row wins so
    /// a re-open never re-targets a binding).
    ///
    /// # Errors
    ///
    /// Fails closed when the adapter does not advertise
    /// `DbCapabilities::plugin_databases`, when provisioning fails, or when
    /// the registry cannot be recorded.
    pub async fn open_binding_databases(
        &self,
        config: &Config,
        store: &bookclerk_library::LibraryStore,
        owner_plugin_id: &str,
        bindings: &[String],
        owner: &PluginSession,
    ) -> PluginResult<Vec<(String, crate::rpc_session::GuestDatabaseFactory)>> {
        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        if owner_plugin_id == BOOKCLERK_SCHEMA_NAMESPACE {
            return Err(PluginError::message(format!(
                "plugin id `{BOOKCLERK_SCHEMA_NAMESPACE}` is reserved for host schema namespace"
            )));
        }
        let caps = self.session.db_capabilities().await?;
        if !caps.plugin_databases {
            return Err(PluginError::message(format!(
                "database plugin `{}` does not support isolated plugin database bindings \
                 (plugin `{owner_plugin_id}` requests {bindings:?})",
                self.plugin_id
            )));
        }
        let kind = DatabasePluginKind::parse(&self.plugin_id);
        let backend_kind = match kind {
            Some(DatabasePluginKind::Sqlite) => "sqlite",
            Some(DatabasePluginKind::Postgres) => "postgres",
            Some(DatabasePluginKind::D1) => "d1",
            None => self.plugin_id.as_str(),
        };
        let mut out = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let default_unit = self.default_binding_unit(config, kind, owner_plugin_id, binding);
            let record = store
                .record_plugin_database(owner_plugin_id, binding, backend_kind, &default_unit)
                .await
                .map_err(|err| PluginError::message(err.to_string()))?;
            if record.backend_kind != backend_kind {
                return Err(PluginError::message(format!(
                    "plugin database binding `{owner_plugin_id}/{binding}` was provisioned on \
                     `{}` but the active adapter is `{backend_kind}`; migrate or drop it with \
                     `bookclerk plugins db drop {owner_plugin_id} {binding}`",
                    record.backend_kind
                )));
            }
            let ctx = self.binding_connect_context(
                config,
                kind,
                owner_plugin_id,
                binding,
                &record.unit_ref,
                true,
            )?;
            let key = format!("{owner_plugin_id}/{binding}");
            let binding_caps = self.session.db_open_binding(&key, ctx).await?;
            if !binding_caps.meets_host_minimums() {
                return Err(PluginError::message(format!(
                    "database binding `{owner_plugin_id}/{binding}` failed host capability \
                     minima: {}",
                    binding_caps.capability_failure_reason()
                )));
            }
            self.ensure_binding_host_schema(&key, owner_plugin_id, binding)
                .await?;
            let registered = owner
                .database_migrations(binding)
                .await
                .map_err(|err| PluginError::message(err.to_string()))?;
            let sequence = prove_plugin_migration_sequence(registered)
                .map_err(|err| PluginError::message(err.to_string()))?;
            self.ensure_plugin_migrations(&key, owner_plugin_id, binding, &sequence)
                .await?;
            let expected_history = self
                .binding_plugin_history(&key, owner_plugin_id, binding)
                .await?;
            let session = Arc::clone(&self.session);
            let factory_key = key.clone();
            let expected_plugin_history_digest = expected_history.digest();
            let factory: crate::rpc_session::GuestDatabaseFactory =
                Arc::new(move |cancel, host_deadline_unix_ms| {
                    Arc::new(BindingGuestDatabase {
                        session: Arc::clone(&session),
                        key: factory_key.clone(),
                        caps: binding_caps.clone(),
                        cancel,
                        host_deadline_unix_ms,
                        expected_plugin_history_digest: expected_plugin_history_digest.clone(),
                    })
                });
            out.push((binding.clone(), factory));
        }
        Ok(out)
    }

    /// Opens one plugin binding as a SeaORM connection through the guest session.
    ///
    /// Physical provisioning is adapter-owned (`provision`). Logical identity is
    /// `(plugin_id, binding)`. Does not switch on sqlite/postgres/d1 beyond
    /// building the adapter's own connect context.
    ///
    /// # Errors
    ///
    /// Returns when the guest cannot open the binding or the proxy fails.
    pub async fn open_binding_seaorm(
        &self,
        config: &Config,
        owner_plugin_id: &str,
        binding: &str,
        unit_ref: &str,
        provision: bool,
    ) -> PluginResult<(DatabaseConnection, DbCapabilities)> {
        let kind = DatabasePluginKind::parse(&self.plugin_id);
        let ctx = self.binding_connect_context(
            config,
            kind,
            owner_plugin_id,
            binding,
            unit_ref,
            provision,
        )?;
        let key = format!("{owner_plugin_id}/{binding}");
        let binding_caps = self.session.db_open_binding(&key, ctx).await?;
        if !binding_caps.meets_host_minimums() {
            return Err(PluginError::message(format!(
                "database binding `{owner_plugin_id}/{binding}` failed host capability \
                 minima: {}",
                binding_caps.capability_failure_reason()
            )));
        }
        if provision {
            self.ensure_binding_host_schema(&key, owner_plugin_id, binding)
                .await?;
        }
        let proxy: Arc<Box<dyn ProxyDatabaseTrait>> = Arc::new(Box::new(RpcDatabaseProxy {
            session: self.session.clone(),
            txn_depth: Arc::new(Mutex::new(HashMap::new())),
            caps: binding_caps.clone(),
            binding: Some(key),
        }));
        let db = Database::connect_proxy(canonical_seaorm_backend(), proxy)
            .await
            .map_err(|err| PluginError::message(err.to_string()))?;
        Ok((db, binding_caps))
    }

    /// Applies host-owned binding bootstrap (SchemaState machine, BookclerkSQL only).
    async fn ensure_binding_host_schema(
        &self,
        key: &str,
        owner: &str,
        binding: &str,
    ) -> PluginResult<()> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.session
            .db_execute_binding_request(
                key,
                stamp_library_adapter_request(
                    binding_sql_request(
                        format!("binding-schema-migrations-{owner}-{binding}"),
                        vec![SCHEMA_MIGRATIONS_DDL.to_string()],
                    ),
                    IsolationReq::AtomicBatch,
                )?,
                Arc::clone(&cancel),
            )
            .await?;
        let select = ExecuteRequest {
            operation_id: format!("binding-schema-state-{owner}-{binding}"),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![TypedDbStatement {
                sql: format!(
                    "SELECT version, state, checksum FROM bookclerk_schema_migrations \
                     WHERE namespace = {}",
                    sql_string_literal(BOOKCLERK_SCHEMA_NAMESPACE)
                ),
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Select,
                max_rows: 64,
                result_selection: DbResultSelection::Rows,
            }],
        };
        let reply = self
            .session
            .db_execute_binding_request(
                key,
                stamp_library_adapter_request(select, IsolationReq::AtomicBatch)?,
                Arc::clone(&cancel),
            )
            .await?;
        let state = binding_schema_state_from_reply(&reply)?;
        let Some(stmts) =
            binding_bootstrap_plan(&state).map_err(|err| PluginError::message(err.to_string()))?
        else {
            return Ok(());
        };
        self.session
            .db_execute_binding_request(
                key,
                stamp_library_adapter_request(
                    binding_sql_request(format!("binding-bootstrap-{owner}-{binding}"), stmts),
                    IsolationReq::AtomicBatch,
                )?,
                cancel,
            )
            .await?;
        Ok(())
    }

    /// Applies the pending registered plugin-migration suffix after host bootstrap.
    ///
    /// Restore uses [`Self::open_binding_seaorm`] with `provision` and does
    /// **not** call this; the next ordinary open registers and walks the suffix.
    ///
    /// Durable history is the outer progress loop. Each expected migration has
    /// its own bounded retry loop. A matching `(id, checksum)` at the expected
    /// ordinal after an ambiguous reply is success.
    async fn ensure_plugin_migrations(
        &self,
        key: &str,
        owner: &str,
        binding: &str,
        registered: &bookclerk_library::PluginMigrationSequence,
    ) -> PluginResult<()> {
        let host = BindingPluginMigrationHost {
            session: &self.session,
            key,
            owner,
            binding,
            registered,
        };
        super::plugin_migration_apply::apply_registered_plugin_migrations(
            &host, owner, binding, registered,
        )
        .await
    }

    /// Reads the host-private plugin migration journal from the binding.
    async fn binding_plugin_history(
        &self,
        key: &str,
        owner: &str,
        binding: &str,
    ) -> PluginResult<PluginMigrationHistory> {
        let cancel = Arc::new(AtomicBool::new(false));
        let select =
            plugin_journal_select_request(format!("binding-plugin-history-{owner}-{binding}"), 0);
        let reply = self
            .session
            .db_execute_binding_request(
                key,
                stamp_adapter_request(
                    select,
                    &binding_bootstrap_type_env(),
                    IsolationReq::AtomicBatch,
                )?,
                cancel,
            )
            .await?;
        history_from_execute_reply(&reply).map_err(|err| PluginError::message(err.to_string()))
    }

    /// Backend-native default unit for one `(plugin, binding)` pair.
    fn default_binding_unit(
        &self,
        config: &Config,
        kind: Option<DatabasePluginKind>,
        owner_plugin_id: &str,
        binding: &str,
    ) -> String {
        plugin_binding_unit_ref(config, kind, owner_plugin_id, binding)
    }

    /// Per-binding `database.openSession` factory context.
    fn binding_connect_context(
        &self,
        config: &Config,
        kind: Option<DatabasePluginKind>,
        owner_plugin_id: &str,
        binding: &str,
        unit_ref: &str,
        provision: bool,
    ) -> PluginResult<bookclerk_plugin_sdk::DatabaseContext> {
        let data_dir = self.plugin_data_dir.display().to_string();
        let params = match kind {
            Some(DatabasePluginKind::Sqlite) => DbConnectParams::Sqlite {
                plugin_data_dir: data_dir,
                sqlite_path: Some(unit_ref.to_string()),
                binding: Some(binding.to_string()),
                provision,
            },
            Some(DatabasePluginKind::Postgres) => DbConnectParams::Postgres {
                plugin_data_dir: data_dir,
                url: resolve_postgres_url(config)
                    .map_err(|err| PluginError::message(err.to_string()))?,
                binding: Some(binding.to_string()),
                database: Some(unit_ref.to_string()),
                provision,
            },
            Some(DatabasePluginKind::D1) => DbConnectParams::D1 {
                plugin_data_dir: data_dir,
                account_id: config.database.d1.account_id.clone(),
                database_id: String::new(),
                api_base: config.database.d1.api_base.clone(),
                api_token: resolve_d1_api_token()
                    .map_err(|err| PluginError::message(err.to_string()))?,
                binding: Some(binding.to_string()),
                database_name: Some(unit_ref.to_string()),
                provision,
            },
            None => {
                let adapter_config = bookclerk_plugin_abi::DatabaseAdapterConfig {
                    plugin_data_dir: data_dir,
                    config: if self.settings_json.is_null() {
                        Value::Object(serde_json::Map::new())
                    } else {
                        self.settings_json.clone()
                    },
                    binding: Some(binding.to_string()),
                    instance_id: Some(binding_instance_id(owner_plugin_id, binding)),
                    provision,
                };
                return bookclerk_plugin_abi::database_context_from_adapter_config(&adapter_config)
                    .map_err(|err| PluginError::message(err.to_string()));
            }
        };
        database_context_from_params(&params).map_err(|err| PluginError::message(err.to_string()))
    }

    /// Physically deletes a provisioned binding unit. The registry row is the
    /// caller's to remove **after** this returns success.
    ///
    /// SQLite deletes the file and journal sidecars. PostgreSQL issues
    /// `DROP DATABASE`. D1 deletes the Cloudflare database by name. Unknown
    /// adapters fail closed so a registry row cannot outlive a unit the host
    /// cannot prove is gone.
    ///
    /// # Errors
    ///
    /// Returns when credentials are missing, the backend refuses the delete,
    /// or the adapter family is unknown.
    pub async fn drop_provisioned_unit(
        config: &Config,
        backend_kind: &str,
        unit_ref: &str,
    ) -> PluginResult<()> {
        match backend_kind {
            "sqlite" => {
                for suffix in ["", "-wal", "-shm", "-journal"] {
                    let path = format!("{unit_ref}{suffix}");
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            return Err(PluginError::message(format!(
                                "could not delete {path}: {err}"
                            )));
                        }
                    }
                }
                Ok(())
            }
            "postgres" => {
                let url = resolve_postgres_url(config)
                    .map_err(|err| PluginError::message(err.to_string()))?;
                bookclerk_plugin_database_postgres::drop_binding(&url, unit_ref)
                    .await
                    .map_err(|err| PluginError::message(err.to_string()))
            }
            "d1" => {
                let token =
                    resolve_d1_api_token().map_err(|err| PluginError::message(err.to_string()))?;
                bookclerk_plugin_database_d1::delete_database(
                    &config.database.d1.api_base,
                    &config.database.d1.account_id,
                    &token,
                    unit_ref,
                )
                .await
                .map_err(|err| PluginError::message(err.to_string()))
            }
            other => Err(PluginError::message(format!(
                "cannot drop adapter `{other}` unit `{unit_ref}`: the host cannot prove deletion; \
                 remove it with the adapter, then retry"
            ))),
        }
    }
}

/// Open the library for a specific `[database].plugin` id (ignoring the active config value).
///
/// # Errors
///
/// Returns an error when the operation fails.
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
///
/// # Errors
///
/// Returns an error when the operation fails.
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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
/// Per-task identity used to stack nested SeaORM / guest transaction ids.
enum TaskKey {
    /// Tokio task id when the proxy runs inside a runtime task.
    Tokio(TaskId),
    /// OS thread id when no Tokio task id is available (blocking paths).
    Thread(ThreadId),
}

/// Current Tokio task id, or the OS thread id outside a task.
fn task_key() -> TaskKey {
    match try_id() {
        Some(id) => TaskKey::Tokio(id),
        None => TaskKey::Thread(std::thread::current().id()),
    }
}

#[derive(Clone)]
/// SeaORM [`ProxyDatabaseTrait`] that forwards query/exec/txn RPCs to the guest.
struct RpcDatabaseProxy {
    /// Cap'n Proto session shared with [`ExternalDatabase`].
    session: Arc<PluginSession>,
    /// Per-task nested begin depth (vat holds a single transaction).
    txn_depth: Arc<Mutex<HashMap<TaskKey, usize>>>,
    /// Negotiated guest capabilities (statement/bind/request byte limits).
    caps: DbCapabilities,
    /// Binding key when this proxy is a plugin database unit (`plugin_id/binding`).
    binding: Option<String>,
}

impl std::fmt::Debug for RpcDatabaseProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcDatabaseProxy").finish_non_exhaustive()
    }
}

impl RpcDatabaseProxy {
    /// Locks the per-task txn depth map, recovering a poisoned mutex.
    fn lock_depth(&self) -> std::sync::MutexGuard<'_, HashMap<TaskKey, usize>> {
        self.txn_depth.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Current nested begin depth for this task.
    fn depth(&self) -> usize {
        self.lock_depth().get(&task_key()).copied().unwrap_or(0)
    }

    /// Increments depth; returns the previous depth.
    fn push_depth(&self) -> usize {
        let mut map = self.lock_depth();
        let entry = map.entry(task_key()).or_insert(0);
        let prev = *entry;
        *entry = prev.saturating_add(1);
        prev
    }

    /// Decrements depth; returns the depth after decrement (`0` means fully closed).
    fn pop_depth(&self) -> Option<usize> {
        let mut map = self.lock_depth();
        let key = task_key();
        let entry = map.get_mut(&key)?;
        if *entry == 0 {
            map.remove(&key);
            return None;
        }
        *entry -= 1;
        let next = *entry;
        if next == 0 {
            map.remove(&key);
        }
        Some(next)
    }

    /// Serializes a SeaORM statement into a typed `ExecuteRequest` statement.
    ///
    /// The proxy is opened with the canonical SQLite SeaORM backend, so
    /// `statement.sql` uses Bookclerk `?` placeholders. Adapters lower at
    /// execute.
    ///
    /// # Errors
    ///
    /// Returns when a SeaORM bind is outside the universal `DbValue` domain.
    fn statement_to_typed(
        statement: &Statement,
        kind: DbPlanStatementKind,
        selection: DbResultSelection,
    ) -> Result<TypedDbStatement, DbErr> {
        let parameters = match &statement.values {
            Some(values) => values
                .0
                .iter()
                .map(db_value_from_sea)
                .collect::<Result<Vec<_>, _>>()
                .map_err(DbErr::Custom)?,
            None => Vec::new(),
        };
        Ok(TypedDbStatement {
            sql: statement.sql.clone(),
            parameters,
            kind,
            max_rows: 0,
            result_selection: selection,
        })
    }

    /// One-statement `ExecuteRequest` used by the proxy query/execute/ping path.
    fn typed_request(stmt: TypedDbStatement, operation_id: String) -> ExecuteRequest {
        ExecuteRequest {
            operation_id,
            request_hash: String::new(),
            statements: vec![stmt],
            deadline_unix_ms: 0,
        }
    }

    /// Validates `req` against negotiated caps, stamps the host request hash,
    /// then sends typed `execute`.
    ///
    /// Guest-supplied statement kinds are replaced with host-authored kinds
    /// derived from the SQL. After validation the host stamps the canonical
    /// Cap'n request hash. A non-empty guest hash must match that digest
    /// (explicit retry token).
    ///
    /// # Errors
    ///
    /// Returns a plugin ABI `invalid_params` error when the request exceeds
    /// negotiated caps or the retry hash does not match, and a plugin error
    /// when typed `execute` fails.
    async fn execute_typed_validated(
        &self,
        mut req: ExecuteRequest,
    ) -> Result<bookclerk_plugin_sdk::ExecuteReply, crate::PluginError> {
        bookclerk_library::authorize_typed_request(&mut req, &self.caps)
            .map_err(|err| crate::PluginError::from_abi(Some("invalid_params"), err.to_string()))?;
        let validate_req = req.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let on_txn = self.depth() > 0;
        let isolation = binding_isolation(on_txn);
        let catalog = if let Some(binding) = self.binding.as_deref() {
            load_binding_sql_type_env(
                &self.session,
                binding,
                &cancel,
                req.deadline_unix_ms,
                self.caps.max_result_rows,
                on_txn,
            )
            .await
            .map_err(|err| crate::PluginError::from_abi(Some("invalid_params"), err.to_string()))?
        } else {
            bookclerk_library::migrations::host_sql_type_env()
        };
        let stamped = stamp_adapter_request(req, &catalog, isolation)?;
        #[cfg(test)]
        note_outbound_adapter_sql(&stamped);
        let reply = if on_txn {
            if let Some(binding) = self.binding.as_deref() {
                self.session
                    .db_txn_execute_binding_request(binding, stamped, cancel)
                    .await
            } else {
                self.session.db_txn_execute_request(stamped, cancel).await
            }
        } else if let Some(binding) = self.binding.as_deref() {
            self.session
                .db_execute_binding_request(binding, stamped, cancel)
                .await
        } else {
            self.session.db_execute_request(stamped, cancel).await
        }?;
        bookclerk_library::validate_execute_reply(&validate_req, &reply, &self.caps)
            .map_err(map_reply_validation_err)?;
        Ok(reply)
    }

    /// Commit the guest transaction for this proxy's library or binding session.
    async fn commit_rpc(&self) -> crate::Result<()> {
        match self.binding.as_deref() {
            Some(binding) => self.session.db_commit_binding(binding).await,
            None => self.session.db_commit().await,
        }
    }

    /// Roll back the guest transaction for this proxy's library or binding session.
    async fn rollback_rpc(&self) -> crate::Result<()> {
        match self.binding.as_deref() {
            Some(binding) => self.session.db_rollback_binding(binding).await,
            None => self.session.db_rollback().await,
        }
    }
}

#[async_trait]
impl ProxyDatabaseTrait for RpcDatabaseProxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let typed = Self::statement_to_typed(
            &statement,
            bookclerk_library::proxy_read_kind(&statement.sql),
            DbResultSelection::Rows,
        )?;
        let req = Self::typed_request(typed, format!("proxy-query-{}", uuid::Uuid::new_v4()));
        let reply = self
            .execute_typed_validated(req)
            .await
            .map_err(map_rpc_err)?;
        let stmt =
            reply.statements.into_iter().next().ok_or_else(|| {
                DbErr::Custom("execute query returned no statement result".into())
            })?;
        proxy_rows_from_typed(&stmt).map_err(DbErr::Custom)
    }

    async fn execute(&self, statement: Statement) -> std::result::Result<ProxyExecResult, DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let typed = Self::statement_to_typed(
            &statement,
            bookclerk_library::proxy_write_kind(&statement.sql),
            DbResultSelection::AffectedRows,
        )?;
        let req = Self::typed_request(typed, format!("proxy-exec-{}", uuid::Uuid::new_v4()));
        let reply = self
            .execute_typed_validated(req)
            .await
            .map_err(map_rpc_err)?;
        let rows_affected = reply
            .statements
            .first()
            .map(|s| s.rows_affected)
            .unwrap_or(0);
        Ok(ProxyExecResult {
            last_insert_id: 0,
            rows_affected,
        })
    }

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        let typed = TypedDbStatement {
            sql: "SELECT 1".into(),
            parameters: Vec::new(),
            kind: DbPlanStatementKind::Select,
            max_rows: 1,
            result_selection: DbResultSelection::Rows,
        };
        let req =
            RpcDatabaseProxy::typed_request(typed, format!("proxy-ping-{}", uuid::Uuid::new_v4()));
        self.execute_typed_validated(req)
            .await
            .map_err(map_rpc_err)?;
        Ok(())
    }

    async fn begin(&self) {
        if bookclerk_library::consume_begin_injection() {
            bookclerk_library::note_begin_failed("injected begin failure");
            return;
        }
        let prev = self.push_depth();
        if prev != 0 {
            return;
        }
        let isolation = bookclerk_db_exec::pending_begin_isolation();
        if let Err(err) = match self.binding.as_deref() {
            Some(binding) => self.session.db_begin_binding(binding, isolation).await,
            None => self.session.db_begin(isolation).await,
        } {
            self.pop_depth();
            bookclerk_library::note_begin_failed(&err);
            tracing::error!(error = %err, "database plugin begin failed");
        }
    }

    async fn commit(&self) {
        if bookclerk_library::consume_commit_injection() {
            if self.pop_depth().is_some() && self.depth() == 0 {
                if let Err(err) = self.rollback_rpc().await {
                    tracing::error!(
                        error = %err,
                        "database plugin rollback after injected commit failure"
                    );
                }
            }
            bookclerk_library::note_commit_failed("injected commit failure");
            return;
        }
        let Some(next) = self.pop_depth() else {
            if bookclerk_library::is_txn_broken() {
                return;
            }
            bookclerk_library::note_commit_failed("no open transaction to commit");
            return;
        };
        if next != 0 {
            return;
        }
        if let Err(err) = self.commit_rpc().await {
            bookclerk_library::note_commit_failed(&err);
            tracing::error!(error = %err, "database plugin commit failed");
            if let Err(rb) = self.rollback_rpc().await {
                tracing::error!(error = %rb, "database plugin rollback after commit failure");
            }
        }
    }

    async fn rollback(&self) {
        let Some(next) = self.pop_depth() else {
            return;
        };
        if next != 0 {
            return;
        }
        if let Err(err) = self.rollback_rpc().await {
            tracing::error!(error = %err, "database plugin rollback failed");
        }
    }

    fn start_rollback(&self) {
        let Some(next) = self.pop_depth() else {
            return;
        };
        if next != 0 {
            return;
        }
        let session = self.session.clone();
        let binding = self.binding.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!("database plugin rollback skipped: no tokio runtime");
            return;
        };
        if let Err(err) = tokio::task::block_in_place(|| {
            handle.block_on(async move {
                match binding.as_deref() {
                    Some(name) => session.db_rollback_binding(name).await.map(|_| ()),
                    None => session.db_rollback().await.map(|_| ()),
                }
            })
        }) {
            tracing::error!(error = %err, "database plugin rollback failed");
        }
    }
}

/// Host [`AtomicTxnBackend`] that runs named security ops as one guest atomic batch.
struct RpcAtomicBackend {
    /// Cap'n Proto session used for a single typed `execute` per operation.
    session: Arc<PluginSession>,
    /// Full negotiated capabilities used to reject oversized plans before RPC.
    caps: DbCapabilities,
}

impl RpcAtomicBackend {
    /// Sends one typed `execute` batch; ambiguous transport maps to [`LibraryError::Unavailable`].
    async fn call(
        &self,
        params: bookclerk_library::DbAtomicParams,
    ) -> bookclerk_library::Result<bookclerk_library::DbAtomicResult> {
        let operation_id = bookclerk_library::db_atomic_operation_id(&params);
        self.call_with_id(operation_id, params).await
    }

    /// Sends typed `execute` with a caller-chosen idempotency key (replay-safe claims).
    async fn call_with_id(
        &self,
        operation_id: String,
        params: bookclerk_library::DbAtomicParams,
    ) -> bookclerk_library::Result<bookclerk_library::DbAtomicResult> {
        let now = chrono::Utc::now().to_rfc3339();
        let compiled = bookclerk_library::compile_named_request(&operation_id, &params, &now)
            .map_err(bookclerk_library::LibraryError::Orm)?;
        self.send_compiled(compiled, operation_id).await
    }

    /// Sends an already-compiled generic plan and interprets the guest statement results.
    ///
    /// # Errors
    ///
    /// Returns when validation, typed `execute`, or result interpretation fails.
    async fn send_compiled(
        &self,
        compiled: bookclerk_library::CompiledAtomic,
        operation_id: String,
    ) -> bookclerk_library::Result<bookclerk_library::DbAtomicResult> {
        bookclerk_library::validate_plan(&compiled, &self.caps)?;
        let deadline_unix_ms = unix_now_ms().saturating_add(120_000);
        let mut typed = compiled.clone().into_typed_request(operation_id.clone());
        typed.deadline_unix_ms = deadline_unix_ms;
        bookclerk_library::authorize_typed_request(&mut typed, &self.caps).map_err(|err| {
            bookclerk_library::LibraryError::Other(anyhow::anyhow!(err.to_string()))
        })?;
        let validate_req = typed.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let _guard = CancelOnDrop(Arc::clone(&cancel));
        let remaining_ms = deadline_unix_ms.saturating_sub(unix_now_ms()).max(1);
        let stamped = stamp_library_adapter_request(typed, IsolationReq::AtomicBatch)
            .map_err(|err| bookclerk_library::LibraryError::Orm(DbErr::Custom(err.to_string())))?;
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(remaining_ms)) => {
                cancel.store(true, Ordering::SeqCst);
                Err(bookclerk_library::LibraryError::Unavailable(
                    "deadline_exceeded: host RPC deadline elapsed".into(),
                ))
            }
            result = self.session.db_execute_request(stamped, Arc::clone(&cancel)) => match result {
                Ok(reply) => {
                    bookclerk_library::validate_execute_reply(&validate_req, &reply, &self.caps)?;
                    Ok(bookclerk_library::interpret_typed_exec(
                        &compiled,
                        &reply,
                        &compiled.expected_hash,
                    ))
                }
                Err(err) => Err(map_atomic_rpc_err(err)),
            }
        }
    }
}

/// Maps guest RPC failures: lost/ambiguous replies become [`LibraryError::Unavailable`].
fn map_plugin_err(err: crate::PluginError) -> bookclerk_library::LibraryError {
    if err.is_ambiguous_transport() {
        bookclerk_library::LibraryError::Unavailable(err.to_string())
    } else {
        bookclerk_library::LibraryError::Other(anyhow::anyhow!(err.to_string()))
    }
}

/// In-flight atomic abort is ambiguous (the guest may already have committed).
fn map_atomic_rpc_err(err: crate::PluginError) -> bookclerk_library::LibraryError {
    match &err {
        crate::PluginError::Abi { code, .. } if code == "cancelled" => {
            bookclerk_library::LibraryError::Unavailable(err.to_string())
        }
        _ => map_plugin_err(err),
    }
}

/// Maps host RPC failures onto the ABI [`PluginError`](AbiPluginError) the
/// granted job session returns to guests.
fn host_err_to_abi(err: crate::PluginError) -> AbiPluginError {
    match err {
        crate::PluginError::Abi { code, message } => AbiPluginError::from_wire(&code, message),
        crate::PluginError::Unavailable(message) => AbiPluginError::unavailable(message),
        other => AbiPluginError::internal(other.to_string()),
    }
}

/// Sets `cancel` when an in-flight `db_execute_request` future is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Current unix time in milliseconds.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Runs host DDL as one typed `execute` batch.
async fn exec_host_ddl_batch(
    session: &PluginSession,
    caps: &DbCapabilities,
    stmts: Vec<String>,
) -> bookclerk_library::Result<()> {
    if stmts.is_empty() {
        return Ok(());
    }
    let operation_id = format!("host-schema-{}", uuid::Uuid::new_v4());
    let typed = ExecuteRequest {
        operation_id: operation_id.clone(),
        request_hash: String::new(),
        statements: stmts
            .into_iter()
            .map(|sql| TypedDbStatement {
                sql,
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            })
            .collect(),
        deadline_unix_ms: 0,
    };
    let validate_req = typed.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let stamped = stamp_library_adapter_request(typed, IsolationReq::AtomicBatch)
        .map_err(|err| bookclerk_library::LibraryError::Orm(DbErr::Custom(err.to_string())))?;
    let reply = session
        .db_execute_request(stamped, cancel)
        .await
        .map_err(|err| bookclerk_library::LibraryError::Orm(DbErr::Custom(err.to_string())))?;
    bookclerk_library::validate_execute_reply(&validate_req, &reply, caps)?;
    Ok(())
}

/// Translates a guest atomic status string into a library error, or `None` on `ok`.
fn atomic_app_err(
    status: &str,
    not_found: bookclerk_library::LibraryError,
) -> Option<bookclerk_library::LibraryError> {
    match status {
        s if s == atomic_status::OK => None,
        s if s == atomic_status::NOT_FOUND => Some(not_found),
        s if s == atomic_status::LAST_OWNER => Some(bookclerk_library::LibraryError::LastOwner),
        s if s == atomic_status::CLAIM_INVALID => Some(bookclerk_library::LibraryError::Other(
            anyhow::anyhow!("claim ticket invalid, expired, or already redeemed"),
        )),
        s if s == atomic_status::PASSWORD_REQUIRED => Some(bookclerk_library::LibraryError::Other(
            anyhow::anyhow!("password required — set a password to finish claim login"),
        )),
        s if s == atomic_status::IDEMPOTENCY_CONFLICT => {
            Some(bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                "database atomic operation_id reused with a different request"
            )))
        }
        other => Some(bookclerk_library::LibraryError::Other(anyhow::anyhow!(
            "database atomic operation failed: {other}"
        ))),
    }
}

/// Deserializes the atomic result payload; errors when the guest omitted it.
fn decode_payload<T: serde::de::DeserializeOwned>(
    payload: Option<Value>,
    what: &str,
) -> bookclerk_library::Result<T> {
    let value = payload.ok_or_else(|| {
        bookclerk_library::LibraryError::Other(anyhow::anyhow!(
            "database atomic operation ok without {what} payload"
        ))
    })?;
    serde_json::from_value(value).map_err(|err| {
        bookclerk_library::LibraryError::Other(anyhow::anyhow!(
            "database atomic {what} payload: {err}"
        ))
    })
}

#[async_trait]
impl bookclerk_library::TypedAtomicExec for RpcAtomicBackend {
    async fn execute_typed(
        &self,
        envelope: AdapterExecuteRequest,
    ) -> std::result::Result<ExecuteReply, AbiPluginError> {
        envelope
            .require_proofs()
            .map_err(|err| AbiPluginError::invalid_params(err.to_string()))?;
        self.caps
            .admit_proven_execute(&envelope.request, &envelope.proofs)
            .map_err(|err| AbiPluginError::invalid_params(err.to_string()))?;
        let validate_req = envelope.request.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let reply = self
            .session
            .db_execute_request(envelope, cancel)
            .await
            .map_err(host_err_to_abi)?;
        bookclerk_library::validate_execute_reply(&validate_req, &reply, &self.caps)
            .map_err(map_reply_validation_abi)?;
        Ok(reply)
    }
}

#[async_trait]
impl bookclerk_library::AtomicTxnBackend for RpcAtomicBackend {
    async fn delete_user(&self, id: i64) -> bookclerk_library::Result<()> {
        let result = self
            .call(DbAtomicParams::DeleteUser { user_id: id })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound(format!("user {id}")),
        ) {
            return Err(err);
        }
        Ok(())
    }

    async fn set_user_status(
        &self,
        id: i64,
        status: bookclerk_library::UserStatus,
    ) -> bookclerk_library::Result<bookclerk_library::UserRecord> {
        let result = self
            .call(DbAtomicParams::SetUserStatus {
                user_id: id,
                status: status.as_str().to_string(),
            })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound(format!("user {id}")),
        ) {
            return Err(err);
        }
        decode_payload(result.payload, "user")
    }

    async fn set_user_password_hash(
        &self,
        id: i64,
        password_hash: Option<&str>,
    ) -> bookclerk_library::Result<bookclerk_library::UserRecord> {
        let result = self
            .call(DbAtomicParams::SetUserPasswordHash {
                user_id: id,
                password_hash: password_hash.map(str::to_string),
            })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound(format!("user {id}")),
        ) {
            return Err(err);
        }
        decode_payload(result.payload, "user")
    }

    async fn set_user_role(
        &self,
        id: i64,
        role: bookclerk_library::UserRole,
    ) -> bookclerk_library::Result<bookclerk_library::UserRecord> {
        let result = self
            .call(DbAtomicParams::SetUserRole {
                user_id: id,
                role: role.as_str().to_string(),
            })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound(format!("user {id}")),
        ) {
            return Err(err);
        }
        decode_payload(result.payload, "user")
    }

    async fn redeem_claim_ticket_to_session(
        &self,
        token_hash: &str,
        session_hash: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
        client: Option<&bookclerk_library::SessionClientInfo>,
        new_password_hash: Option<&str>,
        password_fingerprint: Option<&str>,
    ) -> bookclerk_library::Result<bookclerk_library::PortalIdentity> {
        let result = self
            .call(DbAtomicParams::RedeemClaimTicket {
                token_hash: token_hash.to_string(),
                session_hash: session_hash.to_string(),
                expires_at: expires_at.to_rfc3339(),
                user_agent: client.and_then(|c| c.user_agent.clone()),
                device_type: client.map(|c| c.device_type.clone()),
                client_label: client.map(|c| c.client_label.clone()),
                new_password_hash: new_password_hash.map(str::to_string),
                password_fingerprint: password_fingerprint.map(str::to_string),
            })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::Other(anyhow::anyhow!("claim ticket not found")),
        ) {
            return Err(err);
        }
        decode_payload(result.payload, "portal identity")
    }

    async fn take_oidc_rp_state(
        &self,
        state_hash: &str,
    ) -> bookclerk_library::Result<Option<(String, String, String, String, Option<i64>)>> {
        let result = self
            .call(DbAtomicParams::TakeOidcRpState {
                state_hash: state_hash.to_string(),
            })
            .await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound("oidc rp state".into()),
        ) {
            return Err(err);
        }
        let row: AtomicOidcRpState = decode_payload(result.payload, "oidc rp state")?;
        Ok(Some((
            row.provider_id,
            row.pkce_verifier,
            row.nonce,
            row.purpose,
            row.user_id,
        )))
    }

    async fn take_webauthn_challenge(
        &self,
        challenge_id: &str,
        kind: &str,
    ) -> bookclerk_library::Result<Option<(Option<i64>, String)>> {
        let result = self
            .call(DbAtomicParams::TakeWebauthnChallenge {
                challenge_id: challenge_id.to_string(),
                kind: kind.to_string(),
            })
            .await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound("webauthn challenge".into()),
        ) {
            return Err(err);
        }
        let row: AtomicWebauthnChallenge = decode_payload(result.payload, "webauthn challenge")?;
        Ok(Some((row.user_id, row.state_json)))
    }

    async fn enqueue_job(
        &self,
        spec: bookclerk_library::EnqueueJobSpec,
    ) -> bookclerk_library::Result<bookclerk_library::EnqueueOutcome> {
        let payload_json = serde_json::to_string(&spec.payload).map_err(|err| {
            bookclerk_library::LibraryError::Other(anyhow::anyhow!(err.to_string()))
        })?;
        let result = self
            .call(DbAtomicParams::EnqueueJob {
                kind: spec.kind.as_str().to_string(),
                payload_json,
                priority: spec.priority,
                max_attempts: spec.max_attempts,
                max_pending: spec.max_pending,
                run_after: spec.run_after.map(|t| t.to_rfc3339()),
            })
            .await?;
        match result.status.as_str() {
            s if s == atomic_status::OK => {
                let id = result
                    .payload
                    .as_ref()
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                            "enqueueJob ok without id"
                        ))
                    })?
                    .to_string();
                Ok(bookclerk_library::EnqueueOutcome::Created { id })
            }
            s if s == atomic_status::DUPLICATE => {
                let existing_id = result
                    .payload
                    .as_ref()
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                            "enqueueJob duplicate without id"
                        ))
                    })?
                    .to_string();
                Ok(bookclerk_library::EnqueueOutcome::Duplicate { existing_id })
            }
            s if s == atomic_status::QUEUE_FULL => Ok(bookclerk_library::EnqueueOutcome::QueueFull),
            other => Err(bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                "enqueueJob failed: {other}"
            ))),
        }
    }

    async fn claim_next_job(
        &self,
        resource_class: bookclerk_library::JobResourceClass,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
    ) -> bookclerk_library::Result<Option<bookclerk_library::JobRecord>> {
        let result = self
            .call_with_id(
                operation_id.to_string(),
                DbAtomicParams::ClaimNextJob {
                    resource_class: resource_class.as_str().to_string(),
                    owner: owner.to_string(),
                    lease_secs: i64::try_from(lease_secs).unwrap_or(60),
                },
            )
            .await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound("job".into()),
        ) {
            return Err(err);
        }
        decode_payload(result.payload, "job")
    }

    async fn reserve_job_temp_path(
        &self,
        job_id: &str,
        path: &str,
        reserved_bytes: u64,
        quota_bytes: u64,
    ) -> bookclerk_library::Result<()> {
        let result = self
            .call(DbAtomicParams::ReserveJobTemp {
                job_id: job_id.to_string(),
                path: path.to_string(),
                reserved_bytes: i64::try_from(reserved_bytes).unwrap_or(i64::MAX),
                quota_bytes: i64::try_from(quota_bytes).unwrap_or(i64::MAX),
            })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                "acquire scratch quota exceeded"
            )),
        ) {
            return Err(err);
        }
        Ok(())
    }

    async fn confirm_totp_enrollment(
        &self,
        user_id: i64,
        record: &bookclerk_library::EncryptedSecretRecord,
    ) -> bookclerk_library::Result<()> {
        let result = self
            .call(DbAtomicParams::ConfirmTotpEnrollment {
                user_id,
                format: record.format.clone(),
                ciphertext: bookclerk_library::bytes_to_b64_string(&record.ciphertext),
                cipher_algorithm: record.cipher_algorithm.clone(),
                cipher_nonce: record
                    .cipher_nonce
                    .as_deref()
                    .map(bookclerk_library::bytes_to_b64_string),
                kdf_algorithm: record.kdf_algorithm.clone(),
                kdf_salt: record
                    .kdf_salt
                    .as_deref()
                    .map(bookclerk_library::bytes_to_b64_string),
                kdf_m_cost: record.kdf_m_cost.map(i64::from),
                kdf_t_cost: record.kdf_t_cost.map(i64::from),
                kdf_p_cost: record.kdf_p_cost.map(i64::from),
                created_at: record.created_at.clone(),
            })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound("user".into()),
        ) {
            return Err(err);
        }
        Ok(())
    }

    async fn disable_user_totp(&self, user_id: i64) -> bookclerk_library::Result<()> {
        let result = self
            .call(DbAtomicParams::DisableUserTotp { user_id })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound("user".into()),
        ) {
            return Err(err);
        }
        Ok(())
    }

    async fn publish_domain_event(
        &self,
        spec: bookclerk_library::PublishDomainEventSpec,
    ) -> bookclerk_library::Result<bookclerk_library::PublishDomainEventOutcome> {
        let spec = bookclerk_library::prepare_publish_domain_event(spec)?;
        let result = self
            .call(DbAtomicParams::PublishDomainEvent {
                id: spec.id,
                event_type: spec.event_type,
                schema_version: spec.schema_version,
                account_id: spec.account_id,
                source: spec.source,
                correlation_id: spec.correlation_id,
                causation_id: spec.causation_id,
                dedup_key: spec.dedup_key,
                payload: spec.payload,
                ordering_key: spec.ordering_key,
            })
            .await?;
        match result.status.as_str() {
            s if s == atomic_status::OK => {
                let id = result
                    .payload
                    .as_ref()
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                            "publishDomainEvent ok without id"
                        ))
                    })?
                    .to_string();
                Ok(bookclerk_library::PublishDomainEventOutcome::Created { id })
            }
            s if s == atomic_status::DUPLICATE => {
                let existing_id = result
                    .payload
                    .as_ref()
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                            "publishDomainEvent duplicate without id"
                        ))
                    })?
                    .to_string();
                Ok(bookclerk_library::PublishDomainEventOutcome::Duplicate { existing_id })
            }
            other => Err(bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                "publishDomainEvent failed: {other}"
            ))),
        }
    }

    async fn dispatch_event_deliveries(
        &self,
        event_id: &str,
        subscribers: &[bookclerk_library::EventSubscriber],
        operation_id: &str,
        mark_dispatched: bool,
    ) -> bookclerk_library::Result<u32> {
        let subscribers_json = serde_json::to_string(subscribers).map_err(|err| {
            bookclerk_library::LibraryError::Other(anyhow::anyhow!(err.to_string()))
        })?;
        let result = self
            .call_with_id(
                operation_id.to_string(),
                DbAtomicParams::DispatchEventDeliveries {
                    event_id: event_id.to_string(),
                    subscribers_json,
                    mark_dispatched,
                },
            )
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound(format!("event {event_id}")),
        ) {
            return Err(err);
        }
        Ok(result
            .payload
            .as_ref()
            .and_then(|v| v.get("created"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32)
    }

    #[allow(clippy::too_many_arguments)]
    async fn claim_event_delivery(
        &self,
        delivery_id: &str,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
        plugin_id: &str,
        resource_class: &str,
        max_in_flight: u32,
    ) -> bookclerk_library::Result<Option<bookclerk_library::EventDeliveryRecord>> {
        let now = chrono::Utc::now().to_rfc3339();
        let compiled = bookclerk_library::compile_claim_event_delivery(
            operation_id,
            delivery_id,
            owner,
            i64::try_from(lease_secs).unwrap_or(60),
            plugin_id,
            resource_class,
            i64::from(max_in_flight),
            &now,
        )
        .map_err(bookclerk_library::LibraryError::Orm)?;
        let result = self
            .send_compiled(compiled, operation_id.to_string())
            .await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound("event delivery".into()),
        ) {
            return Err(err);
        }
        decode_payload(result.payload, "event delivery").map(Some)
    }

    async fn set_acquire_status(
        &self,
        book_uuid: &str,
        status: bookclerk_library::AcquireStatus,
        storage_key: Option<&str>,
        error_message: Option<&str>,
        event: Option<bookclerk_library::PublishDomainEventSpec>,
    ) -> bookclerk_library::Result<()> {
        let event = match event {
            Some(spec) => bookclerk_library::prepare_publish_domain_event(spec)?,
            None => bookclerk_library::PublishDomainEventSpec {
                id: String::new(),
                event_type: String::new(),
                schema_version: 0,
                account_id: String::new(),
                source: String::new(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: String::new(),
                payload: String::new(),
                ordering_key: String::new(),
            },
        };
        let result = self
            .call(DbAtomicParams::SetAcquireStatus {
                book_uuid: book_uuid.to_string(),
                status: status.as_str().to_string(),
                storage_key: storage_key.map(str::to_string),
                error_message: error_message.map(str::to_string),
                event_id: event.id,
                event_type: event.event_type,
                schema_version: event.schema_version,
                event_account_id: event.account_id,
                source: event.source,
                correlation_id: event.correlation_id,
                causation_id: event.causation_id,
                dedup_key: event.dedup_key,
                payload: event.payload,
                ordering_key: event.ordering_key,
            })
            .await?;
        if let Some(err) = atomic_app_err(
            &result.status,
            bookclerk_library::LibraryError::NotFound(format!("book {book_uuid}")),
        ) {
            return Err(err);
        }
        Ok(())
    }
}

#[derive(serde::Deserialize)]
/// One-shot OIDC RP state returned by guest `takeOidcRpState` (consumed on read).
struct AtomicOidcRpState {
    /// Configured OIDC provider id that minted this state.
    provider_id: String,
    /// PKCE code verifier for the pending authorization-code exchange.
    pkce_verifier: String,
    /// Nonce that must match the verified ID token.
    nonce: String,
    /// Login vs link (or other) purpose recorded when the state was stored.
    purpose: String,
    #[serde(default)]
    /// Existing user to link when this is not a JIT signup, if any.
    user_id: Option<i64>,
}

#[derive(serde::Deserialize)]
/// One-shot WebAuthn challenge returned by guest `takeWebauthnChallenge`.
struct AtomicWebauthnChallenge {
    #[serde(default)]
    /// User this challenge was issued for; `None` during usernameless login.
    user_id: Option<i64>,
    /// Opaque webauthn-rs state JSON needed to finish the ceremony.
    state_json: String,
}

/// Builds a [`bookclerk_plugin_sdk::DatabaseContext`] for `database.openSession`.
///
/// Used by the CLI diagnose probe and mirrors [`ExternalDatabase::connect`].
///
/// # Errors
///
/// Returns an error when plugin data paths, secrets, or context encoding fail.
pub fn database_connect_context(
    config: &Config,
    plugin: &DiscoveredPlugin,
    session: &PluginSession,
) -> PluginResult<bookclerk_plugin_sdk::DatabaseContext> {
    let plugin_data_dir = plugin_data_dir(config, &plugin.manifest.id)?;
    let table = crate::settings_table(config, plugin);
    let settings_json = toml_to_json(&toml::Value::Table(table));
    connect_context(
        config,
        &plugin.manifest.id,
        &plugin_data_dir,
        session,
        &settings_json,
    )
    .map_err(|err| PluginError::message(err.to_string()))
}

/// Builds the `database.openSession` factory context from host config.
///
/// First-party ids (`sqlite`, `d1`, `postgres`) receive host-private connect
/// params with host-injected paths / secrets. Any other id is a third-party
/// adapter and receives the public [`bookclerk_plugin_abi::DatabaseAdapterConfig`]
/// payload carrying its granted `[database.<id>]` settings, so custom adapters
/// bootstrap without a host registry change.
fn connect_context(
    config: &Config,
    plugin_id: &str,
    plugin_data_dir: &Path,
    session: &PluginSession,
    settings_json: &Value,
) -> Result<bookclerk_plugin_sdk::DatabaseContext, DbErr> {
    let data_dir = plugin_data_dir.display().to_string();
    let params = match DatabasePluginKind::parse(plugin_id) {
        Some(DatabasePluginKind::Sqlite) => sqlite_connect_params(config, plugin_data_dir),
        Some(DatabasePluginKind::D1) => {
            session
                .require_binding("secrets")
                .map_err(|err| DbErr::Custom(err.to_string()))?;
            DbConnectParams::D1 {
                plugin_data_dir: data_dir,
                account_id: config.database.d1.account_id.clone(),
                database_id: config.database.d1.database_id.clone(),
                api_base: config.database.d1.api_base.clone(),
                api_token: resolve_d1_api_token().map_err(map_config_err)?,
                binding: None,
                database_name: None,
                provision: true,
            }
        }
        Some(DatabasePluginKind::Postgres) => {
            session
                .require_binding("secrets")
                .map_err(|err| DbErr::Custom(err.to_string()))?;
            DbConnectParams::Postgres {
                plugin_data_dir: data_dir,
                url: resolve_postgres_url(config).map_err(map_config_err)?,
                binding: None,
                database: None,
                provision: true,
            }
        }
        None => return adapter_config_context(&data_dir, settings_json),
    };
    database_context_from_params(&params).map_err(|err| DbErr::Custom(err.to_string()))
}

/// Public third-party adapter factory context (granted settings + data dir).
fn adapter_config_context(
    data_dir: &str,
    settings_json: &Value,
) -> Result<bookclerk_plugin_sdk::DatabaseContext, DbErr> {
    let adapter_config = bookclerk_plugin_abi::DatabaseAdapterConfig {
        plugin_data_dir: data_dir.to_string(),
        config: if settings_json.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            settings_json.clone()
        },
        binding: None,
        instance_id: None,
        provision: true,
    };
    bookclerk_plugin_abi::database_context_from_adapter_config(&adapter_config)
        .map_err(|err| DbErr::Custom(err.to_string()))
}

/// SeaORM proxy backend used for host query building.
///
/// Always SQLite-shaped so SeaORM emits canonical Bookclerk SQL (`?`
/// placeholders). Adapters lower at execute. Bootstrap `engine` is diagnostic
/// only and never selects a query dialect or gates connect.
fn canonical_seaorm_backend() -> DbBackend {
    DbBackend::Sqlite
}

/// Fills missing diagnostic engine identity from the plugin id.
///
/// Guest-reported `engine` always wins. The host never requires sqlite or
/// postgres names.
fn apply_bootstrap_metadata(bootstrap: &mut DbBootstrap, plugin_id: &str) {
    if !bootstrap.engine.is_empty() {
        return;
    }
    bootstrap.engine = match DatabasePluginKind::parse(plugin_id) {
        Some(DatabasePluginKind::Postgres) => "postgres".into(),
        Some(DatabasePluginKind::D1) => "d1".into(),
        Some(DatabasePluginKind::Sqlite) => "sqlite".into(),
        None => plugin_id.to_string(),
    };
}

/// SQLite connect params for first-party `sqlite` and arbitrary sqlite-family ids.
fn sqlite_connect_params(config: &Config, plugin_data_dir: &Path) -> DbConnectParams {
    let path = config.database.sqlite_path(&config.paths().files_dir);
    DbConnectParams::Sqlite {
        plugin_data_dir: plugin_data_dir.display().to_string(),
        sqlite_path: Some(path.display().to_string()),
        binding: None,
        provision: true,
    }
}

/// Maps host reply validation onto a plugin-host RPC error.
fn map_reply_validation_err(err: bookclerk_library::LibraryError) -> crate::PluginError {
    match err {
        bookclerk_library::LibraryError::Unavailable(message) => {
            crate::PluginError::Unavailable(message)
        }
        other => crate::PluginError::from_abi(Some("invalid_params"), other.to_string()),
    }
}

/// Maps host reply validation onto the ABI error returned to guests.
fn map_reply_validation_abi(err: bookclerk_library::LibraryError) -> AbiPluginError {
    match err {
        bookclerk_library::LibraryError::Unavailable(message) => {
            AbiPluginError::unavailable(message)
        }
        other => AbiPluginError::invalid_params(other.to_string()),
    }
}

/// Wraps a plugin RPC failure as a SeaORM [`DbErr::Custom`].
fn map_rpc_err(err: crate::PluginError) -> DbErr {
    DbErr::Custom(err.to_string())
}

/// Wraps a host config error (missing D1 token / Postgres URL) as [`DbErr::Custom`].
fn map_config_err(err: bookclerk_config::ConfigError) -> DbErr {
    DbErr::Custom(err.to_string())
}

/// Converts plugin settings TOML to JSON for guest spawn; invalid values become `null`.
fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::DbCapabilities;

    #[test]
    fn backup_adapter_id_keeps_third_party_plugin_id() {
        assert_eq!(backup_adapter_id("sqlite"), "sqlite");
        assert_eq!(backup_adapter_id("postgres"), "postgres");
        assert_eq!(backup_adapter_id("d1"), "d1");
        assert_eq!(backup_adapter_id("sql-conformance"), "sql-conformance");
    }

    #[test]
    fn apply_bootstrap_metadata_from_plugin_id() {
        let mut bootstrap = DbBootstrap::default();
        apply_bootstrap_metadata(&mut bootstrap, "d1");
        assert_eq!(bootstrap.engine, "d1");

        let mut pg = DbBootstrap::default();
        apply_bootstrap_metadata(&mut pg, "postgres");
        assert_eq!(pg.engine, "postgres");

        let mut unknown = DbBootstrap::default();
        apply_bootstrap_metadata(&mut unknown, "sql-conformance");
        assert_eq!(unknown.engine, "sql-conformance");
        assert_eq!(canonical_seaorm_backend(), DbBackend::Sqlite);
        assert_eq!(
            canonical_seaorm_backend(),
            DbBackend::Sqlite,
            "SeaORM query building is canonical SQLite-shaped for every engine"
        );
    }

    #[test]
    fn unfamiliar_engine_is_accepted_without_sqlite_or_postgres_metadata() {
        let mut reported = DbBootstrap::diagnostic("foundationdb-sql");
        apply_bootstrap_metadata(&mut reported, "custom-sql");
        assert_eq!(reported.engine, "foundationdb-sql");
        let caps = DbCapabilities::advertised_sqlite();
        assert!(caps.meets_host_minimums());
        bookclerk_library::require_schema_migrations(&caps).unwrap();
        assert_eq!(canonical_seaorm_backend(), DbBackend::Sqlite);
    }

    #[test]
    fn row_migrations_requirement_is_independent_of_sqlite_bootstrap() {
        let caps = DbCapabilities::advertised_sqlite();
        bookclerk_library::require_schema_migrations(&caps).unwrap();
        let mut none = caps;
        none.schema_migrations = false;
        assert!(bookclerk_library::require_schema_migrations(&none).is_err());
        assert_eq!(canonical_seaorm_backend(), DbBackend::Sqlite);
    }

    #[test]
    fn unknown_plugin_id_gets_public_adapter_config_with_granted_settings() {
        assert!(DatabasePluginKind::parse("sql-conformance").is_none());
        // Granted `[database.sql-conformance]` settings the operator wrote.
        let settings = serde_json::json!({ "url": "custom://host/db", "pool_size": 4 });
        let ctx = adapter_config_context("/tmp/plugins/sql-conformance/data", &settings)
            .expect("adapter context");
        // No host-private connect params travel to third-party adapters …
        bookclerk_plugin_abi::db::connect_params_from_context(&ctx)
            .expect_err("public adapter config must not decode as host connect params");
        // … the public payload carries the granted settings, readable without
        // the abi `host` feature.
        let cfg = bookclerk_plugin_abi::database_adapter_config_from_context(&ctx)
            .expect("public decode");
        assert_eq!(cfg.plugin_data_dir, "/tmp/plugins/sql-conformance/data");
        assert_eq!(cfg.config["url"], "custom://host/db");
        assert_eq!(cfg.config["pool_size"], 4);
    }

    #[test]
    fn unknown_plugin_id_without_settings_gets_empty_config_object() {
        let ctx = adapter_config_context("/tmp/plugins/custom/data", &Value::Null)
            .expect("adapter context");
        let cfg = bookclerk_plugin_abi::database_adapter_config_from_context(&ctx)
            .expect("public decode");
        assert!(cfg.config.is_object(), "{:?}", cfg.config);
        assert!(cfg.instance_id.is_none());
    }

    #[test]
    fn binding_instance_ids_are_distinct_for_the_collision_pair() {
        let alpha = binding_instance_id("alpha", "DB");
        let beta = binding_instance_id("beta", "DB");
        assert_ne!(alpha, beta);
        assert_eq!(alpha.len(), 64);
        assert_eq!(
            binding_instance_id("ab_c", "D"),
            binding_instance_id("ab_c", "D"),
            "stable across calls"
        );
        let a = postgres_binding_database_name("ab_c", "D");
        let b = postgres_binding_database_name("ab", "C_D");
        assert_ne!(a, b, "sanitized concatenation must not collide");
        assert!(a.starts_with("pb_"), "{a}");
        assert!(a.len() <= 63, "{a}");
        assert!(b.len() <= 63, "{b}");
        let d1_a = d1_binding_database_name("ab_c", "D");
        let d1_b = d1_binding_database_name("ab", "C_D");
        assert_ne!(d1_a, d1_b);
        let long_owner = "p".repeat(256);
        let long_binding = "b".repeat(256);
        let pg_long = postgres_binding_database_name(&long_owner, &long_binding);
        let d1_long = d1_binding_database_name(&long_owner, &long_binding);
        assert!(pg_long.len() <= 63, "{pg_long}");
        assert!(d1_long.len() <= 64, "{d1_long}");
        assert_eq!(binding_instance_id(&long_owner, &long_binding).len(), 64);
    }

    #[test]
    fn guest_bootstrap_from_session_overrides_plugin_id_inference() {
        let mut bootstrap = DbBootstrap::sqlite();
        apply_bootstrap_metadata(&mut bootstrap, "postgres");
        assert_eq!(bootstrap.engine, "sqlite");
        assert_eq!(canonical_seaorm_backend(), DbBackend::Sqlite);
    }

    #[test]
    fn ambiguous_plugin_errors_map_to_unavailable() {
        let err = crate::PluginError::unavailable("D1 HTTP 502");
        assert!(err.is_ambiguous_transport());
        assert!(matches!(
            map_plugin_err(err),
            bookclerk_library::LibraryError::Unavailable(_)
        ));
        let other = crate::PluginError::message("no such table");
        assert!(!other.is_ambiguous_transport());
        assert!(matches!(
            map_plugin_err(other),
            bookclerk_library::LibraryError::Other(_)
        ));
        let permanent = crate::PluginError::message("D1 HTTP 400: SQL error");
        assert!(!permanent.is_ambiguous_transport());
    }

    #[test]
    fn consume_once_rpc_payload_reuses_stable_operation_id() {
        let params = DbAtomicParams::TakeOidcRpState {
            state_hash: "abc".into(),
        };
        let first = bookclerk_library::db_atomic_operation_id(&params);
        let second = bookclerk_library::db_atomic_operation_id(&params);
        assert_eq!(first, second);
        assert_eq!(first, "takeOidcRpState:abc");
    }

    #[test]
    fn omitted_rows_affected_fails_deserialize() {
        let err = serde_json::from_str::<bookclerk_plugin_abi::StatementResult>(r#"{"rows":[]}"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("rowsAffected") || err.to_string().contains("missing field"),
            "{err}"
        );
    }

    #[test]
    fn malformed_atomic_json_maps_to_unavailable() {
        let err = crate::PluginError::unavailable("lost reply");
        assert!(matches!(
            map_plugin_err(err),
            bookclerk_library::LibraryError::Unavailable(_)
        ));
    }

    /// Defense in depth: even a test-only host-library grant that allows
    /// `books` cannot reach `jobs`. Production jobs never inject this session.
    #[tokio::test]
    async fn granted_job_database_allows_books_and_denies_jobs() {
        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
        .with_db_capabilities(DbCapabilities::advertised_sqlite());
        let session = granted_job_database(store);
        session
            .execute(ExecuteRequest {
                operation_id: "job-books".into(),
                request_hash: String::new(),
                statements: vec![TypedDbStatement {
                    sql: "SELECT id FROM books LIMIT 1".into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Select,
                    max_rows: 8,
                    result_selection: DbResultSelection::Rows,
                }],
                deadline_unix_ms: 0,
            })
            .await
            .expect("books grant");
        let err = session
            .execute(ExecuteRequest {
                operation_id: "job-jobs".into(),
                request_hash: String::new(),
                statements: vec![TypedDbStatement {
                    sql: "SELECT id FROM jobs".into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Select,
                    max_rows: 8,
                    result_selection: DbResultSelection::Rows,
                }],
                deadline_unix_ms: 0,
            })
            .await
            .expect_err("jobs denied");
        assert_eq!(
            err.code,
            bookclerk_plugin_sdk::PluginErrorCode::InvalidParams
        );
        assert!(err.to_string().contains("unauthorized table"), "{err}");
    }
    #[tokio::test]
    async fn granted_job_database_binding_retries_and_conflicts_on_counters() {
        use bookclerk_plugin_sdk::{DatabaseBinding, DbValue, RetryToken};
        use sea_orm::{ConnectionTrait, Statement};

        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
        .with_db_capabilities(DbCapabilities::advertised_sqlite());
        const COUNTERS_DDL: &str =
            "CREATE TABLE counters (id INTEGER PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0)";
        store
            .connection()
            .execute_raw(Statement::from_string(DbBackend::Sqlite, COUNTERS_DDL))
            .await
            .unwrap();
        let mut env = bookclerk_library::migrations::host_sql_type_env();
        bookclerk_plugin_abi::apply_schema_sql_to_env(&mut env, COUNTERS_DDL);
        let session = granted_job_database_with_policy(
            store,
            bookclerk_library::GuestSqlPolicy::allow_tables(["counters"]).with_sql_types(env),
        );
        let binding = DatabaseBinding::from_session(session);
        let token = RetryToken {
            operation_id: "inc-1".into(),
            request_hash: String::new(),
        };
        let insert = "INSERT INTO counters (id, n) VALUES (?, ?)";
        binding
            .prepare(insert)
            .bind(vec![DbValue::Int64(1), DbValue::Int64(1)])
            .run(Some(token.clone()))
            .await
            .expect("first insert");
        binding
            .prepare(insert)
            .bind(vec![DbValue::Int64(1), DbValue::Int64(1)])
            .run(Some(token.clone()))
            .await
            .expect("idempotent retry");
        let row = binding
            .prepare("SELECT n FROM counters WHERE id = 1")
            .first(None)
            .await
            .expect("select")
            .expect("row");
        let n = row
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("n"))
            .map(|(_, v)| v.clone())
            .expect("n column");
        assert_eq!(n, DbValue::Int64(1), "{n:?}");
        let err = binding
            .prepare(insert)
            .bind(vec![DbValue::Int64(1), DbValue::Int64(99)])
            .run(Some(token))
            .await
            .expect_err("hash mismatch must conflict");
        assert_eq!(err.code, bookclerk_plugin_sdk::PluginErrorCode::Conflict);
        let row = binding
            .prepare("SELECT n FROM counters WHERE id = 1")
            .first(None)
            .await
            .expect("select after conflict")
            .expect("row");
        let n = row
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("n"))
            .map(|(_, v)| v.clone())
            .expect("n column");
        assert_eq!(n, DbValue::Int64(1), "{n:?}");
    }

    #[test]
    fn capped_binding_deadline_inherits_host_when_guest_is_unlimited() {
        assert_eq!(
            capped_binding_deadline(0, 1_700_000_000_000),
            1_700_000_000_000
        );
        assert_eq!(capped_binding_deadline(50, 100), 50);
        assert_eq!(capped_binding_deadline(100, 50), 50);
        assert_eq!(capped_binding_deadline(100, 0), 100);
        assert_eq!(capped_binding_deadline(0, 0), 0);
    }

    #[tokio::test]
    async fn drop_provisioned_sqlite_unit_deletes_file_and_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binding.db");
        std::fs::write(&path, b"sqlite").expect("db file");
        let unit = path.display().to_string();
        std::fs::write(format!("{unit}-wal"), b"wal").expect("wal");
        std::fs::write(format!("{unit}-shm"), b"shm").expect("shm");
        std::fs::write(format!("{unit}-journal"), b"j").expect("journal");
        ExternalDatabase::drop_provisioned_unit(&Config::default(), "sqlite", &unit)
            .await
            .expect("drop sqlite unit");
        assert!(!path.exists(), "binding file must be gone");
        assert!(!std::path::Path::new(&format!("{unit}-wal")).exists());
        assert!(!std::path::Path::new(&format!("{unit}-shm")).exists());
        assert!(!std::path::Path::new(&format!("{unit}-journal")).exists());
    }

    #[tokio::test]
    async fn drop_provisioned_unknown_adapter_fails_closed() {
        let err =
            ExternalDatabase::drop_provisioned_unit(&Config::default(), "custom-sql", "unit-ref")
                .await
                .expect_err("unknown adapter must not unregister");
        assert!(err.to_string().contains("cannot drop"), "{err}");
    }

    #[tokio::test]
    async fn guest_database_execute_does_not_require_caller_proofs() {
        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("mem db"),
        );
        let db = granted_job_database(store);
        let req = ExecuteRequest {
            operation_id: "guest-no-proofs".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 1".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        db.execute(req)
            .await
            .expect("GuestDatabase.execute stays untrusted ExecuteRequest");
    }
}
