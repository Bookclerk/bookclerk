//! Bookclerk-facing audible API: auth, accounts, library sync, download options.
//!
//! Thin wrapper over [audible-rs](https://github.com/mkb79/audible-rs) so Bookclerk
//! keeps stable naming. All credentials live in the `encrypted_secrets` DB table.

mod accounts;
mod artifacts;
mod auth;
pub mod db;
mod download;
mod error;
mod options;
mod qr;
mod secret;
mod source;
mod sync;
mod throttle;
mod widevine;

pub use accounts::{
    import_auth_file, import_libation_accounts_json, import_mkb79_auth_json, AccountInfo,
    AccountStatus,
};
pub use artifacts::{
    download_companion_pdf, download_cover_jpeg, fetch_chapter_info, fetch_clips_bookmarks,
    fetch_product_metadata,
};
pub use audible_rs::models::content::DownloadLicense;
pub use auth::{begin_login, AuthLoginOptions, AuthSession, LoginMode, LoginProgress};
pub use bookclerk_source::{ScanOptions, ScanSummary};
pub use db::{
    delete_audible_account_from_db, list_audible_accounts_from_db, load_authenticator_from_db,
    load_widevine_cdm_from_db, save_authenticator_to_db, save_widevine_cdm_to_db,
};
pub use download::{
    download_licensed_audio, fetch_and_download, fetch_and_download_with_client,
    fetch_and_download_with_options, invalidate_account_client_cache, license_full_json,
    open_account_client, parse_license_json, request_content_license, summarize_license,
    AccountClient, DrmKind, EncryptedDownload, LicenseSummary,
};
pub use error::{AudibleError, Result};
pub use options::DownloadOptions;
pub use qr::{render_login_qr, QrRenderMode};
pub use secret::{resolve_auth_password, AUTH_PASSWORD_ENV};
pub use source::{from_config, register, AudibleSource, ID as AUDIBLE_SOURCE_ID};
pub use sync::{scan_account_into_library, scan_library};
pub use widevine::{
    effective_cdm_provider, ensure_widevine_cdm, load_widevine_cdm, WidevineCdm, WidevineDownload,
    DEFAULT_WIDEVINE_CDM_PROVIDER,
};
