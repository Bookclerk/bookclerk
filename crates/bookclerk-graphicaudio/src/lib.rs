//! GraphicAudio content source: auth, library sync, and DRM-free download.
//!
//! Access paths (see `docs/source-candidates.md`), selected by
//! `[sources.graphicaudio] access` (default `web`):
//! 1. Browser Player (`/library`) — Magento session + CloudFront cookies
//! 2. Magento ZIP (`My Downloadable Products`) — opt-in (`access = "zip"`)
//! 3. Access App Retrofit API (`/access`) — opt-in device activation (`access = "device"`)

mod auth;
mod client;
mod download;
mod error;
mod http_util;
mod magento;
mod options;
mod source;
mod sync;

pub use auth::{
    auth_file_for, auth_file_for_account, auth_stem, find_auth_file, list_auth_files, load_auth,
    save_auth, GraphicAudioAuthFile, AUTH_SUFFIX,
};
pub use client::{
    DownloadLinks, GraphicAudioClient, Product, DEFAULT_BASE_URL, LOGIN_PATH, PRODUCTS_PATH,
    REMOVE_PATH,
};
pub use download::{
    fetch_title_materials, fetch_title_with_mode, password_from_env, TitleFetchRequest,
    GA_ACCESS_ENV, GA_FETCH_ENV, GA_PASSWORD_ENV,
};
pub use error::{GraphicAudioError, Result};
pub use magento::{DownloadableProduct, LibraryItem, MagentoClient, DEFAULT_STORE_URL};
pub use options::{GraphicAudioAccess, GraphicAudioBitrate, GraphicAudioContainer};
pub use source::{from_config, register, GraphicAudioSource, ID as GRAPHICAUDIO_SOURCE_ID};
pub use sync::{product_to_new_book, scan_library, ScanContext, ScanOptions};
