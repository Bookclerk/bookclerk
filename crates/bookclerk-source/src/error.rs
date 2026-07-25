//! Error types for content sources.

use thiserror::Error;

/// Result alias for content-source operations.
pub type Result<T> = std::result::Result<T, SourceError>;

/// Errors from content-source auth, scan, or fetch.
#[derive(Debug, Error)]
pub enum SourceError {
    /// No accounts are configured for this source (safe to skip in multi-source scan).
    #[error("no accounts configured for source: {0}")]
    NoAccounts(String),
    #[error("{0}")]
    Auth(String),
    #[error("{0}")]
    Api(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Library(#[from] bookclerk_library::LibraryError),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl SourceError {
    #[must_use]
    pub fn no_accounts(msg: impl Into<String>) -> Self {
        Self::NoAccounts(msg.into())
    }

    #[must_use]
    pub fn auth(msg: impl Into<String>) -> Self {
        Self::Auth(msg.into())
    }

    #[must_use]
    pub fn api(msg: impl Into<String>) -> Self {
        Self::Api(msg.into())
    }
}
