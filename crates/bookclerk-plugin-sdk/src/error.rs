//! Guest SDK errors.

use thiserror::Error;

/// Result alias for guest helpers.
pub type Result<T> = std::result::Result<T, SdkError>;

/// Errors from the guest stdio loop (not the plugin's business logic).
#[derive(Debug, Error)]
pub enum SdkError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
