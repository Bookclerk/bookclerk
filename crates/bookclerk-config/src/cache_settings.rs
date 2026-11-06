//! Parsing of the image caches' retention settings (`cache.*` keys).
//!
//! The values arrive as text from `PATCH /api/settings`, `bookclerk config
//! set` and stored `core.cache` documents, so the parser is a pure function
//! over the key and value, fuzzed by the `cache_settings` target against an
//! independent digit-by-digit reading
//! (`docs/contributing/fuzzing.md`).

/// Default disk budget for one node's image caches: 1 GiB.
pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 1 << 30;

/// Smallest accepted cache budget: 16 MiB, room for a few hundred covers.
pub const MIN_CACHE_BUDGET_BYTES: u64 = 16 << 20;

/// Largest accepted cache budget: 1 TiB.
pub const MAX_CACHE_BUDGET_BYTES: u64 = 1 << 40;

/// Default days an entry may go unviewed before it is evicted.
pub const DEFAULT_CACHE_UNVIEWED_DAYS: u32 = 90;

/// Largest accepted `unviewed_days`, about ten years.
pub const MAX_CACHE_UNVIEWED_DAYS: u32 = 3650;

/// A `cache.*` settings key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSettingKey {
    /// `cache.budget_bytes`: bytes one node's image caches may hold.
    BudgetBytes,
    /// `cache.unviewed_days`: days an entry may go unviewed.
    UnviewedDays,
}

impl CacheSettingKey {
    /// Every key, in display order.
    pub const ALL: [Self; 2] = [Self::BudgetBytes, Self::UnviewedDays];

    /// The dotted key, such as `cache.budget_bytes`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BudgetBytes => "cache.budget_bytes",
            Self::UnviewedDays => "cache.unviewed_days",
        }
    }

    /// The key `key` names, matched exactly.
    ///
    /// # Arguments
    ///
    /// * `key` - A dotted settings key.
    ///
    /// # Returns
    ///
    /// `None` for anything but a `cache.*` key.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == key)
    }

    /// The smallest and largest value the key accepts.
    #[must_use]
    pub const fn range(self) -> (u64, u64) {
        match self {
            Self::BudgetBytes => (MIN_CACHE_BUDGET_BYTES, MAX_CACHE_BUDGET_BYTES),
            Self::UnviewedDays => (1, MAX_CACHE_UNVIEWED_DAYS as u64),
        }
    }
}

/// Why a `cache.*` value was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheSettingError {
    /// The key is not a `cache.*` key.
    #[error("unknown cache setting: {0}")]
    UnknownKey(String),
    /// The value is not a whole number without sign or separators.
    #[error("{0} must be a whole number")]
    NotANumber(&'static str),
    /// The value is a whole number outside the key's range.
    #[error("{key} must be between {min} and {max}")]
    OutOfRange {
        /// The dotted key.
        key: &'static str,
        /// Smallest accepted value.
        min: u64,
        /// Largest accepted value.
        max: u64,
    },
}

/// Parses one `cache.*` setting.
///
/// The value may have surrounding ASCII whitespace; inside, it must be one
/// or more ASCII digits, no sign, and within the key's [`CacheSettingKey::range`].
///
/// # Arguments
///
/// * `key` - The dotted key.
/// * `value` - The value as text.
///
/// # Returns
///
/// The key and the value.
///
/// # Errors
///
/// [`CacheSettingError`] for an unknown key, a value that is not a whole
/// number, or one out of range.
pub fn parse_cache_setting(
    key: &str,
    value: &str,
) -> Result<(CacheSettingKey, u64), CacheSettingError> {
    let parsed =
        CacheSettingKey::parse(key).ok_or_else(|| CacheSettingError::UnknownKey(key.to_owned()))?;
    let digits = value.trim_matches(|ch: char| ch.is_ascii_whitespace());
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CacheSettingError::NotANumber(parsed.as_str()));
    }
    let (min, max) = parsed.range();
    let out_of_range = CacheSettingError::OutOfRange {
        key: parsed.as_str(),
        min,
        max,
    };
    // All digits, so the only parse failure left is overflow: out of range.
    let number: u64 = digits.parse().map_err(|_| out_of_range.clone())?;
    if (min..=max).contains(&number) {
        Ok((parsed, number))
    } else {
        Err(out_of_range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_values_are_bounded() {
        for key in CacheSettingKey::ALL {
            assert_eq!(CacheSettingKey::parse(key.as_str()), Some(key));
        }
        assert_eq!(CacheSettingKey::parse("cache.other"), None);
        assert_eq!(
            parse_cache_setting("cache.budget_bytes", " 16777216\n"),
            Ok((CacheSettingKey::BudgetBytes, 16 << 20))
        );
        assert_eq!(
            parse_cache_setting("cache.unviewed_days", "3650"),
            Ok((CacheSettingKey::UnviewedDays, 3650))
        );
        for (key, value) in [
            ("cache.budget_bytes", "16777215"),
            ("cache.budget_bytes", "99999999999999999999999"),
            ("cache.unviewed_days", "0"),
        ] {
            assert!(matches!(
                parse_cache_setting(key, value),
                Err(CacheSettingError::OutOfRange { .. })
            ));
        }
        for value in ["", " ", "+5", "-1", "1_000", "1e3", "٣"] {
            assert!(matches!(
                parse_cache_setting("cache.unviewed_days", value),
                Err(CacheSettingError::NotANumber(_))
            ));
        }
        assert_eq!(
            parse_cache_setting("cache.nope", "1"),
            Err(CacheSettingError::UnknownKey("cache.nope".into()))
        );
    }
}
