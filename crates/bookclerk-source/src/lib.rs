//! Host-side content-source traits plus the storefront types guests share.
//!
//! # Audience
//!
//! Host registration code and the plugin host adapter implementing
//! [`ContentSource`]. The types a storefront guest also needs (DTOs, ABI
//! conversions, listing / record helpers) live in the slim
//! [`bookclerk_storefront`] crate and are re-exported here unchanged, so host
//! code keeps importing them from `bookclerk_source`. Guests depend on
//! `bookclerk-storefront` directly and never link this crate, which pulls in
//! the library store and host configuration.
//!
//! Product narrative: `docs/plugins/README.md`. Style: `docs/contributing/code-documentation.md`.

pub mod abi;
mod fetch;
mod options;
mod registry;
mod traits;

pub use bookclerk_storefront::{
    audio_extension, default_preferred_language, extension_from_bytes, extension_from_content_type,
    extension_from_url, language_rank, listing, normalize_language, records, CatalogHit,
    CatalogSearchField, CatalogSearchOpts, CatalogSearchSort, CatalogSortDir, ConfigOptionValue,
    ExpandSeed, ImportCredentialsOptions, LoginOptions, OAuthProgress, PlainAudioPart, PlainFetch,
    PurchaseHintOpts, Result, ScanOptions, ScanSummary, SourceAccount, SourceBrand,
    SourceConfigOption, SourceError, SourceFetch, SourcePurchaseHint,
};
pub use fetch::FetchOptions;
pub use options::DownloadOptions;
pub use registry::SourceRegistry;
pub use traits::{revoke_credentials_default, source_scope, ContentSource, PortalAuthMode};
