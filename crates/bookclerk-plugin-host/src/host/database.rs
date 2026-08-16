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
use bookclerk_plugin_sdk::v2::PRODUCT_API_VERSION;
use bookclerk_plugin_sdk::{
    atomic_status, exec_result_from_dto, proxy_rows_from_dto, statement_to_dto, DbAtomicParams,
    DbAtomicRequest, DbAtomicResult, DbConnectParams, DbConnectResult, ExecResultDto, ProxyRowDto,
};
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement,
};
use serde_json::Value;
use tokio::task::{try_id, Id as TaskId};

use crate::discover::DiscoveredPlugin;
use crate::jail::plugin_data_dir;
use crate::rpc_v2::{V2PluginSession, OPERATOR_ACCOUNT};
use crate::{PluginError, Result as PluginResult};

/// External database backend spawned for `[database].plugin`.
#[derive(Clone)]
pub struct ExternalDatabase {
    /// Cap'n Proto v2 session (vat holds the database session).
    session: Arc<V2PluginSession>,
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
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(PluginError::message(format!(
                "plugin `{}` api_version {} is not v2",
                plugin.manifest.id, plugin.manifest.api_version
            )));
        }
        let table = crate::settings_table(config, plugin);
        let config_json = toml_to_json(&toml::Value::Table(table));
        let plugin_data_dir = plugin_data_dir(config, &plugin.manifest.id)?;
        let extra_env;
        let extra_refs: &[(&str, std::ffi::OsString)];
        if plugin.manifest.id.eq_ignore_ascii_case("sqlite") {
            let path = config.database.sqlite_path(&config.paths().files_dir);
            extra_env = vec![(
                "BOOKCLERK_SQLITE_PATH",
                std::ffi::OsString::from(path.as_os_str()),
            )];
            extra_refs = &extra_env;
        } else {
            extra_env = Vec::new();
            extra_refs = &extra_env;
        }
        let session = Arc::new(
            V2PluginSession::spawn_for_account_with_env(
                plugin,
                config,
                config_json,
                OPERATOR_ACCOUNT,
                extra_refs,
            )
            .await?,
        );
        Ok(Self {
            session,
            plugin_id: plugin.manifest.id.clone(),
            plugin_data_dir,
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
    ) -> Result<(DatabaseConnection, DbConnectResult), DbErr> {
        let params = connect_params(
            config,
            &self.plugin_id,
            &self.plugin_data_dir,
            &self.session,
        )?;
        let ctx = serde_json::to_string(&params).map_err(|err| DbErr::Custom(err.to_string()))?;
        self.session.db_open(ctx).await.map_err(map_rpc_err)?;

        let _ = self
            .session
            .db_query("SELECT 1", "[]")
            .await
            .map_err(map_rpc_err)?;

        let connect_result = match DatabasePluginKind::parse(&self.plugin_id) {
            Some(DatabasePluginKind::Postgres) => DbConnectResult::postgres(),
            Some(DatabasePluginKind::D1) => DbConnectResult::d1(),
            _ => DbConnectResult::sqlite(),
        };
        let backend = dialect_to_backend(&connect_result.dialect)?;
        let proxy: Arc<Box<dyn ProxyDatabaseTrait>> = Arc::new(Box::new(RpcDatabaseProxy {
            session: self.session.clone(),
            txn_depth: Arc::new(Mutex::new(HashMap::new())),
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
            session: ext.session.clone(),
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
    /// Cap'n Proto v2 session shared with [`ExternalDatabase`].
    session: Arc<V2PluginSession>,
    /// Per-task nested begin depth (vat holds a single transaction).
    txn_depth: Arc<Mutex<HashMap<TaskKey, usize>>>,
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

    /// Serializes a SeaORM statement into SQL + JSON bind values.
    fn statement_parts(statement: &Statement) -> (String, String) {
        let dto = statement_to_dto(statement);
        let values_json = serde_json::to_string(&dto.values).unwrap_or_else(|_| "[]".into());
        (dto.sql, values_json)
    }
}

#[async_trait]
impl ProxyDatabaseTrait for RpcDatabaseProxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let (sql, values_json) = Self::statement_parts(&statement);
        let mut rows = Vec::new();
        let mut cursor = String::new();
        // 4096 pages × MAX_LIST_PAGE (256) is a hard stop against a malicious next_cursor loop.
        for _ in 0..4096 {
            let page = self
                .session
                .db_query_page(sql.clone(), values_json.clone(), cursor.clone(), 0)
                .await
                .map_err(map_rpc_err)?;
            let page_rows: Vec<ProxyRowDto> = if page.rows_json.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&page.rows_json)
                    .map_err(|err| DbErr::Custom(format!("database plugin query rows: {err}")))?
            };
            rows.extend(page_rows);
            match page.next_cursor {
                Some(next) if !next.is_empty() && next != cursor => cursor = next,
                _ => return Ok(proxy_rows_from_dto(rows)),
            }
        }
        Err(DbErr::Custom(
            "database plugin query exceeded 4096 pages".into(),
        ))
    }

    async fn execute(&self, statement: Statement) -> std::result::Result<ProxyExecResult, DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let (sql, values_json) = Self::statement_parts(&statement);
        let result = self
            .session
            .db_execute(sql, values_json)
            .await
            .map_err(map_rpc_err)?;
        Ok(exec_result_from_dto(ExecResultDto {
            last_insert_id: u64::try_from(result.last_insert_id).unwrap_or(0),
            rows_affected: result.rows_affected,
        }))
    }

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        self.session
            .db_query("SELECT 1", "[]")
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
        if let Err(err) = self.session.db_begin().await {
            self.pop_depth();
            bookclerk_library::note_begin_failed(&err);
            tracing::error!(error = %err, "database plugin dbBegin failed");
        }
    }

    async fn commit(&self) {
        if bookclerk_library::consume_commit_injection() {
            if self.pop_depth().is_some() && self.depth() == 0 {
                if let Err(err) = self.session.db_rollback().await {
                    tracing::error!(
                        error = %err,
                        "database plugin dbRollback after injected commit failure"
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
        if let Err(err) = self.session.db_commit().await {
            bookclerk_library::note_commit_failed(&err);
            tracing::error!(error = %err, "database plugin dbCommit failed");
            if let Err(rb) = self.session.db_rollback().await {
                tracing::error!(error = %rb, "database plugin dbRollback after commit failure");
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
        if let Err(err) = self.session.db_rollback().await {
            tracing::error!(error = %err, "database plugin dbRollback failed");
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
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!("database plugin dbRollback skipped: no tokio runtime");
            return;
        };
        if let Err(err) = tokio::task::block_in_place(|| {
            handle.block_on(async move { session.db_rollback().await.map(|_| ()) })
        }) {
            tracing::error!(error = %err, "database plugin dbRollback failed");
        }
    }
}

/// Host [`AtomicTxnBackend`] that runs named security ops as one guest `dbAtomic`.
struct RpcAtomicBackend {
    /// Cap'n Proto v2 session used for a single `bookclerk.atomic` query per operation.
    session: Arc<V2PluginSession>,
}

impl RpcAtomicBackend {
    /// Sends one `bookclerk.atomic` query; ambiguous transport maps to [`LibraryError::Unavailable`].
    async fn call(&self, params: DbAtomicParams) -> bookclerk_library::Result<DbAtomicResult> {
        let operation_id = bookclerk_library::db_atomic_operation_id(&params);
        self.call_with_id(operation_id, params).await
    }

    /// Sends `bookclerk.atomic` with a caller-chosen idempotency key (replay-safe claims).
    async fn call_with_id(
        &self,
        operation_id: String,
        params: DbAtomicParams,
    ) -> bookclerk_library::Result<DbAtomicResult> {
        let request = DbAtomicRequest {
            operation_id,
            operation: params,
        };
        let payload = serde_json::to_string(&request).map_err(|err| {
            bookclerk_library::LibraryError::Other(anyhow::anyhow!(err.to_string()))
        })?;
        match self.session.db_query("bookclerk.atomic", payload).await {
            Ok(page) => serde_json::from_str(&page.rows_json).map_err(|err| {
                bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                    "database atomic result: {err}"
                ))
            }),
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
    session: &V2PluginSession,
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
            session
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
            session
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
