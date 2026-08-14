//! Libro.fm content source: auth, library sync, and DRM-free download.
//!
//! Uses the unofficial mobile API documented by community clients (see
//! `client` module). Audible ASIN enrichment (metadata confidence matching)
//! lives in `bookclerk-enrich` / CLI — this crate must not depend on
//! `bookclerk-audible` or `bookclerk-enrich`. Public explore catalog helpers
//! live in [`catalog`] (reqwest, no enrich).

mod auth;
pub mod catalog;
mod client;
mod container;
pub mod db;
mod download;
mod error;
pub mod guest;
mod source;
mod sync;

pub use auth::LibroAuthFile;
pub use catalog::{
    catalog_detail, expand_candidates as catalog_expand_candidates,
    purchase_hint as catalog_purchase_hint, search_catalog as catalog_search,
};
pub use client::{
    Audiobook, DownloadManifest, DownloadPart, LibraryPage, LibroClient, ManifestFormat,
    ManifestTrack, PackagedM4b, TokenResponse, APP_VER, CLIENT_ID, DEFAULT_BASE_URL,
    DOWNLOAD_MANIFEST_PATH, LIBRARY_PATH, OAUTH_TOKEN_PATH, PACKAGED_M4B_PATH, USER_AGENT_VALUE,
};
pub use container::LibroContainer;
pub use db::{delete_auth_from_db, list_auth_from_db, load_auth_from_db, save_auth_to_db};
pub use download::{chapters_from_tracks, fetch_title_materials};
pub use error::{LibroError, Result};
pub use guest::{
    catalog_hit_to_dto, guest_fetch_title, guest_fetch_title_rpc, guest_login, guest_login_rpc,
    guest_scan, guest_scan_rpc, new_book_to_scan, plain_to_dto, purchase_hint_to_dto,
    resolve_base_url, resolve_container,
};
pub use source::{from_config, register, LibroSource, ID as LIBRO_SOURCE_ID, PASSWORD_ENV};
pub use sync::{
    audiobook_to_new_book, collect_account_books, scan_account_into_library, scan_library,
    ScanOptions,
};
