//! [`ProxyDatabaseTrait`] adapter over an external database plugin process.
//!
//! The host mediates credentials and forwards SeaORM proxy calls over JSON-RPC.
//! Engine connect / migrate / proxy quirks live entirely in the database guest.
//! There is no in-process fallback.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use async_trait::async_trait;
use bookclerk_config::{resolve_d1_api_token, resolve_postgres_url, Config, DatabasePluginKind};
use bookclerk_plugin_sdk::{
    atomic_status, exec_result_from_dto, methods, proxy_rows_from_dto, statement_to_dto,
    DbAtomicParams, DbAtomicRequest, DbAtomicResult, DbBeginParams, DbBeginResult, DbConnectParams,
    DbConnectResult, DbTxnParams, ExecResultDto, QueryResultDto, StatementDto,
};
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement,
};
use serde_json::Value;
use tokio::task::{try_id, Id as TaskId};

use crate::discover::DiscoveredPlugin;
use crate::jail::plugin_data_dir;
use crate::rpc::PluginClient;
use crate::{PluginError, Result as PluginResult};

/// External database backend spawned for `[database].plugin`.
#[derive(Clone)]
pub struct ExternalDatabase {
    /// JSON-RPC client for the jailed database guest process.
    client: Arc<PluginClient>,
    /// Manifest id (`sqlite`, `d1`, `postgres`) used to build connect params.
    plugin_id: String,
    /// Guest HOME / data directory passed on `db.connect`.
    plugin_data_dir: std::path::PathBuf,
}

impl ExternalDatabase {
    /// Spawn and handshake a database plugin (connection happens later).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn connect(
        &self,
        config: &Config,
    ) -> Result<(DatabaseConnection, DbConnectResult), DbErr> {
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
            txn_stacks: Arc::new(Mutex::new(HashMap::new())),
        }));
        let db = Database::connect_proxy(backend, proxy).await?;
        Ok((db, connect_result))
    }
}

/// Long-lived external database plugin for the active `[database].plugin`.
#[derive(Default, Clone)]
pub struct DatabaseRegistry {
    /// Spawned guest matching `[database].plugin`, if handshake succeeded.
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
    let (db, _caps) = ext
        .connect(config)
        .await
        .map_err(bookclerk_library::LibraryError::Orm)?;
    let store = bookclerk_library::LibraryStore::from_connection(db).with_atomic_txn(Arc::new(
        RpcAtomicBackend {
            client: ext.client.clone(),
        },
    ));
    store.ensure_users_bridged().await?;
    Ok(store)
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
    /// JSON-RPC client shared with [`ExternalDatabase`].
    client: Arc<PluginClient>,
    /// Per-task stack of guest txn ids (nested SeaORM begin = nested RPC).
    txn_stacks: Arc<Mutex<HashMap<TaskKey, Vec<String>>>>,
}

impl std::fmt::Debug for RpcDatabaseProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcDatabaseProxy").finish_non_exhaustive()
    }
}

impl RpcDatabaseProxy {
    /// Locks the per-task txn stack, recovering a poisoned mutex.
    fn lock_stacks(&self) -> std::sync::MutexGuard<'_, HashMap<TaskKey, Vec<String>>> {
        self.txn_stacks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Innermost guest txn id for this task, if a `db.begin` is open.
    fn current_txn_id(&self) -> Option<String> {
        self.lock_stacks()
            .get(&task_key())
            .and_then(|stack| stack.last())
            .cloned()
    }

    /// Pushes a guest txn id after a successful `db.begin` (supports nesting).
    fn push_txn(&self, txn_id: String) {
        self.lock_stacks()
            .entry(task_key())
            .or_default()
            .push(txn_id);
    }

    /// Pops the innermost guest txn id and drops an empty per-task stack.
    fn pop_txn(&self) -> Option<String> {
        let mut stacks = self.lock_stacks();
        let key = task_key();
        let id = stacks.get_mut(&key).and_then(Vec::pop);
        if stacks.get(&key).is_some_and(Vec::is_empty) {
            stacks.remove(&key);
        }
        id
    }

    /// Serializes a SeaORM statement and attaches the current guest txn id.
    fn statement_dto(&self, statement: &Statement) -> StatementDto {
        let mut dto = statement_to_dto(statement);
        dto.txn_id = self.current_txn_id();
        dto
    }

    /// Starts a guest transaction, optionally nested under `parent`.
    async fn rpc_begin(&self, parent: Option<String>) -> std::result::Result<String, DbErr> {
        let result: DbBeginResult = self
            .client
            .call(
                methods::DB_BEGIN,
                serde_json::to_value(DbBeginParams {
                    parent_txn_id: parent,
                })
                .map_err(map_json_err)?,
            )
            .await
            .map_err(map_rpc_err)?;
        Ok(result.txn_id)
    }

    /// Commits or rolls back the guest transaction identified by `txn_id`.
    async fn rpc_finish(&self, commit: bool, txn_id: String) -> std::result::Result<(), DbErr> {
        let method = if commit {
            methods::DB_COMMIT
        } else {
            methods::DB_ROLLBACK
        };
        self.client
            .call::<Value>(
                method,
                serde_json::to_value(DbTxnParams { txn_id }).map_err(map_json_err)?,
            )
            .await
            .map_err(map_rpc_err)?;
        Ok(())
    }
}

#[async_trait]
impl ProxyDatabaseTrait for RpcDatabaseProxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let dto = self.statement_dto(&statement);
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
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let dto = self.statement_dto(&statement);
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

    async fn begin(&self) {
        if bookclerk_library::consume_begin_injection() {
            bookclerk_library::note_begin_failed("injected begin failure");
            return;
        }
        let parent = self.current_txn_id();
        match self.rpc_begin(parent).await {
            Ok(txn_id) => self.push_txn(txn_id),
            Err(err) => {
                bookclerk_library::note_begin_failed(&err);
                tracing::error!(error = %err, "database plugin dbBegin failed");
            }
        }
    }

    async fn commit(&self) {
        if bookclerk_library::consume_commit_injection() {
            if let Some(txn_id) = self.pop_txn() {
                if let Err(err) = self.rpc_finish(false, txn_id).await {
                    tracing::error!(
                        error = %err,
                        "database plugin dbRollback after injected commit failure"
                    );
                }
            }
            bookclerk_library::note_commit_failed("injected commit failure");
            return;
        }
        let Some(txn_id) = self.pop_txn() else {
            if bookclerk_library::is_txn_broken() {
                return;
            }
            bookclerk_library::note_commit_failed("no open transaction to commit");
            return;
        };
        if let Err(err) = self.rpc_finish(true, txn_id.clone()).await {
            bookclerk_library::note_commit_failed(&err);
            tracing::error!(error = %err, "database plugin dbCommit failed");
            if let Err(rb) = self.rpc_finish(false, txn_id).await {
                tracing::error!(error = %rb, "database plugin dbRollback after commit failure");
            }
        }
    }

    async fn rollback(&self) {
        let Some(txn_id) = self.pop_txn() else {
            return;
        };
        if let Err(err) = self.rpc_finish(false, txn_id).await {
            tracing::error!(error = %err, "database plugin dbRollback failed");
        }
    }

    fn start_rollback(&self) {
        let Some(txn_id) = self.pop_txn() else {
            return;
        };
        let client = self.client.clone();
        let Ok(params) = serde_json::to_value(DbTxnParams { txn_id }) else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!("database plugin dbRollback skipped: no tokio runtime");
            return;
        };
        if let Err(err) = tokio::task::block_in_place(|| {
            handle.block_on(async move {
                client
                    .call::<Value>(methods::DB_ROLLBACK, params)
                    .await
                    .map(|_| ())
            })
        }) {
            tracing::error!(error = %err, "database plugin dbRollback failed");
        }
    }
}

/// Host [`AtomicTxnBackend`] that runs named security ops as one guest `dbAtomic`.
struct RpcAtomicBackend {
    /// JSON-RPC client used for a single `db.atomic` round-trip per operation.
    client: Arc<PluginClient>,
}

impl RpcAtomicBackend {
    /// Sends one `db.atomic` RPC; ambiguous transport maps to [`LibraryError::Unavailable`].
    async fn call(&self, params: DbAtomicParams) -> bookclerk_library::Result<DbAtomicResult> {
        let operation_id = bookclerk_library::db_atomic_operation_id(&params);
        let request = DbAtomicRequest {
            operation_id,
            operation: params,
        };
        let payload = serde_json::to_value(&request).map_err(|err| {
            bookclerk_library::LibraryError::Other(anyhow::anyhow!(err.to_string()))
        })?;
        // Single host RPC. The D1 guest already retries incomplete 2xx / missing
        // receipts with the same operation id; multiplying attempts here used to
        // submit the batch up to 27 times. Guest `unavailable` (lost reply after
        // those inner retries) maps to [`LibraryError::Unavailable`] so the HTTP
        // client retries the whole redeem with the same derived session token.
        match self.client.call(methods::DB_ATOMIC, payload).await {
            Ok(result) => Ok(result),
            Err(err) => Err(map_plugin_err(err)),
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

/// Builds guest `db.connect` params (SQLite path, D1 token, or Postgres URL) from host config.
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

/// Maps the guest-reported dialect string to a SeaORM [`DbBackend`].
fn dialect_to_backend(dialect: &str) -> Result<DbBackend, DbErr> {
    match dialect.trim().to_ascii_lowercase().as_str() {
        "sqlite" => Ok(DbBackend::Sqlite),
        "postgres" | "postgresql" => Ok(DbBackend::Postgres),
        other => Err(DbErr::Custom(format!(
            "database plugin returned unknown dialect `{other}`"
        ))),
    }
}

/// Wraps a plugin RPC failure as a SeaORM [`DbErr::Custom`].
fn map_rpc_err(err: crate::PluginError) -> DbErr {
    DbErr::Custom(err.to_string())
}

/// Wraps a JSON serialize failure for database RPC params as [`DbErr::Custom`].
fn map_json_err(err: serde_json::Error) -> DbErr {
    DbErr::Custom(format!("serialize database RPC params: {err}"))
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

    #[test]
    fn maps_dialects() {
        assert_eq!(dialect_to_backend("sqlite").unwrap(), DbBackend::Sqlite);
        assert_eq!(dialect_to_backend("postgres").unwrap(), DbBackend::Postgres);
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
        let first = serde_json::to_value(DbAtomicRequest {
            operation_id: bookclerk_library::db_atomic_operation_id(&params),
            operation: params.clone(),
        })
        .unwrap();
        let second = serde_json::to_value(DbAtomicRequest {
            operation_id: bookclerk_library::db_atomic_operation_id(&params),
            operation: params,
        })
        .unwrap();
        assert_eq!(first["operationId"], second["operationId"]);
        assert_eq!(first["operationId"], "takeOidcRpState:abc");
    }
}
