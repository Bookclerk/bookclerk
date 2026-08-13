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
//! | Native guest (`runtime = "native"`) | [`BookclerkPlugin`] + [`BookclerkPluginGuest::serve`] |
//! | Raw stdio dispatch | [`PluginGuest`] |
//! | Fetch / upload FD side channels | [`fetch_work_dir`], [`upload_file_path`] |
//! | OAuth callback without guest listen | [`callback_tunnel`] |
//! | Workerd / Wasm guests | [`workerd`] + npm `@bookclerk/plugin-sdk` |
//! | ABI DTOs / method names | [`protocol`] (re-exports `bookclerk-plugin-abi`) |
//! | SeaORM ↔ wire helpers | feature `db` (crate-root re-exports) |
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
//! (handshake DTOs, [`PluginError`], FD helpers, tunnel types). Prefer
//! `use bookclerk_plugin_sdk::…` over reaching into submodules unless you need
//! module-level docs.

pub mod callback_tunnel;
#[cfg(feature = "db")]
mod db;
mod error;
mod fetch_dir;
mod guest;
mod pass_fd;
pub mod plugin;
pub mod protocol;
pub mod tools;
pub mod workerd;

#[cfg(feature = "db")]
pub use db::{
    atomic_status, b64_string_to_bytes, bytes_to_b64_string, exec_result_from_dto,
    exec_result_to_dto, json_to_sea_value, proxy_rows_from_dto, proxy_rows_to_dto,
    sea_value_to_json, statement_from_dto, statement_to_dto, DbAtomicParams, DbAtomicRequest,
    DbAtomicResult, DbAtomicTiming, DbBeginParams, DbBeginResult, DbConnectParams, DbConnectResult,
    DbTxnParams, ExecResultDto, ProxyRowDto, QueryResultDto, StatementDto,
};

pub use callback_tunnel::{TunnelGuest, TunnelHost, TunnelStream};
pub use error::{Result, SdkError};
pub use fetch_dir::{fetch_work_dir, upload_file_path, FetchWorkDir, UploadFile};
pub use guest::PluginGuest;
pub use pass_fd::{fd_proc_path, recv_passed_fd, PLUGIN_FD_CHANNEL, PLUGIN_FD_CHANNEL_ENV};
pub use plugin::{plugin_error_from_message, BookclerkPlugin, BookclerkPluginGuest};
pub use protocol::{
    methods, AuthenticateUserParams, BookAcquiredDto, BrandDto, CatalogDetailParams, CatalogHitDto,
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    ConfigOptionDto, ConfigOptionValueDto, CopyParams, CredentialsUpdateParams, EventPollResultDto,
    ExistsResultDto, ExpandCandidatesParams, ExternalUserDto, FetchTitleParams, GetParams,
    GetResultDto, HandshakeResult, HealthDto, HealthResult, KeyParams, ListAccountsParams,
    ListDealsParams, ListParams, ListeningProgressDto, LocalCopyParams, LocalGetParams,
    LocalKeyParams, LocalListParams, LocalPutFileParams, LocalPutParams, LocalTouchFileParams,
    LoginCompleteParams, LoginParams, LoginResultDto, LoginStartParams, LoginStartResultDto,
    ObjectInfoDto, ObjectMetaDto, ObjectProbeDto, OutputCopyParams, OutputGetParams,
    OutputKeyParams, OutputListParams, OutputLocalContextDto, OutputPutFileParams, OutputPutParams,
    OutputS3ContextDto, OutputTouchFileParams, PlainPartDto, PurchaseHintDto, PurchaseHintParams,
    PutFileParams, PutParams, S3CredentialsDto, ScanBookDto, ScanLibraryParams, ScanParams,
    ScanSummaryDto, SearchCatalogParams, SourceAccountDto, SourceFetchDto, SyncListeningResultDto,
    TouchFileParams, HOST_API_VERSION_MAX, HOST_API_VERSION_MIN, MAX_RPC_LINE_BYTES,
    PLUGIN_API_VERSION, PROTOCOL_NAME,
};

pub use bookclerk_plugin_abi::{
    DiagnoseResult, HandshakeParams, HostToPluginEvent, PluginError, PluginErrorCode,
    PluginToHostEvent, API_VERSION,
};
