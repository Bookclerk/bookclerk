//! Error types for library database and secret operations.

use thiserror::Error;

/// Result alias for [`LibraryError`].
pub type Result<T> = std::result::Result<T, LibraryError>;

/// Failures from library store, schema apply, secrets, or config.
#[derive(Debug, Error)]
pub enum LibraryError {
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

    /// Contact email failed library-backed syntax validation.
    #[error("invalid email address")]
    InvalidEmail,

    /// Backend temporarily unreachable or an atomic RPC response was lost.
    ///
    /// Callers that still hold the original consume-once / session token should
    /// retry the same atomic operation id rather than minting a new one.
    #[error("unavailable: {0}")]
    Unavailable(String),

    /// Identity or ownership conflict (for example a taken OIDC `client_id`).
    #[error("{0}")]
    Conflict(String),

    /// Catch-all for otherwise unclassified failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl LibraryError {
    /// Maps a SeaORM / adapter [`sea_orm::DbErr`] onto a typed library error.
    ///
    /// Busy, deadlock, and lost-commit tokens become [`Self::Unavailable`].
    /// Unique/constraint tokens become [`Self::Conflict`]. Everything else
    /// stays [`Self::Orm`].
    #[must_use]
    pub fn from_db_err(err: sea_orm::DbErr) -> Self {
        match bookclerk_db_exec::classify_db_err(&err) {
            bookclerk_db_exec::DbErrorClass::Unavailable => Self::Unavailable(err.to_string()),
            bookclerk_db_exec::DbErrorClass::Conflict => Self::Conflict(err.to_string()),
            bookclerk_db_exec::DbErrorClass::Other => Self::Orm(err),
        }
    }
}
