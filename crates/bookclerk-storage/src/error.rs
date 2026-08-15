use thiserror::Error;

/// Result alias for storage backend operations in this crate.
pub type Result<T> = std::result::Result<T, StorageError>;

/// Errors from local / S3 / fan-out storage backends.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Object key was not found on get/probe/copy source.
    #[error("object not found: {0}")]
    NotFound(String),

    /// Key failed validation (path escape, empty bucket, bad destination config).
    #[error("invalid storage key: {0}")]
    InvalidKey(String),

    /// Local filesystem I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// S3 SDK or HTTP failure (operator-facing detail).
    #[error("S3 error: {0}")]
    S3(String),

    /// Scalar get/put exceeded the ABI v1 fail-closed size cap.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// List cursor is missing, stale, or not from this backend.
    #[error("invalid cursor: {0}")]
    InvalidCursor(String),

    /// Catch-all for wrapped backend failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
