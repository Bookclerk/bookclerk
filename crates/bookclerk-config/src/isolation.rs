//! How strictly a confined child process must be jailed.

use serde::{Deserialize, Serialize};

/// What to do when a child process cannot be confined.
///
/// Shared by every tier that runs untrusted work in a child: `[media]` for
/// codec workers and `[plugins]` for storefront guests. The tiers differ in what
/// they reach for, not in how an operator wants a missing jail handled.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    /// Refuse to run the work when the jail does not engage. The default: a
    /// host that cannot sandbox should not process untrusted input in reach of
    /// the master key.
    #[default]
    Required,
    /// Confine where the platform allows and log what did not engage. Use on
    /// kernels without Landlock, accepting that the work runs unconfined there.
    BestEffort,
    /// Run with no jail at all. Development only.
    Off,
}

impl Isolation {
    /// Canonical spelling, as written in `config.toml`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::BestEffort => "best-effort",
            Self::Off => "off",
        }
    }

    /// Parse a config or environment value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" | "on" | "true" => Some(Self::Required),
            "best-effort" | "best_effort" | "besteffort" => Some(Self::BestEffort),
            "off" | "false" | "none" => Some(Self::Off),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_required() {
        assert_eq!(Isolation::default(), Isolation::Required);
    }

    /// `config show` prints these, and an operator should be able to paste one
    /// back into `config.toml`.
    #[test]
    fn every_mode_round_trips_through_its_canonical_spelling() {
        for mode in [Isolation::Required, Isolation::BestEffort, Isolation::Off] {
            assert_eq!(Isolation::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn parses_the_documented_spellings() {
        assert_eq!(Isolation::parse("required"), Some(Isolation::Required));
        assert_eq!(Isolation::parse("best-effort"), Some(Isolation::BestEffort));
        assert_eq!(
            Isolation::parse(" BEST_EFFORT "),
            Some(Isolation::BestEffort)
        );
        assert_eq!(Isolation::parse("off"), Some(Isolation::Off));
        assert_eq!(Isolation::parse("maybe"), None);
    }
}
