use thiserror::Error;

pub type Result<T> = std::result::Result<T, LiberateError>;

#[derive(Debug, Error)]
pub enum LiberateError {
    #[error("audible error: {0}")]
    Audible(#[from] libation_audible::AudibleError),

    #[error("storage error: {0}")]
    Storage(#[from] libation_storage::StorageError),

    #[error("decrypt error: {0}")]
    Decrypt(#[from] libation_decrypt::DecryptError),

    #[error("library error: {0}")]
    Library(#[from] libation_library::LibraryError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
