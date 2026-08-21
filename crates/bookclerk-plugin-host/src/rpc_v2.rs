//! Host session for ABI v2 Cap'n Proto guests (object-capability + streams).
//!
//! Cap'n Proto clients are `!Send`, so the vat runs on a dedicated current-thread
//! runtime. Host [`StorageBackend`] methods send work onto that thread.

#![allow(clippy::missing_docs_in_private_items)]
#![allow(clippy::arc_with_non_send_sync)]

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_plugin_sdk::v2::{
    connect_plugin, negotiate_rpc_features, ByteRange as AbiByteRange, Cancellation, CopyResult,
    Database, Destination, DestinationContext, JobInvocation, JobInvocationLease, ListOptions,
    ObjectMetadata, OidcClientTemplate, PluginClient, PluginDescribe, PutResult, ReadResult,
    ScalarLimits, Source, StreamCopySpec, WorkerContext, WriteOptions, FEATURE_SCALAR_LIMITS,
    FEATURE_STORAGE_COPY, FEATURE_STREAMS, MAX_SCALAR_BYTES, MAX_STREAM_WINDOW_BYTES,
    PRODUCT_API_VERSION,
};
use bookclerk_storage::{
    ByteRange, ListPage, ObjectInfo, ObjectMeta, ObjectProbe, PutStreamResult, StorageBackend,
    StorageError,
};
use bytes::Bytes;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::discover::DiscoveredPlugin;
use crate::{PluginError, Result};

/// Work item executed on the v2 vat thread.
enum Work {
    /// `BookclerkPlugin.describe`.
    Describe {
        /// Reply channel.
        reply: oneshot::Sender<Result<PluginDescribe>>,
    },
    /// Ensure a destination stub exists for `ctx`.
    EnsureDest {
        /// Destination factory context.
        ctx: DestinationContext,
        /// Reply channel.
        reply: oneshot::Sender<Result<()>>,
    },
    /// `Destination.head`.
    Head {
        /// Object key.
        key: String,
        /// Reply channel.
        reply: oneshot::Sender<Result<Option<ObjectMetadata>>>,
    },
    /// `Destination.list`.
    List {
        /// List options.
        options: ListOptions,
        /// Reply channel.
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::v2::ListPage>>,
    },
    /// Streamed get.
    GetStream {
        /// Object key.
        key: String,
        /// Optional range.
        range: Option<AbiByteRange>,
        /// Reply channel.
        reply: oneshot::Sender<Result<ReadResult>>,
    },
    /// Streamed put.
    PutStream {
        /// Object key.
        key: String,
        /// Body stream.
        body: Pin<Box<dyn AsyncRead + Send>>,
        /// Write options.
        options: WriteOptions,
        /// Reply channel.
        reply: oneshot::Sender<Result<PutResult>>,
    },
    /// Server-side copy.
    Copy {
        /// Source key.
        from: String,
        /// Destination key.
        to: String,
        /// Reply channel.
        reply: oneshot::Sender<Result<u64>>,
    },
    /// Delete key.
    Delete {
        /// Object key.
        key: String,
        /// Reply channel.
        reply: oneshot::Sender<Result<()>>,
    },
    /// JobHandler stream-copy vertical slice.
    StreamCopy {
        /// Claimed-lease invocation envelope.
        lease: bookclerk_plugin_sdk::v2::JobInvocationLease,
        /// Copy spec.
        spec: StreamCopySpec,
        /// Host fence / cancel flag.
        cancel: Arc<AtomicBool>,
        /// Durable fenced progress (library row + lease identity).
        progress: Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
        /// Reply channel.
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::v2::JobOutcome>>,
    },
    ContentSource {
        ctx_json: String,
        op: String,
        params: String,
        reply: oneshot::Sender<Result<String>>,
    },
    Integration {
        ctx_json: String,
        op: String,
        params: String,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<String>>,
    },
    CliDescribe {
        reply: oneshot::Sender<Result<String>>,
    },
    CliInvoke {
        params: String,
        reply: oneshot::Sender<Result<String>>,
    },
    OidcClients {
        reply: oneshot::Sender<Result<Vec<OidcClientTemplate>>>,
    },
    DbOpen {
        ctx_json: String,
        reply: oneshot::Sender<Result<()>>,
    },
    DbExecute {
        sql: String,
        values_json: String,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::v2::ExecResult>>,
    },
    DbQuery {
        sql: String,
        values_json: String,
        cursor: String,
        limit: u32,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::v2::QueryPage>>,
    },
    DbBegin {
        reply: oneshot::Sender<Result<()>>,
    },
    DbCommit {
        reply: oneshot::Sender<Result<()>>,
    },
    DbRollback {
        reply: oneshot::Sender<Result<()>>,
    },
    /// Drop the vat.
    Shutdown,
}

/// Isolation key: different accounts never share a plugin isolate.
pub const OPERATOR_ACCOUNT: &str = "operator";

/// Registry-loaded source/integration guests (not a user account).
pub const HOST_SHARED_ACCOUNT: &str = "host";

/// Isolation key: different accounts never share a plugin isolate.
#[must_use]
pub fn plugin_instance_key(plugin_id: &str, account_id: &str) -> String {
    format!("{plugin_id}:{account_id}")
}

/// Expanded executor identity. Pooling is an optimization; correctness must not
/// depend on a PID surviving.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecutorIdentity {
    /// Manifest plugin id.
    pub plugin_id: String,
    /// Artifact digest or install path stand-in.
    pub artifact_digest: String,
    /// Manifest version.
    pub version: String,
    /// Role (`destination`, `source`, `database`, `integration`).
    pub role: String,
    /// Account / principal.
    pub account_id: String,
    /// Configuration revision.
    pub configuration_revision: String,
    /// Grant revision (revocation changes this).
    pub grant_revision: String,
    /// `workerd` / `native` / `native-behind-workerd`.
    pub runtime_backend: String,
    /// Workerd compatibility date when applicable.
    pub compatibility_date: String,
}

impl ExecutorIdentity {
    /// Builds an identity from a discovered plugin and account.
    #[must_use]
    pub fn from_plugin(plugin: &DiscoveredPlugin, account_id: &str) -> Self {
        Self {
            plugin_id: plugin.manifest.id.clone(),
            artifact_digest: plugin.command.to_string_lossy().into_owned(),
            version: plugin.manifest.version.clone().unwrap_or_default(),
            role: plugin.manifest.kind.as_str().to_string(),
            account_id: account_id.to_string(),
            configuration_revision: String::new(),
            grant_revision: String::new(),
            runtime_backend: format!("{:?}", plugin.manifest.runtime),
            compatibility_date: plugin
                .manifest
                .workerd
                .as_ref()
                .map(|w| w.compatibility_date.clone())
                .unwrap_or_default(),
        }
    }

    /// Stable session key. Distinct PIDs with the same key are the same logical
    /// instance; pooling must use this key, not a PID.
    #[must_use]
    pub fn session_key(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.plugin_id,
            self.artifact_digest,
            self.version,
            self.role,
            self.account_id,
            self.configuration_revision,
            self.grant_revision,
            self.runtime_backend,
            self.compatibility_date
        )
    }
}

/// Sources and integrations must not share the operator isolate.
#[must_use]
fn account_bearing_requires_non_operator(kind: crate::PluginKind, account_id: &str) -> bool {
    matches!(
        kind,
        crate::PluginKind::Source | crate::PluginKind::Integration
    ) && (account_id.is_empty() || account_id == OPERATOR_ACCOUNT)
}

/// Host-side v2 plugin session (one jailed child + one vat thread).
pub struct V2PluginSession {
    /// Work queue into the vat thread.
    tx: mpsc::UnboundedSender<Work>,
    /// Plugin id.
    id: String,
    /// Guest data directory.
    data: std::path::PathBuf,
    /// Instance key `(plugin_id, account_id)`.
    instance_key: String,
    /// Expanded executor identity (not a PID).
    session_key: String,
    /// Guest child PID, when the OS still reports one after spawn.
    guest_pid: Option<u32>,
    /// Negotiated scalar limits.
    limits: ScalarLimits,
    /// Intersected RPC features.
    features: Vec<String>,
    /// Last `describe()` snapshot (identity + metadata JSON).
    describe: PluginDescribe,
    /// Covering operator grant.
    grant: crate::PluginGrant,
    /// Guest TMPDIR.
    scratch: std::path::PathBuf,
    /// Handshake/config JSON captured at spawn.
    handshake_config: Value,
    /// AppContainer package SID.
    #[cfg(windows)]
    package_sid: Option<String>,
}

impl V2PluginSession {
    /// Spawns a v2 guest and connects Cap'n Proto on stdio.
    ///
    /// # Errors
    ///
    /// Fails when the child cannot start, describe fails, or `apiVersion` is not 2.
    pub async fn spawn(
        plugin: &DiscoveredPlugin,
        config: &Config,
        config_table: Value,
    ) -> Result<Self> {
        Self::spawn_for_account(plugin, config, config_table, OPERATOR_ACCOUNT).await
    }

    /// [`Self::spawn`] keyed by `(plugin_id, account_id)` so different accounts
    /// never share a plugin isolate.
    ///
    /// # Errors
    ///
    /// Fails when the child cannot start, describe fails, or negotiation fails.
    pub async fn spawn_for_account(
        plugin: &DiscoveredPlugin,
        config: &Config,
        config_table: Value,
        account_id: &str,
    ) -> Result<Self> {
        Self::spawn_for_account_with_env(plugin, config, config_table, account_id, &[]).await
    }

    /// [`Self::spawn_for_account`] with transport-private extra environment.
    ///
    /// # Errors
    ///
    /// Fails when the child cannot start, describe fails, or negotiation fails.
    pub async fn spawn_for_account_with_env(
        plugin: &DiscoveredPlugin,
        config: &Config,
        config_table: Value,
        account_id: &str,
        extra_env: &[(&str, std::ffi::OsString)],
    ) -> Result<Self> {
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(PluginError::message(format!(
                "plugin `{}` api_version {} is not v2",
                plugin.manifest.id, plugin.manifest.api_version
            )));
        }
        if account_bearing_requires_non_operator(plugin.manifest.kind, account_id) {
            return Err(PluginError::message(format!(
                "plugin `{}` is account-bearing and requires a non-operator account_id",
                plugin.manifest.id
            )));
        }
        let spawned =
            crate::spawn_stdio::spawn_stdio_guest(plugin, config, config_table, extra_env).await?;
        Self::connect_spawned(spawned, plugin, account_id).await
    }

    async fn connect_spawned(
        spawned: crate::spawn_stdio::SpawnedStdio,
        plugin: &DiscoveredPlugin,
        account_id: &str,
    ) -> Result<Self> {
        let expected_id = plugin.manifest.id.clone();
        let expected_kind = plugin.manifest.kind.as_str().to_string();
        let id = spawned.id.clone();
        let data = spawned.data.clone();
        let scratch = spawned.scratch.clone();
        let grant = spawned.grant.clone();
        let handshake_config = spawned.handshake_config.clone();
        #[cfg(windows)]
        let package_sid = spawned.package_sid.clone();
        let guest_pid = spawned.child.id();
        let instance_key = plugin_instance_key(&id, account_id);
        let session_key = ExecutorIdentity::from_plugin(plugin, account_id).session_key();
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) =
            oneshot::channel::<Result<(PluginDescribe, ScalarLimits, Vec<String>)>>();
        thread::Builder::new()
            .name(format!("plugin-v2-{}", id))
            .spawn(move || vat_thread(spawned, expected_id, expected_kind, rx, ready_tx))
            .map_err(|err| PluginError::message(format!("v2 vat thread: {err}")))?;
        let (desc, limits, features) = ready_rx
            .await
            .map_err(|err| PluginError::message(format!("v2 vat dropped: {err}")))??;
        if desc.api_version != PRODUCT_API_VERSION {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe apiVersion {} is not {PRODUCT_API_VERSION}",
                desc.api_version
            )));
        }
        Ok(Self {
            tx,
            id,
            data,
            instance_key,
            session_key,
            guest_pid,
            limits,
            features,
            describe: desc,
            grant,
            scratch,
            handshake_config,
            #[cfg(windows)]
            package_sid,
        })
    }

    /// Isolation instance key (`plugin_id:account_id`).
    #[must_use]
    pub fn instance_key(&self) -> &str {
        &self.instance_key
    }

    /// Expanded executor session key (artifact, role, grant revision, …).
    #[must_use]
    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    /// Guest child PID for this isolate, when known.
    #[must_use]
    pub fn guest_pid(&self) -> Option<u32> {
        self.guest_pid
    }

    /// Negotiated scalar limits.
    #[must_use]
    pub fn limits(&self) -> ScalarLimits {
        self.limits
    }

    /// True when the guest accepted `storage.copy`.
    #[must_use]
    pub fn supports_server_copy(&self) -> bool {
        self.features.iter().any(|f| f == FEATURE_STORAGE_COPY)
    }

    /// Plugin id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Guest data directory.
    #[must_use]
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data
    }

    /// Sends work to the vat thread.
    async fn call<T>(&self, build: impl FnOnce(oneshot::Sender<Result<T>>) -> Work) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(build(reply))
            .map_err(|_| PluginError::unavailable("v2 vat thread closed"))?;
        rx.await
            .map_err(|_| PluginError::unavailable("v2 vat thread dropped reply"))?
    }

    /// Instantiates the destination stub with `ctx`.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory call fails.
    pub async fn ensure_destination(&self, ctx: DestinationContext) -> Result<()> {
        self.call(|reply| Work::EnsureDest { ctx, reply }).await
    }

    /// Calls `BookclerkPlugin.describe`.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn describe(&self) -> Result<PluginDescribe> {
        self.call(|reply| Work::Describe { reply }).await
    }

    /// Runs the stream-copy job handler on the guest.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the handler fails.
    pub async fn stream_copy(
        &self,
        job_id: &str,
        from: &str,
        to: &str,
    ) -> Result<bookclerk_plugin_sdk::v2::JobOutcome> {
        let job_id = job_id.to_string();
        self.stream_copy_with_cancel(
            JobInvocationLease {
                job_id: job_id.clone(),
                attempt: 1,
                generation: 1,
                dedup_key: job_id,
                deadline_unix_ms: u64::MAX / 2,
                checkpoint: None,
                invocation_sequence: 1,
            },
            from,
            to,
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await
    }

    /// [`Self::stream_copy`] raced against a host cancel/fence flag.
    ///
    /// When `progress` is set, reports are persisted with
    /// `LibraryStore::set_job_progress`; a lost fence surfaces as cancellation.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the handler fails or the fence is lost.
    pub async fn stream_copy_with_cancel(
        &self,
        lease: JobInvocationLease,
        from: &str,
        to: &str,
        cancel: Arc<AtomicBool>,
        progress: Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
    ) -> Result<bookclerk_plugin_sdk::v2::JobOutcome> {
        self.call(|reply| Work::StreamCopy {
            lease,
            spec: StreamCopySpec {
                from: from.into(),
                to: to.into(),
            },
            cancel,
            progress,
            reply,
        })
        .await
    }

    /// Snapshot from the guest `describe()` call at spawn.
    #[must_use]
    pub fn describe_snapshot(&self) -> &PluginDescribe {
        &self.describe
    }

    /// Covering consent grant from spawn.
    #[must_use]
    pub fn grant(&self) -> &crate::PluginGrant {
        &self.grant
    }

    /// Fail closed when a delivery site needs an ungranted binding.
    ///
    /// # Errors
    ///
    /// Returns an error when the binding is missing from the grant.
    pub fn require_binding(&self, name: &str) -> Result<()> {
        crate::require_binding(&self.grant, name)
    }

    /// Guest TMPDIR / scratch directory.
    #[must_use]
    pub fn scratch_dir(&self) -> &std::path::Path {
        &self.scratch
    }

    /// AppContainer package SID when the guest is jailed on Windows.
    #[must_use]
    #[cfg(windows)]
    pub fn package_sid(&self) -> Option<&str> {
        self.package_sid.as_deref()
    }

    /// AppContainer package SID when the guest is jailed on Windows.
    #[must_use]
    #[cfg(not(windows))]
    pub fn package_sid(&self) -> Option<&str> {
        None
    }

    /// Handshake/config JSON captured at spawn.
    #[must_use]
    pub fn handshake_config(&self) -> &Value {
        &self.handshake_config
    }

    /// Handshake-era extras parsed from `describe.metadataJson`.
    #[must_use]
    pub fn handshake_metadata(&self) -> crate::HandshakeResult {
        if self.describe.metadata_json.trim().is_empty() {
            return crate::HandshakeResult {
                api_version: PRODUCT_API_VERSION,
                id: self.describe.id.clone(),
                kind: self.describe.kind.clone(),
                display_name: self.describe.display_name.clone(),
                capabilities: self.describe.supported_roles.clone(),
                ..crate::HandshakeResult::default()
            };
        }
        serde_json::from_str(&self.describe.metadata_json).unwrap_or_else(|_| {
            crate::HandshakeResult {
                api_version: PRODUCT_API_VERSION,
                id: self.describe.id.clone(),
                kind: self.describe.kind.clone(),
                display_name: self.describe.display_name.clone(),
                ..crate::HandshakeResult::default()
            }
        })
    }

    /// True when `describe.metadataJson` lists a v1-style capability name.
    #[must_use]
    pub fn has_capability(&self, cap: &str) -> bool {
        let hs = self.handshake_metadata();
        hs.capabilities.iter().any(|c| c == cap)
            || self.describe.supported_roles.iter().any(|c| c == cap)
    }

    /// One content-source method (create → invoke → dispose).
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory or method fails.
    pub async fn content_source_json(
        &self,
        ctx_json: impl Into<String>,
        op: impl Into<String>,
        params: impl Into<String>,
    ) -> Result<String> {
        self.call(|reply| Work::ContentSource {
            ctx_json: ctx_json.into(),
            op: op.into(),
            params: params.into(),
            reply,
        })
        .await
    }

    /// One integration method (create → invoke → dispose).
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory or method fails.
    pub async fn integration_json(
        &self,
        ctx_json: impl Into<String>,
        op: impl Into<String>,
        params: impl Into<String>,
    ) -> Result<String> {
        self.integration_json_cancelable(ctx_json, op, params, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// Integration JSON-RPC, aborted when `cancel` is set (delivery fence loss).
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails or is cancelled.
    pub async fn integration_json_cancelable(
        &self,
        ctx_json: impl Into<String>,
        op: impl Into<String>,
        params: impl Into<String>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        self.call(|reply| Work::Integration {
            ctx_json: ctx_json.into(),
            op: op.into(),
            params: params.into(),
            cancel,
            reply,
        })
        .await
    }

    /// Guest CLI schema JSON.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn cli_describe(&self) -> Result<String> {
        self.call(|reply| Work::CliDescribe { reply }).await
    }

    /// Guest CLI invoke (`CliInvokeParams` JSON).
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn cli_invoke_json(&self, params: impl Into<String>) -> Result<String> {
        self.call(|reply| Work::CliInvoke {
            params: params.into(),
            reply,
        })
        .await
    }

    /// Plugin-provided OIDC authorization-server client templates.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails. Older guests that lack
    /// `oidcClients` returns templates, or an empty list when unused.
    pub async fn oidc_clients(&self) -> Result<Vec<OidcClientTemplate>> {
        self.call(|reply| Work::OidcClients { reply }).await
    }

    /// Opens a database session (held on the vat until drop).
    ///
    /// # Errors
    ///
    /// Returns a plugin error when `database` / `openSession` fails.
    pub async fn db_open(&self, ctx_json: impl Into<String>) -> Result<()> {
        self.call(|reply| Work::DbOpen {
            ctx_json: ctx_json.into(),
            reply,
        })
        .await
    }

    /// Session execute.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when execute fails.
    pub async fn db_execute(
        &self,
        sql: impl Into<String>,
        values_json: impl Into<String>,
    ) -> Result<bookclerk_plugin_sdk::v2::ExecResult> {
        self.call(|reply| Work::DbExecute {
            sql: sql.into(),
            values_json: values_json.into(),
            reply,
        })
        .await
    }

    /// Session query.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when query fails.
    pub async fn db_query(
        &self,
        sql: impl Into<String>,
        values_json: impl Into<String>,
    ) -> Result<bookclerk_plugin_sdk::v2::QueryPage> {
        self.db_query_page(sql, values_json, "", 0).await
    }

    /// Session query for one bounded page.
    ///
    /// `limit == 0` means the guest's `MAX_LIST_PAGE`.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when query fails.
    pub async fn db_query_page(
        &self,
        sql: impl Into<String>,
        values_json: impl Into<String>,
        cursor: impl Into<String>,
        limit: u32,
    ) -> Result<bookclerk_plugin_sdk::v2::QueryPage> {
        self.call(|reply| Work::DbQuery {
            sql: sql.into(),
            values_json: values_json.into(),
            cursor: cursor.into(),
            limit,
            reply,
        })
        .await
    }

    /// Begin a vat-held transaction.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when begin fails.
    pub async fn db_begin(&self) -> Result<()> {
        self.call(|reply| Work::DbBegin { reply }).await
    }

    /// Commit the vat-held transaction.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when commit fails.
    pub async fn db_commit(&self) -> Result<()> {
        self.call(|reply| Work::DbCommit { reply }).await
    }

    /// Roll back the vat-held transaction.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when rollback fails.
    pub async fn db_rollback(&self) -> Result<()> {
        self.call(|reply| Work::DbRollback { reply }).await
    }
}

impl Drop for V2PluginSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Work::Shutdown);
    }
}

/// Maps ABI errors onto host [`PluginError`].
fn map_abi(err: bookclerk_plugin_sdk::PluginError) -> PluginError {
    let code = err.wire_str().to_string();
    PluginError::from_abi(Some(&code), err.message)
}

async fn dispatch_content_source(
    client: &PluginClient,
    ctx_json: String,
    op: &str,
    params: &str,
) -> Result<String> {
    use bookclerk_plugin_sdk::v2::ContentSource;
    let src = client
        .content_source(bookclerk_plugin_sdk::v2::ContentSourceContext { json: ctx_json })
        .await
        .map_err(map_abi)?;
    let out = match op {
        "login" => src.login(params).await,
        "scan" => src.scan(params).await,
        "fetchTitle" => src.fetch_title(params).await,
        "listAccounts" => src.list_accounts().await,
        "loginStart" => src.login_start(params).await,
        "loginComplete" => src.login_complete(params).await,
        "searchCatalog" => src.search_catalog(params).await,
        "expandCandidates" => src.expand_candidates(params).await,
        "purchaseHint" => src.purchase_hint(params).await,
        "listDeals" => src.list_deals(params).await,
        "catalogDetail" => src.catalog_detail(params).await,
        "health" => src.health().await.and_then(|h| {
            serde_json::to_string(&h)
                .map_err(|e| bookclerk_plugin_sdk::PluginError::internal(e.to_string()))
        }),
        "diagnose" => src.diagnose().await,
        other => Err(bookclerk_plugin_sdk::PluginError::unsupported(other)),
    };
    out.map_err(map_abi)
}

async fn dispatch_integration(
    client: &PluginClient,
    ctx_json: String,
    op: &str,
    params: &str,
) -> Result<String> {
    use bookclerk_plugin_sdk::v2::{DomainEvent, EventResult, Integration};
    let role = client
        .integration(bookclerk_plugin_sdk::v2::IntegrationContext { json: ctx_json })
        .await
        .map_err(map_abi)?;
    let out = match op {
        "health" => role.health().await.and_then(|h| {
            serde_json::to_string(&h)
                .map_err(|e| bookclerk_plugin_sdk::PluginError::internal(e.to_string()))
        }),
        "onEvent" => {
            let event: DomainEvent = serde_json::from_str(params).unwrap_or(DomainEvent {
                delivery_attempt: 1,
                payload: params.as_bytes().to_vec(),
                ..DomainEvent::default()
            });
            role.on_event(event).await.map(|r| match r {
                EventResult::Ack => "{\"kind\":\"ack\"}".into(),
                EventResult::Retry {
                    retry_at_unix_ms,
                    reason,
                } => format!(
                    "{{\"kind\":\"retry\",\"retryAtUnixMs\":{retry_at_unix_ms},\"reason\":{}}}",
                    serde_json::to_string(&reason).unwrap_or_else(|_| "\"\"".into())
                ),
                EventResult::Reject { reason } => format!(
                    "{{\"kind\":\"reject\",\"reason\":{}}}",
                    serde_json::to_string(&reason).unwrap_or_else(|_| "\"\"".into())
                ),
                EventResult::DeadLetter { reason } => format!(
                    "{{\"kind\":\"deadLetter\",\"reason\":{}}}",
                    serde_json::to_string(&reason).unwrap_or_else(|_| "\"\"".into())
                ),
                EventResult::Suspended {
                    checkpoint_json,
                    checkpoint_schema_version,
                    wake_at_unix_ms,
                    wake_on_event_type,
                    wake_on_filter_json,
                } => format!(
                    "{{\"kind\":\"suspended\",\"checkpointJson\":{},\"checkpointSchemaVersion\":{checkpoint_schema_version},\"wakeAtUnixMs\":{wake_at_unix_ms},\"wakeOnEventType\":{},\"wakeOnFilterJson\":{}}}",
                    serde_json::to_string(&checkpoint_json).unwrap_or_else(|_| "\"\"".into()),
                    serde_json::to_string(&wake_on_event_type).unwrap_or_else(|_| "\"\"".into()),
                    serde_json::to_string(&wake_on_filter_json).unwrap_or_else(|_| "\"\"".into())
                ),
            })
        }
        "start" => role.start().await.map(|()| "{}".into()),
        "stop" => role.stop().await.map(|()| "{}".into()),
        "diagnose" => role.diagnose().await,
        "scanLibrary" => role.scan_library(params).await.map(|()| "{}".into()),
        "syncListening" => role.sync_listening().await,
        "authenticateUser" => role.authenticate_user(params).await,
        "pollEvents" => role.poll_events().await,
        other => Err(bookclerk_plugin_sdk::PluginError::unsupported(other)),
    };
    out.map_err(map_abi)
}

async fn wait_flag(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn negotiate_describe(
    desc: &PluginDescribe,
    expected_id: &str,
    expected_kind: &str,
) -> Result<(ScalarLimits, Vec<String>)> {
    if desc.api_version != PRODUCT_API_VERSION {
        return Err(PluginError::message(format!(
            "plugin `{}` describe apiVersion {} is not {PRODUCT_API_VERSION}",
            expected_id, desc.api_version
        )));
    }
    if desc.id != expected_id {
        return Err(PluginError::message(format!(
            "plugin id mismatch: described `{}`, expected `{expected_id}`",
            desc.id
        )));
    }
    if desc.kind != expected_kind {
        return Err(PluginError::message(format!(
            "plugin kind mismatch: described `{}`, expected `{expected_kind}`",
            desc.kind
        )));
    }
    let features = negotiate_rpc_features(
        &[FEATURE_SCALAR_LIMITS, FEATURE_STREAMS, FEATURE_STORAGE_COPY],
        &desc.rpc_features,
    )
    .map_err(map_abi)?;
    if matches!(expected_kind, "output") && !features.iter().any(|f| f == FEATURE_STREAMS) {
        return Err(PluginError::message(format!(
            "plugin `{expected_id}` kind `{expected_kind}` requires `{FEATURE_STREAMS}`"
        )));
    }
    let guest_limits = ScalarLimits::from(desc.scalar_limits)
        .validate()
        .map_err(map_abi)?;
    let limits = ScalarLimits::default()
        .intersect(guest_limits)
        .validate()
        .map_err(map_abi)?;
    Ok((limits, features))
}

fn vat_thread(
    spawned: crate::spawn_stdio::SpawnedStdio,
    expected_id: String,
    expected_kind: String,
    mut rx: mpsc::UnboundedReceiver<Work>,
    ready: oneshot::Sender<Result<(PluginDescribe, ScalarLimits, Vec<String>)>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            let _ = ready.send(Err(PluginError::message(format!("v2 runtime: {err}"))));
            return;
        }
    };
    rt.block_on(async move {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let (client, rpc) =
                    connect_plugin(spawned.stdout, spawned.stdin, MAX_STREAM_WINDOW_BYTES);
                tokio::task::spawn_local(rpc);
                let client = match client.describe().await {
                    Ok(desc) => match negotiate_describe(&desc, &expected_id, &expected_kind) {
                        Ok((limits, features)) => {
                            let client = client.with_limits(limits);
                            let _ = ready.send(Ok((desc, limits, features)));
                            client
                        }
                        Err(err) => {
                            let _ = ready.send(Err(err));
                            return;
                        }
                    },
                    Err(err) => {
                        let _ = ready.send(Err(map_abi(err)));
                        return;
                    }
                };
                let mut dest: Option<bookclerk_plugin_sdk::v2::DestinationClient> = None;
                let mut db_session: Option<Box<dyn bookclerk_plugin_sdk::v2::DatabaseSession>> =
                    None;
                let mut db_txn: Option<Box<dyn bookclerk_plugin_sdk::v2::Transaction>> = None;
                while let Some(work) = rx.recv().await {
                    match work {
                        Work::Shutdown => break,
                        Work::Describe { reply } => {
                            let _ = reply.send(client.describe().await.map_err(map_abi));
                        }
                        Work::EnsureDest { ctx, reply } => {
                            let out = client.destination(ctx).await.map_err(map_abi);
                            match out {
                                Ok(d) => {
                                    dest = Some(d);
                                    let _ = reply.send(Ok(()));
                                }
                                Err(err) => {
                                    let _ = reply.send(Err(err));
                                }
                            }
                        }
                        Work::Head { key, reply } => {
                            let out = match dest.as_ref() {
                                Some(d) => d.head(&key).await.map_err(map_abi),
                                None => Err(PluginError::message("v2 destination not created")),
                            };
                            let _ = reply.send(out);
                        }
                        Work::List { options, reply } => {
                            let out = match dest.as_ref() {
                                Some(d) => d.list(options).await.map_err(map_abi),
                                None => Err(PluginError::message("v2 destination not created")),
                            };
                            let _ = reply.send(out);
                        }
                        Work::GetStream { key, range, reply } => {
                            let out = match dest.as_ref() {
                                Some(d) => d.get(&key, range).await.map_err(map_abi),
                                None => Err(PluginError::message("v2 destination not created")),
                            };
                            let _ = reply.send(out);
                        }
                        Work::PutStream {
                            key,
                            body,
                            options,
                            reply,
                        } => {
                            let out = match dest.as_ref() {
                                Some(d) => d.put(&key, body, options).await.map_err(map_abi),
                                None => Err(PluginError::message("v2 destination not created")),
                            };
                            let _ = reply.send(out);
                        }
                        Work::Copy { from, to, reply } => {
                            let out = match dest.as_ref() {
                                Some(d) => d
                                    .copy(&from, &to)
                                    .await
                                    .map(|r| r.bytes_copied)
                                    .map_err(map_abi),
                                None => Err(PluginError::message("v2 destination not created")),
                            };
                            let _ = reply.send(out);
                        }
                        Work::Delete { key, reply } => {
                            let out = match dest.as_ref() {
                                Some(d) => d.delete(&key).await.map_err(map_abi),
                                None => Err(PluginError::message("v2 destination not created")),
                            };
                            let _ = reply.send(out);
                        }
                        Work::StreamCopy {
                            lease,
                            spec,
                            cancel,
                            progress,
                            reply,
                        } => {
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "fence lost"))
                                }
                                out = run_stream_copy(
                                    &client,
                                    dest.as_ref(),
                                    lease,
                                    spec,
                                    cancel,
                                    progress,
                                ) => out,
                            };
                            let _ = reply.send(out);
                        }
                        Work::ContentSource {
                            ctx_json,
                            op,
                            params,
                            reply,
                        } => {
                            let out =
                                dispatch_content_source(&client, ctx_json, &op, &params).await;
                            let _ = reply.send(out);
                        }
                        Work::Integration {
                            ctx_json,
                            op,
                            params,
                            cancel,
                            reply,
                        } => {
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "fence lost"))
                                }
                                out = dispatch_integration(&client, ctx_json, &op, &params) => out,
                            };
                            let _ = reply.send(out);
                        }
                        Work::CliDescribe { reply } => {
                            let _ = reply.send(client.cli_describe().await.map_err(map_abi));
                        }
                        Work::CliInvoke { params, reply } => {
                            let _ = reply.send(client.cli_invoke(&params).await.map_err(map_abi));
                        }
                        Work::OidcClients { reply } => {
                            let _ = reply.send(client.oidc_clients().await.map_err(map_abi));
                        }
                        Work::DbOpen { ctx_json, reply } => {
                            let out = async {
                                let db = client
                                    .database(bookclerk_plugin_sdk::v2::DatabaseContext {
                                        json: ctx_json,
                                    })
                                    .await
                                    .map_err(map_abi)?;
                                let sess = db.open_session().await.map_err(map_abi)?;
                                db_session = Some(sess);
                                db_txn = None;
                                Ok(())
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbExecute {
                            sql,
                            values_json,
                            reply,
                        } => {
                            let stmt = bookclerk_plugin_sdk::v2::Statement { sql, values_json };
                            let out = async {
                                if let Some(txn) = db_txn.as_mut() {
                                    txn.execute(stmt).await.map_err(map_abi)
                                } else {
                                    match db_session.as_mut() {
                                        Some(s) => s.execute(stmt).await.map_err(map_abi),
                                        None => Err(PluginError::message(
                                            "v2 database session not open",
                                        )),
                                    }
                                }
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbQuery {
                            sql,
                            values_json,
                            cursor,
                            limit,
                            reply,
                        } => {
                            let stmt = bookclerk_plugin_sdk::v2::Statement { sql, values_json };
                            let out = async {
                                if let Some(txn) = db_txn.as_mut() {
                                    txn.query(stmt, &cursor, limit).await.map_err(map_abi)
                                } else {
                                    match db_session.as_mut() {
                                        Some(s) => {
                                            s.query(stmt, &cursor, limit).await.map_err(map_abi)
                                        }
                                        None => Err(PluginError::message(
                                            "v2 database session not open",
                                        )),
                                    }
                                }
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbBegin { reply } => {
                            let out = async {
                                let sess = db_session.as_mut().ok_or_else(|| {
                                    PluginError::message("v2 database session not open")
                                })?;
                                db_txn = Some(sess.begin().await.map_err(map_abi)?);
                                Ok(())
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbCommit { reply } => {
                            let out = async {
                                let txn = db_txn.take().ok_or_else(|| {
                                    PluginError::message("v2 database transaction not open")
                                })?;
                                txn.commit().await.map_err(map_abi)
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbRollback { reply } => {
                            let out = async {
                                let txn = db_txn.take().ok_or_else(|| {
                                    PluginError::message("v2 database transaction not open")
                                })?;
                                txn.rollback().await.map_err(map_abi)
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                    }
                }
                drop(spawned.child);
            })
            .await;
    });
}

async fn run_stream_copy(
    client: &PluginClient,
    dest: Option<&bookclerk_plugin_sdk::v2::DestinationClient>,
    lease: JobInvocationLease,
    spec: StreamCopySpec,
    cancel: Arc<AtomicBool>,
    progress: Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
) -> Result<bookclerk_plugin_sdk::v2::JobOutcome> {
    let Some(dest) = dest else {
        return Err(PluginError::message("v2 destination not created"));
    };
    let handler = client
        .worker(WorkerContext {
            job_id: lease.job_id.clone(),
            json: String::new(),
        })
        .await
        .map_err(map_abi)?;
    let payload =
        serde_json::to_string(&spec).map_err(|err| PluginError::message(err.to_string()))?;
    let invocation = JobInvocation::stream_copy_from_lease(lease, payload);
    let input: Arc<dyn Source> = Arc::new(DestAsSource { dest: dest.clone() });
    let output: Arc<dyn Destination> = Arc::new(FencedDestination {
        inner: Arc::new(dest.clone()),
        cancel: Arc::clone(&cancel),
        library: progress.clone(),
        commit_hold: None,
    });
    let progress: Arc<dyn bookclerk_plugin_sdk::v2::ProgressSink> = Arc::new(FencedProgress {
        cancel: Arc::clone(&cancel),
        library: progress,
    });
    let cancel: Arc<dyn Cancellation> = Arc::new(FlagCancel(cancel));
    client
        .handle_job_with_cancel(handler, invocation, input, output, progress, cancel)
        .await
        .map_err(map_abi)
}

struct FlagCancel(Arc<AtomicBool>);

#[async_trait(?Send)]
impl Cancellation for FlagCancel {
    async fn poll(&self) -> std::result::Result<bool, bookclerk_plugin_sdk::PluginError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

struct DestAsSource {
    dest: bookclerk_plugin_sdk::v2::DestinationClient,
}

#[async_trait(?Send)]
impl Source for DestAsSource {
    async fn open(
        &self,
        key: &str,
    ) -> std::result::Result<ReadResult, bookclerk_plugin_sdk::PluginError> {
        Destination::get(&self.dest, key, None).await
    }
}

/// Test-only pause between the live-fence check and `inner.commit()`.
///
/// Production commits pass [`None`]. Tests subscribe to [`Self::after_fence`],
/// reclaim the lease, then notify [`Self::release`] so a commit that observes
/// a lost fence returns cancelled without calling the inner destination.
struct CommitHold {
    /// Signalled after the first [`require_live_fence`] succeeds.
    after_fence: Notify,
    /// Test notifies this after reclaiming so commit may continue.
    release: Notify,
}

struct FencedDestination {
    inner: Arc<dyn Destination>,
    cancel: Arc<AtomicBool>,
    library: Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
    /// When set, [`Destination::commit`] waits here after the first fence check.
    commit_hold: Option<Arc<CommitHold>>,
}

async fn require_live_fence(
    cancel: &AtomicBool,
    library: &Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(bookclerk_plugin_sdk::PluginError::cancelled("fence lost"));
    }
    let Some((library, fence)) = library else {
        return Ok(());
    };
    match library.heartbeat_job(fence, 60, None).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            cancel.store(true, Ordering::SeqCst);
            Err(bookclerk_plugin_sdk::PluginError::cancelled("fence lost"))
        }
        Err(err) => Err(bookclerk_plugin_sdk::PluginError::internal(err.to_string())),
    }
}

#[async_trait(?Send)]
impl Destination for FencedDestination {
    async fn head(
        &self,
        key: &str,
    ) -> std::result::Result<Option<ObjectMetadata>, bookclerk_plugin_sdk::PluginError> {
        self.inner.head(key).await
    }

    async fn list(
        &self,
        options: ListOptions,
    ) -> std::result::Result<bookclerk_plugin_sdk::v2::ListPage, bookclerk_plugin_sdk::PluginError>
    {
        self.inner.list(options).await
    }

    async fn get(
        &self,
        key: &str,
        range: Option<bookclerk_plugin_sdk::v2::ByteRange>,
    ) -> std::result::Result<ReadResult, bookclerk_plugin_sdk::PluginError> {
        self.inner.get(key, range).await
    }

    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> std::result::Result<PutResult, bookclerk_plugin_sdk::PluginError> {
        require_live_fence(&self.cancel, &self.library).await?;
        self.inner.put(key, body, options).await
    }

    async fn copy(
        &self,
        from: &str,
        to: &str,
    ) -> std::result::Result<CopyResult, bookclerk_plugin_sdk::PluginError> {
        require_live_fence(&self.cancel, &self.library).await?;
        self.inner.copy(from, to).await
    }

    async fn delete(
        &self,
        key: &str,
    ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
        require_live_fence(&self.cancel, &self.library).await?;
        self.inner.delete(key).await
    }

    async fn commit(
        &self,
        key: &str,
        commit_token: &str,
    ) -> std::result::Result<PutResult, bookclerk_plugin_sdk::PluginError> {
        require_live_fence(&self.cancel, &self.library).await?;
        if let Some(hold) = &self.commit_hold {
            hold.after_fence.notify_waiters();
            hold.release.notified().await;
        }
        // Best-effort re-check shrinks the window. This is not a CAS at the
        // destination visibility boundary: library leases and object publish
        // cannot be committed atomically, so `inner.commit()` may still run
        // after a lost fence. Publication is at-least-once; retry-stable
        // commit tokens make a duplicate publish idempotent.
        require_live_fence(&self.cancel, &self.library).await?;
        self.inner.commit(key, commit_token).await
    }

    async fn abort_stage(
        &self,
        key: &str,
        commit_token: &str,
    ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
        require_live_fence(&self.cancel, &self.library).await?;
        self.inner.abort_stage(key, commit_token).await
    }
}

struct FencedProgress {
    cancel: Arc<AtomicBool>,
    library: Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
}

#[async_trait(?Send)]
impl bookclerk_plugin_sdk::v2::ProgressSink for FencedProgress {
    async fn report(
        &self,
        percent: f32,
        message: &str,
    ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(bookclerk_plugin_sdk::PluginError::cancelled("fence lost"));
        }
        let Some((library, fence)) = &self.library else {
            return Ok(());
        };
        let text = format!("{percent:.0}% {message}");
        match library.set_job_progress(fence, &text).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.cancel.store(true, Ordering::SeqCst);
                Err(bookclerk_plugin_sdk::PluginError::cancelled("fence lost"))
            }
            Err(err) => Err(bookclerk_plugin_sdk::PluginError::internal(err.to_string())),
        }
    }
}

/// [`StorageBackend`] over a v2 destination capability (streams, fail-closed scalars).
#[derive(Clone)]
pub struct V2Storage {
    /// Vat session.
    session: Arc<V2PluginSession>,
}

impl V2Storage {
    /// Wraps a connected session after [`V2PluginSession::ensure_destination`].
    #[must_use]
    pub fn new(session: Arc<V2PluginSession>) -> Self {
        Self { session }
    }

    fn map_err(err: PluginError) -> StorageError {
        match err {
            PluginError::Abi { code, message } if code == "not_found" => {
                StorageError::NotFound(message)
            }
            PluginError::Abi { code, message } if code == "payload_too_large" => {
                StorageError::PayloadTooLarge(message)
            }
            PluginError::Abi { code, message } if code == "invalid_cursor" => {
                StorageError::InvalidCursor(message)
            }
            other => StorageError::Other(anyhow!(other)),
        }
    }
}

#[async_trait]
impl StorageBackend for V2Storage {
    fn name(&self) -> &'static str {
        "plugin-v2"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> bookclerk_storage::Result<()> {
        if data.len() > MAX_SCALAR_BYTES as usize {
            return Err(StorageError::PayloadTooLarge(format!(
                "v2 scalar put of {} bytes exceeds {MAX_SCALAR_BYTES} (use put_stream)",
                data.len()
            )));
        }
        self.put_stream(key, Box::pin(std::io::Cursor::new(data)), meta)
            .await
            .map(|_| ())
    }

    async fn put_file(
        &self,
        key: &str,
        path: &std::path::Path,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<()> {
        let file = tokio::fs::File::open(path).await?;
        self.put_stream(key, Box::pin(file), meta).await.map(|_| ())
    }

    async fn get(&self, key: &str) -> bookclerk_storage::Result<Bytes> {
        let probe = self.probe(key).await?;
        if probe.size > u64::from(MAX_SCALAR_BYTES) {
            return Err(StorageError::PayloadTooLarge(format!(
                "v2 scalar get of {} bytes exceeds {MAX_SCALAR_BYTES} (use get_stream)",
                probe.size
            )));
        }
        let (_probe, mut body) = self.get_stream(key, None).await?;
        let mut buf = Vec::new();
        body.read_to_end(&mut buf).await?;
        Ok(Bytes::from(buf))
    }

    async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
        Ok(self.head(key).await?.is_some())
    }

    async fn list(&self, prefix: &str) -> bookclerk_storage::Result<Vec<ObjectInfo>> {
        let mut out = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.list_page(prefix, cursor.as_deref(), 0).await?;
            out.extend(page.objects);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    async fn probe(&self, key: &str) -> bookclerk_storage::Result<ObjectProbe> {
        match self.head(key).await? {
            Some(probe) => Ok(probe),
            None => Err(StorageError::NotFound(key.into())),
        }
    }

    async fn copy(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        self.session
            .call(|reply| Work::Copy {
                from: from.into(),
                to: to.into(),
                reply,
            })
            .await
            .map(|_| ())
            .map_err(Self::map_err)
    }

    async fn delete(&self, key: &str) -> bookclerk_storage::Result<()> {
        self.session
            .call(|reply| Work::Delete {
                key: key.into(),
                reply,
            })
            .await
            .map_err(Self::map_err)
    }

    async fn list_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> bookclerk_storage::Result<ListPage> {
        let page = self
            .session
            .call(|reply| Work::List {
                options: ListOptions {
                    prefix: prefix.into(),
                    cursor: cursor.map(str::to_string),
                    limit,
                },
                reply,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(ListPage {
            objects: page
                .objects
                .into_iter()
                .map(|o| ObjectInfo {
                    key: o.key,
                    size: o.size,
                })
                .collect(),
            next_cursor: page.next_cursor,
        })
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> bookclerk_storage::Result<(ObjectProbe, Pin<Box<dyn AsyncRead + Send>>)> {
        let abi_range = range.map(|r| AbiByteRange {
            offset: r.offset,
            length: r.length,
        });
        let read = self
            .session
            .call(|reply| Work::GetStream {
                key: key.into(),
                range: abi_range,
                reply,
            })
            .await
            .map_err(Self::map_err)?;
        Ok((meta_to_probe(read.meta), read.body))
    }

    async fn put_stream(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<PutStreamResult> {
        let put = self
            .session
            .call(|reply| Work::PutStream {
                key: key.into(),
                body,
                options: WriteOptions {
                    content_type: meta.content_type,
                    content_length: meta.content_length,
                    sha256: None,
                    commit_token: None,
                    stage_only: false,
                },
                reply,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(PutStreamResult {
            bytes_written: put.bytes_written,
            etag: put.etag,
        })
    }

    async fn head(&self, key: &str) -> bookclerk_storage::Result<Option<ObjectProbe>> {
        let meta = self
            .session
            .call(|reply| Work::Head {
                key: key.into(),
                reply,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(meta.map(meta_to_probe))
    }

    fn supports_server_copy(&self) -> bool {
        self.session.supports_server_copy()
    }
}

fn meta_to_probe(meta: ObjectMetadata) -> ObjectProbe {
    ObjectProbe {
        key: meta.key.clone(),
        size: meta.size,
        content_type: meta.content_type.clone(),
        meta: ObjectMeta {
            content_type: meta.content_type,
            content_length: Some(meta.size),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::v2::{
        PluginDescribe, ProgressSink, ScalarLimits, FEATURE_SCALAR_LIMITS, FEATURE_STREAMS,
        PRODUCT_API_VERSION,
    };
    use sea_orm::EntityTrait;

    #[test]
    fn instance_key_separates_accounts() {
        assert_ne!(
            plugin_instance_key("audible", "acct-a"),
            plugin_instance_key("audible", "acct-b")
        );
        assert_eq!(
            plugin_instance_key("audible", "acct-a"),
            plugin_instance_key("audible", "acct-a")
        );
        assert_ne!(
            plugin_instance_key("local", OPERATOR_ACCOUNT),
            plugin_instance_key("local", "acct-a")
        );
    }

    #[test]
    fn account_bearing_kinds_reject_operator_isolate() {
        assert!(account_bearing_requires_non_operator(
            crate::PluginKind::Source,
            OPERATOR_ACCOUNT
        ));
        assert!(account_bearing_requires_non_operator(
            crate::PluginKind::Integration,
            ""
        ));
        assert!(!account_bearing_requires_non_operator(
            crate::PluginKind::Source,
            "acct-a"
        ));
        assert!(!account_bearing_requires_non_operator(
            crate::PluginKind::Output,
            OPERATOR_ACCOUNT
        ));
    }

    #[test]
    fn negotiate_rejects_id_and_kind_mismatch() {
        let desc = PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "other".into(),
            kind: "output".into(),
            display_name: None,
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into(), FEATURE_STREAMS.into()],
            scalar_limits: ScalarLimits::default().into(),
            ..PluginDescribe::default()
        };
        let err = negotiate_describe(&desc, "local", "output").unwrap_err();
        assert!(err.to_string().contains("id mismatch"));

        let desc = PluginDescribe {
            id: "local".into(),
            kind: "source".into(),
            ..desc
        };
        let err = negotiate_describe(&desc, "local", "output").unwrap_err();
        assert!(err.to_string().contains("kind mismatch"));
    }

    #[test]
    fn negotiate_rejects_missing_features_and_zero_limits() {
        let desc = PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "local".into(),
            kind: "output".into(),
            display_name: None,
            rpc_features: vec![FEATURE_STREAMS.into()],
            scalar_limits: ScalarLimits::default().into(),
            ..PluginDescribe::default()
        };
        assert!(negotiate_describe(&desc, "local", "output").is_err());

        let desc = PluginDescribe {
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into(), FEATURE_STREAMS.into()],
            scalar_limits: ScalarLimits {
                max_scalar_bytes: 0,
                max_stream_window_bytes: 1024,
                max_list_page: 10,
            }
            .into(),
            ..desc
        };
        assert!(negotiate_describe(&desc, "local", "output").is_err());
    }

    #[tokio::test]
    async fn wait_flag_is_timeout_bounded() {
        let flag = Arc::new(AtomicBool::new(false));
        let wait = wait_flag(Arc::clone(&flag));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        tokio::time::timeout(Duration::from_secs(2), wait)
            .await
            .expect("wait_flag hung");
    }

    #[tokio::test]
    async fn fenced_progress_persists_and_rejects_stale_generation() {
        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let created = store
            .enqueue_job(bookclerk_library::EnqueueJobSpec {
                kind: bookclerk_library::JobKind::PluginCopy,
                payload: bookclerk_library::JobPayload {
                    plugin_id: Some("local".into()),
                    source_key: Some("from".into()),
                    dest_key: Some("to".into()),
                    trigger: bookclerk_library::JobTrigger::Api,
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 8,
                run_after: None,
            })
            .await
            .unwrap();
        let bookclerk_library::EnqueueOutcome::Created { id } = created else {
            panic!("expected created");
        };
        let claimed = store
            .claim_next_job(
                bookclerk_library::JobResourceClass::Network,
                "worker-progress",
                60,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .unwrap()
            .expect("claim");
        let fence = claimed.fence().expect("fence");
        let cancel = Arc::new(AtomicBool::new(false));
        let sink = FencedProgress {
            cancel: Arc::clone(&cancel),
            library: Some((store.clone(), fence.clone())),
        };
        sink.report(10.0, "staging").await.unwrap();
        let row = store.get_job(&id).await.unwrap().unwrap();
        assert_eq!(row.progress.as_deref(), Some("10% staging"));

        let stale = bookclerk_library::JobFence {
            job_id: fence.job_id.clone(),
            owner: fence.owner.clone(),
            generation: fence.generation.saturating_sub(1),
        };
        let stale_sink = FencedProgress {
            cancel: Arc::clone(&cancel),
            library: Some((store.clone(), stale)),
        };
        let err = stale_sink.report(50.0, "lost").await.unwrap_err();
        assert_eq!(err.wire_str(), "cancelled");
        assert!(cancel.load(Ordering::SeqCst));
        let unchanged = store.get_job(&id).await.unwrap().unwrap();
        assert_eq!(unchanged.progress.as_deref(), Some("10% staging"));
    }

    struct RecordingDest {
        staged: std::sync::Mutex<std::collections::HashMap<(String, String), Vec<u8>>>,
        published: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    }

    impl RecordingDest {
        fn new() -> Self {
            Self {
                staged: std::sync::Mutex::new(std::collections::HashMap::new()),
                published: std::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl Destination for RecordingDest {
        async fn head(
            &self,
            _key: &str,
        ) -> std::result::Result<Option<ObjectMetadata>, bookclerk_plugin_sdk::PluginError>
        {
            Ok(None)
        }

        async fn list(
            &self,
            _options: ListOptions,
        ) -> std::result::Result<
            bookclerk_plugin_sdk::v2::ListPage,
            bookclerk_plugin_sdk::PluginError,
        > {
            Ok(bookclerk_plugin_sdk::v2::ListPage::default())
        }

        async fn get(
            &self,
            key: &str,
            _range: Option<bookclerk_plugin_sdk::v2::ByteRange>,
        ) -> std::result::Result<ReadResult, bookclerk_plugin_sdk::PluginError> {
            Err(bookclerk_plugin_sdk::PluginError::not_found(key))
        }

        async fn put(
            &self,
            key: &str,
            mut body: Pin<Box<dyn AsyncRead + Send>>,
            options: WriteOptions,
        ) -> std::result::Result<PutResult, bookclerk_plugin_sdk::PluginError> {
            let mut buf = Vec::new();
            body.read_to_end(&mut buf)
                .await
                .map_err(|err| bookclerk_plugin_sdk::PluginError::internal(err.to_string()))?;
            let bytes_written = buf.len() as u64;
            if options.stage_only {
                let token = options.commit_token.clone().unwrap_or_default();
                self.staged
                    .lock()
                    .expect("recording dest staged lock")
                    .insert((key.to_string(), token), buf);
            } else {
                self.published
                    .lock()
                    .expect("recording dest published lock")
                    .insert(key.to_string(), buf);
            }
            Ok(PutResult {
                key: key.to_string(),
                bytes_written,
                etag: None,
                sha256: None,
            })
        }

        async fn copy(
            &self,
            _from: &str,
            _to: &str,
        ) -> std::result::Result<CopyResult, bookclerk_plugin_sdk::PluginError> {
            Err(bookclerk_plugin_sdk::PluginError::unsupported("copy"))
        }

        async fn delete(
            &self,
            key: &str,
        ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
            self.published
                .lock()
                .expect("recording dest published lock")
                .remove(key);
            Ok(())
        }

        async fn commit(
            &self,
            key: &str,
            commit_token: &str,
        ) -> std::result::Result<PutResult, bookclerk_plugin_sdk::PluginError> {
            let staged = self
                .staged
                .lock()
                .expect("recording dest staged lock")
                .remove(&(key.to_string(), commit_token.to_string()))
                .ok_or_else(|| {
                    bookclerk_plugin_sdk::PluginError::not_found("staged object missing")
                })?;
            let bytes_written = staged.len() as u64;
            self.published
                .lock()
                .expect("recording dest published lock")
                .insert(key.to_string(), staged);
            Ok(PutResult {
                key: key.to_string(),
                bytes_written,
                etag: None,
                sha256: None,
            })
        }

        async fn abort_stage(
            &self,
            key: &str,
            commit_token: &str,
        ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
            self.staged
                .lock()
                .expect("recording dest staged lock")
                .remove(&(key.to_string(), commit_token.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn lost_fence_cancels_commit_before_inner_publish() {
        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let created = store
            .enqueue_job(bookclerk_library::EnqueueJobSpec {
                kind: bookclerk_library::JobKind::PluginCopy,
                payload: bookclerk_library::JobPayload {
                    plugin_id: Some("local".into()),
                    source_key: Some("from".into()),
                    dest_key: Some("to".into()),
                    trigger: bookclerk_library::JobTrigger::Api,
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 8,
                run_after: None,
            })
            .await
            .unwrap();
        let bookclerk_library::EnqueueOutcome::Created { id } = created else {
            panic!("expected created");
        };
        let claimed = store
            .claim_next_job(
                bookclerk_library::JobResourceClass::Network,
                "worker-commit",
                60,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .unwrap()
            .expect("claim");
        let fence = claimed.fence().expect("fence");
        let cancel = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(RecordingDest::new());
        let dest = FencedDestination {
            inner: Arc::clone(&inner) as Arc<dyn Destination>,
            cancel: Arc::clone(&cancel),
            library: Some((store.clone(), fence.clone())),
            commit_hold: None,
        };
        dest.put(
            "library/title.m4b",
            Box::pin(std::io::Cursor::new(b"staged-bytes".to_vec())),
            WriteOptions {
                commit_token: Some("tok".into()),
                stage_only: true,
                ..WriteOptions::default()
            },
        )
        .await
        .unwrap();
        let sink = FencedProgress {
            cancel: Arc::clone(&cancel),
            library: Some((store.clone(), fence.clone())),
        };
        sink.report(90.0, "committing").await.unwrap();

        let model = bookclerk_library::entities::jobs::Entity::find_by_id(&id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap();
        let mut am: bookclerk_library::entities::jobs::ActiveModel = model.into();
        am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
            (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
        ));
        sea_orm::ActiveModelTrait::update(am, store.db())
            .await
            .unwrap();
        assert_eq!(store.reclaim_expired_leases().await.unwrap(), 1);
        let _next = store
            .claim_next_job(
                bookclerk_library::JobResourceClass::Network,
                "worker-new",
                60,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .unwrap()
            .expect("reclaim claim");

        let err = dest.commit("library/title.m4b", "tok").await.unwrap_err();
        assert_eq!(err.wire_str(), "cancelled");
        assert!(cancel.load(Ordering::SeqCst));
        assert!(
            inner
                .published
                .lock()
                .expect("recording dest published lock")
                .is_empty(),
            "commit that observes a lost fence must not call inner publish"
        );
        assert!(
            inner
                .staged
                .lock()
                .expect("recording dest staged lock")
                .contains_key(&("library/title.m4b".into(), "tok".into())),
            "staged object remains unpublished"
        );
    }

    #[tokio::test]
    async fn lost_fence_cancels_commit_after_post_check_barrier() {
        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let created = store
            .enqueue_job(bookclerk_library::EnqueueJobSpec {
                kind: bookclerk_library::JobKind::PluginCopy,
                payload: bookclerk_library::JobPayload {
                    plugin_id: Some("local".into()),
                    source_key: Some("from".into()),
                    dest_key: Some("to".into()),
                    trigger: bookclerk_library::JobTrigger::Api,
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 8,
                run_after: None,
            })
            .await
            .unwrap();
        let bookclerk_library::EnqueueOutcome::Created { id } = created else {
            panic!("expected created");
        };
        let claimed = store
            .claim_next_job(
                bookclerk_library::JobResourceClass::Network,
                "worker-commit-barrier",
                60,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .unwrap()
            .expect("claim");
        let fence = claimed.fence().expect("fence");
        let cancel = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(RecordingDest::new());
        let hold = Arc::new(CommitHold {
            after_fence: Notify::new(),
            release: Notify::new(),
        });
        let dest = FencedDestination {
            inner: Arc::clone(&inner) as Arc<dyn Destination>,
            cancel: Arc::clone(&cancel),
            library: Some((store.clone(), fence.clone())),
            commit_hold: Some(Arc::clone(&hold)),
        };
        dest.put(
            "library/title.m4b",
            Box::pin(std::io::Cursor::new(b"staged-bytes".to_vec())),
            WriteOptions {
                commit_token: Some("tok".into()),
                stage_only: true,
                ..WriteOptions::default()
            },
        )
        .await
        .unwrap();

        let mut commit = std::pin::pin!(dest.commit("library/title.m4b", "tok"));
        let mut passed_fence = std::pin::pin!(hold.after_fence.notified());
        tokio::select! {
            biased;
            () = &mut passed_fence => {}
            result = &mut commit => {
                panic!("commit finished before post-fence barrier: {result:?}");
            }
        }

        let model = bookclerk_library::entities::jobs::Entity::find_by_id(&id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap();
        let mut am: bookclerk_library::entities::jobs::ActiveModel = model.into();
        am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
            (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
        ));
        sea_orm::ActiveModelTrait::update(am, store.db())
            .await
            .unwrap();
        assert_eq!(store.reclaim_expired_leases().await.unwrap(), 1);
        let _next = store
            .claim_next_job(
                bookclerk_library::JobResourceClass::Network,
                "worker-new-barrier",
                60,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .unwrap()
            .expect("reclaim claim");

        hold.release.notify_waiters();
        let err = commit.await.unwrap_err();
        assert_eq!(err.wire_str(), "cancelled");
        assert!(cancel.load(Ordering::SeqCst));
        assert!(
            inner
                .published
                .lock()
                .expect("recording dest published lock")
                .is_empty(),
            "commit that observes a lost fence after the first check must not call inner publish"
        );
        assert!(
            inner
                .staged
                .lock()
                .expect("recording dest staged lock")
                .contains_key(&("library/title.m4b".into(), "tok".into())),
            "staged object remains unpublished"
        );
    }
}
