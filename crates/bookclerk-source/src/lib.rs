//! Shared traits and DTOs for Bookclerk content-source plugins.
//!
//! # Audience
//!
//! Host registration code and first-party / third-party **source plugin**
//! authors implementing [`ContentSource`]. Guests that speak the JSON-RPC ABI
//! map onto these types via the plugin host; they do not depend on this crate
//! directly from workerd.
//!
//! Product narrative: `docs/plugins.md`. Style: `docs/code-documentation.md`.

mod brand;
mod error;
mod language;
mod media;
mod options;
mod registry;
mod traits;
mod types;

pub use brand::SourceBrand;
pub use error::{Result, SourceError};
pub use language::{default_preferred_language, language_rank, normalize_language};
pub use media::{
    audio_extension, extension_from_bytes, extension_from_content_type, extension_from_url,
};
pub use options::DownloadOptions;
pub use registry::SourceRegistry;
pub use traits::{revoke_credentials_default, ContentSource, PortalAuthMode};
pub use types::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, CatalogSearchSort, CatalogSortDir,
    ConfigOptionValue, ExpandSeed, FetchOptions, ImportCredentialsOptions, LoginOptions,
    OAuthProgress, PlainAudioPart, PlainFetch, PurchaseHintOpts, ScanOptions, ScanSummary,
    SourceAccount, SourceConfigOption, SourceFetch, SourcePurchaseHint,
};
