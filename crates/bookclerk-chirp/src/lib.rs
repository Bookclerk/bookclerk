//! Chirp content source: GraphQL auth, library sync, and DRM-free download.
//!
//! Uses the reverse-engineered Android Mockingjay GraphQL API
//! (`https://api.chirpbooks.com/api/graphql`).

mod auth;
mod client;
pub mod db;
mod download;
mod error;
mod source;
mod sync;

pub use auth::ChirpAuthFile;
pub use client::{
    chirp_slug_candidates, Audiobook, AuthorCatalog, CatalogAudiobook, CatalogAuthor,
    CatalogSeries, ChirpClient, ChirpProductPricing, LibraryItem, RelatedCatalog, SeriesCatalog,
    Track, TypeaheadCatalog, DEFAULT_GRAPHQL_URL,
};
pub use db::{delete_auth_from_db, list_auth_from_db, load_auth_from_db, save_auth_to_db};
pub use download::fetch_title_materials;
pub use error::{ChirpError, Result};
pub use source::{from_config, register, ChirpSource, ID as CHIRP_SOURCE_ID, PASSWORD_ENV};
pub use sync::{audiobook_to_new_book, scan_library, ScanOptions};
