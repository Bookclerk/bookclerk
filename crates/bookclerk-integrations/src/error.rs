//! Integration errors.

use thiserror::Error;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, IntegrationError>;

/// Errors from outbound integrations and the connect portal.
#[derive(Debug, Error)]
pub enum IntegrationError {
    /// Remote HTTP API returned a non-success status.
    #[error("integration API error ({status}): {message}")]
    Api {
        /// HTTP status code from the remote API.
        status: u16,
        /// Response body or summarized error text.
        message: String,
    },

    /// Error propagated from [`bookclerk_library`].
    #[error(transparent)]
    Library(#[from] bookclerk_library::LibraryError),

    /// Error propagated from [`bookclerk_config`].
    #[error(transparent)]
    Config(#[from] bookclerk_config::ConfigError),

    /// Opaque error wrapped from `anyhow`.
    #[error("{0}")]
    Other(#[from] anyhow::Error),

    /// Operator-facing error text with no structured code.
    #[error("{0}")]
    Message(String),
}

impl IntegrationError {
    /// Builds an error from a human-readable message string.
    ///
    /// # Arguments
    ///
    /// * `msg` - Operator-facing error text.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }

    /// Builds an [`IntegrationError::Api`] from an HTTP status and response text.
    ///
    /// # Arguments
    ///
    /// * `status` - HTTP status code from the remote API.
    /// * `message` - Response body or summarized error text.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    #[must_use]
    pub fn api(status: u16, message: impl Into<String>) -> Self {
        Self::Api {
            status,
            message: message.into(),
        }
    }
}
