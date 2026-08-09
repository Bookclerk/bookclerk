//! Guest-only Bookclerk plugin SDK.
//!
//! Third-party Rust plugins depend on **this** crate (path/git today; crates.io
//! later) — not the host `bookclerk-plugin-host` crate, which pulls library/config
//! and other Bookclerk internals.
//!
//! ```toml
//! # In a standalone plugin repo / workspace:
//! [dependencies]
//! bookclerk-plugin-sdk = { git = "https://github.com/fritz-fritz/bookclerk", package = "bookclerk-plugin-sdk" }
//! # or path = "../bookclerk/crates/bookclerk-plugin-sdk"
//! ```
//!
//! See `docs/plugin-registry.md`.

pub mod callback_tunnel;
mod db;
mod error;
mod fetch_dir;
mod guest;
mod pass_fd;
pub mod protocol;

pub use db::{
    b64_string_to_bytes, bytes_to_b64_string, exec_result_from_dto, exec_result_to_dto,
    json_to_sea_value, proxy_rows_from_dto, proxy_rows_to_dto, sea_value_to_json,
    statement_from_dto, statement_to_dto, DbConnectParams, DbConnectResult, ExecResultDto,
    ProxyRowDto, QueryResultDto, StatementDto,
};

pub use callback_tunnel::{TunnelGuest, TunnelHost, TunnelStream};
pub use error::{Result, SdkError};
pub use fetch_dir::{fetch_work_dir, upload_file_path, FetchWorkDir, UploadFile};
pub use guest::PluginGuest;
pub use pass_fd::{fd_proc_path, recv_passed_fd, PLUGIN_FD_CHANNEL, PLUGIN_FD_CHANNEL_ENV};
pub use protocol::{
    methods, BookAcquiredDto, BrandDto, CatalogHitDto, CliArgKind, CliArgSpec, CliCommandSpec,
    CliInvokeParams, CliInvokeResult, CliSchema, ConfigOptionDto, ConfigOptionValueDto, CopyParams,
    CredentialsUpdateParams, EventPollResultDto, ExpandCandidatesParams, ExternalUserDto,
    FetchTitleParams, GetParams, GetResultDto, HandshakeResult, HealthDto, KeyParams, ListParams,
    ListeningProgressDto, LocalCopyParams, LocalGetParams, LocalKeyParams, LocalListParams,
    LocalPutFileParams, LocalPutParams, LocalTouchFileParams, LoginCompleteParams, LoginParams,
    LoginResultDto, LoginStartResultDto, ObjectInfoDto, ObjectMetaDto, ObjectProbeDto,
    OutputLocalContextDto, OutputS3ContextDto, PlainPartDto, PurchaseHintDto, PurchaseHintParams,
    PutFileParams, PutParams, S3CredentialsDto, ScanBookDto, ScanParams, ScanSummaryDto,
    SearchCatalogParams, SourceAccountDto, SourceFetchDto, SyncListeningResultDto, TouchFileParams,
    HOST_API_VERSION_MAX, HOST_API_VERSION_MIN, MAX_RPC_LINE_BYTES, PLUGIN_API_VERSION,
    PROTOCOL_NAME,
};
