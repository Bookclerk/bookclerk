//! Shared role method names and typed method payloads.
//!
//! Audience: guest authors who need the Cap'n Proto method parameter / result
//! structs (generated from `schema/plugin.capnp`) without depending on host
//! crates. Wire types live in `bookclerk_plugin_abi`; this module re-exports
//! them for a stable SDK import path.
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
    Abridgement, AccountCredential, AuthenticateUserParams, Brand, CatalogDetail,
    CatalogDetailParams, CatalogField, CatalogHit, CatalogHits, CatalogSort, ChapterMarker, CliArg,
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    ConfigOption, ConfigOptionValue, EventPollResult, ExpandCandidatesParams, ExternalUser,
    FetchOptions, FetchTitleParams, ListDealsParams, ListeningProgress, LoginCompleteParams,
    LoginParams, LoginResult, LoginStartResult, OutputLocalContextDto, OutputS3ContextDto,
    PlainFetch, PlainPart, PortalAuthMode, PurchaseHint, PurchaseHintParams, PurchaseHintResult,
    S3CredentialsDto, ScanBook, ScanLibraryParams, ScanParams, ScanSummary, SearchCatalogParams,
    SourceAccount, SourceAccounts, SyncListeningResult,
};

/// Logical ABI identifier for diagnostics (not a `plugin.toml` field).
///
/// Manifests advertise compatibility via `api_version` only.
pub const PROTOCOL_NAME: &str = "workers-rpc";

/// Maximum length of one framed bridge line in bytes (including newline).
///
/// Used by the workerd bridge framing. Currently 16 MiB.
pub const MAX_RPC_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Role capability method name constants (camelCase wire strings).
///
/// Re-exports `bookclerk_plugin_abi::methods::names` so guests can reference
/// capability names such as `methods::LOGIN` without depending on the ABI
/// crate path directly.
pub mod methods {
    pub use bookclerk_plugin_abi::methods::names::*;
}
