//! Author-facing async traits for plugin ABI role classes.

use std::pin::Pin;

use tokio::io::AsyncRead;

use crate::rpc_types::{
    CopyResult, DestinationContext, DomainEvent, EventResult, JobInvocation, JobOutcome,
    ListOptions, ListPage, ObjectMetadata, PluginDescribe, PutResult, SourceContext, WorkerContext,
    WriteOptions,
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

/// Granted stubs for one [`JobHandler::handle`] invocation.
pub struct JobHandlerContext {
    /// Input source capability.
    pub input: Box<dyn Source>,
    /// Output destination capability.
    pub output: Box<dyn Destination>,
    /// Progress sink (durable job row).
    pub progress: Box<dyn ProgressSink>,
    /// Always `None`. Jobs never receive the host library as guest SQL;
    /// durable plugin state uses [`Self::databases`].
    pub database: Option<Box<dyn GuestDatabase>>,
    /// Named plugin-owned database bindings: isolated
    /// databases from `plugin.toml` `capabilities.bindings.databases`,
    /// separate from the Bookclerk library and from every other plugin.
    pub databases: Vec<(String, Box<dyn GuestDatabase>)>,
    /// Cancellation capability (host fence / lease).
    pub cancel: Box<dyn Cancellation>,
}

impl JobHandlerContext {
    /// Takes the named plugin database binding `name`, if granted.
    #[must_use]
    pub fn take_named_database(&mut self, name: &str) -> Option<Box<dyn GuestDatabase>> {
        let idx = self.databases.iter().position(|(n, _)| n == name)?;
        Some(self.databases.swap_remove(idx).1)
    }
}

/// Plugin worker that handles one durable job invocation.
#[async_trait::async_trait(?Send)]
pub trait JobHandler {
    /// Runs `invocation` using granted capabilities until completion or cancel.
    async fn handle(
        &self,
        invocation: JobInvocation,
        context: JobHandlerContext,
    ) -> Result<JobOutcome>;
}

/// Storefront content source (not byte [`Source`]).
///
/// JSON arguments and results are a migration bridge for existing storefront
/// DTOs. New fields should use typed Cap'n Proto structs.
#[async_trait::async_trait(?Send)]
pub trait ContentSource {
    /// Interactive or password login.
    async fn login(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("login"))
    }

    /// Library scan.
    async fn scan(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("scan"))
    }

    /// Fetch one title into the download cache.
    async fn fetch_title(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("fetchTitle"))
    }

    /// List connected accounts.
    async fn list_accounts(&self) -> Result<String> {
        Err(PluginError::unsupported("listAccounts"))
    }

    /// Start an OAuth login.
    async fn login_start(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("loginStart"))
    }

    /// Complete an OAuth login.
    async fn login_complete(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("loginComplete"))
    }

    /// Search the storefront catalog.
    async fn search_catalog(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("searchCatalog"))
    }

    /// Expand a catalog hit into download candidates.
    async fn expand_candidates(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("expandCandidates"))
    }

    /// Purchase / ownership hint.
    async fn purchase_hint(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("purchaseHint"))
    }

    /// List current deals.
    async fn list_deals(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("listDeals"))
    }

    /// Catalog product detail.
    async fn catalog_detail(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("catalogDetail"))
    }

    /// Storefront health.
    async fn health(&self) -> Result<crate::rpc_types::HealthOk> {
        Ok(crate::rpc_types::HealthOk {
            ok: true,
            detail: String::new(),
        })
    }

    /// Operator-facing diagnostic lines (JSON array).
    async fn diagnose(&self) -> Result<String> {
        Ok("[]".into())
    }
}

/// Integration role (`onEvent` is not a generic job container).
#[async_trait::async_trait(?Send)]
pub trait Integration {
    /// Liveness.
    async fn health(&self) -> Result<crate::rpc_types::HealthOk> {
        Ok(crate::rpc_types::HealthOk {
            ok: true,
            detail: String::new(),
        })
    }

    /// Consume one domain event. Delivery is at-least-once; consume idempotently.
    async fn on_event(&self, _event: DomainEvent) -> Result<EventResult> {
        Err(PluginError::unsupported("onEvent"))
    }

    /// Start long-running integration work.
    async fn start(&self) -> Result<()> {
        Ok(())
    }

    /// Stop long-running integration work.
    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    /// Operator-facing diagnostic lines (JSON array).
    async fn diagnose(&self) -> Result<String> {
        Ok("[]".into())
    }

    /// Scan an external library.
    async fn scan_library(&self, _params_json: &str) -> Result<()> {
        Err(PluginError::unsupported("scanLibrary"))
    }

    /// Sync listening progress.
    async fn sync_listening(&self) -> Result<String> {
        Err(PluginError::unsupported("syncListening"))
    }

    /// Validate an external user.
    async fn authenticate_user(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("authenticateUser"))
    }

    /// Drain queued plugin-to-host events (JSON).
    async fn poll_events(&self) -> Result<String> {
        Err(PluginError::unsupported("pollEvents"))
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
/// Injected factory context for storefronts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentSourceContext {
    /// Opaque JSON knobs (migration bridge).
    pub json: String,
}

/// Injected factory context for integrations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrationContext {
    /// Opaque JSON knobs (migration bridge).
    pub json: String,
}

/// Injected factory context for databases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseContext {
    /// Opaque JSON knobs (migration bridge).
    pub json: String,
    /// Structured connect/config payload (preferred over [`Self::json`]).
    pub config: crate::ExtensibleConfig,
}

/// Root `BookclerkPlugin` capability (`describe` / role factories / shutdown).
///
/// Absent factories return typed [`PluginError::unsupported`]. `describe()`
/// advertises `supported_roles`; the signed manifest is the host allowlist.
#[async_trait::async_trait(?Send)]
pub trait PluginRoot: 'static {
    /// Advertises identity, features, and scalar limits.
    async fn describe(&self) -> Result<PluginDescribe>;

    /// Returns a destination capability for this invocation.
    async fn destination(&self, _context: DestinationContext) -> Result<Box<dyn Destination>> {
        Err(PluginError::unsupported("destination"))
    }

    /// Returns a source capability for this invocation.
    async fn source(&self, _context: SourceContext) -> Result<Box<dyn Source>> {
        Err(PluginError::unsupported("source"))
    }

    /// Returns a job handler for this invocation.
    async fn worker(&self, _context: WorkerContext) -> Result<Box<dyn JobHandler>> {
        Err(PluginError::unsupported("worker"))
    }

    /// Returns a storefront content-source capability.
    async fn content_source(
        &self,
        _context: ContentSourceContext,
    ) -> Result<Box<dyn ContentSource>> {
        Err(PluginError::unsupported("contentSource"))
    }

    /// Returns an integration capability.
    async fn integration(&self, _context: IntegrationContext) -> Result<Box<dyn Integration>> {
        Err(PluginError::unsupported("integration"))
    }

    /// Returns a database factory.
    async fn database(&self, _context: DatabaseContext) -> Result<Box<dyn Database>> {
        Err(PluginError::unsupported("database"))
    }

    /// Embedded CLI schema JSON (`CliSchema`). Empty object when unused.
    async fn cli_describe(&self) -> Result<String> {
        Ok("{}".into())
    }

    /// Invokes a guest CLI command. `params_json` is [`crate::CliInvokeParams`].
    async fn cli_invoke(&self, _params_json: &str) -> Result<String> {
        Err(PluginError::unsupported("cliInvoke"))
    }

    /// Plugin-provided OIDC authorization-server client templates.
    ///
    /// Empty when the guest is not a relying party. Hosts treat
    /// [`PluginError::unsupported`] from older guests as an empty list.
    async fn oidc_clients(&self) -> Result<Vec<crate::rpc_types::OidcClientTemplate>> {
        Ok(Vec::new())
    }

    /// Complete ordered plugin-owned migration sequence for one named binding.
    ///
    /// Called at binding initialization before ordinary execute. Empty means
    /// the binding has no plugin-owned migrations. `id` values are opaque
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
