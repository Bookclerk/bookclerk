use thiserror::Error;

pub type Result<T> = std::result::Result<T, MigrateError>;

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error("source classic Libation Files not found or incomplete: {0}")]
    Source(String),

    #[error("settings import error: {0}")]
    Settings(String),

    #[error("accounts import error: {0}")]
    Accounts(String),

    #[error("library database import error: {0}")]
    Library(String),

    #[error("auth conversion error: {0}")]
    Auth(String),

    #[error("config error: {0}")]
    Config(#[from] bookclerk_config::ConfigError),

    #[error("library store error: {0}")]
    Store(#[from] bookclerk_library::LibraryError),

    #[error("audible error: {0}")]
    Audible(#[from] bookclerk_audible::AudibleError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
