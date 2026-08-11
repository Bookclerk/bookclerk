use thiserror::Error;

/// Result alias for [`AcquireError`].
pub type Result<T> = std::result::Result<T, AcquireError>;

/// Failures from the acquire / convert / reconcile pipeline.
#[derive(Debug, Error)]
pub enum AcquireError {
    /// Failure originating in a content-source plugin.
    #[error("source error: {0}")]
    Source(#[from] bookclerk_source::SourceError),

    /// Failure originating in an object-storage backend.
    #[error("storage error: {0}")]
    Storage(#[from] bookclerk_storage::StorageError),

    /// Failure originating in media encode/remux/fixup.
    #[error("media error: {0}")]
    Media(#[from] bookclerk_media::MediaError),

    /// Failure originating in the library database layer.
    #[error("library error: {0}")]
    Library(#[from] bookclerk_library::LibraryError),

    /// Filesystem or other I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Catch-all for otherwise unclassified failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
