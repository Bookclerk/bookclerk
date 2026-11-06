//! Shared oracle for the `cache_settings` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! `cache.*` values reach [`parse_cache_setting`] as text from the settings
//! API and the CLI. The input's first byte picks the key (even:
//! `cache.budget_bytes`, odd: `cache.unviewed_days`, `0xff`: an unknown key)
//! and the rest is the value. The invariant, checked against an independent
//! digit-by-digit reading with overflow detection: the parser accepts
//! exactly the values that are, after ASCII whitespace is trimmed, one or
//! more ASCII digits whose number lies in the key's documented range, and
//! returns that number.

use bookclerk_config::{parse_cache_setting, CacheSettingError, CacheSettingKey};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 128;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Key selector byte, then the value bytes.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, value)) = data.split_first() else {
        return;
    };
    let Ok(value) = std::str::from_utf8(value) else {
        return;
    };
    if selector == 0xff {
        assert!(matches!(
            parse_cache_setting("cache.unknown", value),
            Err(CacheSettingError::UnknownKey(_))
        ));
        return;
    }
    // The documented ranges, written out rather than read from the parser.
    let (key, name, min, max) = if selector % 2 == 0 {
        (
            CacheSettingKey::BudgetBytes,
            "cache.budget_bytes",
            16 * 1024 * 1024,
            1024 * 1024 * 1024 * 1024_u64,
        )
    } else {
        (
            CacheSettingKey::UnviewedDays,
            "cache.unviewed_days",
            1,
            3650,
        )
    };
    let got = parse_cache_setting(name, value);
    match expected(value) {
        Reading::NotANumber => assert!(
            matches!(got, Err(CacheSettingError::NotANumber(_))),
            "{name}={value:?}: {got:?}"
        ),
        Reading::Number(n) if (min..=max).contains(&n) => {
            assert_eq!(got, Ok((key, n)), "{name}={value:?}");
        }
        Reading::Number(_) | Reading::Overflow => assert!(
            matches!(got, Err(CacheSettingError::OutOfRange { .. })),
            "{name}={value:?}: {got:?}"
        ),
    }
}

/// What the oracle reads in a value.
enum Reading {
    /// Not one or more ASCII digits after trimming.
    NotANumber,
    /// Digits too large for a `u64`.
    Overflow,
    /// The number the digits spell.
    Number(u64),
}

/// Reads `value` digit by digit without `str::parse`.
fn expected(value: &str) -> Reading {
    let bytes = value.as_bytes();
    let is_space = |b: &u8| matches!(*b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c);
    let start = bytes
        .iter()
        .position(|b| !is_space(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !is_space(b))
        .map_or(start, |i| i + 1);
    let digits = &bytes[start..end];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Reading::NotANumber;
    }
    let mut number: u128 = 0;
    for &b in digits {
        number = number * 10 + u128::from(b - b'0');
        if number > u128::from(u64::MAX) {
            return Reading::Overflow;
        }
    }
    u64::try_from(number).map_or(Reading::Overflow, Reading::Number)
}
