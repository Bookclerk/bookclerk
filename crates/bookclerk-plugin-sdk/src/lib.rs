//! Guest-only Bookclerk plugin SDK.
//!
//! Third-party Rust plugins depend on **this** crate (path/git today; crates.io
//! later) — not the host `bookclerk-plugin` crate, which pulls library/config
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

mod error;
mod guest;
pub mod protocol;

pub use error::{Result, SdkError};
pub use guest::PluginGuest;
pub use protocol::{
    methods, BookAcquiredDto, BrandDto, CatalogHitDto, CliArgKind, CliArgSpec, CliCommandSpec,
    CliInvokeParams, CliInvokeResult, CliSchema, ConfigOptionDto, ConfigOptionValueDto,
    CredentialsUpdateParams, EventPollResultDto, ExpandCandidatesParams, ExternalUserDto,
    FetchTitleParams, HandshakeResult, HealthDto, ListeningProgressDto, LoginCompleteParams,
    LoginParams, LoginResultDto, LoginStartResultDto, PlainPartDto, PurchaseHintDto,
    PurchaseHintParams, ScanBookDto, ScanParams, ScanSummaryDto, SearchCatalogParams,
    SourceAccountDto, SourceFetchDto, SyncListeningResultDto, PLUGIN_API_VERSION,
};
