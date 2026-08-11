//! Error types for content-source auth, scan, and fetch.
//!
//! # Audience
//!
//! Host job runners and [`crate::ContentSource`] implementors.

use thiserror::Error;

/// Result alias for content-source operations.
pub type Result<T> = std::result::Result<T, SourceError>;

/// Errors from content-source auth, scan, or fetch.
#[derive(Debug, Error)]
pub enum SourceError {
    /// No accounts are configured for this source (safe to skip in multi-source scan).
    #[error("no accounts configured for source: {0}")]
    NoAccounts(String),
    /// Authentication or credential failure (operator-facing message).
    #[error("{0}")]
    Auth(String),
    /// Upstream store HTTP / protocol failure (operator-facing message).
    #[error("{0}")]
    Api(String),
    /// Local filesystem failure while caching or writing downloads.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Library database / secret-store failure.
    #[error("{0}")]
    Library(#[from] bookclerk_library::LibraryError),
    /// Catch-all for plugin-internal failures wrapped as [`anyhow::Error`].
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl SourceError {
    /// Construct [`Self::NoAccounts`] with an operator-facing message.
    #[must_use]
    pub fn no_accounts(msg: impl Into<String>) -> Self {
        Self::NoAccounts(msg.into())
    }

    /// Construct [`Self::Auth`] with an operator-facing message.
    #[must_use]
    pub fn auth(msg: impl Into<String>) -> Self {
        Self::Auth(msg.into())
    }

    /// Construct [`Self::Api`] with an operator-facing message.
    #[must_use]
    pub fn api(msg: impl Into<String>) -> Self {
        Self::Api(msg.into())
    }
}
