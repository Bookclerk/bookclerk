//! Error types for the GraphicAudio content source.

use thiserror::Error;

/// Result alias for GraphicAudio auth, sync, and download operations.
pub type Result<T> = std::result::Result<T, GraphicAudioError>;

/// Failures from Magento / Access App auth, library listing, or download.
#[derive(Debug, Error)]
pub enum GraphicAudioError {
    /// Magento customer login or Access App device activation failed.
    #[error("authentication error: {0}")]
    Auth(String),

    /// No GraphicAudio accounts are configured (safe to skip in multi-source scan).
    #[error("no accounts configured: {0}")]
    NoAccounts(String),

    /// Requested account id is not present in `encrypted_secrets`.
    #[error("account not found: {0}")]
    AccountNotFound(String),

    /// Upstream Magento, Access App, or CloudFront API error.
    #[error("API error: {0}")]
    Api(String),

    /// Browser-player / ZIP / device download or packaging failed.
    #[error("download error: {0}")]
    Download(String),

    /// Local filesystem I/O while reading or writing cache files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Library database upsert / secret storage failure.
    #[error(transparent)]
    Library(#[from] bookclerk_library::LibraryError),

    /// Unexpected failure wrapped from `anyhow`.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl GraphicAudioError {
    /// Builds an [`Self::Auth`] error from a displayable message.
    #[must_use]
    pub fn auth(msg: impl Into<String>) -> Self {
        Self::Auth(msg.into())
    }

    /// Builds a [`Self::NoAccounts`] error (multi-source scan may skip this store).
    #[must_use]
    pub fn no_accounts(msg: impl Into<String>) -> Self {
        Self::NoAccounts(msg.into())
    }

    /// Builds an [`Self::Api`] error from a displayable message.
    #[must_use]
    pub fn api(msg: impl Into<String>) -> Self {
        Self::Api(msg.into())
    }

    /// Builds a [`Self::Download`] error from a displayable message.
    #[must_use]
    pub fn download(msg: impl Into<String>) -> Self {
        Self::Download(msg.into())
    }
}

impl From<reqwest::Error> for GraphicAudioError {
    fn from(err: reqwest::Error) -> Self {
        Self::Api(err.to_string())
    }
}

impl From<serde_json::Error> for GraphicAudioError {
    fn from(err: serde_json::Error) -> Self {
        Self::Api(format!("JSON: {err}"))
    }
}

impl From<GraphicAudioError> for bookclerk_source::SourceError {
    fn from(err: GraphicAudioError) -> Self {
        match err {
            GraphicAudioError::NoAccounts(m) => Self::NoAccounts(m),
            GraphicAudioError::Auth(m) | GraphicAudioError::AccountNotFound(m) => Self::Auth(m),
            GraphicAudioError::Api(m) | GraphicAudioError::Download(m) => Self::Api(m),
            GraphicAudioError::Io(e) => Self::Io(e),
            GraphicAudioError::Library(e) => Self::Library(e),
            GraphicAudioError::Other(e) => Self::Other(e),
        }
    }
}
