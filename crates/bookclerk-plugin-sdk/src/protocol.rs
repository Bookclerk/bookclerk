//! Shared Workers RPC method names and payload types (`api_version = 1`).
//!
//! DTOs live in [`bookclerk_plugin_abi`]; this module re-exports them and keeps
//! legacy constants (`PLUGIN_API_VERSION`, `PROTOCOL_NAME`, …).
//!
//! # Trust boundary
//!
//! External plugins are **untrusted** relative to the host. The host must never
//! hand them `library.db`, `master.key`, or the Bookclerk files-dir root. Plugins
//! receive only a scoped `plugin_data_dir` / `cache_dir`, and credentials are
//! host-mediated (login returns a blob the host seals; scan and fetch receive
//! that blob from the host). Scan returns book DTOs for the host to upsert.

pub use bookclerk_plugin_abi::{
    AuthenticateUserParams, BookAcquiredDto, BrandDto, CatalogDetailParams, CatalogHitDto,
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    ConfigOptionDto, ConfigOptionValueDto, CopyParams, CredentialsUpdateParams, EventPollResultDto,
    ExistsResultDto, ExpandCandidatesParams, ExternalUserDto, FetchTitleParams, GetParams,
    GetResultDto, HandshakeParams, HandshakeResult, HealthDto, HealthResult, KeyParams,
    ListAccountsParams, ListDealsParams, ListParams, ListeningProgressDto, LocalCopyParams,
    LocalGetParams, LocalKeyParams, LocalListParams, LocalPutFileParams, LocalPutParams,
    LocalTouchFileParams, LoginCompleteParams, LoginParams, LoginResultDto, LoginStartParams,
    LoginStartResultDto, ObjectInfoDto, ObjectMetaDto, ObjectProbeDto, OutputCopyParams,
    OutputGetParams, OutputKeyParams, OutputListParams, OutputLocalContextDto, OutputPutFileParams,
    OutputPutParams, OutputS3ContextDto, OutputTouchFileParams, PlainPartDto, PurchaseHintDto,
    PurchaseHintParams, PutFileParams, PutParams, S3CredentialsDto, ScanBookDto, ScanLibraryParams,
    ScanParams, ScanSummaryDto, SearchCatalogParams, SourceAccountDto, SourceFetchDto,
    SyncListeningResultDto, TouchFileParams, API_VERSION,
};

/// Current host↔plugin API version (`api_version` in manifest + handshake).
pub const PLUGIN_API_VERSION: u32 = API_VERSION;

/// Logical ABI identifier (not a manifest field — use `api_version` only).
pub const PROTOCOL_NAME: &str = "workers-rpc";

/// Maximum length of one RPC request/response line (including newline).
pub const MAX_RPC_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Oldest host API version a guest may speak.
pub const HOST_API_VERSION_MIN: u32 = 1;

/// Newest host API version a guest may speak.
pub const HOST_API_VERSION_MAX: u32 = 1;

/// Method names (Workers RPC camelCase).
pub mod methods {
    pub use bookclerk_plugin_abi::methods::names::*;
}
