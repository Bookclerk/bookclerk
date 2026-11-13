//! Guest-only Bookclerk plugin SDK.
//!
//! For third-party Rust plugin authors of native and workerd plugins
//! (`api_version = 3`). Depend on this crate from a standalone plugin
//! workspace, not on the host `bookclerk-plugin-host` crate. The Rust SDK
//! guide (`docs/plugins/sdk/rust/README.md`, published with this reference)
//! covers the author model, the socket proxy and the author CLI; the entry
//! points are:
//!
//! | Need | Entry point |
//! | --- | --- |
//! | Native guest (`runtime = "native"`) | [`PluginWorker`] / [`serve`] (`api_version = 3`) |
//! | Fetch / upload work paths | [`fetch_work_dir`], [`upload_file_path`] |
//! | Mediated TCP sockets | [`connect_socket`] / [`net`] (Workers `connect()` equivalent) |
//! | Native HTTPS through the socket proxy | feature `http` → `http::Client` |
//! | OAuth callback without guest listen | [`callback_tunnel`] |
//! | Workerd / Wasm guests | [`workerd`] + npm `@bookclerk/plugin-sdk` |
//! | ABI DTOs / method names | [`protocol`] (re-exports `bookclerk-plugin-abi`) |
//! | Database guest session / atomic execution | feature `db` → [`database_adapter`] |
//! | Author CLI (`check` / `fmt` / `sync-embed` / `package` / `smoke`) | separate crate `bookclerk-plugin-tools` (`cargo plugin`) |
//!
//! Crate-root `pub use` items are the stable import surface for guests
//! (describe-metadata DTOs, [`PluginError`], fetch path helpers, tunnel
//! types). Prefer `use bookclerk_plugin_sdk::…` over reaching into
//! submodules unless you need module-level docs.

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::unwrap_used
)]

pub mod callback_tunnel;
#[cfg(feature = "db")]
pub mod database_adapter;
#[cfg(feature = "db")]
mod db;
mod db_binding;
mod error;
mod fetch_dir;
#[cfg(feature = "http")]
pub mod http;
mod json;
mod manifest_caps;
pub mod mux;
pub mod net;
mod pass_fd;
pub mod protocol;
mod scan_page;
pub mod workerd;

pub use callback_tunnel::{TunnelGuest, TunnelHost, TunnelStream};
#[cfg(feature = "db")]
pub use db::{db_value_from_sea, proxy_rows_from_typed};
pub use db_binding::{
    chunk_indexes, execute_reply_to_d1_results, statement_result_to_d1_result,
    write_chunks_operation_id, ChunkCaps, D1ExecResult, D1Meta, D1Result, DatabaseBinding,
    DatabaseBindingOptions, PreparedStatement, RetryToken, RowStream, WriteChunksError,
    WriteProgress, MAX_WRITE_KEY_BYTES,
};
pub use error::{Result, SdkError};
pub use fetch_dir::{fetch_work_dir, upload_file_path, FetchWorkDir, UploadFile};
pub use json::{decode as decode_json, encode as encode_json, encode_atomic_result, page_rows};
pub use manifest_caps::{manifest_capabilities, manifest_media};
pub use net::{
    connect as connect_socket, nested_native_jail_requested, ConnectOptions, PluginSocket,
    SecureTransport, SocketAddress, GUEST_IPC_DIR_ENV, NESTED_NATIVE_JAIL_ENV,
    OWN_USER_AGENT_HOSTS_ENV, SESSION_CHALLENGE_ENV, SESSION_CHALLENGE_LEN,
    SOCKET_PROXY_ABSTRACT_PREFIX, SOCKET_PROXY_ENV, SOCKET_PROXY_WRITE_ENV,
    TEST_EXTRA_CANDIDATE_ENV, TEST_EXTRA_CANDIDATE_WRITE_ENV, TEST_EXTRA_ENDPOINT_ENV,
    TEST_EXTRA_ENDPOINT_WRITE_ENV, USER_AGENT_ENV,
};
pub use pass_fd::{fd_proc_path, recv_passed_fd, PLUGIN_FD_CHANNEL, PLUGIN_FD_CHANNEL_ENV};
pub use protocol::{
    methods, Abridgement, AccountCredential, AuthenticateUserParams, Brand, CatalogCapabilities,
    CatalogDetail, CatalogDetailParams, CatalogField, CatalogHit, CatalogHits, CatalogSort,
    ChapterMarker, CliArg, CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams,
    CliInvokeResult, CliSchema, ConfigOption, ConfigOptionValue, Credit, CreditLevel, CreditRole,
    EventPollResult, ExpandCandidatesParams, ExpressionKind, ExternalUser, FetchOptions,
    FetchTitleParams, FetchedFile, FetchedFileRole, Identifier, IdentifierScheme, ListDealsParams,
    ListeningProgress, LoginCompleteParams, LoginParams, LoginResult, LoginStartResult, MediaKind,
    MediaSupport, OutputLocalContextDto, OutputS3ContextDto, PlainFetch, PortalAuthMode,
    PurchaseHint, PurchaseHintParams, PurchaseHintResult, PurchaseHintsParams, PurchaseHintsResult,
    RevokeAccountParams, S3CredentialsDto, ScanBook, ScanLibraryParams, ScanPage, ScanParams,
    SearchCatalogParams, SeriesEntry, SourceAccount, SourceAccounts, SyncListeningResult, WorkKind,
    MAX_RPC_LINE_BYTES, PROTOCOL_NAME,
};
pub use scan_page::{scan_page, StorePage};

pub use bookclerk_plugin_abi::{
    byte_source_from_async_read, canonical_execute_request_hash, connect_plugin,
    database_adapter_config_from_bindings, decode_db_value_bytes, decode_execute_request_bytes,
    decode_execute_result_reply_bytes, encoded_db_value_bytes, encoded_execute_reply_bytes,
    encoded_execute_request_bytes, encoded_execute_result_reply_bytes,
    encoded_statement_result_bytes, negotiate_rpc_features, pull_byte_source_to_writer,
    require_plugin_migration_registration, serve_plugin, serve_plugin_stdio, sql_payload_bytes,
    sql_payload_exceeds, stream_copy_keys, AdapterBackupOps, AdapterDatabaseSession,
    AdapterExecuteRequest, BindingValues, Bindings, ByteRange, Cancellation, ContentSource,
    ContentSourceClient, CopyResult, CoverFetchParams, CoverFetcher, CoverFetcherClient,
    CoverImage, CoverImages, Database, DatabaseAdapterConfig, DatabaseClient, DbBootstrap,
    DbCapabilities, DbColumn, DbIdentityHighWater, DbPlanStatementKind, DbResultSelection, DbRow,
    DbTiming, DbType, DbValue, Destination, DestinationClient, DestinationServer, DiagnoseResult,
    DomainEvent, Entrypoints, EventConsumer, EventConsumerClient, EventPublisher,
    EventPublisherClient, EventReadPage, EventReadRequest, EventResult, ExecuteReply,
    ExecuteRequest, ExecuteStream, ExecuteStreamLimits, ExtensibleConfig, GuestDatabase, HealthOk,
    HostBindings, Invocation, IsolationReq, JobCheckpoint, JobController, JobInvocation,
    JobInvocationLease, JobOutcome, JobRunner, JobRunnerClient, KvDeleteOk, KvGetOk, KvListKey,
    KvListOk, KvListOptions, KvNamespace, KvNamespaceClient, KvPutOk, KvPutOptions, LimitExceeded,
    ListOptions, ListPage, MetadataChapter, MetadataChapters, MetadataChaptersParams,
    MetadataLookupParams, MetadataProvider, MetadataProviderClient, MetadataRecord,
    MetadataSearchParams, NamedKvBinding, NamedQueueBinding, NeverCancel, ObjectInfo,
    ObjectMetadata, Oidc, OidcClient, OidcClientTemplate, OpenedEntrypoints, PluginCli,
    PluginCliClient, PluginClient, PluginDescribe, PluginError, PluginErrorCode, PluginEvent,
    PluginMigration, PluginMigrationOp, PluginServer, PluginWorker, ProgressSink, PublishOk,
    PutResult, QueryPage, QueueBatch, QueueConsumer, QueueConsumerClient, QueueMessage,
    QueueProducer, QueueResult, QueueRetry, QueueSendMessage, QueueSendOk, ReadResult,
    RemoteLibrary, RemoteLibraryClient, ScalarLimits, Source, SourceClient, SourceServer,
    StatementResult, StreamCopyHandler, StreamCopySpec, TypedDbStatement, WriteOptions,
    FEATURE_SCALAR_LIMITS, FEATURE_STORAGE_COPY, FEATURE_STREAMS, MAX_EVENT_CURSOR_BYTES,
    MAX_KV_KEY_BYTES, MAX_KV_METADATA_BYTES, MAX_KV_VALUE_BYTES, MAX_LIST_PAGE,
    MAX_PLUGIN_MIGRATION_OPS, MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
    MAX_PLUGIN_MIGRATION_TOTAL_OPS, MAX_REFRESHED_CREDENTIAL_BYTES, MAX_SCALAR_BYTES,
    MAX_SCAN_CURSOR_BYTES, MAX_SCAN_PAGE_BOOKS, MAX_STREAM_WINDOW_BYTES, PRODUCT_API_VERSION,
    SQL_CONTRACT_VERSION,
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
