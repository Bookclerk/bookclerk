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

    /// Audible refused a license because the account is downloading too
    /// much (it throttles for a day or two). The host pauses automatic
    /// downloads on the account instead of retrying into the throttle.
    #[error("Audible is throttling this account: {0}")]
    Throttled(String),

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
    /// The error for a refused license: [`Self::Throttled`] when Audible's
    /// reason says the account is being throttled, otherwise
    /// [`Self::License`] with `message`.
    ///
    /// # Arguments
    ///
    /// * `reason` - Audible's denial reason, as it sent it.
    /// * `message` - The full message for a plain refusal.
    #[must_use]
    pub fn license_refused(reason: &str, message: String) -> Self {
        if bookclerk_storefront::is_throttle_reason(reason) {
            Self::Throttled(message)
        } else {
            Self::License(message)
        }
    }

    /// True when this error says Audible is throttling the account.
    #[must_use]
    pub fn is_throttled(&self) -> bool {
        matches!(self, Self::Throttled(_))
    }

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
                status,
                error_code,
                message,
                request_id,
            } => {
                // An HTTP 429 is a throttle whatever the body says; the
                // status joins the words so the classifier sees it.
                let reason = format!("{} {error_code} {message}", status.as_u16());
                Self::license_refused(
                    &reason,
                    format!(
                        "{asin}: license rejected ({error_code}): {message} (request_id={request_id})"
                    ),
                )
            }
            ApiError::LicenseDenied(message) => {
                Self::license_refused(&message, format!("license denied: {message}"))
            }
            other => Self::Sync(other.to_string()),
        }
    }
}

#[cfg(test)]
mod throttle_tests {
    use super::AudibleError;
    use bookclerk_storefront::is_throttle_reason;

    #[test]
    fn throttle_words_are_recognised() {
        assert!(is_throttle_reason("Request throttled"));
        assert!(is_throttle_reason("Too Many Requests"));
        assert!(is_throttle_reason("HTTP 429"));
        assert!(is_throttle_reason("rate limit exceeded"));
        assert!(!is_throttle_reason("Not owned"));
        assert!(!is_throttle_reason(""));
    }

    #[test]
    fn a_throttled_refusal_is_its_own_error() {
        assert!(AudibleError::license_refused("throttled", "x".into()).is_throttled());
        assert!(!AudibleError::license_refused("not owned", "x".into()).is_throttled());
    }

    #[test]
    fn a_429_license_answer_is_a_throttle_whatever_its_text() {
        use audible_rs::api::client::ApiError;
        use bookclerk_plugin_sdk::http::StatusCode;
        let rejected = |status: StatusCode| ApiError::LicenseRejected {
            asin: "B000000000".into(),
            status,
            error_code: String::new(),
            message: String::new(),
            request_id: "-".into(),
        };
        assert!(AudibleError::from(rejected(StatusCode::TOO_MANY_REQUESTS)).is_throttled());
        assert!(!AudibleError::from(rejected(StatusCode::FORBIDDEN)).is_throttled());
    }
}
