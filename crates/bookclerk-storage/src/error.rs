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

    /// AWS/MinIO SDK or HTTP failure (operator-facing detail).
    #[error("S3 error: {0}")]
    S3(String),

    /// Catch-all for wrapped backend failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
