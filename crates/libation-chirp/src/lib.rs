//! Chirp content source: GraphQL auth, library sync, and DRM-free download.
//!
//! Uses the reverse-engineered Android Mockingjay GraphQL API
//! (`https://api.chirpbooks.com/api/graphql`).

mod auth;
mod client;
mod download;
mod error;
mod source;
mod sync;

pub use auth::{
    auth_file_for, auth_file_for_account, auth_stem, find_auth_file, list_auth_files, load_auth,
    save_auth, ChirpAuthFile,
};
pub use client::{Audiobook, ChirpClient, LibraryItem, Track, DEFAULT_GRAPHQL_URL};
pub use download::fetch_title_materials;
pub use error::{ChirpError, Result};
pub use source::ChirpSource;
pub use sync::{audiobook_to_new_book, scan_library, ScanOptions};
