//! Host session for plugin Cap'n Proto guests (object-capability + streams).
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
use bookclerk_plugin_abi::HostAdapterDatabaseSession;
use bookclerk_plugin_sdk::{
    connect_plugin, negotiate_rpc_features, BindingValues, ByteRange as AbiByteRange, Cancellation,
    CopyResult, Destination, DomainEvent, EventConsumer, EventPublisher, EventResult, HostBindings,
    Invocation, JobInvocation, JobInvocationLease, ListOptions, ObjectMetadata, Oidc,
    OidcClientTemplate, OpenedEntrypoints, PluginCli, PluginClient, PluginDescribe, PutResult,
    ReadResult, ScalarLimits, Source, StreamCopySpec, WriteOptions, FEATURE_SCALAR_LIMITS,
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
use crate::event_publisher::EventOutbox;
use crate::PluginManifest;
use crate::{PluginError, Result};

/// Send-safe constructor for a per-binding [`bookclerk_plugin_sdk::GuestDatabase`].
///
/// `GuestDatabase` trait objects are `?Send`, so named plugin database
/// bindings cross into the vat task as factories and are constructed on the
/// vat thread just before the per-job `PluginWorker.open`. The factory
/// receives the job cancel flag and the host lease deadline so binding
/// execute can abort and cap `deadlineUnixMs`.
pub type GuestDatabaseFactory =
    Arc<dyn Fn(Arc<AtomicBool>, u64) -> Arc<dyn bookclerk_plugin_sdk::GuestDatabase> + Send + Sync>;

/// Type-erased typed storefront call executed on the vat thread against the
/// opened `storefront` entrypoint (or the open / missing-entrypoint error).
type ContentSourceCall = Box<
    dyn FnOnce(
            std::result::Result<
                Box<dyn bookclerk_plugin_sdk::ContentSource>,
                bookclerk_plugin_sdk::PluginError,
            >,
        ) -> LocalBoxFuture<'static, ()>
        + Send,
>;

/// Type-erased typed remote-library call executed on the vat thread against
/// the opened `remoteLibrary` entrypoint (or the open / missing error).
type RemoteLibraryCall = Box<
    dyn FnOnce(
            std::result::Result<
                Box<dyn bookclerk_plugin_sdk::RemoteLibrary>,
                bookclerk_plugin_sdk::PluginError,
            >,
        ) -> LocalBoxFuture<'static, ()>
        + Send,
>;

/// `!Send` boxed future pinned for the vat's `LocalSet`.
type LocalBoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

/// Work item executed on the plugin vat thread.
enum Work {
    /// `PluginWorker.describe`.
    Describe {
        /// Reply channel.
        reply: oneshot::Sender<Result<PluginDescribe>>,
    },
    /// `PluginWorker.open` for the session's primary entrypoints with the
    /// granted binding values (re-opens when the values change).
    Open {
        /// `CONFIG` / `SECRETS` values.
        values: BindingValues,
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
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::ListPage>>,
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
    /// `JobRunner.job` stream-copy vertical slice (one `open` per job).
    StreamCopy {
        /// Claimed-lease invocation envelope.
        lease: bookclerk_plugin_sdk::JobInvocationLease,
        /// Copy spec.
        spec: StreamCopySpec,
        /// Host fence / cancel flag.
        cancel: Arc<AtomicBool>,
        /// Durable fenced progress (library row + lease identity).
        progress: Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
        /// Named plugin-owned database bindings (constructed on the vat thread).
        databases: Vec<(String, GuestDatabaseFactory)>,
        /// Reply channel.
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::JobOutcome>>,
    },
    /// One typed `storefront` method on the opened primary entrypoints.
    Storefront {
        /// Typed call; replies through the sender it captured.
        call: ContentSourceCall,
    },
    /// One typed `remoteLibrary` method on the opened primary entrypoints.
    RemoteLibrary {
        /// Typed call; replies through the sender it captured.
        call: RemoteLibraryCall,
        /// Abort flag (fence loss).
        cancel: Arc<AtomicBool>,
    },
    /// `EventConsumer.event` batch delivery on the opened primary entrypoints.
    DeliverEvents {
        /// Ordered batch (at most `MAX_LIST_PAGE`).
        batch: Vec<DomainEvent>,
        /// Abort flag (delivery fence loss).
        cancel: Arc<AtomicBool>,
        /// Reply channel.
        reply: oneshot::Sender<Result<Vec<EventResult>>>,
    },
    CliDescribe {
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::CliSchema>>,
    },
    CliInvoke {
        params: bookclerk_plugin_sdk::CliInvokeParams,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::CliInvokeResult>>,
    },
    OidcClients {
        reply: oneshot::Sender<Result<Vec<OidcClientTemplate>>>,
    },
    /// `Oidc.authenticateUser` on the opened primary entrypoints.
    OidcAuthenticate {
        params: bookclerk_plugin_sdk::AuthenticateUserParams,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::ExternalUser>>,
    },
    DatabaseMigrations {
        binding: String,
        reply: oneshot::Sender<Result<Vec<bookclerk_plugin_sdk::PluginMigration>>>,
    },
    /// Opens the library adapter session: its own `PluginWorker.open` with
    /// host-private connect params, taking the `databaseAdapter` entrypoint.
    DbOpen {
        values: BindingValues,
        reply: oneshot::Sender<Result<()>>,
    },
    DbBegin {
        isolation: bookclerk_plugin_abi::IsolationReq,
        reply: oneshot::Sender<Result<()>>,
    },
    DbCommit {
        reply: oneshot::Sender<Result<()>>,
    },
    DbRollback {
        reply: oneshot::Sender<Result<()>>,
    },
    DbCapabilities {
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::DbCapabilities>>,
    },
    DbBootstrap {
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::DbBootstrap>>,
    },
    DbExecuteRequest {
        request: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::ExecuteReply>>,
    },
    DbExecuteEnvelopeRequest {
        envelope: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::ExecuteReply>>,
    },
    DbTxnExecuteRequest {
        request: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::ExecuteReply>>,
    },
    /// Opens an isolated adapter session for one named plugin database binding.
    DbOpenBinding {
        /// Binding name (`plugin.toml` `[[databases]]`).
        name: String,
        /// Per-binding open values (host-private connect params).
        values: BindingValues,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::DbCapabilities>>,
    },
    /// Typed execute on a named plugin database binding session.
    DbExecuteBindingRequest {
        /// Binding name previously opened with [`Work::DbOpenBinding`].
        name: String,
        request: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::ExecuteReply>>,
    },
    /// Host-private envelope execute on a named plugin database binding.
    DbExecuteBindingEnvelopeRequest {
        /// Binding name previously opened with [`Work::DbOpenBinding`].
        name: String,
        envelope: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::ExecuteReply>>,
    },
    /// Begin a vat-held transaction on a named plugin database binding.
    DbBeginBinding {
        name: String,
        isolation: bookclerk_plugin_abi::IsolationReq,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Commit the vat-held binding transaction.
    DbCommitBinding {
        name: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Roll back the vat-held binding transaction.
    DbRollbackBinding {
        name: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Typed execute on the vat-held binding transaction.
    DbTxnExecuteBindingRequest {
        name: String,
        request: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<bookclerk_plugin_sdk::ExecuteReply>>,
    },
    DbBackup {
        binding: Option<String>,
        kind: BackupKind,
        reply: oneshot::Sender<Result<BackupOutcome>>,
    },
    /// Drop the vat.
    Shutdown,
}

enum BackupKind {
    ExportIdentity,
    ImportIdentity(Vec<bookclerk_plugin_abi::DbIdentityHighWater>),
    ListUserRelations,
    PrepareUnitRestore,
    DropUserRelations(Vec<String>),
    AssertRestoreConstraints,
}

enum BackupOutcome {
    Identity(Vec<bookclerk_plugin_abi::DbIdentityHighWater>),
    Names(Vec<String>),
    Unit,
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
            role: plugin.manifest.primary_family().as_str().to_string(),
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
fn account_bearing_requires_non_operator(manifest: &PluginManifest, account_id: &str) -> bool {
    manifest.families().iter().any(|family| {
        matches!(
            family,
            crate::PluginFamily::Source | crate::PluginFamily::Integration
        )
    }) && (account_id.is_empty() || account_id == OPERATOR_ACCOUNT)
}

/// Host services a session hands its guest as `Bindings` on every
/// `PluginWorker.open`.
///
/// Nothing here is guest-visible on its own: the session still gates each
/// binding on the manifest declaration plus the covering consent grant.
#[derive(Clone, Default)]
pub struct SessionServices {
    /// Library store whose outbox backs the `EVENTS` binding. `None` (the
    /// default) never exposes `EVENTS`, even to a granted producer.
    pub event_outbox: Option<bookclerk_library::LibraryStore>,
}

impl SessionServices {
    /// Services with the library outbox attached.
    #[must_use]
    pub fn with_event_outbox(store: bookclerk_library::LibraryStore) -> Self {
        Self {
            event_outbox: Some(store),
        }
    }

    /// Services with the library outbox attached when `store` is present.
    #[must_use]
    pub fn from_outbox(store: Option<&bookclerk_library::LibraryStore>) -> Self {
        Self {
            event_outbox: store.cloned(),
        }
    }
}

/// Host-side plugin session (one jailed child + one vat thread).
pub struct PluginSession {
    /// Work queue into the vat thread.
    tx: mpsc::UnboundedSender<Work>,
    /// Plugin id.
    id: String,
    /// Guest data directory.
    data: std::path::PathBuf,
    /// Instance key `(plugin_id, account_id)`.
    instance_key: String,
    /// Account scope carried on every `Invocation`.
    account_id: String,
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
    /// Spawn config JSON captured at spawn.
    spawn_config: Value,
    /// AppContainer package SID.
    #[cfg(windows)]
    package_sid: Option<String>,
}

impl PluginSession {
    /// Spawns a plugin guest and connects Cap'n Proto on stdio.
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
        Self::spawn_with(
            plugin,
            config,
            config_table,
            account_id,
            extra_env,
            SessionServices::default(),
        )
        .await
    }

    /// [`Self::spawn_for_account_with_env`] plus the host services the guest
    /// may receive as bindings (`EVENTS` outbox, …).
    ///
    /// # Errors
    ///
    /// Fails when the child cannot start, describe fails, or negotiation fails.
    pub async fn spawn_with(
        plugin: &DiscoveredPlugin,
        config: &Config,
        config_table: Value,
        account_id: &str,
        extra_env: &[(&str, std::ffi::OsString)],
        services: SessionServices,
    ) -> Result<Self> {
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(PluginError::message(format!(
                "plugin `{}` api_version {} is not supported",
                plugin.manifest.id, plugin.manifest.api_version
            )));
        }
        if account_bearing_requires_non_operator(&plugin.manifest, account_id) {
            return Err(PluginError::message(format!(
                "plugin `{}` is account-bearing and requires a non-operator account_id",
                plugin.manifest.id
            )));
        }
        let spawned =
            crate::spawn_stdio::spawn_stdio_guest(plugin, config, config_table, extra_env).await?;
        Self::connect_spawned(spawned, plugin, account_id, services).await
    }

    async fn connect_spawned(
        spawned: crate::spawn_stdio::SpawnedStdio,
        plugin: &DiscoveredPlugin,
        account_id: &str,
        services: SessionServices,
    ) -> Result<Self> {
        let manifest = plugin.manifest.clone();
        let id = spawned.id.clone();
        let data = spawned.data.clone();
        let scratch = spawned.scratch.clone();
        let grant = spawned.grant.clone();
        // `EVENTS` needs all three: a host outbox, a manifest producer, and
        // the operator grant covering that producer.
        let events = services.event_outbox.and_then(|store| {
            EventOutbox::new(store, &id, manifest.producer_types(), &grant.producers)
        });
        let spawn_config = spawned.spawn_config.clone();
        #[cfg(windows)]
        let package_sid = spawned.package_sid.clone();
        let guest_pid = spawned.child.id();
        let instance_key = plugin_instance_key(&id, account_id);
        let session_key = ExecutorIdentity::from_plugin(plugin, account_id).session_key();
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) =
            oneshot::channel::<Result<(PluginDescribe, ScalarLimits, Vec<String>)>>();
        let vat_account = account_id.to_string();
        thread::Builder::new()
            .name(format!("plugin-vat-{}", id))
            .spawn(move || vat_thread(spawned, manifest, vat_account, events, rx, ready_tx))
            .map_err(|err| PluginError::message(format!("plugin vat thread: {err}")))?;
        let (desc, limits, features) = ready_rx
            .await
            .map_err(|err| PluginError::message(format!("plugin vat dropped: {err}")))??;
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
            account_id: account_id.to_string(),
            session_key,
            guest_pid,
            limits,
            features,
            describe: desc,
            grant,
            scratch,
            spawn_config,
            #[cfg(windows)]
            package_sid,
        })
    }

    /// Isolation instance key (`plugin_id:account_id`).
    #[must_use]
    pub fn instance_key(&self) -> &str {
        &self.instance_key
    }

    /// Account scope carried on every `PluginWorker.open` invocation.
    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
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
            .map_err(|_| PluginError::unavailable("plugin vat thread closed"))?;
        rx.await
            .map_err(|_| PluginError::unavailable("plugin vat thread dropped reply"))?
    }

    /// Opens the session's primary entrypoints with the granted binding
    /// values (`CONFIG` / `SECRETS`).
    ///
    /// Idempotent for equal values; different values re-open. Entrypoint
    /// calls that run before `open` use empty bindings.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when `PluginWorker.open` fails.
    pub async fn open(&self, values: BindingValues) -> Result<()> {
        self.call(|reply| Work::Open { values, reply }).await
    }

    /// Calls `PluginWorker.describe`.
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
    ) -> Result<bookclerk_plugin_sdk::JobOutcome> {
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
    ) -> Result<bookclerk_plugin_sdk::JobOutcome> {
        self.stream_copy_with_databases(lease, from, to, cancel, progress, Vec::new())
            .await
    }

    /// [`Self::stream_copy_with_cancel`] with named plugin database bindings.
    ///
    /// Each `(name, factory)` pair becomes an isolated `GuestDatabase` on the
    /// per-job `PluginWorker.open` bindings; factories run on the vat thread.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the handler fails or the fence is lost.
    pub async fn stream_copy_with_databases(
        &self,
        lease: JobInvocationLease,
        from: &str,
        to: &str,
        cancel: Arc<AtomicBool>,
        progress: Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
        databases: Vec<(String, GuestDatabaseFactory)>,
    ) -> Result<bookclerk_plugin_sdk::JobOutcome> {
        self.call(|reply| Work::StreamCopy {
            lease,
            spec: StreamCopySpec {
                from: from.into(),
                to: to.into(),
            },
            cancel,
            progress,
            databases,
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

    /// Spawn config JSON captured at spawn.
    #[must_use]
    pub fn spawn_config(&self) -> &Value {
        &self.spawn_config
    }

    /// True when the guest exported `entrypoint` in `describe()`.
    #[must_use]
    pub fn has_entrypoint(&self, entrypoint: crate::Entrypoint) -> bool {
        self.describe.has_entrypoint(entrypoint)
    }

    /// True when the guest's default entrypoint consumes `event_type`.
    #[must_use]
    pub fn consumes_event(&self, event_type: &str) -> bool {
        self.describe.consumes_event(event_type)
    }

    /// True when the guest's default entrypoint consumes any event type.
    #[must_use]
    pub fn consumes_events(&self) -> bool {
        !self.describe.capabilities.consumes.is_empty()
    }

    /// One typed `storefront` method on the opened primary entrypoints.
    ///
    /// `call` runs on the vat thread; its output crosses back to the caller's
    /// runtime.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest did not export `storefront` or
    /// the method fails.
    pub async fn storefront<T, F, Fut>(&self, call: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Box<dyn bookclerk_plugin_sdk::ContentSource>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::result::Result<T, bookclerk_plugin_sdk::PluginError>>
            + 'static,
    {
        let (reply, rx) = oneshot::channel::<Result<T>>();
        let erased: ContentSourceCall = Box::new(move |stub| {
            Box::pin(async move {
                let out = match stub {
                    Ok(stub) => call(stub).await.map_err(map_abi),
                    Err(err) => Err(map_abi(err)),
                };
                let _ = reply.send(out);
            })
        });
        self.tx
            .send(Work::Storefront { call: erased })
            .map_err(|_| PluginError::unavailable("plugin vat thread closed"))?;
        rx.await
            .map_err(|_| PluginError::unavailable("plugin vat thread dropped reply"))?
    }

    /// One typed `remoteLibrary` method on the opened primary entrypoints.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest did not export `remoteLibrary`
    /// or the method fails.
    pub async fn remote_library<T, F, Fut>(&self, call: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Box<dyn bookclerk_plugin_sdk::RemoteLibrary>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::result::Result<T, bookclerk_plugin_sdk::PluginError>>
            + 'static,
    {
        self.remote_library_cancelable(Arc::new(AtomicBool::new(false)), call)
            .await
    }

    /// Typed `remoteLibrary` method aborted when `cancel` is set.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails or is cancelled.
    pub async fn remote_library_cancelable<T, F, Fut>(
        &self,
        cancel: Arc<AtomicBool>,
        call: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Box<dyn bookclerk_plugin_sdk::RemoteLibrary>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::result::Result<T, bookclerk_plugin_sdk::PluginError>>
            + 'static,
    {
        let (reply, rx) = oneshot::channel::<Result<T>>();
        let erased: RemoteLibraryCall = Box::new(move |stub| {
            Box::pin(async move {
                let out = match stub {
                    Ok(stub) => call(stub).await.map_err(map_abi),
                    Err(err) => Err(map_abi(err)),
                };
                let _ = reply.send(out);
            })
        });
        self.tx
            .send(Work::RemoteLibrary {
                call: erased,
                cancel,
            })
            .map_err(|_| PluginError::unavailable("plugin vat thread closed"))?;
        rx.await
            .map_err(|_| PluginError::unavailable("plugin vat thread dropped reply"))?
    }

    /// Delivers one ordered event batch to the guest `eventConsumer` trigger
    /// (`EventConsumer.event`), aborted when `cancel` is set (fence loss).
    ///
    /// Returns exactly one [`EventResult`] per input event.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest did not export an event
    /// consumer, the RPC fails, or the delivery is cancelled.
    pub async fn deliver_events(
        &self,
        batch: Vec<DomainEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<EventResult>> {
        self.call(|reply| Work::DeliverEvents {
            batch,
            cancel,
            reply,
        })
        .await
    }

    /// Guest CLI schema.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn cli_describe(&self) -> Result<bookclerk_plugin_sdk::CliSchema> {
        self.call(|reply| Work::CliDescribe { reply }).await
    }

    /// Guest CLI invoke.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn cli_invoke(
        &self,
        params: bookclerk_plugin_sdk::CliInvokeParams,
    ) -> Result<bookclerk_plugin_sdk::CliInvokeResult> {
        self.call(|reply| Work::CliInvoke { params, reply }).await
    }

    /// Plugin-provided OIDC authorization-server client templates
    /// (`Oidc.clients`); empty when the guest exports no `oidc` entrypoint.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn oidc_clients(&self) -> Result<Vec<OidcClientTemplate>> {
        self.call(|reply| Work::OidcClients { reply }).await
    }

    /// Verifies remote credentials through the guest `oidc` entrypoint
    /// (`Oidc.authenticateUser`).
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest exports no `oidc` entrypoint or
    /// the RPC fails.
    pub async fn oidc_authenticate_user(
        &self,
        params: bookclerk_plugin_sdk::AuthenticateUserParams,
    ) -> Result<bookclerk_plugin_sdk::ExternalUser> {
        self.call(|reply| Work::OidcAuthenticate { params, reply })
            .await
    }

    /// Complete ordered plugin-owned migration sequence for one named binding.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn database_migrations(
        &self,
        binding: &str,
    ) -> Result<Vec<bookclerk_plugin_sdk::PluginMigration>> {
        let binding = binding.to_string();
        self.call(|reply| Work::DatabaseMigrations { binding, reply })
            .await
    }

    /// Opens the library adapter session (held on the vat until drop).
    ///
    /// Performs a dedicated `PluginWorker.open` whose bindings carry the
    /// host-private connect params in `values`, then `openSession` on the
    /// returned `databaseAdapter` entrypoint.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when `open` / `openSession` fails or the guest
    /// exports no `databaseAdapter`.
    pub async fn db_open(&self, values: BindingValues) -> Result<()> {
        self.call(|reply| Work::DbOpen { values, reply }).await
    }

    /// Opens an isolated adapter session for one named plugin database binding.
    ///
    /// Negotiates [`bookclerk_plugin_sdk::DbCapabilities`] on the binding
    /// session itself (not the library session).
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest rejects the open or the binding
    /// session fails host capability minima.
    pub async fn db_open_binding(
        &self,
        name: &str,
        values: BindingValues,
    ) -> Result<bookclerk_plugin_sdk::DbCapabilities> {
        let name = name.to_string();
        self.call(|reply| Work::DbOpenBinding {
            name,
            values,
            reply,
        })
        .await
    }

    /// Typed execute on a named plugin database binding session.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the binding is not open or the guest
    /// rejects the call.
    pub async fn db_execute_binding_request(
        &self,
        name: &str,
        request: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<bookclerk_plugin_sdk::ExecuteReply> {
        let name = name.to_string();
        self.call(|reply| Work::DbExecuteBindingRequest {
            name,
            request,
            cancel,
            reply,
        })
        .await
    }

    /// Host-private envelope execute on a named plugin database binding.
    ///
    /// Forwards [`bookclerk_plugin_abi::AdapterExecuteRequest`] so the adapter
    /// persists guest receipt payloads before COMMIT.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the binding is not open, the guest rejects
    /// the call, or `cancel` is set.
    pub async fn db_execute_binding_envelope_request(
        &self,
        name: &str,
        envelope: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<bookclerk_plugin_sdk::ExecuteReply> {
        let name = name.to_string();
        self.call(|reply| Work::DbExecuteBindingEnvelopeRequest {
            name,
            envelope,
            cancel,
            reply,
        })
        .await
    }

    /// Typed `AdapterDatabaseSession.capabilities`.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest rejects the call.
    pub async fn db_capabilities(&self) -> Result<bookclerk_plugin_sdk::DbCapabilities> {
        self.call(|reply| Work::DbCapabilities { reply }).await
    }

    /// Typed `AdapterDatabaseSession.bootstrap`.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest rejects the call or lacks `bootstrap`.
    pub async fn db_bootstrap(&self) -> Result<bookclerk_plugin_sdk::DbBootstrap> {
        self.call(|reply| Work::DbBootstrap { reply }).await
    }

    /// Typed `AdapterDatabaseSession.execute`.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest rejects the call or `cancel` is set.
    pub async fn db_execute_request(
        &self,
        request: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<bookclerk_plugin_sdk::ExecuteReply> {
        self.call(|reply| Work::DbExecuteRequest {
            request,
            cancel,
            reply,
        })
        .await
    }

    /// Typed `HostAdapterDatabaseSession.execute`.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the guest rejects the call or `cancel` is set.
    pub async fn db_execute_envelope_request(
        &self,
        envelope: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<bookclerk_plugin_sdk::ExecuteReply> {
        self.call(|reply| Work::DbExecuteEnvelopeRequest {
            envelope,
            cancel,
            reply,
        })
        .await
    }

    /// Typed `AdapterTransaction.execute` on the vat-held open transaction.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when no transaction is open, the guest rejects
    /// the call, or `cancel` is set.
    pub async fn db_txn_execute_request(
        &self,
        request: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<bookclerk_plugin_sdk::ExecuteReply> {
        self.call(|reply| Work::DbTxnExecuteRequest {
            request,
            cancel,
            reply,
        })
        .await
    }

    /// Begin a vat-held transaction.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when begin fails.
    pub async fn db_begin(&self, isolation: bookclerk_plugin_abi::IsolationReq) -> Result<()> {
        self.call(|reply| Work::DbBegin { isolation, reply }).await
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

    /// Begin a transaction on a named plugin database binding.
    ///
    /// # Errors
    ///
    /// Returns when the binding is not open or begin fails.
    pub async fn db_begin_binding(
        &self,
        name: &str,
        isolation: bookclerk_plugin_abi::IsolationReq,
    ) -> Result<()> {
        let name = name.to_string();
        self.call(|reply| Work::DbBeginBinding {
            name,
            isolation,
            reply,
        })
        .await
    }

    /// Commit a named plugin database binding transaction.
    ///
    /// # Errors
    ///
    /// Returns when no binding transaction is open or commit fails.
    pub async fn db_commit_binding(&self, name: &str) -> Result<()> {
        let name = name.to_string();
        self.call(|reply| Work::DbCommitBinding { name, reply })
            .await
    }

    /// Roll back a named plugin database binding transaction.
    ///
    /// # Errors
    ///
    /// Returns when no binding transaction is open or rollback fails.
    pub async fn db_rollback_binding(&self, name: &str) -> Result<()> {
        let name = name.to_string();
        self.call(|reply| Work::DbRollbackBinding { name, reply })
            .await
    }

    /// Typed execute on the vat-held binding transaction.
    ///
    /// # Errors
    ///
    /// Returns when no binding transaction is open or the guest rejects the call.
    pub async fn db_txn_execute_binding_request(
        &self,
        name: &str,
        request: bookclerk_plugin_abi::AdapterExecuteRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<bookclerk_plugin_sdk::ExecuteReply> {
        let name = name.to_string();
        self.call(|reply| Work::DbTxnExecuteBindingRequest {
            name,
            request,
            cancel,
            reply,
        })
        .await
    }

    /// Adapter backup primitive on the library or binding session (uses the open txn when present).
    async fn db_backup(&self, binding: Option<&str>, kind: BackupKind) -> Result<BackupOutcome> {
        let binding = binding.map(str::to_string);
        self.call(|reply| Work::DbBackup {
            binding,
            kind,
            reply,
        })
        .await
    }

    /// Identity high-water from the adapter (open txn if any).
    ///
    /// # Errors
    ///
    /// Returns when the session is closed or the guest rejects the call.
    pub async fn db_export_identity(
        &self,
        binding: Option<&str>,
    ) -> Result<Vec<bookclerk_plugin_abi::DbIdentityHighWater>> {
        match self.db_backup(binding, BackupKind::ExportIdentity).await? {
            BackupOutcome::Identity(rows) => Ok(rows),
            _ => Err(PluginError::message("unexpected backup reply")),
        }
    }

    /// Restore identity high-water into the adapter.
    ///
    /// # Errors
    ///
    /// Returns when the session is closed or the guest rejects the call.
    pub async fn db_import_identity(
        &self,
        binding: Option<&str>,
        rows: Vec<bookclerk_plugin_abi::DbIdentityHighWater>,
    ) -> Result<()> {
        match self
            .db_backup(binding, BackupKind::ImportIdentity(rows))
            .await?
        {
            BackupOutcome::Unit => Ok(()),
            _ => Err(PluginError::message("unexpected backup reply")),
        }
    }

    /// User-visible relation names from the adapter.
    ///
    /// # Errors
    ///
    /// Returns when the session is closed or the guest rejects the call.
    pub async fn db_list_user_relations(&self, binding: Option<&str>) -> Result<Vec<String>> {
        match self
            .db_backup(binding, BackupKind::ListUserRelations)
            .await?
        {
            BackupOutcome::Names(names) => Ok(names),
            _ => Err(PluginError::message("unexpected backup reply")),
        }
    }

    /// Prepare the open restore transaction.
    ///
    /// # Errors
    ///
    /// Returns when the session is closed or the guest rejects the call.
    pub async fn db_prepare_unit_restore(&self, binding: Option<&str>) -> Result<()> {
        match self
            .db_backup(binding, BackupKind::PrepareUnitRestore)
            .await?
        {
            BackupOutcome::Unit => Ok(()),
            _ => Err(PluginError::message("unexpected backup reply")),
        }
    }

    /// Drop named user relations.
    ///
    /// # Errors
    ///
    /// Returns when the session is closed or the guest rejects the call.
    pub async fn db_drop_user_relations(
        &self,
        binding: Option<&str>,
        names: Vec<String>,
    ) -> Result<()> {
        match self
            .db_backup(binding, BackupKind::DropUserRelations(names))
            .await?
        {
            BackupOutcome::Unit => Ok(()),
            _ => Err(PluginError::message("unexpected backup reply")),
        }
    }

    /// Fail closed when restore FK checks still fail.
    ///
    /// # Errors
    ///
    /// Returns when the session is closed or the guest rejects the call.
    pub async fn db_assert_restore_constraints(&self, binding: Option<&str>) -> Result<()> {
        match self
            .db_backup(binding, BackupKind::AssertRestoreConstraints)
            .await?
        {
            BackupOutcome::Unit => Ok(()),
            _ => Err(PluginError::message("unexpected backup reply")),
        }
    }
}

impl Drop for PluginSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Work::Shutdown);
    }
}

/// Maps ABI errors onto host [`PluginError`].
fn map_abi(err: bookclerk_plugin_sdk::PluginError) -> PluginError {
    let code = err.wire_str().to_string();
    PluginError::from_abi(Some(&code), err.message)
}

fn host_err_to_abi(err: PluginError) -> bookclerk_plugin_abi::PluginError {
    match err {
        PluginError::Abi { code, message } => {
            bookclerk_plugin_abi::PluginError::from_wire(&code, message)
        }
        other => bookclerk_plugin_abi::PluginError::internal(other.to_string()),
    }
}

async fn backup_on_session(
    session: &mut dyn bookclerk_plugin_sdk::AdapterDatabaseSession,
    kind: BackupKind,
) -> Result<BackupOutcome> {
    match kind {
        BackupKind::ExportIdentity => Ok(BackupOutcome::Identity(
            session.export_identity().await.map_err(map_abi)?,
        )),
        BackupKind::ImportIdentity(rows) => {
            session.import_identity(&rows).await.map_err(map_abi)?;
            Ok(BackupOutcome::Unit)
        }
        BackupKind::ListUserRelations => Ok(BackupOutcome::Names(
            session.list_user_relations().await.map_err(map_abi)?,
        )),
        BackupKind::PrepareUnitRestore => {
            session.prepare_unit_restore().await.map_err(map_abi)?;
            Ok(BackupOutcome::Unit)
        }
        BackupKind::DropUserRelations(names) => {
            session.drop_user_relations(&names).await.map_err(map_abi)?;
            Ok(BackupOutcome::Unit)
        }
        BackupKind::AssertRestoreConstraints => {
            session
                .assert_restore_constraints()
                .await
                .map_err(map_abi)?;
            Ok(BackupOutcome::Unit)
        }
    }
}

async fn backup_on_txn(
    txn: &mut dyn bookclerk_plugin_abi::AdapterTransaction,
    kind: BackupKind,
) -> Result<BackupOutcome> {
    match kind {
        BackupKind::ExportIdentity => Ok(BackupOutcome::Identity(
            txn.export_identity().await.map_err(map_abi)?,
        )),
        BackupKind::ImportIdentity(rows) => {
            txn.import_identity(&rows).await.map_err(map_abi)?;
            Ok(BackupOutcome::Unit)
        }
        BackupKind::ListUserRelations => Ok(BackupOutcome::Names(
            txn.list_user_relations().await.map_err(map_abi)?,
        )),
        BackupKind::PrepareUnitRestore => {
            txn.prepare_unit_restore().await.map_err(map_abi)?;
            Ok(BackupOutcome::Unit)
        }
        BackupKind::DropUserRelations(names) => {
            txn.drop_user_relations(&names).await.map_err(map_abi)?;
            Ok(BackupOutcome::Unit)
        }
        BackupKind::AssertRestoreConstraints => {
            txn.assert_restore_constraints().await.map_err(map_abi)?;
            Ok(BackupOutcome::Unit)
        }
    }
}

/// Send-safe backup primitives over a vat-held adapter session (or its open txn).
pub struct RpcBackupOps {
    session: Arc<PluginSession>,
    binding: Option<String>,
}

impl RpcBackupOps {
    /// Library adapter session (no named binding).
    #[must_use]
    pub fn library(session: Arc<PluginSession>) -> Self {
        Self {
            session,
            binding: None,
        }
    }

    /// Named plugin-database binding (`plugin_id/binding`).
    #[must_use]
    pub fn binding(session: Arc<PluginSession>, name: impl Into<String>) -> Self {
        Self {
            session,
            binding: Some(name.into()),
        }
    }

    /// Shared handle stored on backup/restore options.
    #[must_use]
    pub fn shared(self) -> bookclerk_plugin_abi::SharedAdapterBackupOps {
        Arc::new(self)
    }

    fn binding_ref(&self) -> Option<&str> {
        self.binding.as_deref()
    }
}

#[async_trait]
impl bookclerk_plugin_abi::AdapterBackupOps for RpcBackupOps {
    async fn export_identity(
        &self,
    ) -> bookclerk_plugin_abi::Result<Vec<bookclerk_plugin_abi::DbIdentityHighWater>> {
        self.session
            .db_export_identity(self.binding_ref())
            .await
            .map_err(host_err_to_abi)
    }

    async fn import_identity(
        &self,
        rows: &[bookclerk_plugin_abi::DbIdentityHighWater],
    ) -> bookclerk_plugin_abi::Result<()> {
        self.session
            .db_import_identity(self.binding_ref(), rows.to_vec())
            .await
            .map_err(host_err_to_abi)
    }

    async fn list_user_relations(&self) -> bookclerk_plugin_abi::Result<Vec<String>> {
        self.session
            .db_list_user_relations(self.binding_ref())
            .await
            .map_err(host_err_to_abi)
    }

    async fn prepare_unit_restore(&self) -> bookclerk_plugin_abi::Result<()> {
        self.session
            .db_prepare_unit_restore(self.binding_ref())
            .await
            .map_err(host_err_to_abi)
    }

    async fn drop_user_relations(&self, names: &[String]) -> bookclerk_plugin_abi::Result<()> {
        self.session
            .db_drop_user_relations(self.binding_ref(), names.to_vec())
            .await
            .map_err(host_err_to_abi)
    }

    async fn assert_restore_constraints(&self) -> bookclerk_plugin_abi::Result<()> {
        self.session
            .db_assert_restore_constraints(self.binding_ref())
            .await
            .map_err(host_err_to_abi)
    }
}

async fn wait_flag(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Validates `describe()` against the installed manifest and covering grant,
/// then negotiates RPC features and scalar limits.
///
/// The typed capability block must not widen `plugin.toml` (see
/// [`crate::validate_described_capabilities`]); a `storage` entrypoint also
/// requires the streams feature.
fn negotiate_describe(
    desc: &PluginDescribe,
    manifest: &PluginManifest,
    grant: &crate::PluginGrant,
) -> Result<(ScalarLimits, Vec<String>)> {
    let expected_id = manifest.id.as_str();
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
    crate::validate_described_capabilities(
        manifest,
        grant,
        &desc.capabilities,
        desc.portal_auth_mode,
    )?;
    let features = negotiate_rpc_features(
        &[FEATURE_SCALAR_LIMITS, FEATURE_STREAMS, FEATURE_STORAGE_COPY],
        &desc.rpc_features,
    )
    .map_err(map_abi)?;
    if desc.has_entrypoint(crate::Entrypoint::Storage)
        && !features.iter().any(|f| f == FEATURE_STREAMS)
    {
        return Err(PluginError::message(format!(
            "plugin `{expected_id}` entrypoint `storage` requires `{FEATURE_STREAMS}`"
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

/// Primary entrypoints opened for the session plus the values they were
/// opened with (re-open when values change).
struct PrimaryOpen {
    values: BindingValues,
    entrypoints: OpenedEntrypoints,
}

/// Vat-thread state for the session's primary `open` plus the host services
/// every `open` (primary or per-job) hands the guest as bindings.
struct PrimaryState {
    /// Current primary open, if any.
    open: Option<PrimaryOpen>,
    /// `EVENTS` outbox hook; `None` when no producer is declared and granted.
    events: Option<EventOutbox>,
}

impl PrimaryState {
    /// Fresh state before the first `open`.
    fn new(events: Option<EventOutbox>) -> Self {
        Self { open: None, events }
    }

    /// Binding values of the current primary open (empty before `open`).
    fn values(&self) -> BindingValues {
        self.open
            .as_ref()
            .map(|p| p.values.clone())
            .unwrap_or_default()
    }

    /// `EVENTS` publisher for `invocation`, when the outbox is granted.
    fn events_for(&self, invocation: &Invocation) -> Option<Arc<dyn EventPublisher>> {
        self.events
            .as_ref()
            .map(|outbox| Arc::new(outbox.publisher(invocation)) as Arc<dyn EventPublisher>)
    }
}

/// Host-issued invocation identity for one `PluginWorker.open`.
fn new_invocation(account_id: &str, id: impl Into<String>, deadline_unix_ms: u64) -> Invocation {
    Invocation {
        id: id.into(),
        account_id: account_id.to_string(),
        deadline_unix_ms,
        ..Invocation::default()
    }
}

/// Opens (or reuses) the primary entrypoints for `values`.
async fn primary_entrypoints<'a>(
    client: &PluginClient,
    account_id: &str,
    primary: &'a mut PrimaryState,
    values: Option<BindingValues>,
) -> Result<&'a OpenedEntrypoints> {
    let reuse = match (&primary.open, &values) {
        (Some(_), None) => true,
        (Some(open), Some(values)) => open.values == *values,
        (None, _) => false,
    };
    if !reuse {
        let want = values.unwrap_or_default();
        let invocation = new_invocation(account_id, uuid::Uuid::new_v4().to_string(), 0);
        let entrypoints = client
            .open(
                &invocation,
                HostBindings {
                    events: primary.events_for(&invocation),
                    ..HostBindings::from_values(want.clone())
                },
            )
            .await
            .map_err(map_abi)?;
        primary.open = Some(PrimaryOpen {
            values: want,
            entrypoints,
        });
    }
    Ok(&primary.open.as_ref().expect("primary is open").entrypoints)
}

/// Fails closed when the guest did not export `name`.
fn missing_entrypoint(name: &str) -> PluginError {
    PluginError::message(format!("plugin exported no `{name}` entrypoint"))
}

/// Dedicated `open` for a database adapter: returns the `databaseAdapter`
/// entrypoint or fails closed.
async fn open_database_adapter(
    client: &PluginClient,
    account_id: &str,
    values: BindingValues,
) -> Result<bookclerk_plugin_sdk::DatabaseClient> {
    let opened = client
        .open(
            &new_invocation(account_id, uuid::Uuid::new_v4().to_string(), 0),
            HostBindings::from_values(values),
        )
        .await
        .map_err(map_abi)?;
    opened
        .database_adapter
        .ok_or_else(|| missing_entrypoint("databaseAdapter"))
}

fn vat_thread(
    spawned: crate::spawn_stdio::SpawnedStdio,
    manifest: PluginManifest,
    account_id: String,
    events: Option<EventOutbox>,
    mut rx: mpsc::UnboundedReceiver<Work>,
    ready: oneshot::Sender<Result<(PluginDescribe, ScalarLimits, Vec<String>)>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            let _ = ready.send(Err(PluginError::message(format!("plugin runtime: {err}"))));
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
                    Ok(desc) => match negotiate_describe(&desc, &manifest, &spawned.grant) {
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
                let mut primary = PrimaryState::new(events);
                struct BindingOpen {
                    session: Box<dyn bookclerk_plugin_sdk::AdapterDatabaseSession>,
                    host: bookclerk_plugin_abi::HostAdapterDatabaseSessionClient,
                }
                let mut db_bindings: std::collections::HashMap<String, BindingOpen> =
                    std::collections::HashMap::new();
                let mut db_session: Option<Box<dyn bookclerk_plugin_sdk::AdapterDatabaseSession>> =
                    None;
                let mut db_host_session: Option<
                    bookclerk_plugin_abi::HostAdapterDatabaseSessionClient,
                > = None;
                let mut db_txn: Option<Box<dyn bookclerk_plugin_abi::AdapterTransaction>> = None;
                let mut db_binding_txns: std::collections::HashMap<
                    String,
                    Box<dyn bookclerk_plugin_abi::AdapterTransaction>,
                > = std::collections::HashMap::new();
                while let Some(work) = rx.recv().await {
                    match work {
                        Work::Shutdown => break,
                        Work::Describe { reply } => {
                            let _ = reply.send(client.describe().await.map_err(map_abi));
                        }
                        Work::Open { values, reply } => {
                            let out = primary_entrypoints(
                                &client,
                                &account_id,
                                &mut primary,
                                Some(values),
                            )
                            .await
                            .map(|_| ());
                            let _ = reply.send(out);
                        }
                        Work::Head { key, reply } => {
                            let out = match storage(&client, &account_id, &mut primary).await {
                                Ok(d) => d.head(&key).await.map_err(map_abi),
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::List { options, reply } => {
                            let out = match storage(&client, &account_id, &mut primary).await {
                                Ok(d) => d.list(options).await.map_err(map_abi),
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::GetStream { key, range, reply } => {
                            let out = match storage(&client, &account_id, &mut primary).await {
                                Ok(d) => d.get(&key, range).await.map_err(map_abi),
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::PutStream {
                            key,
                            body,
                            options,
                            reply,
                        } => {
                            let out = match storage(&client, &account_id, &mut primary).await {
                                Ok(d) => d.put(&key, body, options).await.map_err(map_abi),
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::Copy { from, to, reply } => {
                            let out = match storage(&client, &account_id, &mut primary).await {
                                Ok(d) => d
                                    .copy(&from, &to)
                                    .await
                                    .map(|r| r.bytes_copied)
                                    .map_err(map_abi),
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::Delete { key, reply } => {
                            let out = match storage(&client, &account_id, &mut primary).await {
                                Ok(d) => d.delete(&key).await.map_err(map_abi),
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::StreamCopy {
                            lease,
                            spec,
                            cancel,
                            progress,
                            databases,
                            reply,
                        } => {
                            let host_deadline = lease.deadline_unix_ms;
                            let job_cancel = Arc::clone(&cancel);
                            let dest = match storage(&client, &account_id, &mut primary).await {
                                Ok(d) => d.clone(),
                                Err(err) => {
                                    let _ = reply.send(Err(err));
                                    continue;
                                }
                            };
                            let values = primary.values();
                            let job_invocation = new_invocation(
                                &account_id,
                                lease.job_id.clone(),
                                lease.deadline_unix_ms,
                            );
                            let events = primary.events_for(&job_invocation);
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "fence lost"))
                                }
                                out = run_stream_copy(
                                    &client,
                                    job_invocation,
                                    values,
                                    events,
                                    dest,
                                    lease,
                                    spec,
                                    cancel,
                                    progress,
                                    databases
                                        .into_iter()
                                        .map(|(name, factory)| {
                                            (name, factory(Arc::clone(&job_cancel), host_deadline))
                                        })
                                        .collect(),
                                ) => out,
                            };
                            let _ = reply.send(out);
                        }
                        Work::Storefront { call } => {
                            let stub = primary_entrypoints(
                                &client,
                                &account_id,
                                &mut primary,
                                None,
                            )
                            .await
                            .and_then(|eps| {
                                eps.storefront
                                    .clone()
                                    .ok_or_else(|| missing_entrypoint("storefront"))
                            })
                            .map(|stub| {
                                Box::new(stub) as Box<dyn bookclerk_plugin_sdk::ContentSource>
                            })
                            .map_err(host_err_to_abi);
                            call(stub).await;
                        }
                        Work::RemoteLibrary { call, cancel } => {
                            let stub = primary_entrypoints(
                                &client,
                                &account_id,
                                &mut primary,
                                None,
                            )
                            .await
                            .and_then(|eps| {
                                eps.remote_library
                                    .clone()
                                    .ok_or_else(|| missing_entrypoint("remoteLibrary"))
                            })
                            .map(|stub| {
                                Box::new(stub) as Box<dyn bookclerk_plugin_sdk::RemoteLibrary>
                            })
                            .map_err(host_err_to_abi);
                            tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    // The captured reply sender drops with the
                                    // future, surfacing "fence lost" to the caller.
                                    tracing::debug!("remoteLibrary call aborted: fence lost");
                                }
                                () = call(stub) => {}
                            }
                        }
                        Work::DeliverEvents {
                            batch,
                            cancel,
                            reply,
                        } => {
                            let consumer = primary_entrypoints(
                                &client,
                                &account_id,
                                &mut primary,
                                None,
                            )
                            .await
                            .and_then(|eps| {
                                eps.event_consumer
                                    .clone()
                                    .ok_or_else(|| missing_entrypoint("eventConsumer"))
                            });
                            let out = match consumer {
                                Err(err) => Err(err),
                                Ok(consumer) => tokio::select! {
                                    () = wait_flag(Arc::clone(&cancel)) => {
                                        Err(PluginError::from_abi(Some("cancelled"), "fence lost"))
                                    }
                                    out = consumer.event(batch) => out.map_err(map_abi),
                                },
                            };
                            let _ = reply.send(out);
                        }
                        Work::CliDescribe { reply } => {
                            let out = match primary_entrypoints(
                                &client,
                                &account_id,
                                &mut primary,
                                None,
                            )
                            .await
                            {
                                Ok(eps) => match eps.cli.as_ref() {
                                    Some(cli) => cli.describe().await.map_err(map_abi),
                                    None => Ok(bookclerk_plugin_sdk::CliSchema::default()),
                                },
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::CliInvoke { params, reply } => {
                            let out = match primary_entrypoints(
                                &client,
                                &account_id,
                                &mut primary,
                                None,
                            )
                            .await
                            {
                                Ok(eps) => match eps.cli.as_ref() {
                                    Some(cli) => cli.invoke(params).await.map_err(map_abi),
                                    None => Err(missing_entrypoint("cli")),
                                },
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::OidcClients { reply } => {
                            let out = match primary_entrypoints(
                                &client,
                                &account_id,
                                &mut primary,
                                None,
                            )
                            .await
                            {
                                Ok(eps) => match eps.oidc.as_ref() {
                                    Some(oidc) => oidc.clients().await.map_err(map_abi),
                                    None => Ok(Vec::new()),
                                },
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::OidcAuthenticate { params, reply } => {
                            let out = match primary_entrypoints(
                                &client,
                                &account_id,
                                &mut primary,
                                None,
                            )
                            .await
                            {
                                Ok(eps) => match eps.oidc.as_ref() {
                                    Some(oidc) => {
                                        oidc.authenticate_user(params).await.map_err(map_abi)
                                    }
                                    None => Err(missing_entrypoint("oidc")),
                                },
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(out);
                        }
                        Work::DatabaseMigrations { binding, reply } => {
                            let _ = reply.send(
                                client
                                    .database_migrations(&binding)
                                    .await
                                    .map_err(map_abi),
                            );
                        }
                        Work::DbOpen { values, reply } => {
                            let out = async {
                                let db = open_database_adapter(&client, &account_id, values).await?;
                                let handle = db.open_session_handle().await.map_err(map_abi)?;
                                db_session = Some(handle.session);
                                db_host_session = Some(handle.host);
                                db_txn = None;
                                Ok(())
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbCapabilities { reply } => {
                            let out = async {
                                match db_session.as_mut() {
                                    Some(s) => s.capabilities().await.map_err(map_abi),
                                    None => Err(PluginError::message("database session not open")),
                                }
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbBootstrap { reply } => {
                            let out = async {
                                match db_session.as_mut() {
                                    Some(s) => s.bootstrap().await.map_err(map_abi),
                                    None => Err(PluginError::message("database session not open")),
                                }
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbBegin { isolation, reply } => {
                            let out = async {
                                let host = db_host_session.as_ref().ok_or_else(|| {
                                    PluginError::message("database session not open")
                                })?;
                                db_txn = Some(host.begin(isolation).await.map_err(map_abi)?);
                                Ok(())
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbCommit { reply } => {
                            let out = async {
                                let txn = db_txn.take().ok_or_else(|| {
                                    PluginError::message("database transaction not open")
                                })?;
                                txn.commit().await.map_err(map_abi)
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbRollback { reply } => {
                            let out = async {
                                let txn = db_txn.take().ok_or_else(|| {
                                    PluginError::message("database transaction not open")
                                })?;
                                txn.rollback().await.map_err(map_abi)
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbExecuteRequest {
                            request,
                            cancel,
                            reply,
                        } => {
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "rpc cancelled"))
                                }
                                out = async {
                                    match db_session.as_mut() {
                                        Some(s) => s.execute(request).await.map_err(map_abi),
                                        None => Err(PluginError::message(
                                            "database session not open",
                                        )),
                                    }
                                } => out,
                            };
                            let _ = reply.send(out);
                        }
                        Work::DbExecuteEnvelopeRequest {
                            envelope,
                            cancel,
                            reply,
                        } => {
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "rpc cancelled"))
                                }
                                out = async {
                                    let host = db_host_session.as_ref().ok_or_else(|| {
                                        PluginError::message("database session not open")
                                    })?;
                                    host.execute(envelope).await.map_err(map_abi)
                                } => out,
                            };
                            let _ = reply.send(out);
                        }
                        Work::DbTxnExecuteRequest {
                            request,
                            cancel,
                            reply,
                        } => {
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "rpc cancelled"))
                                }
                                out = async {
                                    match db_txn.as_mut() {
                                        Some(txn) => {
                                            txn.execute(request).await.map_err(map_abi)
                                        }
                                        None => Err(PluginError::message(
                                            "database transaction not open",
                                        )),
                                    }
                                } => out,
                            };
                            let _ = reply.send(out);
                        }
                        Work::DbOpenBinding {
                            name,
                            values,
                            reply,
                        } => {
                            let out = async {
                                let db = open_database_adapter(&client, &account_id, values).await?;
                                let handle = db.open_session_handle().await.map_err(map_abi)?;
                                let caps = handle.session.capabilities().await.map_err(map_abi)?;
                                if !caps.meets_host_minimums() {
                                    return Err(PluginError::message(
                                        caps.capability_failure_reason(),
                                    ));
                                }
                                db_bindings.insert(
                                    name,
                                    BindingOpen {
                                        session: handle.session,
                                        host: handle.host,
                                    },
                                );
                                Ok(caps)
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbExecuteBindingRequest {
                            name,
                            request,
                            cancel,
                            reply,
                        } => {
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "rpc cancelled"))
                                }
                                out = async {
                                    match db_bindings.get_mut(&name) {
                                        Some(s) => s.session.execute(request).await.map_err(map_abi),
                                        None => Err(PluginError::message(format!(
                                            "database binding `{name}` session not open",
                                        ))),
                                    }
                                } => out,
                            };
                            let _ = reply.send(out);
                        }
                        Work::DbExecuteBindingEnvelopeRequest {
                            name,
                            envelope,
                            cancel,
                            reply,
                        } => {
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "rpc cancelled"))
                                }
                                out = async {
                                    let host = db_bindings.get(&name).ok_or_else(|| {
                                        PluginError::message(format!(
                                            "database binding `{name}` session not open",
                                        ))
                                    })?;
                                    host.host.execute(envelope).await.map_err(map_abi)
                                } => out,
                            };
                            let _ = reply.send(out);
                        }
                        Work::DbBeginBinding { name, isolation, reply } => {
                            let out = async {
                                let host = db_bindings.get(&name).ok_or_else(|| {
                                    PluginError::message(format!(
                                        "database binding `{name}` session not open",
                                    ))
                                })?;
                                let txn = host.host.begin(isolation).await.map_err(map_abi)?;
                                db_binding_txns.insert(name, txn);
                                Ok(())
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbBackup {
                            binding,
                            kind,
                            reply,
                        } => {
                            let out = async {
                                match binding.as_deref() {
                                    None => {
                                        if let Some(txn) = db_txn.as_mut() {
                                            backup_on_txn(txn.as_mut(), kind).await
                                        } else if let Some(session) = db_session.as_mut() {
                                            backup_on_session(session.as_mut(), kind).await
                                        } else {
                                            Err(PluginError::message("database session not open"))
                                        }
                                    }
                                    Some(name) => {
                                        if let Some(txn) = db_binding_txns.get_mut(name) {
                                            backup_on_txn(txn.as_mut(), kind).await
                                        } else if let Some(open) = db_bindings.get_mut(name) {
                                            backup_on_session(open.session.as_mut(), kind).await
                                        } else {
                                            Err(PluginError::message(format!(
                                                "database binding `{name}` session not open",
                                            )))
                                        }
                                    }
                                }
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbCommitBinding { name, reply } => {
                            let out = async {
                                let txn = db_binding_txns.remove(&name).ok_or_else(|| {
                                    PluginError::message(format!(
                                        "database binding `{name}` transaction not open",
                                    ))
                                })?;
                                txn.commit().await.map_err(map_abi)
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbRollbackBinding { name, reply } => {
                            let out = async {
                                let txn = db_binding_txns.remove(&name).ok_or_else(|| {
                                    PluginError::message(format!(
                                        "database binding `{name}` transaction not open",
                                    ))
                                })?;
                                txn.rollback().await.map_err(map_abi)
                            }
                            .await;
                            let _ = reply.send(out);
                        }
                        Work::DbTxnExecuteBindingRequest {
                            name,
                            request,
                            cancel,
                            reply,
                        } => {
                            let out = tokio::select! {
                                () = wait_flag(Arc::clone(&cancel)) => {
                                    Err(PluginError::from_abi(Some("cancelled"), "rpc cancelled"))
                                }
                                out = async {
                                    match db_binding_txns.get_mut(&name) {
                                        Some(txn) => txn.execute(request).await.map_err(map_abi),
                                        None => Err(PluginError::message(format!(
                                            "database binding `{name}` transaction not open",
                                        ))),
                                    }
                                } => out,
                            };
                            let _ = reply.send(out);
                        }
                    }
                }
                drop(spawned.child);
            })
            .await;
    });
}

/// `storage` entrypoint of the primary open, failing closed when absent.
async fn storage<'a>(
    client: &PluginClient,
    account_id: &str,
    primary: &'a mut PrimaryState,
) -> Result<&'a bookclerk_plugin_sdk::DestinationClient> {
    primary_entrypoints(client, account_id, primary, None)
        .await?
        .storage
        .as_ref()
        .ok_or_else(|| missing_entrypoint("storage"))
}

/// One job: a dedicated `PluginWorker.open` carrying the job's named database
/// bindings and cancel, then `JobRunner.job` with host-served input / output.
#[allow(clippy::too_many_arguments)]
async fn run_stream_copy(
    client: &PluginClient,
    invocation: Invocation,
    values: BindingValues,
    events: Option<Arc<dyn EventPublisher>>,
    dest: bookclerk_plugin_sdk::DestinationClient,
    lease: JobInvocationLease,
    spec: StreamCopySpec,
    cancel: Arc<AtomicBool>,
    progress: Option<(bookclerk_library::LibraryStore, bookclerk_library::JobFence)>,
    databases: Vec<(String, Arc<dyn bookclerk_plugin_sdk::GuestDatabase>)>,
) -> Result<bookclerk_plugin_sdk::JobOutcome> {
    // Plugins never receive the host library on `Bindings`. Durable plugin
    // state uses consented named `[[databases]]` bindings on physically
    // separate units.
    let opened = client
        .open(
            &invocation,
            HostBindings {
                values,
                events,
                databases,
                cancel: Arc::new(FlagCancel(Arc::clone(&cancel))),
                storage: None,
            },
        )
        .await
        .map_err(map_abi)?;
    let runner = opened
        .job_runner
        .ok_or_else(|| missing_entrypoint("jobRunner"))?;
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
    let progress: Arc<dyn bookclerk_plugin_sdk::ProgressSink> = Arc::new(FencedProgress {
        cancel: Arc::clone(&cancel),
        library: progress,
    });
    let cancel: Arc<dyn Cancellation> = Arc::new(FlagCancel(cancel));
    runner
        .job(&invocation, input, output, progress, cancel)
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
    dest: bookclerk_plugin_sdk::DestinationClient,
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
    ) -> std::result::Result<bookclerk_plugin_sdk::ListPage, bookclerk_plugin_sdk::PluginError>
    {
        self.inner.list(options).await
    }

    async fn get(
        &self,
        key: &str,
        range: Option<bookclerk_plugin_sdk::ByteRange>,
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
impl bookclerk_plugin_sdk::ProgressSink for FencedProgress {
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

/// [`StorageBackend`] over a plugin destination capability (streams, fail-closed scalars).
#[derive(Clone)]
pub struct PluginStorage {
    /// Vat session.
    session: Arc<PluginSession>,
}

impl PluginStorage {
    /// Wraps a connected session after [`PluginSession::open`] granted the
    /// `storage` entrypoint.
    #[must_use]
    pub fn new(session: Arc<PluginSession>) -> Self {
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
impl StorageBackend for PluginStorage {
    fn name(&self) -> &'static str {
        "plugin"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> bookclerk_storage::Result<()> {
        if data.len() > MAX_SCALAR_BYTES as usize {
            return Err(StorageError::PayloadTooLarge(format!(
                "scalar put of {} bytes exceeds {MAX_SCALAR_BYTES} (use put_stream)",
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
                "scalar get of {} bytes exceeds {MAX_SCALAR_BYTES} (use get_stream)",
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
    use bookclerk_plugin_sdk::{
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

    fn manifest_with(entrypoint: &str) -> PluginManifest {
        PluginManifest::parse(&format!(
            r#"
api_version = 3
id = "local"
runtime = "native"
command = "./guest"
entrypoints = ["{entrypoint}"]

[capabilities.network]
mode = "deny"
"#
        ))
        .expect("manifest")
    }

    fn output_describe() -> PluginDescribe {
        PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "local".into(),
            capabilities: bookclerk_plugin_abi::PluginCapabilities {
                entrypoints: vec![crate::Entrypoint::Storage],
                ..Default::default()
            },
            display_name: None,
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into(), FEATURE_STREAMS.into()],
            scalar_limits: ScalarLimits::default().into(),
            ..PluginDescribe::default()
        }
    }

    #[test]
    fn account_bearing_families_reject_operator_isolate() {
        assert!(account_bearing_requires_non_operator(
            &manifest_with("storefront"),
            OPERATOR_ACCOUNT
        ));
        assert!(account_bearing_requires_non_operator(
            &manifest_with("remoteLibrary"),
            ""
        ));
        assert!(!account_bearing_requires_non_operator(
            &manifest_with("storefront"),
            "acct-a"
        ));
        assert!(!account_bearing_requires_non_operator(
            &manifest_with("storage"),
            OPERATOR_ACCOUNT
        ));
    }

    #[test]
    fn negotiate_rejects_id_mismatch_and_widened_entrypoints() {
        let manifest = manifest_with("storage");
        let grant = crate::consent_request(&manifest);
        let desc = PluginDescribe {
            id: "other".into(),
            ..output_describe()
        };
        let err = negotiate_describe(&desc, &manifest, &grant).unwrap_err();
        assert!(err.to_string().contains("id mismatch"));

        let desc = PluginDescribe {
            capabilities: bookclerk_plugin_abi::PluginCapabilities {
                entrypoints: vec![crate::Entrypoint::Storage, crate::Entrypoint::Storefront],
                ..Default::default()
            },
            ..output_describe()
        };
        let err = negotiate_describe(&desc, &manifest, &grant).unwrap_err();
        assert!(err.to_string().contains("storefront"), "{err}");

        let narrower_grant = crate::PluginGrant {
            entrypoints: Default::default(),
            ..grant.clone()
        };
        let err = negotiate_describe(&output_describe(), &manifest, &narrower_grant).unwrap_err();
        assert!(err.to_string().contains("grant lacks entrypoint"), "{err}");

        assert!(negotiate_describe(&output_describe(), &manifest, &grant).is_ok());
    }

    #[test]
    fn negotiate_rejects_missing_features_and_zero_limits() {
        let manifest = manifest_with("storage");
        let grant = crate::consent_request(&manifest);
        let desc = PluginDescribe {
            rpc_features: vec![FEATURE_STREAMS.into()],
            ..output_describe()
        };
        assert!(negotiate_describe(&desc, &manifest, &grant).is_err());

        let desc = PluginDescribe {
            scalar_limits: ScalarLimits {
                max_scalar_bytes: 0,
                max_stream_window_bytes: 1024,
                max_list_page: 10,
            }
            .into(),
            ..output_describe()
        };
        assert!(negotiate_describe(&desc, &manifest, &grant).is_err());
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
        ) -> std::result::Result<bookclerk_plugin_sdk::ListPage, bookclerk_plugin_sdk::PluginError>
        {
            Ok(bookclerk_plugin_sdk::ListPage::default())
        }

        async fn get(
            &self,
            key: &str,
            _range: Option<bookclerk_plugin_sdk::ByteRange>,
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
