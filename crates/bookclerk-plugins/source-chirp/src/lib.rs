//! Chirp content source: GraphQL auth, library sync, and DRM-free download.
//!
//! Uses the reverse-engineered Android Mockingjay GraphQL API
//! (`https://api.chirpbooks.com/api/graphql`).

mod auth;
mod client;
pub mod db;
mod download;
mod error;
pub mod guest;
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
pub use guest::{
    catalog_hit_to_dto, guest_fetch_title, guest_fetch_title_rpc, guest_login, guest_login_rpc,
    guest_scan, guest_scan_rpc, new_book_to_scan, plain_to_dto, purchase_hint_to_dto,
    resolve_graphql_url,
};
pub use source::{from_config, register, ChirpSource, ID as CHIRP_SOURCE_ID, PASSWORD_ENV};
pub use sync::{
    audiobook_to_new_book, collect_account_books, scan_account_into_library, scan_library,
    ScanOptions,
};
