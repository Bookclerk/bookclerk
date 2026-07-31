//! GraphicAudio content source: auth, library sync, and DRM-free download.
//!
//! Access paths (see `docs/source-candidates.md`), selected by
//! `[sources.graphicaudio] access` (default `web`):
//! 1. Browser Player (`/library`) — Magento session + CloudFront cookies
//! 2. Magento ZIP (`My Downloadable Products`) — opt-in (`access = "zip"`)
//! 3. Access App Retrofit API (`/access`) — opt-in device activation (`access = "device"`)

mod auth;
mod catalog;
mod client;
pub mod db;
mod download;
mod error;
pub mod guest;
mod http_util;
mod magento;
mod options;
mod source;
mod sync;

pub use auth::GraphicAudioAuthFile;
pub use catalog::{
    catalog_http_client, expand_from_product_id, expand_from_search, fetch_product_by_id,
    fetch_series_page, parse_catalog_grid, parse_related_products, search_catalog,
    MagentoCatalogProduct,
};
pub use client::{
    DownloadLinks, GraphicAudioClient, Product, DEFAULT_BASE_URL, LOGIN_PATH, PRODUCTS_PATH,
    REMOVE_PATH,
};
pub use db::{delete_auth_from_db, list_auth_from_db, load_auth_from_db, save_auth_to_db};
pub use download::{
    fetch_title_materials, fetch_title_with_mode, password_from_env, TitleFetchRequest,
    GA_ACCESS_ENV, GA_FETCH_ENV, GA_PASSWORD_ENV,
};
pub use error::{GraphicAudioError, Result};
pub use guest::{
    catalog_hit_to_dto, guest_fetch_title, guest_fetch_title_rpc, guest_login, guest_login_rpc,
    guest_scan, guest_scan_rpc, new_book_to_scan, plain_to_dto, purchase_hint_to_dto,
    resolve_access, resolve_access_base_url, resolve_bitrate, resolve_container,
    resolve_store_base_url,
};
pub use magento::{DownloadableProduct, LibraryItem, MagentoClient, DEFAULT_STORE_URL};
pub use options::{GraphicAudioAccess, GraphicAudioBitrate, GraphicAudioContainer};
pub use source::{from_config, register, GraphicAudioSource, ID as GRAPHICAUDIO_SOURCE_ID};
pub use sync::{
    collect_account_books, product_to_new_book, scan_library, ScanContext, ScanOptions,
};
