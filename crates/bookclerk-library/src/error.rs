use thiserror::Error;

pub type Result<T> = std::result::Result<T, LibraryError>;

#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] rusqlite_migration::Error),

    #[error("ORM / database plugin error: {0}")]
    Orm(#[from] sea_orm::DbErr),

    #[error("config error: {0}")]
    Config(#[from] bookclerk_config::ConfigError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("book not found: {0}")]
    NotFound(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
