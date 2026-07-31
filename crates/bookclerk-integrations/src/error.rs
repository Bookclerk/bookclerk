//! Integration errors.

use thiserror::Error;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, IntegrationError>;

/// Errors from outbound integrations and the connect portal.
#[derive(Debug, Error)]
pub enum IntegrationError {
    #[error("integration API error ({status}): {message}")]
    Api { status: u16, message: String },

    #[error(transparent)]
    Library(#[from] bookclerk_library::LibraryError),

    #[error(transparent)]
    Config(#[from] bookclerk_config::ConfigError),

    #[error("{0}")]
    Other(#[from] anyhow::Error),

    #[error("{0}")]
    Message(String),
}

impl IntegrationError {
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }

    #[must_use]
    pub fn api(status: u16, message: impl Into<String>) -> Self {
        Self::Api {
            status,
            message: message.into(),
        }
    }
}
