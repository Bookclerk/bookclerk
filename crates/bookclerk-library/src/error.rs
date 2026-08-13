//! Error types for library database and secret operations.

use thiserror::Error;

/// Result alias for [`LibraryError`].
pub type Result<T> = std::result::Result<T, LibraryError>;

/// Failures from library store, migrations, secrets, or config.
#[derive(Debug, Error)]
pub enum LibraryError {
    /// Low-level `rusqlite` failure (legacy paths still using the C API).
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// Schema migration failure from `rusqlite_migration`.
    #[error("migration error: {0}")]
    Migrate(#[from] rusqlite_migration::Error),

    /// SeaORM / database-plugin failure (`DbErr`).
    #[error("ORM / database plugin error: {0}")]
    Orm(#[from] sea_orm::DbErr),

    /// Configuration load or validation failure.
    #[error("config error: {0}")]
    Config(#[from] bookclerk_config::ConfigError),

    /// Filesystem or other I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Requested book (or similar library row) was not found.
    #[error("book not found: {0}")]
    NotFound(String),

    /// Demoting or disabling the last active administrator is refused.
    #[error("cannot demote or disable the last active administrator")]
    LastAdministrator,

    /// Demoting, disabling, or deleting the last active owner is refused.
    #[error("cannot demote or disable the last active owner")]
    LastOwner,

    /// Backend temporarily unreachable or an atomic RPC response was lost.
    ///
    /// Callers that still hold the original consume-once / session token should
    /// retry the same `dbAtomic` operation id rather than minting a new one.
    #[error("unavailable: {0}")]
    Unavailable(String),

    /// Catch-all for otherwise unclassified failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
