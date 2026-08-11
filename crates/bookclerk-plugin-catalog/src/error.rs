//! Catalog and install errors.

use thiserror::Error;

/// Failures from discovery, manifest validation, download, or installation.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// Operator-facing error text with no structured code.
    #[error("{0}")]
    Message(String),
    /// Filesystem I/O failure during download, extract, or activate.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON encode/decode failure (static index, receipt, …).
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// TOML decode failure for package manifests.
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    /// Invalid URL in a coordinate, artifact, or index.
    #[error(transparent)]
    Url(#[from] url::ParseError),
}

impl CatalogError {
    /// Builds a [`CatalogError::Message`] from any displayable string.
    ///
    /// # Arguments
    ///
    /// * `msg` - Operator-facing explanation; must not embed secrets.
    ///
    /// # Returns
    ///
    /// A message-only [`CatalogError`].
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }
}

/// Result alias for [`CatalogError`].
pub type Result<T> = std::result::Result<T, CatalogError>;
