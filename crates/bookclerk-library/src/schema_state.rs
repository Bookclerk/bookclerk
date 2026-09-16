//! Explicit library schema state. Not an integer version.
//!
//! SQLite `PRAGMA user_version` defaults to 0, so an empty database and an
//! applied unreleased development schema must not share a numeric identity.
//! Unreleased always records the frozen base it was built on (`0` means no
//! frozen revisions exist yet, not “schema version zero”).

use std::fmt;

/// Durable Bookclerk library schema state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaState {
    /// No Bookclerk bookkeeping and no host tables.
    Uninitialized,
    /// Development pack applied on top of a frozen base (or pre-v1).
    Unreleased {
        /// Highest frozen plan version this unreleased pack sits on (`0` = none).
        base_version: i64,
        /// SHA-256 hex of [`crate::migrations::unreleased_ops`].
        checksum: String,
    },
    /// A frozen [`crate::migrations::HostMigrationStep`] is applied.
    Frozen {
        /// Frozen plan version (`>= 1`).
        version: i64,
        /// SHA-256 hex recorded for that frozen step.
        checksum: String,
    },
}

impl SchemaState {
    /// CLI / JSON display: `uninitialized`, `unreleased@base<n>+<checksum>`,
    /// `frozen@<version>+<checksum>`.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Uninitialized => "uninitialized".into(),
            Self::Unreleased {
                base_version,
                checksum,
            } => format!("unreleased@base{base_version}+{checksum}"),
            Self::Frozen { version, checksum } => format!("frozen@{version}+{checksum}"),
        }
    }

    /// Frozen revision when this state is [`Self::Frozen`]; otherwise `None`.
    ///
    /// Never use `0` as a stand-in for uninitialized or unreleased.
    #[must_use]
    pub fn frozen_version(&self) -> Option<i64> {
        match self {
            Self::Frozen { version, .. } => Some(*version),
            _ => None,
        }
    }

    /// Frozen base recorded on an unreleased database (`0` = pre-v1).
    #[must_use]
    pub fn unreleased_base_version(&self) -> Option<i64> {
        match self {
            Self::Unreleased { base_version, .. } => Some(*base_version),
            _ => None,
        }
    }

    /// Checksum when the database has applied Bookclerk schema.
    #[must_use]
    pub fn checksum(&self) -> Option<&str> {
        match self {
            Self::Uninitialized => None,
            Self::Unreleased { checksum, .. } | Self::Frozen { checksum, .. } => Some(checksum),
        }
    }
}

impl fmt::Display for SchemaState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// On-disk `schema_migrations.state` for an unreleased development pack.
pub const SCHEMA_STATE_UNRELEASED: &str = "unreleased";

/// On-disk `schema_migrations.state` for a frozen plan version.
pub const SCHEMA_STATE_FROZEN: &str = "frozen";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_does_not_collapse_empty_and_unreleased_to_zero() {
        assert_eq!(SchemaState::Uninitialized.display(), "uninitialized");
        assert_eq!(
            SchemaState::Unreleased {
                base_version: 0,
                checksum: "abc".into()
            }
            .display(),
            "unreleased@base0+abc"
        );
        assert_eq!(
            SchemaState::Unreleased {
                base_version: 1,
                checksum: "abc".into()
            }
            .display(),
            "unreleased@base1+abc"
        );
        assert_eq!(
            SchemaState::Frozen {
                version: 1,
                checksum: "def".into()
            }
            .display(),
            "frozen@1+def"
        );
        assert_eq!(SchemaState::Uninitialized.frozen_version(), None);
        assert_eq!(
            SchemaState::Unreleased {
                base_version: 0,
                checksum: "abc".into()
            }
            .frozen_version(),
            None
        );
        assert_eq!(
            SchemaState::Unreleased {
                base_version: 0,
                checksum: "abc".into()
            }
            .unreleased_base_version(),
            Some(0)
        );
        assert_ne!(
            SchemaState::Unreleased {
                base_version: 0,
                checksum: "abc".into()
            }
            .display(),
            "0"
        );
    }
}
