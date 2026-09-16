//! Guest-only Bookclerk plugin SDK.
//!
//! # Audience
//!
//! Third-party **Rust plugin authors**. Depend on this crate (path/git today;
//! crates.io later) — not the host `bookclerk-plugin-host` crate, which pulls
//! library/config and other Bookclerk internals.
//!
//! # What to use
//!
//! | Need | Entry point |
//! | --- | --- |
//! | Native guest (`runtime = "native"`) | [`PluginWorker`] / [`serve`] (`api_version = 3`) |
//! | Fetch / upload work paths | [`fetch_work_dir`], [`upload_file_path`] |
//! | OAuth callback without guest listen | [`callback_tunnel`] |
//! | Workerd / Wasm guests | [`workerd`] + npm `@bookclerk/plugin-sdk` |
//! | ABI DTOs / method names | [`protocol`] (re-exports `bookclerk-plugin-abi`) |
//! | Database guest session / atomic execution | feature `db` → [`database_adapter`] |
//! | Author CLI (`check` / `fmt` / `package` / `smoke`) | feature `tools` → [`tools`] |
//!
//! ```toml
//! # In a standalone plugin repo / workspace:
//! [dependencies]
//! bookclerk-plugin-sdk = { git = "https://github.com/fritz-fritz/bookclerk", package = "bookclerk-plugin-sdk" }
//! # Database guests also need:
//! # bookclerk-plugin-sdk = { …, features = ["db"] }
//! ```
//!
//! Authoring helpers (`check` / `fmt` / `package` / `smoke`) live behind feature
//! `tools` (pulls `bookclerk-workerd` for smoke only). Guest plugins should use
//! default features.
//!
//! ```bash
//! cargo run -p bookclerk-plugin-sdk --features tools --bin bookclerk-plugin -- check .
//! cargo plugin -- smoke .   # alias enables --features tools
//! ```
//!
//! # API documentation
//!
//! Generated rustdoc for this crate is part of the workspace API reference
//! (`./scripts/generate-api-docs.sh` → `docs/api/rust/`). Style guide:
//! `docs/code-documentation.md`. Product narrative: `docs/plugins.md`,
//! `docs/plugin-registry.md`.
//!
//! # Re-exports
//!
//! Crate-root `pub use` items are the stable import surface for guests
//! (describe-metadata DTOs, [`PluginError`], fetch path helpers, tunnel types). Prefer
//! `use bookclerk_plugin_sdk::…` over reaching into submodules unless you need
//! module-level docs.

pub mod callback_tunnel;
#[cfg(feature = "db")]
pub mod database_adapter;
#[cfg(feature = "db")]
mod db;
mod db_binding;
mod error;
mod fetch_dir;
mod json;
mod manifest_caps;
mod pass_fd;
pub mod protocol;
pub mod tools;
pub mod workerd;

pub use callback_tunnel::{TunnelGuest, TunnelHost, TunnelStream};
#[cfg(feature = "db")]
pub use db::{db_value_from_sea, proxy_rows_from_typed};
pub use db_binding::{
    execute_reply_to_d1_results, statement_result_to_d1_result, D1ExecResult, D1Meta, D1Result,
    DatabaseBinding, DatabaseBindingOptions, PreparedStatement, RetryToken,
};
pub use error::{Result, SdkError};
pub use fetch_dir::{fetch_work_dir, upload_file_path, FetchWorkDir, UploadFile};
pub use json::{decode as decode_json, encode as encode_json, encode_atomic_result, page_rows};
pub use manifest_caps::manifest_capabilities;
pub use pass_fd::{fd_proc_path, recv_passed_fd, PLUGIN_FD_CHANNEL, PLUGIN_FD_CHANNEL_ENV};
pub use protocol::{
    methods, Abridgement, AccountCredential, AuthenticateUserParams, Brand, CatalogDetail,
    CatalogDetailParams, CatalogField, CatalogHit, CatalogHits, CatalogSort, ChapterMarker, CliArg,
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    ConfigOption, ConfigOptionValue, EventPollResult, ExpandCandidatesParams, ExternalUser,
    FetchOptions, FetchTitleParams, ListDealsParams, ListeningProgress, LoginCompleteParams,
    LoginParams, LoginResult, LoginStartResult, OutputLocalContextDto, OutputS3ContextDto,
    PlainFetch, PlainPart, PortalAuthMode, PurchaseHint, PurchaseHintParams, PurchaseHintResult,
    S3CredentialsDto, ScanBook, ScanLibraryParams, ScanParams, ScanSummary, SearchCatalogParams,
    SourceAccount, SourceAccounts, SyncListeningResult, MAX_RPC_LINE_BYTES, PROTOCOL_NAME,
};

pub use bookclerk_plugin_abi::{
    byte_source_from_async_read, canonical_execute_request_hash, connect_plugin,
    database_adapter_config_from_bindings, decode_db_value_bytes, decode_execute_request_bytes,
    decode_execute_result_reply_bytes, encoded_db_value_bytes, encoded_execute_reply_bytes,
    encoded_execute_request_bytes, encoded_execute_result_reply_bytes,
    encoded_statement_result_bytes, negotiate_rpc_features, pull_byte_source_to_writer,
    require_plugin_migration_registration, serve_plugin, serve_plugin_stdio, sql_payload_bytes,
    sql_payload_exceeds, stream_copy_keys, AdapterBackupOps, AdapterDatabaseSession,
    AdapterExecuteRequest, BindingValues, Bindings, ByteRange, Cancellation, ContentSource,
    ContentSourceClient, CopyResult, Database, DatabaseAdapterConfig, DatabaseClient, DbBootstrap,
    DbCapabilities, DbColumn, DbIdentityHighWater, DbPlanStatementKind, DbResultSelection, DbRow,
    DbTiming, DbType, DbValue, Destination, DestinationClient, DestinationServer, DiagnoseResult,
    DomainEvent, Entrypoints, EventConsumer, EventConsumerClient, EventPublisher,
    EventPublisherClient, EventResult, ExecuteReply, ExecuteRequest, ExtensibleConfig,
    GuestDatabase, HealthOk, HostBindings, Invocation, IsolationReq, JobCheckpoint, JobController,
    JobInvocation, JobInvocationLease, JobOutcome, JobRunner, JobRunnerClient, ListOptions,
    ListPage, NeverCancel, ObjectInfo, ObjectMetadata, Oidc, OidcClient, OidcClientTemplate,
    OpenedEntrypoints, PluginCli, PluginCliClient, PluginClient, PluginDescribe, PluginError,
    PluginErrorCode, PluginEvent, PluginMigration, PluginMigrationOp, PluginServer, PluginWorker,
    ProgressSink, PublishOk, PutResult, QueryPage, ReadResult, RemoteLibrary, RemoteLibraryClient,
    ScalarLimits, Source, SourceClient, SourceServer, StatementResult, StreamCopyHandler,
    StreamCopySpec, TypedDbStatement, WriteOptions, FEATURE_SCALAR_LIMITS, FEATURE_STORAGE_COPY,
    FEATURE_STREAMS, MAX_LIST_PAGE, MAX_PLUGIN_MIGRATION_OPS,
    MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES, MAX_PLUGIN_MIGRATION_TOTAL_OPS, MAX_SCALAR_BYTES,
    MAX_STREAM_WINDOW_BYTES, PRODUCT_API_VERSION, SQL_CONTRACT_VERSION,
};

/// Serves a [`PluginWorker`] on stdin/stdout (Cap'n Proto RPC).
///
/// Must run on a current-thread tokio runtime inside a `LocalSet` (this helper
/// creates the `LocalSet`). Abort is capability drop / stream cancel.
///
/// # Errors
///
/// Returns [`SdkError`] when the vat fails.
pub async fn serve(plugin: impl PluginWorker) -> Result<()> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(serve_plugin_stdio(
            std::sync::Arc::new(plugin),
            MAX_STREAM_WINDOW_BYTES,
        ))
        .await
        .map_err(|err| SdkError::message(err.to_string()))
}
