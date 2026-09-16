//! Author-facing async traits for the plugin ABI: the `PluginWorker` root,
//! its exported entrypoints, and the host-served bindings.

use std::pin::Pin;

use tokio::io::AsyncRead;

use crate::generated::{
    AuthenticateUserParams, CatalogDetailParams, CatalogHit, CliInvokeParams, CliInvokeResult,
    CliSchema, DatabaseAdapterConfig, ExpandCandidatesParams, ExternalUser, FetchTitleParams,
    Invocation, ListDealsParams, ListeningProgress, LoginCompleteParams, LoginParams, LoginResult,
    LoginStartResult, PlainFetch, PluginEvent, PublishOk, PurchaseHint, PurchaseHintParams,
    ScanLibraryParams, ScanParams, ScanSummary, SearchCatalogParams, SourceAccount,
};
use crate::rpc_types::{
    CopyResult, DomainEvent, EventResult, ExtensibleConfig, JobInvocation, JobOutcome, ListOptions,
    ListPage, ObjectMetadata, PluginDescribe, PutResult, WriteOptions,
};
use crate::{PluginError, Result};

/// Inclusive byte range for a streamed read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// Starting offset.
    pub offset: u64,
    /// Number of bytes; `None` means to end of object.
    pub length: Option<u64>,
}

/// Streamed read result. `body` ownership is transferred to the caller.
pub struct ReadResult {
    /// Object metadata (size, type, checksums).
    pub meta: ObjectMetadata,
    /// Byte stream; drop/cancel aborts the read.
    pub body: Pin<Box<dyn AsyncRead + Send>>,
}

/// Destination capability (storage).
///
/// Cap'n Proto stubs are `!Send`; call these traits from a `LocalSet`.
#[async_trait::async_trait(?Send)]
pub trait Destination {
    /// Metadata without a body; `Ok(None)` when the key is missing.
    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>>;

    /// One page of keys under `options.prefix`.
    async fn list(&self, options: ListOptions) -> Result<ListPage>;

    /// Streamed read. The body is a transferred stream, not a scalar.
    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<ReadResult>;

    /// Streamed write. `body` ownership is transferred to the destination.
    ///
    /// When [`WriteOptions::stage_only`] is true, bytes stream into
    /// destination-managed temporary/multipart storage and are not published
    /// until [`Self::commit`]. Hosts, adapters, brokers, and guests must not
    /// spool the complete object locally.
    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> Result<PutResult>;

    /// Server-side copy when the backend supports it.
    async fn copy(&self, from: &str, to: &str) -> Result<CopyResult>;

    /// Delete a key (no-op if missing).
    async fn delete(&self, key: &str) -> Result<()>;

    /// Finalize a destination-side staged object using `commit_token`.
    async fn commit(&self, _key: &str, _commit_token: &str) -> Result<PutResult> {
        Err(PluginError::unsupported("commit"))
    }

    /// Abort a destination-side staged object.
    async fn abort_stage(&self, _key: &str, _commit_token: &str) -> Result<()> {
        Err(PluginError::unsupported("abortStage"))
    }
}

/// Source capability that can open a named object as a stream.
#[async_trait::async_trait(?Send)]
pub trait Source {
    /// Opens `key` for streamed reading.
    async fn open(&self, key: &str) -> Result<ReadResult>;
}

/// Progress reports for a job invocation (never carries media).
#[async_trait::async_trait(?Send)]
pub trait ProgressSink {
    /// Reports `percent` in `0..=100` and an operator-facing `message`.
    async fn report(&self, percent: f32, message: &str) -> Result<()>;
}

/// Transport cancellation. SDKs project this into a locally created
/// `AbortSignal` (AbortSignal is not a serializable Workers RPC value).
#[async_trait::async_trait(?Send)]
pub trait Cancellation {
    /// Returns true when the host has cancelled this invocation.
    ///
    /// Transport or capability failures must surface as `Err`. Callers must
    /// fail closed (abort the invocation) rather than treating a failed poll as
    /// "not cancelled".
    async fn poll(&self) -> Result<bool>;
}

/// Everything one [`JobRunner::job`] invocation may touch.
///
/// Named database bindings are not per job: they arrive on
/// [`Bindings::databases`] at [`PluginWorker::open`].
pub struct JobController {
    /// Durable command envelope.
    pub invocation: JobInvocation,
    /// Input source capability.
    pub input: Box<dyn Source>,
    /// Output destination capability.
    pub output: Box<dyn Destination>,
    /// Progress sink (durable job row).
    pub progress: Box<dyn ProgressSink>,
    /// Cancellation capability (host fence / lease).
    pub cancel: Box<dyn Cancellation>,
}

/// `[triggers] jobs` handler ([`Entrypoints::job_runner`]).
#[async_trait::async_trait(?Send)]
pub trait JobRunner {
    /// Runs `controller.invocation` using the granted capabilities until
    /// completion, suspension, or cancellation.
    async fn job(&self, controller: JobController) -> Result<JobOutcome>;
}

/// `[[events.consumers]]` handler ([`Entrypoints::event_consumer`]).
///
/// Delivery is at-least-once; consume idempotently on
/// [`DomainEvent::deduplication_key`].
#[async_trait::async_trait(?Send)]
pub trait EventConsumer {
    /// Handles one ordered batch and returns exactly one [`EventResult`] per
    /// input event, in order. A short reply is a host-side error: the
    /// missing tail is redelivered.
    async fn event(&self, batch: Vec<DomainEvent>) -> Result<Vec<EventResult>>;
}

/// `EVENTS` binding: host-served outbox publisher ([`Bindings::events`]).
#[async_trait::async_trait(?Send)]
pub trait EventPublisher {
    /// Appends `event` to the host outbox. The host stamps `source` (this
    /// plugin id) and the invocation account; `event_type` must be listed in
    /// `[[events.producers]]`.
    async fn publish(&self, event: PluginEvent) -> Result<PublishOk>;
}

/// Storefront content source (not byte [`Source`]).
///
/// Every method takes and returns the typed Cap'n Proto payload structs from
/// [`crate::generated`]; absent methods return
/// [`PluginError::unsupported`].
#[async_trait::async_trait(?Send)]
pub trait ContentSource {
    /// Password or one-shot OAuth login. The host seals
    /// [`LoginResult::credentials`] into `encrypted_secrets`.
    async fn login(&self, _params: LoginParams) -> Result<LoginResult> {
        Err(PluginError::unsupported("login"))
    }

    /// Library scan; the host upserts [`ScanSummary::books`].
    async fn scan(&self, _params: ScanParams) -> Result<ScanSummary> {
        Err(PluginError::unsupported("scan"))
    }

    /// Fetch one title into `params.cache_dir` and return plain media paths.
    async fn fetch_title(&self, _params: FetchTitleParams) -> Result<PlainFetch> {
        Err(PluginError::unsupported("fetchTitle"))
    }

    /// Accounts the guest knows about.
    async fn list_accounts(&self) -> Result<Vec<SourceAccount>> {
        Err(PluginError::unsupported("listAccounts"))
    }

    /// Begin an interactive OAuth login.
    async fn login_start(&self, _params: LoginParams) -> Result<LoginStartResult> {
        Err(PluginError::unsupported("loginStart"))
    }

    /// Finish an interactive OAuth login started by [`Self::login_start`].
    async fn login_complete(&self, _params: LoginCompleteParams) -> Result<LoginResult> {
        Err(PluginError::unsupported("loginComplete"))
    }

    /// Free-text storefront catalog search.
    async fn search_catalog(&self, _params: SearchCatalogParams) -> Result<Vec<CatalogHit>> {
        Err(PluginError::unsupported("searchCatalog"))
    }

    /// Related-title expansion from a seed title.
    async fn expand_candidates(&self, _params: ExpandCandidatesParams) -> Result<Vec<CatalogHit>> {
        Err(PluginError::unsupported("expandCandidates"))
    }

    /// Purchase link / price hint; `Ok(None)` when the title is unknown.
    async fn purchase_hint(&self, _params: PurchaseHintParams) -> Result<Option<PurchaseHint>> {
        Err(PluginError::unsupported("purchaseHint"))
    }

    /// Current storefront deals.
    async fn list_deals(&self, _params: ListDealsParams) -> Result<Vec<CatalogHit>> {
        Err(PluginError::unsupported("listDeals"))
    }

    /// Full catalog record for one product; `Ok(None)` when unknown.
    async fn catalog_detail(&self, _params: CatalogDetailParams) -> Result<Option<CatalogHit>> {
        Err(PluginError::unsupported("catalogDetail"))
    }

    /// Storefront health.
    async fn health(&self) -> Result<crate::rpc_types::HealthOk> {
        Ok(crate::rpc_types::HealthOk {
            ok: true,
            detail: String::new(),
        })
    }

    /// Operator-facing diagnostic lines.
    async fn diagnose(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// `remoteLibrary` entrypoint: long-running remote-library lifecycle.
///
/// Event delivery is [`EventConsumer`]; credential verification is [`Oidc`].
#[async_trait::async_trait(?Send)]
pub trait RemoteLibrary {
    /// Liveness.
    async fn health(&self) -> Result<crate::rpc_types::HealthOk> {
        Ok(crate::rpc_types::HealthOk {
            ok: true,
            detail: String::new(),
        })
    }

    /// Start long-running work after the host has granted bindings.
    async fn start(&self) -> Result<()> {
        Ok(())
    }

    /// Stop long-running work.
    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    /// Operator-facing diagnostic lines.
    async fn diagnose(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Re-sync the remote library.
    async fn scan_library(&self, _params: ScanLibraryParams) -> Result<()> {
        Err(PluginError::unsupported("scanLibrary"))
    }

    /// Push / pull listening progress; the host upserts the rows.
    async fn sync_listening(&self) -> Result<Vec<ListeningProgress>> {
        Err(PluginError::unsupported("syncListening"))
    }

    /// Drain external users observed since the last poll.
    async fn poll_events(&self) -> Result<Vec<ExternalUser>> {
        Err(PluginError::unsupported("pollEvents"))
    }
}

/// `oidc` entrypoint: relying-party client templates and credential
/// verification on behalf of the host authorization server.
#[async_trait::async_trait(?Send)]
pub trait Oidc {
    /// Plugin-provided OIDC authorization-server client templates. Empty when
    /// the guest only verifies credentials.
    async fn clients(&self) -> Result<Vec<crate::rpc_types::OidcClientTemplate>> {
        Ok(Vec::new())
    }

    /// Verify remote credentials on behalf of the host.
    async fn authenticate_user(&self, _params: AuthenticateUserParams) -> Result<ExternalUser> {
        Err(PluginError::unsupported("authenticateUser"))
    }
}

/// `cli` entrypoint: guest commands under `bookclerk plugins <id> <command>`.
#[async_trait::async_trait(?Send)]
pub trait PluginCli {
    /// Declared CLI surface. Empty when the guest exposes no commands.
    async fn describe(&self) -> Result<CliSchema> {
        Ok(CliSchema::default())
    }

    /// Runs one plugin CLI command.
    async fn invoke(&self, _params: CliInvokeParams) -> Result<CliInvokeResult> {
        Err(PluginError::unsupported("invoke"))
    }
}

/// Database factory. Sessions cannot survive suspension.
#[async_trait::async_trait(?Send)]
pub trait Database {
    /// Opens an invocation-scoped adapter session.
    async fn open_session(&self) -> Result<Box<dyn AdapterDatabaseSession>>;

    /// Host-private interactive-transaction view of the adapter connection.
    ///
    /// First-party adapters override this. The default advertises no host
    /// machinery; hosts then fall back to the public typed `execute` plane.
    #[cfg(feature = "host")]
    fn host_session(&self) -> Option<Box<dyn crate::host_roles::HostAdapterDatabaseSession>> {
        None
    }
}

/// Host ↔ database adapter session (`capabilities` + typed `execute`).
#[async_trait::async_trait(?Send)]
pub trait AdapterDatabaseSession {
    /// Typed SQL-contract advertisement.
    async fn capabilities(&self) -> Result<crate::DbCapabilities>;

    /// Bootstrap-only SeaORM proxy metadata for the open session.
    async fn bootstrap(&self) -> Result<crate::DbBootstrap> {
        Err(crate::PluginError::unsupported(
            "AdapterDatabaseSession.bootstrap",
        ))
    }

    /// Typed atomic batch (`execute`). Canonical SQL plus required 1:1 proofs.
    async fn execute(
        &self,
        request: crate::host_envelope::AdapterExecuteRequest,
    ) -> Result<crate::ExecuteReply>;

    /// Close the session.
    async fn close(&self) -> Result<()> {
        Ok(())
    }

    /// Identity high-water from adapter catalogs.
    async fn export_identity(&self) -> Result<Vec<crate::DbIdentityHighWater>> {
        Err(crate::PluginError::unsupported(
            "AdapterDatabaseSession.exportIdentity",
        ))
    }

    /// Restore identity high-water into adapter catalogs.
    async fn import_identity(&self, _rows: &[crate::DbIdentityHighWater]) -> Result<()> {
        Err(crate::PluginError::unsupported(
            "AdapterDatabaseSession.importIdentity",
        ))
    }

    /// User-visible relation names (excludes engine catalogs).
    async fn list_user_relations(&self) -> Result<Vec<String>> {
        Err(crate::PluginError::unsupported(
            "AdapterDatabaseSession.listUserRelations",
        ))
    }

    /// Prepare an open restore transaction (deferred FK checks).
    async fn prepare_unit_restore(&self) -> Result<()> {
        Err(crate::PluginError::unsupported(
            "AdapterDatabaseSession.prepareUnitRestore",
        ))
    }

    /// Drop named user relations (adapter-owned CASCADE / identity companions).
    async fn drop_user_relations(&self, _names: &[String]) -> Result<()> {
        Err(crate::PluginError::unsupported(
            "AdapterDatabaseSession.dropUserRelations",
        ))
    }

    /// Fail closed when the restore transaction still has FK violations.
    async fn assert_restore_constraints(&self) -> Result<()> {
        Err(crate::PluginError::unsupported(
            "AdapterDatabaseSession.assertRestoreConstraints",
        ))
    }
}

/// Host-granted SQL transport for job plugin authors (no `capabilities`).
#[async_trait::async_trait(?Send)]
pub trait GuestDatabase {
    /// Host-mediated typed batch (`execute`).
    async fn execute(&self, request: crate::ExecuteRequest) -> Result<crate::ExecuteReply>;

    /// Close the grant.
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Plain-data portion of [`Bindings`]: every granted value that is not a
/// capability. Hosts build this off the vat thread and attach capabilities
/// when they call `open`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingValues {
    /// `CONFIG`: granted plugin settings as `application/json`.
    #[serde(default)]
    pub config: ExtensibleConfig,
    /// `SECRETS`: granted secret values as `application/json`; empty payload
    /// when the manifest declares no `[secrets]`.
    #[serde(default)]
    pub secrets: ExtensibleConfig,
    /// Database-adapter bootstrap for the `databaseAdapter` entrypoint.
    /// First-party host-managed adapters receive host-private connect params
    /// in [`Self::config`]; third-party adapters receive this typed bootstrap
    /// (and an empty `config`). `plugin_data_dir` is empty when `config`
    /// carries host-private params instead.
    #[serde(default)]
    pub adapter: DatabaseAdapterConfig,
}

impl BindingValues {
    /// Bindings carrying only `CONFIG`.
    #[must_use]
    pub fn config(config: ExtensibleConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }
}

/// Host-granted bindings for one [`PluginWorker::open`] (`env` in the
/// Workers idiom). Capability fields are `None` when the manifest does not
/// declare (or the operator did not grant) the binding.
pub struct Bindings {
    /// `CONFIG`: granted plugin settings as `application/json`.
    pub config: ExtensibleConfig,
    /// `SECRETS`: granted secret values; empty payload when none.
    pub secrets: ExtensibleConfig,
    /// Database-adapter bootstrap (see [`BindingValues::adapter`]).
    pub adapter: DatabaseAdapterConfig,
    /// `EVENTS`: outbox publisher; `None` unless `[[events.producers]]` is
    /// granted.
    pub events: Option<Box<dyn EventPublisher>>,
    /// Named plugin-owned `[[databases]]` bindings: isolated databases
    /// separate from the Bookclerk library and from every other plugin.
    pub databases: Vec<(String, Box<dyn GuestDatabase>)>,
    /// Host cancellation for the whole invocation (fence / lease loss).
    pub cancel: Box<dyn Cancellation>,
    /// `WORK_FS`: host-granted object storage for durable plugin files;
    /// `None` unless `[work_fs]` is granted. Job input/output travel on
    /// [`JobController`], never here.
    pub storage: Option<Box<dyn Destination>>,
}

impl Bindings {
    /// Bindings with only plain values (no capabilities); tests and
    /// in-process hosts.
    #[must_use]
    pub fn from_values(values: BindingValues) -> Self {
        Self {
            config: values.config,
            secrets: values.secrets,
            adapter: values.adapter,
            events: None,
            databases: Vec::new(),
            cancel: Box::new(NeverCancel),
            storage: None,
        }
    }

    /// Plain-data view (`config` / `secrets` / `adapter`).
    #[must_use]
    pub fn values(&self) -> BindingValues {
        BindingValues {
            config: self.config.clone(),
            secrets: self.secrets.clone(),
            adapter: self.adapter.clone(),
        }
    }

    /// Takes the named plugin database binding `name`, if granted.
    #[must_use]
    pub fn take_named_database(&mut self, name: &str) -> Option<Box<dyn GuestDatabase>> {
        let idx = self.databases.iter().position(|(n, _)| n == name)?;
        Some(self.databases.swap_remove(idx).1)
    }
}

/// Exported entrypoints returned by [`PluginWorker::open`]: one capability
/// per `plugin.toml` `entrypoints` entry / trigger, `None` when not exported.
///
/// The host refuses an entrypoint the manifest or operator grant did not
/// allow, so a guest cannot widen its surface here.
#[derive(Default)]
pub struct Entrypoints {
    /// `[[events.consumers]]` trigger.
    pub event_consumer: Option<Box<dyn EventConsumer>>,
    /// `[triggers] jobs` trigger.
    pub job_runner: Option<Box<dyn JobRunner>>,
    /// `storefront` entrypoint.
    pub storefront: Option<Box<dyn ContentSource>>,
    /// `storage` entrypoint.
    pub storage: Option<Box<dyn Destination>>,
    /// `databaseAdapter` entrypoint.
    pub database_adapter: Option<Box<dyn Database>>,
    /// `remoteLibrary` entrypoint.
    pub remote_library: Option<Box<dyn RemoteLibrary>>,
    /// `cli` entrypoint.
    pub cli: Option<Box<dyn PluginCli>>,
    /// `oidc` entrypoint.
    pub oidc: Option<Box<dyn Oidc>>,
}

/// Root `PluginWorker` capability (`describe` / `open` / `shutdown`).
///
/// `describe()` advertises typed capabilities; the signed manifest plus the
/// operator grant is the host allowlist. `open()` returns the exported
/// [`Entrypoints`] for one invocation.
#[async_trait::async_trait(?Send)]
pub trait PluginWorker: 'static {
    /// Advertises identity, features, and scalar limits.
    async fn describe(&self) -> Result<PluginDescribe>;

    /// Opens the exported entrypoints for `invocation` with the granted
    /// `bindings`.
    async fn open(&self, invocation: Invocation, bindings: Bindings) -> Result<Entrypoints>;

    /// Complete ordered plugin-owned migration sequence for one named binding.
    ///
    /// Called while the host provisions `[[databases]]`, before the binding
    /// session is handed out on [`Bindings::databases`]. Empty means the
    /// binding has no plugin-owned migrations. `id` values are opaque
    /// plugin-chosen identities; registration order is the forward sequence.
    async fn database_migrations(&self, _binding: &str) -> Result<Vec<crate::PluginMigration>> {
        Ok(Vec::new())
    }

    /// Releases guest resources.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// Cancellation that never fires (tests / hosts without a fence).
pub struct NeverCancel;

#[async_trait::async_trait(?Send)]
impl Cancellation for NeverCancel {
    async fn poll(&self) -> Result<bool> {
        Ok(false)
    }
}
