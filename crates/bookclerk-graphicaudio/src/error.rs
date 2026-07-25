//! Error types for the GraphicAudio content source.

use thiserror::Error;

/// Result alias for GraphicAudio operations.
pub type Result<T> = std::result::Result<T, GraphicAudioError>;

/// Errors from GraphicAudio auth, sync, or download.
#[derive(Debug, Error)]
pub enum GraphicAudioError {
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
    Library(#[from] bookclerk_library::LibraryError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl GraphicAudioError {
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
