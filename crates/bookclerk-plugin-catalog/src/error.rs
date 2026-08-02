//! Catalog and install errors.

use thiserror::Error;

/// Errors from discovery, manifest validation, or installation.
#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
}

impl CatalogError {
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, CatalogError>;
