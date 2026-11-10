//! Chirp content source: GraphQL auth, library sync, and DRM-free download.
//!
//! Uses the reverse-engineered Android Mockingjay GraphQL API
//! (`https://api.chirpbooks.com/api/graphql`).

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

mod auth;
mod client;
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
pub use download::fetch_title_materials;
pub use error::{ChirpError, Result};
pub use guest::{
    guest_fetch_title, guest_fetch_title_rpc, guest_login, guest_login_rpc, guest_scan_page,
    resolve_graphql_url,
};
pub use source::{ChirpSource, ID as CHIRP_SOURCE_ID};
pub use sync::{audiobook_to_scan_book, collect_account_books, fetch_account_page};
