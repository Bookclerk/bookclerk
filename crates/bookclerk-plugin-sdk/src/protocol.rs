//! Shared role method names and JSON payload types.
//!
//! Audience: guest authors who need the JSON payload DTOs carried inside
//! `Text` fields of the Cap'n Proto ABI without depending on host crates.
//! Wire types live in `bookclerk_plugin_abi`; this module re-exports them
//! for a stable SDK import path.
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
    ConfigOptionDto, ConfigOptionValueDto, CopyParams, EventPollResultDto, ExistsResultDto,
    ExpandCandidatesParams, ExternalUserDto, FetchTitleParams, GetParams, GetResultDto, HealthDto,
    HealthResult, KeyParams, ListAccountsParams, ListDealsParams, ListParams, ListeningProgressDto,
    LocalCopyParams, LocalGetParams, LocalKeyParams, LocalListParams, LocalPutFileParams,
    LocalPutParams, LocalTouchFileParams, LoginCompleteParams, LoginParams, LoginResultDto,
    LoginStartParams, LoginStartResultDto, ObjectInfoDto, ObjectMetaDto, ObjectProbeDto,
    OutputCopyParams, OutputGetParams, OutputKeyParams, OutputListParams, OutputLocalContextDto,
    OutputPutFileParams, OutputPutParams, OutputS3ContextDto, OutputTouchFileParams, PlainPartDto,
    PluginMetadata, PurchaseHintDto, PurchaseHintParams, PutFileParams, PutParams,
    S3CredentialsDto, ScanBookDto, ScanLibraryParams, ScanParams, ScanSummaryDto,
    SearchCatalogParams, SourceAccountDto, SourceFetchDto, SyncListeningResultDto, TouchFileParams,
};

/// Logical ABI identifier for diagnostics (not a `plugin.toml` field).
///
/// Manifests advertise compatibility via `api_version` only.
pub const PROTOCOL_NAME: &str = "workers-rpc";

/// Maximum length of one JSON payload line in bytes (including newline).
///
/// Used by JSON payload helpers and workerd bridge framing. Currently 16 MiB.
pub const MAX_RPC_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Role capability method name constants (camelCase wire strings).
///
/// Re-exports `bookclerk_plugin_abi::methods::names` so guests can reference
/// capability names such as `methods::LOGIN` without depending on the ABI
/// crate path directly.
pub mod methods {
    pub use bookclerk_plugin_abi::methods::names::*;
}
