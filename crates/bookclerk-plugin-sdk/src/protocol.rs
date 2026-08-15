//! Shared Workers RPC method names and payload types.
//!
//! Audience: guest authors who need ABI DTOs and version constants without
//! depending on host crates. Wire types live in `bookclerk_plugin_abi`; this
//! module re-exports them for a stable SDK import path and keeps legacy
//! constants ([`PLUGIN_API_VERSION`], [`PROTOCOL_NAME`], …).
//!
//! # Trust boundary
//!
//! External plugins are **untrusted** relative to the host. The host must never
//! hand them `library.db`, `master.key`, or the Bookclerk files-dir root. Plugins
//! receive only a scoped `plugin_data_dir` / `cache_dir`, and credentials are
//! host-mediated (login returns a blob the host seals; scan and fetch receive
//! that blob from the host). Scan returns book DTOs for the host to upsert.
//!
//! Prefer product docs under `docs/plugins.md` for jail / capability rules.

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

/// Internal handshake version for JSON DTOs still used on role methods.
///
/// Same numeric value as `bookclerk_plugin_abi::API_VERSION`. Product spawn uses
/// Cap'n Proto [`crate::v2::PRODUCT_API_VERSION`] (`2`).
pub const PLUGIN_API_VERSION: u32 = API_VERSION;

/// Logical ABI identifier for diagnostics (not a `plugin.toml` field).
///
/// Manifests advertise compatibility via `api_version` only.
pub const PROTOCOL_NAME: &str = "workers-rpc";

/// Maximum length of one RPC request/response line in bytes (including newline).
///
/// Used by remaining JSON DTO helpers and workerd bridge framing. Currently 16 MiB.
pub const MAX_RPC_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Highest `plugin.toml` `api_version` this host/SDK generation understands.
pub const HOST_MANIFEST_API_VERSION_MAX: u32 = 2;

/// Workers RPC method name constants (camelCase wire strings).
///
/// Re-exports `bookclerk_plugin_abi::methods::names` so guests can compare
/// `req.method` against `methods::handshake::NAME` without depending on the ABI
/// crate path directly.
pub mod methods {
    pub use bookclerk_plugin_abi::methods::names::*;
}
