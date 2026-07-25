use thiserror::Error;

pub type Result<T> = std::result::Result<T, AcquireError>;

#[derive(Debug, Error)]
pub enum AcquireError {
    #[error("audible error: {0}")]
    Audible(#[from] bookclerk_audible::AudibleError),

    #[error("source error: {0}")]
    Source(#[from] bookclerk_source::SourceError),

    #[error("storage error: {0}")]
    Storage(#[from] bookclerk_storage::StorageError),

    #[error("decrypt error: {0}")]
    Decrypt(#[from] bookclerk_decrypt::DecryptError),

    #[error("library error: {0}")]
    Library(#[from] bookclerk_library::LibraryError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
