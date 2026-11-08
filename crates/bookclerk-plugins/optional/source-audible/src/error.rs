//! Error types for the Audible storefront guest.

use thiserror::Error;

/// Result alias for Audible auth, sync, license, and download operations.
pub type Result<T> = std::result::Result<T, AudibleError>;

/// Failures from Audible login, library sync, licensing, or download/decrypt.
#[derive(Debug, Error)]
pub enum AudibleError {
    /// Login, token refresh, or authenticator load failed.
    #[error("authentication error: {0}")]
    Auth(String),

    /// The host passed no credentials, or none matching the requested accounts.
    #[error("no accounts configured: {0}")]
    NoAccounts(String),

    /// Library listing or metadata sync against Audible APIs failed.
    #[error("library sync error: {0}")]
    Sync(String),

    /// Content license request was rejected or returned an unexpected payload.
    #[error("license error: {0}")]
    License(String),

    /// Adrm licenserequest returned 000307 — title is Widevine-only (no aaxc).
    #[error(
        "{asin}: no downloadable aaxc asset (Audible serves this title via Widevine only): {message}"
    )]
    NoAaxcAsset {
        /// Title ASIN that has no AAXC asset.
        asin: String,
        /// Upstream error detail from Audible.
        message: String,
    },

    /// Widevine CDM load, provisioning, or CENC decrypt failed.
    #[error("Widevine error: {0}")]
    Widevine(String),

    /// HTTP download of audio/PDF/cover bytes failed.
    #[error("download error: {0}")]
    Download(String),

    /// Local filesystem I/O while reading or writing cache files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Unexpected failure wrapped from `anyhow`.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl AudibleError {
    /// Returns true when this error is [`Self::NoAaxcAsset`] (retry via Widevine).
    #[must_use]
    pub fn is_no_aaxc_asset(&self) -> bool {
        matches!(self, Self::NoAaxcAsset { .. })
    }
}

impl From<audible_rs::auth::login::LoginError> for AudibleError {
    fn from(err: audible_rs::auth::login::LoginError) -> Self {
        Self::Auth(err.to_string())
    }
}

impl From<audible_rs::auth::AuthError> for AudibleError {
    fn from(err: audible_rs::auth::AuthError) -> Self {
        Self::Auth(err.to_string())
    }
}

impl From<audible_rs::api::client::ApiError> for AudibleError {
    fn from(err: audible_rs::api::client::ApiError) -> Self {
        use audible_rs::api::client::ApiError;
        match err {
            ApiError::LicenseRejected {
                asin,
                error_code,
                message,
                ..
            } if error_code == "000307" => Self::NoAaxcAsset { asin, message },
            ApiError::LicenseRejected {
                asin,
                error_code,
                message,
                request_id,
                ..
            } => Self::License(format!(
                "{asin}: license rejected ({error_code}): {message} (request_id={request_id})"
            )),
            ApiError::LicenseDenied(message) => Self::License(format!("license denied: {message}")),
            other => Self::Sync(other.to_string()),
        }
    }
}
