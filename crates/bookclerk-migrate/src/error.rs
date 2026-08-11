use thiserror::Error;

/// Result alias for this crate's error type.
pub type Result<T> = std::result::Result<T, MigrateError>;

/// Errors from Libation import / native backup export and import.
#[derive(Debug, Error)]
pub enum MigrateError {
    /// Failure while reading or interpreting the migration source tree.
    #[error("source classic Libation Files not found or incomplete: {0}")]
    Source(String),

    /// Failure while importing or exporting settings.
    #[error("settings import error: {0}")]
    Settings(String),

    /// Failure while importing or exporting accounts / credentials.
    #[error("accounts import error: {0}")]
    Accounts(String),

    /// Error propagated from [`bookclerk_library`].
    #[error("library database import error: {0}")]
    Library(String),

    /// Error propagated from [`bookclerk_config`].
    #[error("config error: {0}")]
    Config(#[from] bookclerk_config::ConfigError),

    /// Failure while writing into the destination library store.
    #[error("library store error: {0}")]
    Store(#[from] bookclerk_library::LibraryError),

    /// Underlying filesystem or stream I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// SQLite error while reading a classic Libation database.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Opaque error wrapped from `anyhow`.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
