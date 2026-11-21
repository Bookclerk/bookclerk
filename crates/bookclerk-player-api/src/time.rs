//! Lengths of time on the wire and in the playback core.
//!
//! The apps speak seconds as JSON numbers (fractional, `currentTime:
//! 754.2`); the playback core keeps whole milliseconds, at most sixty days
//! (`bookclerk_library::MAX_PLAYBACK_MS`). Conversions here are exact in
//! both directions for every value in range.

/// The longest length of time the API accepts, in milliseconds: sixty
/// days, the playback core's own bound.
pub const MAX_MS: u64 = 60 * 24 * 60 * 60 * 1000;

/// The longest length of time the API accepts, in seconds.
pub const MAX_SECS: f64 = 5_184_000.0;

/// Whole milliseconds as wire seconds, exactly.
///
/// # Arguments
///
/// * `ms` - Milliseconds; values past [`MAX_MS`] are clamped to it.
///
/// # Returns
///
/// The same length in seconds.
#[must_use]
pub fn ms_to_secs(ms: u64) -> f64 {
    let ms = ms.min(MAX_MS);
    // Both parts fit in a u32 because ms is at most sixty days.
    let whole = u32::try_from(ms / 1000).unwrap_or(u32::MAX);
    let part = u32::try_from(ms % 1000).unwrap_or(0);
    f64::from(whole) + f64::from(part) / 1000.0
}

/// Wire seconds as whole milliseconds, rounded to the nearest.
///
/// # Arguments
///
/// * `secs` - A number an app sent.
///
/// # Returns
///
/// `None` for a value that is not finite, is negative or is past
/// [`MAX_SECS`].
#[must_use]
pub fn secs_to_ms(secs: f64) -> Option<u64> {
    if !secs.is_finite() || !(0.0..=MAX_SECS).contains(&secs) {
        return None;
    }
    let ms = (secs * 1000.0).round();
    // In range and whole: at most MAX_MS, which a u64 holds exactly.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = ms as u64;
    Some(ms.min(MAX_MS))
}

/// Milliseconds since the Unix epoch, as the apps send times of day
/// (`updatedAt`, `startedAt`, `lastUpdate`).
///
/// # Arguments
///
/// * `value` - A number an app sent.
///
/// # Returns
///
/// `None` unless the value is a whole, non-negative number of milliseconds
/// before the year 10000.
#[must_use]
pub fn epoch_ms(value: f64) -> Option<i64> {
    /// 9999-12-31T23:59:59.999Z in milliseconds since the epoch.
    const MAX_EPOCH_MS: f64 = 253_402_300_799_999.0;
    if !value.is_finite() || !(0.0..=MAX_EPOCH_MS).contains(&value) {
        return None;
    }
    let rounded = value.round();
    // In range and whole: well inside an i64.
    #[allow(clippy::cast_possible_truncation)]
    let ms = rounded as i64;
    Some(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_and_milliseconds_round_trip() {
        for ms in [0, 1, 999, 1000, 754_200, MAX_MS - 1, MAX_MS] {
            assert_eq!(secs_to_ms(ms_to_secs(ms)), Some(ms));
        }
        assert_eq!(ms_to_secs(MAX_MS + 5), ms_to_secs(MAX_MS));
        assert_eq!(secs_to_ms(754.2), Some(754_200));
        assert_eq!(secs_to_ms(0.0004), Some(0));
        assert_eq!(secs_to_ms(0.0005), Some(1));
    }

    #[test]
    fn out_of_range_seconds_are_refused() {
        for bad in [-0.5, f64::NAN, f64::INFINITY, MAX_SECS + 0.001] {
            assert_eq!(secs_to_ms(bad), None);
        }
    }

    #[test]
    fn epoch_times_are_bounded() {
        assert_eq!(epoch_ms(1_763_600_000_000.4), Some(1_763_600_000_000));
        assert_eq!(epoch_ms(-1.0), None);
        assert_eq!(epoch_ms(1e300), None);
    }
}
