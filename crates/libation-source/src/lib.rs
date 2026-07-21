//! Multi-source content providers for Libation (Audible, Libro.fm, …).

mod error;
mod options;
mod registry;
mod traits;
mod types;

pub use error::{Result, SourceError};
pub use options::DownloadOptions;
pub use registry::SourceRegistry;
pub use traits::ContentSource;
pub use types::{
    EncryptedDrmKind, EncryptedFetch, FetchOptions, LoginOptions, PlainAudioPart, PlainFetch,
    ScanOptions, ScanSummary, SourceAccount, SourceFetch, SourceKind,
};
