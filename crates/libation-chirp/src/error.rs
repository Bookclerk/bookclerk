//! Error types for the Chirp content source.

use thiserror::Error;

/// Result alias for Chirp operations.
pub type Result<T> = std::result::Result<T, ChirpError>;

/// Errors from Chirp auth, sync, or download.
#[derive(Debug, Error)]
pub enum ChirpError {
    #[error("authentication error: {0}")]
    Auth(String),

    #[error("no accounts configured: {0}")]
    NoAccounts(String),

    #[error("account not found: {0}")]
    AccountNotFound(String),

    #[error("API error: {0}")]
    Api(String),

    #[error("download error: {0}")]
    Download(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Library(#[from] libation_library::LibraryError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ChirpError {
    #[must_use]
    pub fn auth(msg: impl Into<String>) -> Self {
        Self::Auth(msg.into())
    }

    #[must_use]
    pub fn no_accounts(msg: impl Into<String>) -> Self {
        Self::NoAccounts(msg.into())
    }

    #[must_use]
    pub fn api(msg: impl Into<String>) -> Self {
        Self::Api(msg.into())
    }

    #[must_use]
    pub fn download(msg: impl Into<String>) -> Self {
        Self::Download(msg.into())
    }
}

impl From<reqwest::Error> for ChirpError {
    fn from(err: reqwest::Error) -> Self {
        Self::Api(err.to_string())
    }
}

impl From<serde_json::Error> for ChirpError {
    fn from(err: serde_json::Error) -> Self {
        Self::Api(format!("JSON: {err}"))
    }
}

impl From<ChirpError> for libation_source::SourceError {
    fn from(err: ChirpError) -> Self {
        match err {
            ChirpError::NoAccounts(m) => Self::NoAccounts(m),
            ChirpError::Auth(m) | ChirpError::AccountNotFound(m) => Self::Auth(m),
            ChirpError::Api(m) | ChirpError::Download(m) => Self::Api(m),
            ChirpError::Io(e) => Self::Io(e),
            ChirpError::Library(e) => Self::Library(e),
            ChirpError::Other(e) => Self::Other(e),
        }
    }
}
