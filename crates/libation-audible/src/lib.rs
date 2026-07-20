//! Libation-facing audible API: auth, accounts, library sync, download options.
//!
//! Thin wrapper over [audible-rs](https://github.com/mkb79/audible-rs) so Libation
//! keeps stable naming and a `LIBATION_FILES_DIR`-rooted auth layout.

mod accounts;
mod artifacts;
mod auth;
mod download;
mod error;
mod options;
mod paths;
mod qr;
mod sync;
mod throttle;
mod widevine;

pub use accounts::{
    import_auth_file, import_libation_accounts_json, import_mkb79_auth_json, list_accounts,
    resolve_auth_file, resolve_auth_file_async, session_to_info, AccountInfo, AccountStatus,
};
pub use artifacts::{
    download_companion_pdf, download_cover_jpeg, fetch_chapter_info, fetch_clips_bookmarks,
    fetch_product_metadata,
};
pub use audible_rs::models::content::DownloadLicense;
pub use auth::{begin_login, AuthLoginOptions, AuthSession, LoginMode, LoginProgress};
pub use download::{
    download_licensed_audio, fetch_and_download, fetch_and_download_with_options,
    license_full_json, open_account_client, parse_license_json, request_content_license,
    summarize_license, AccountClient, DrmKind, EncryptedDownload, LicenseSummary,
};
pub use error::{AudibleError, Result};
pub use options::DownloadOptions;
pub use paths::{auth_dir, auth_file_for, list_auth_files};
pub use qr::{render_login_qr, QrRenderMode};
pub use sync::{scan_account_into_library, scan_library, ScanOptions, ScanSummary};
pub use widevine::{load_widevine_cdm, WidevineCdm, WidevineDownload};
