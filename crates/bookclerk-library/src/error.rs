//! Error types for library database and secret operations.

use thiserror::Error;

/// Result alias for [`LibraryError`].
pub type Result<T> = std::result::Result<T, LibraryError>;

/// Failures from library store, schema apply, secrets, or config.
#[derive(Debug, Error)]
pub enum LibraryError {
    /// Schema versioning, backup, or canonical SQL failure.
    #[error("{0}")]
    Schema(String),

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

    /// The caller may not perform this write (for example a configuration
    /// write without operator authority).
    #[error("{0}")]
    Forbidden(String),

    /// The request or stored document failed validation.
    #[error("{0}")]
    Invalid(String),

    /// A request or its result exceeded a negotiated SQL-v1 cap
    /// (`maxResultBytes`, `maxResultRows`, `maxRequestBytes`, ...).
    ///
    /// Not retryable as is: the caller narrows the request (fewer rows, fewer
    /// statements) or switches to a transport that fits.
    #[error("{0}")]
    LimitExceeded(bookclerk_plugin_abi::LimitExceeded),

    /// Catch-all for otherwise unclassified failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl LibraryError {
    /// Maps a SeaORM / adapter [`sea_orm::DbErr`] onto a typed library error.
    ///
    /// Busy, deadlock, and lost-commit tokens become [`Self::Unavailable`].
    /// Unique/duplicate-object tokens become [`Self::Conflict`]. Foreign-key,
    /// CHECK, and NOT NULL stay [`Self::Orm`]. Everything else stays [`Self::Orm`].
    #[must_use]
    pub fn from_db_err(err: sea_orm::DbErr) -> Self {
        if let sea_orm::DbErr::Custom(message) = &err {
            if let Some(limit) = bookclerk_plugin_abi::LimitExceeded::from_message(message) {
                return Self::LimitExceeded(limit);
            }
        }
        match bookclerk_db_exec::classify_db_err(&err) {
            bookclerk_db_exec::DbErrorClass::Unavailable => Self::Unavailable(err.to_string()),
            bookclerk_db_exec::DbErrorClass::Conflict => Self::Conflict(err.to_string()),
            bookclerk_db_exec::DbErrorClass::Other => Self::Orm(err),
        }
    }

    /// Maps an ABI [`bookclerk_plugin_abi::PluginError`] from an adapter or
    /// admission check onto a typed library error.
    ///
    /// A `limit_exceeded` error becomes [`Self::LimitExceeded`]; everything
    /// else is [`Self::Other`] with the same message.
    #[must_use]
    pub fn from_abi_err(err: bookclerk_plugin_abi::PluginError) -> Self {
        match err.limit() {
            Some(limit) => Self::LimitExceeded(limit),
            None => Self::Other(anyhow::anyhow!(err.to_string())),
        }
    }

    /// The exceeded cap when this error is (or wraps the message of) a
    /// SQL-v1 limit error.
    ///
    /// Matches [`Self::LimitExceeded`] directly and parses the canonical
    /// `limit exceeded: <name> is <cap>; used <n>` text out of
    /// [`Self::Orm`], [`Self::Unavailable`] and [`Self::Other`], so a limit
    /// error survives a string hop through the adapter boundary.
    #[must_use]
    pub fn limit(&self) -> Option<bookclerk_plugin_abi::LimitExceeded> {
        match self {
            Self::LimitExceeded(limit) => Some(limit.clone()),
            Self::Orm(_) | Self::Unavailable(_) | Self::Other(_) => {
                bookclerk_plugin_abi::LimitExceeded::from_message(&self.to_string())
            }
            Self::Schema(_)
            | Self::Config(_)
            | Self::Io(_)
            | Self::NotFound(_)
            | Self::LastAdministrator
            | Self::LastOwner
            | Self::InvalidEmail
            | Self::Conflict(_)
            | Self::Forbidden(_)
            | Self::Invalid(_) => None,
        }
    }

    /// True when host schema apply may retry after re-reading durable state.
    #[must_use]
    pub fn is_schema_apply_retryable(&self) -> bool {
        match self {
            Self::Unavailable(_) | Self::Conflict(_) => true,
            Self::Orm(err) => bookclerk_db_exec::is_schema_apply_retryable(err),
            _ => false,
        }
    }
}

impl From<LibraryError> for bookclerk_storefront::SourceError {
    /// Wraps a store failure so content-source code can use `?` on library calls.
    fn from(err: LibraryError) -> Self {
        Self::Library(Box::new(err))
    }
}

#[cfg(test)]
mod tests {
    use super::LibraryError;
    use bookclerk_plugin_abi::{LimitExceeded, PluginError};

    #[test]
    fn limit_errors_stay_typed_across_string_hops() {
        let limit = LimitExceeded::new("maxResultBytes", 262_144, 300_000);
        let typed = LibraryError::LimitExceeded(limit.clone());
        assert_eq!(typed.limit(), Some(limit.clone()));
        assert_eq!(typed.to_string(), limit.to_message());

        let orm = LibraryError::from_db_err(sea_orm::DbErr::Custom(format!(
            "statement 1: {}",
            limit.to_message()
        )));
        assert!(matches!(orm, LibraryError::LimitExceeded(_)), "{orm}");

        let unavailable = LibraryError::Unavailable(format!("adapter: {}", limit.to_message()));
        assert_eq!(unavailable.limit(), Some(limit.clone()));
        let other = LibraryError::Other(anyhow::anyhow!("{}", limit.to_message()));
        assert_eq!(other.limit(), Some(limit.clone()));

        assert_eq!(LibraryError::Invalid(limit.to_message()).limit(), None);
        assert_eq!(
            LibraryError::Unavailable("maxResultBytes mentioned, not exceeded".into()).limit(),
            None
        );
    }

    #[test]
    fn abi_errors_map_by_code() {
        let limit = LimitExceeded::new("maxLoweredStatementBytes", 100_000, 120_000);
        let mapped = LibraryError::from_abi_err(PluginError::limit_exceeded(limit.clone()));
        assert!(
            matches!(mapped, LibraryError::LimitExceeded(ref l) if *l == limit),
            "{mapped}"
        );
        let other = LibraryError::from_abi_err(PluginError::invalid_params("bad SQL"));
        assert!(matches!(other, LibraryError::Other(_)), "{other}");
        assert!(other.to_string().contains("bad SQL"));
    }

    #[test]
    fn library_errors_become_chained_source_errors() {
        let err = bookclerk_storefront::SourceError::from(LibraryError::Conflict("busy".into()));
        assert!(matches!(err, bookclerk_storefront::SourceError::Library(_)));
        let source = std::error::Error::source(&err).map(ToString::to_string);
        assert_eq!(
            source,
            Some(LibraryError::Conflict("busy".into()).to_string())
        );
    }
}
