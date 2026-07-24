//! Multi-source content providers for Libation (Audible, Libro.fm, …).

mod auth_files;
mod error;
mod media;
mod options;
mod registry;
mod traits;
mod types;

pub use auth_files::{
    accounts_dir, auth_file_for, auth_file_for_account, auth_stem, ensure_accounts_dir,
    list_auth_files, sanitize_name, save_json_auth,
};
pub use error::{Result, SourceError};
pub use libation_config::GraphicAudioAccess;
pub use media::{
    audio_extension, extension_from_bytes, extension_from_content_type, extension_from_url,
};
pub use options::DownloadOptions;
pub use registry::SourceRegistry;
pub use traits::ContentSource;
pub use types::{
    EncryptedDrmKind, EncryptedFetch, FetchOptions, LoginOptions, PlainAudioPart, PlainFetch,
    ScanOptions, ScanSummary, SourceAccount, SourceFetch, SourceKind,
};
