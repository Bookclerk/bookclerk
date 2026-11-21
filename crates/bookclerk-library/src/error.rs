//! Error types for library database and secret operations.

use thiserror::Error;

/// The kind of thing a [`LibraryError::NotFound`] could not find.
///
/// Each variant displays as the lower-case phrase an error message uses,
/// such as `enrollment link`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    /// A store account, keyed by plugin and account id.
    Account,
    /// A library title (`books` row).
    Book,
    /// A work of the library data model.
    Work,
    /// An edition of the library data model.
    Edition,
    /// A store listing of the library data model (`store_listings`).
    Listing,
    /// A title request (wishlist entry).
    TitleRequest,
    /// A user.
    User,
    /// A portal identity linked to a user.
    PortalIdentity,
    /// An operator session.
    OperatorSession,
    /// A registered passkey.
    Passkey,
    /// An OpenID Connect relying-party sign-in state.
    OidcState,
    /// A WebAuthn ceremony challenge.
    WebauthnChallenge,
    /// A library node.
    Node,
    /// A node enrollment link.
    EnrollmentLink,
    /// A pending join of this host to another library.
    PendingJoin,
    /// This library's identity.
    LibraryIdentity,
    /// A link to another library.
    LibraryLink,
    /// The node keys a linked library published.
    LibraryNodeKeys,
    /// A library this install holds a library card for, as a clerk.
    ClerkLink,
    /// The sealed library card of a clerk link.
    ClerkCard,
    /// A configuration document, by namespace.
    Configuration,
    /// A plugin instance.
    PluginInstance,
    /// A plugin deployment.
    PluginDeployment,
    /// A plugin instance's scheduler placement.
    Placement,
    /// An installed plugin version.
    PluginVersion,
    /// A plugin's active version pointer.
    VersionPointer,
    /// A plugin's pinned settings schema.
    SettingsSchema,
    /// A remote plugin consent.
    RemoteConsent,
    /// A request from a linked library.
    BranchRequest,
    /// A domain event on the outbox.
    DomainEvent,
    /// An event delivery.
    EventDelivery,
    /// A job.
    Job,
    /// A saved library filter.
    SavedFilter,
    /// A claim ticket.
    ClaimTicket,
    /// One source's metadata claim about a library subject.
    MetadataClaim,
}

impl EntityKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 35] = [
        Self::Account,
        Self::Book,
        Self::Work,
        Self::Edition,
        Self::Listing,
        Self::TitleRequest,
        Self::User,
        Self::PortalIdentity,
        Self::OperatorSession,
        Self::Passkey,
        Self::OidcState,
        Self::WebauthnChallenge,
        Self::Node,
        Self::EnrollmentLink,
        Self::PendingJoin,
        Self::LibraryIdentity,
        Self::LibraryLink,
        Self::LibraryNodeKeys,
        Self::ClerkLink,
        Self::ClerkCard,
        Self::Configuration,
        Self::PluginInstance,
        Self::PluginDeployment,
        Self::Placement,
        Self::PluginVersion,
        Self::VersionPointer,
        Self::SettingsSchema,
        Self::RemoteConsent,
        Self::BranchRequest,
        Self::DomainEvent,
        Self::EventDelivery,
        Self::Job,
        Self::SavedFilter,
        Self::ClaimTicket,
        Self::MetadataClaim,
    ];

    /// The phrase an error message names this kind with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Book => "book",
            Self::Work => "work",
            Self::Edition => "edition",
            Self::Listing => "listing",
            Self::TitleRequest => "title request",
            Self::User => "user",
            Self::PortalIdentity => "portal identity",
            Self::OperatorSession => "operator session",
            Self::Passkey => "passkey",
            Self::OidcState => "sign-in state",
            Self::WebauthnChallenge => "passkey challenge",
            Self::Node => "node",
            Self::EnrollmentLink => "enrollment link",
            Self::PendingJoin => "pending join",
            Self::LibraryIdentity => "library identity",
            Self::LibraryLink => "library link",
            Self::LibraryNodeKeys => "node keys of library",
            Self::ClerkLink => "library card",
            Self::ClerkCard => "sealed library card",
            Self::Configuration => "configuration",
            Self::PluginInstance => "plugin instance",
            Self::PluginDeployment => "plugin deployment",
            Self::Placement => "placement",
            Self::PluginVersion => "plugin version",
            Self::VersionPointer => "active version of plugin",
            Self::SettingsSchema => "settings schema of plugin",
            Self::RemoteConsent => "remote plugin consent",
            Self::BranchRequest => "request",
            Self::DomainEvent => "event",
            Self::EventDelivery => "event delivery",
            Self::Job => "job",
            Self::SavedFilter => "saved filter",
            Self::ClaimTicket => "claim ticket",
            Self::MetadataClaim => "metadata claim",
        }
    }
}

impl std::fmt::Display for EntityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a [`LibraryError::NotFound`] could not find: the kind of row and,
/// when the caller named one, its id.
///
/// Displays as `<entity> <id> not found`, or `<entity> not found` without
/// an id, so an error body names the missing thing rather than presuming a
/// book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// The kind of row.
    pub entity: EntityKind,
    /// The id the caller asked for, when it named one.
    pub id: Option<String>,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.id {
            Some(id) => write!(f, "{} {id} not found", self.entity),
            None => write!(f, "{} not found", self.entity),
        }
    }
}

impl LibraryError {
    /// A [`Self::NotFound`] for `entity` with the id `id`.
    ///
    /// # Arguments
    ///
    /// * `entity` - The kind of row that is missing.
    /// * `id` - The id that was asked for, as the caller names it.
    #[must_use]
    pub fn not_found(entity: EntityKind, id: impl std::fmt::Display) -> Self {
        Self::NotFound(Missing {
            entity,
            id: Some(id.to_string()),
        })
    }

    /// A [`Self::NotFound`] for `entity` without an id (a singleton, or a
    /// lookup by something other than an id).
    #[must_use]
    pub const fn missing(entity: EntityKind) -> Self {
        Self::NotFound(Missing { entity, id: None })
    }
}

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

    /// A requested row was not found; the payload names what and which.
    #[error("{0}")]
    NotFound(Missing),

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
    use super::{EntityKind, LibraryError, Missing};
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

    #[test]
    fn every_entity_kind_has_a_distinct_phrase() {
        let phrases: std::collections::HashSet<&str> =
            EntityKind::ALL.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(phrases.len(), EntityKind::ALL.len());
        for kind in EntityKind::ALL {
            assert!(!kind.as_str().is_empty());
            assert_eq!(kind.to_string(), kind.as_str());
            assert_eq!(
                LibraryError::missing(kind).to_string(),
                format!("{} not found", kind.as_str())
            );
        }
    }

    #[test]
    fn not_found_names_the_missing_entity() {
        assert_eq!(
            LibraryError::not_found(EntityKind::EnrollmentLink, "5f1e").to_string(),
            "enrollment link 5f1e not found"
        );
        assert_eq!(
            LibraryError::not_found(EntityKind::User, 7).to_string(),
            "user 7 not found"
        );
        assert_eq!(
            LibraryError::missing(EntityKind::LibraryIdentity).to_string(),
            "library identity not found"
        );
        assert!(matches!(
            LibraryError::not_found(EntityKind::Node, "n_1"),
            LibraryError::NotFound(Missing { entity: EntityKind::Node, id: Some(id) }) if id == "n_1"
        ));
    }
}
