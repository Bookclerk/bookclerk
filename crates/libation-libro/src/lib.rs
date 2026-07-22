//! Libro.fm content source: auth, library sync, and DRM-free download.
//!
//! Uses the unofficial mobile API documented by community clients (see
//! [`client`] module). Audible enrichment by ISBN (when present) lives in
//! `libation-audible` / CLI — this crate must not depend on `libation-audible`.

mod auth;
mod client;
mod download;
mod error;
mod source;
mod sync;

pub use auth::{
    auth_file_for, auth_file_for_account, auth_stem, find_auth_file, list_auth_files, load_auth,
    save_auth, LibroAuthFile,
};
pub use client::{
    Audiobook, DownloadManifest, DownloadPart, LibraryPage, LibroClient, ManifestFormat,
    ManifestTrack, PackagedM4b, TokenResponse, APP_VER, CLIENT_ID, DEFAULT_BASE_URL,
    DOWNLOAD_MANIFEST_PATH, LIBRARY_PATH, OAUTH_TOKEN_PATH, PACKAGED_M4B_PATH, USER_AGENT_VALUE,
};
pub use download::{chapters_from_tracks, fetch_title_materials};
pub use error::{LibroError, Result};
pub use source::LibroSource;
pub use sync::{audiobook_to_new_book, scan_account_into_library, scan_library, ScanOptions};
