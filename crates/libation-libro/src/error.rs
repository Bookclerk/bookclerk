//! Error types for the Libro.fm content source.

use thiserror::Error;

/// Result alias for Libro.fm operations.
pub type Result<T> = std::result::Result<T, LibroError>;

/// Errors from Libro.fm auth, sync, or download.
#[derive(Debug, Error)]
pub enum LibroError {
    #[error("authentication error: {0}")]
    Auth(String),

    /// No accounts configured for this source (safe to skip in multi-source scan).
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

impl LibroError {
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

impl From<reqwest::Error> for LibroError {
    fn from(err: reqwest::Error) -> Self {
        Self::Api(err.to_string())
    }
}

impl From<serde_json::Error> for LibroError {
    fn from(err: serde_json::Error) -> Self {
        Self::Api(format!("JSON: {err}"))
    }
}

impl From<LibroError> for libation_source::SourceError {
    fn from(err: LibroError) -> Self {
        match err {
            LibroError::NoAccounts(m) => Self::NoAccounts(m),
            LibroError::Auth(m) | LibroError::AccountNotFound(m) => Self::Auth(m),
            LibroError::Api(m) | LibroError::Download(m) => Self::Api(m),
            LibroError::Io(e) => Self::Io(e),
            LibroError::Library(e) => Self::Library(e),
            LibroError::Other(e) => Self::Other(e),
        }
    }
}
