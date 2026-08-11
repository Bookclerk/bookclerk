//! Shared Workers RPC method names and payload types (`api_version = 1`).
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

/// Current host↔plugin API version (`api_version` in manifest + handshake).
///
/// Same numeric value as `bookclerk_plugin_abi::API_VERSION`. Guests must reject
/// handshakes whose `apiVersion` differs (see [`crate::BookclerkPluginGuest`]).
pub const PLUGIN_API_VERSION: u32 = API_VERSION;

/// Logical ABI identifier for diagnostics (not a `plugin.toml` field).
///
/// Manifests advertise compatibility via `api_version` only; this string labels
/// the newline-delimited JSON Workers RPC framing used on stdio / workerd bridges.
pub const PROTOCOL_NAME: &str = "workers-rpc";

/// Maximum length of one RPC request/response line in bytes (including newline).
///
/// Stdio runners ([`crate::PluginGuest`], [`crate::BookclerkPluginGuest`]) reject
/// frames larger than this to bound memory. Currently 16 MiB.
pub const MAX_RPC_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Oldest host API version a guest built with this SDK may speak.
///
/// Inclusive lower bound checked conceptually against handshake `apiVersion`
/// (today both min and max are `1`).
pub const HOST_API_VERSION_MIN: u32 = 1;

/// Newest host API version a guest built with this SDK may speak.
///
/// Inclusive upper bound; bump when the ABI gains a backward-incompatible
/// change and guests need to advertise support.
pub const HOST_API_VERSION_MAX: u32 = 1;

/// Workers RPC method name constants (camelCase wire strings).
///
/// Re-exports `bookclerk_plugin_abi::methods::names` so guests can compare
/// `req.method` against `methods::handshake::NAME` without depending on the ABI
/// crate path directly.
pub mod methods {
    pub use bookclerk_plugin_abi::methods::names::*;
}
