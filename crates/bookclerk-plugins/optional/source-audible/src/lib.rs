//! Audible storefront guest: OAuth login, library scan, catalog lookups and
//! download + decrypt, over [audible-rs](https://github.com/mkb79/audible-rs).
//!
//! The binary serves these to the host as a jailed plugin guest. The host
//! seals the credentials and passes them in on each call; nothing here opens
//! the library database.

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::unwrap_used
)]

mod artifacts;
mod auth;
pub mod catalog;
mod download;
mod drm;
mod error;
mod guest;
mod options;
mod sync;
mod throttle;
mod widevine;

/// Plugin id of this storefront (`audible`), stamped on accounts it returns.
pub const ID: &str = "audible";

pub use artifacts::fetch_chapter_info;
pub use download::{request_content_license, summarize_license, LicenseSummary};
pub use error::{AudibleError, Result};
pub use guest::{
    deregister_device, guest_fetch_title, guest_login_complete, guest_login_start,
    guest_revoke_account, guest_scan_page, AudibleScanCursor,
};
pub use options::{Bitrate, DownloadOptions};
pub use sync::{collect_account_books, fetch_library_page, LibraryPageRequest, LibraryPageRows};
