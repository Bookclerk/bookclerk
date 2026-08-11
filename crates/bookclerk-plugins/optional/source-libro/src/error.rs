//! Error types for the Libro.fm content source.

use thiserror::Error;

/// Result alias for Libro.fm auth, sync, and download operations.
pub type Result<T> = std::result::Result<T, LibroError>;

/// Failures from Libro.fm OAuth, library listing, or DRM-free download.
#[derive(Debug, Error)]
pub enum LibroError {
    /// Password/token login or refresh failed.
    #[error("authentication error: {0}")]
    Auth(String),

    /// No Libro.fm accounts are configured (safe to skip in multi-source scan).
    #[error("no accounts configured: {0}")]
    NoAccounts(String),

    /// Requested account id is not present in `encrypted_secrets`.
    #[error("account not found: {0}")]
    AccountNotFound(String),

    /// Upstream mobile API or JSON payload error.
    #[error("API error: {0}")]
    Api(String),

    /// Title download or packaging into the preferred container failed.
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

impl LibroError {
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

impl From<LibroError> for bookclerk_source::SourceError {
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
