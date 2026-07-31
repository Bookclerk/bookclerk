//! Multi-source content providers for Bookclerk (Audible, Libro.fm, …).

mod brand;
mod error;
mod media;
mod options;
mod registry;
mod traits;
mod types;

pub use brand::SourceBrand;
pub use error::{Result, SourceError};
pub use media::{
    audio_extension, extension_from_bytes, extension_from_content_type, extension_from_url,
};
pub use options::DownloadOptions;
pub use registry::SourceRegistry;
pub use traits::{revoke_credentials_default, ContentSource, PortalAuthMode};
pub use types::{
    CatalogHit, CatalogSearchOpts, ConfigOptionValue, ExpandSeed, FetchOptions,
    ImportCredentialsOptions, LoginOptions, OAuthProgress, PlainAudioPart, PlainFetch,
    PurchaseHintOpts, ScanOptions, ScanSummary, SourceAccount, SourceConfigOption, SourceFetch,
    SourcePurchaseHint,
};
