//! Five-field cron schedules for `[triggers] crons` (Workers `scheduled`).
//!
//! A [`CronSchedule`] is parsed from an untrusted `plugin.toml` string, so
//! parsing is sans-IO and bounded: at most [`MAX_CRON_SOURCE_BYTES`] bytes,
//! exactly five whitespace-separated fields, and every value range-checked.
//! Times are UTC, as in Cloudflare Workers.
//!
//! | Field | Values | Names |
//! | --- | --- | --- |
//! | minute | `0-59` | |
//! | hour | `0-23` | |
//! | day of month | `1-31` | |
//! | month | `1-12` | `JAN`-`DEC` |
//! | day of week | `0-7` (`0` and `7` are Sunday) | `SUN`-`SAT` |
//!
//! Each field is a comma list of items. An item is `*`, a value `n`, or a
//! range `a-b`, optionally followed by `/step`; `n/step` means `n` to the
//! field maximum. Names are case-insensitive. Quartz extensions (`L`, `W`,
//! `#`, `?`) and `@daily`-style macros are rejected.
//!
//! When both day of month and day of week are restricted (neither field
//! starts with `*`), a day matches if **either** matches, as in Vixie cron
//! and Workers. A schedule that can never fire (`0 0 30 2 *`) is rejected.

use std::fmt;

/// Longest accepted cron source text, in bytes.
pub const MAX_CRON_SOURCE_BYTES: usize = bookclerk_plugin_abi::MAX_CRON_BYTES as usize;

/// Minutes in one day.
const MINUTES_PER_DAY: i64 = 1440;

/// Days searched before a schedule is reported as never firing again.
///
/// Nine years covers the longest gap a valid schedule can have (February 29
/// across a skipped century leap year: 2096 to 2104 is eight years).
const SEARCH_DAYS: i64 = 366 * 9;

/// One minute on the UTC timeline, counted from the Unix epoch.
///
/// Cron schedules have minute resolution, so every fire time is a whole
/// minute. Negative values are minutes before 1970-01-01T00:00Z.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixMinute(pub i64);

impl UnixMinute {
    /// The minute containing `unix_ms` (floored, so it never rounds up).
    ///
    /// # Arguments
    ///
    /// * `unix_ms` - Milliseconds since the Unix epoch.
    #[must_use]
    pub fn from_unix_ms(unix_ms: i64) -> Self {
        Self(unix_ms.div_euclid(60_000))
    }

    /// Milliseconds since the Unix epoch at the start of this minute.
    ///
    /// # Returns
    ///
    /// `None` when the result does not fit an `i64`.
    #[must_use]
    pub fn to_unix_ms(self) -> Option<i64> {
        self.0.checked_mul(60_000)
    }
}

/// A validated five-field cron schedule.
///
/// Equality compares the normalized source (fields joined by one space),
/// which fully determines the parsed fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CronSchedule {
    /// Normalized source: the five fields joined by a single space.
    source: String,
    /// Bit `n` set when minute `n` (0-59) matches.
    minutes: u64,
    /// Bit `n` set when hour `n` (0-23) matches.
    hours: u64,
    /// Bit `n` set when day of month `n` (1-31) matches.
    days_of_month: u64,
    /// Bit `n` set when month `n` (1-12) matches.
    months: u64,
    /// Bit `n` set when weekday `n` (0 = Sunday .. 6) matches.
    days_of_week: u64,
    /// Day of month field does not start with `*`.
    dom_restricted: bool,
    /// Day of week field does not start with `*`.
    dow_restricted: bool,
}

/// Why a cron string was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronError(String);

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CronError {}

/// Builds a [`CronError`] from a message.
fn err(msg: impl Into<String>) -> CronError {
    CronError(msg.into())
}

/// Static description of one cron field.
struct FieldSpec {
    /// Field name used in error messages.
    name: &'static str,
    /// Smallest accepted value.
    min: u32,
    /// Largest accepted value (for day of week, `7` before folding to `0`).
    max: u32,
    /// Three-letter names for `min..`, in order; empty when names are not
    /// accepted.
    names: &'static [&'static str],
}

/// Month names, starting at 1.
const MONTH_NAMES: &[&str] = &[
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Weekday names, starting at 0 (Sunday).
const DAY_NAMES: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// The five fields in source order.
const FIELDS: [FieldSpec; 5] = [
    FieldSpec {
        name: "minute",
        min: 0,
        max: 59,
        names: &[],
    },
    FieldSpec {
        name: "hour",
        min: 0,
        max: 23,
        names: &[],
    },
    FieldSpec {
        name: "day of month",
        min: 1,
        max: 31,
        names: &[],
    },
    FieldSpec {
        name: "month",
        min: 1,
        max: 12,
        names: MONTH_NAMES,
    },
    FieldSpec {
        name: "day of week",
        min: 0,
        max: 7,
        names: DAY_NAMES,
    },
];

/// Parses one value (number or name) of `spec`.
///
/// # Errors
///
/// Returns a [`CronError`] when the value is empty, unknown or outside the
/// field's range.
fn parse_value(text: &str, spec: &FieldSpec) -> Result<u32, CronError> {
    if text.is_empty() {
        return Err(err(format!("{} has an empty value", spec.name)));
    }
    let value = if text.bytes().all(|b| b.is_ascii_digit()) {
        if text.len() > 2 {
            return Err(err(format!("{} value `{text}` is out of range", spec.name)));
        }
        text.parse::<u32>()
            .map_err(|_| err(format!("{} value `{text}` is not a number", spec.name)))?
    } else {
        let lower = text.to_ascii_lowercase();
        let index = spec
            .names
            .iter()
            .position(|name| *name == lower)
            .ok_or_else(|| err(format!("{} value `{text}` is not recognized", spec.name)))?;
        spec.min + u32::try_from(index).map_err(|_| err(format!("{} name index", spec.name)))?
    };
    if value < spec.min || value > spec.max {
        return Err(err(format!(
            "{} value {value} is outside {}-{}",
            spec.name, spec.min, spec.max
        )));
    }
    Ok(value)
}

/// Parses one comma-separated field into a bit set.
///
/// # Errors
///
/// Returns a [`CronError`] when an item, range or step is malformed.
fn parse_field(text: &str, spec: &FieldSpec) -> Result<u64, CronError> {
    let mut bits = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                if step.is_empty() || !step.bytes().all(|b| b.is_ascii_digit()) || step.len() > 2 {
                    return Err(err(format!("{} step `{step}` is invalid", spec.name)));
                }
                let step = step
                    .parse::<u32>()
                    .map_err(|_| err(format!("{} step `{step}` is invalid", spec.name)))?;
                if step == 0 || step > spec.max {
                    return Err(err(format!(
                        "{} step {step} is outside 1-{}",
                        spec.name, spec.max
                    )));
                }
                (range, Some(step))
            }
            None => (item, None),
        };
        let (start, end) = if range == "*" {
            (spec.min, spec.max)
        } else if let Some((a, b)) = range.split_once('-') {
            let a = parse_value(a, spec)?;
            let b = parse_value(b, spec)?;
            if a > b {
                return Err(err(format!("{} range {a}-{b} is reversed", spec.name)));
            }
            (a, b)
        } else {
            let a = parse_value(range, spec)?;
            // `n/step` runs from n to the field maximum.
            (a, if step.is_some() { spec.max } else { a })
        };
        let step = step.unwrap_or(1);
        let mut value = start;
        while value <= end {
            bits |= 1u64 << value;
            value += step;
        }
    }
    Ok(bits)
}

/// Days in `month` (1-12) of a leap year: the most a month can have.
fn max_days_in_month(month: u32) -> u32 {
    match month {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl CronSchedule {
    /// Parses and validates a five-field cron expression.
    ///
    /// # Arguments
    ///
    /// * `text` - Cron source from `plugin.toml` (`*/30 * * * *`).
    ///
    /// # Returns
    ///
    /// The schedule, with its source normalized to single spaces.
    ///
    /// # Errors
    ///
    /// Returns [`CronError`] when the text is too long, not ASCII, does not
    /// have exactly five fields, has a value, range or step out of bounds,
    /// or describes a schedule that can never fire.
    pub fn parse(text: &str) -> Result<Self, CronError> {
        if text.len() > MAX_CRON_SOURCE_BYTES {
            return Err(err(format!(
                "cron is longer than {MAX_CRON_SOURCE_BYTES} bytes"
            )));
        }
        if !text.is_ascii() {
            return Err(err("cron must be ASCII"));
        }
        let fields: Vec<&str> = text.split_ascii_whitespace().collect();
        if fields.len() != FIELDS.len() {
            return Err(err(format!(
                "cron needs 5 fields (minute hour day-of-month month day-of-week), got {}",
                fields.len()
            )));
        }
        let mut bits = [0u64; 5];
        for (slot, (field, spec)) in bits.iter_mut().zip(fields.iter().zip(FIELDS.iter())) {
            *slot = parse_field(field, spec)?;
        }
        // Fold weekday 7 onto Sunday.
        let mut days_of_week = bits[4];
        if days_of_week & (1 << 7) != 0 {
            days_of_week = (days_of_week & !(1 << 7)) | 1;
        }
        let schedule = Self {
            source: fields.join(" "),
            minutes: bits[0],
            hours: bits[1],
            days_of_month: bits[2],
            months: bits[3],
            days_of_week,
            dom_restricted: !fields[2].starts_with('*'),
            dow_restricted: !fields[4].starts_with('*'),
        };
        // Only a day-of-month-only schedule can name days no chosen month has.
        if schedule.dom_restricted && !schedule.dow_restricted {
            let possible = (1..=12u32).any(|month| {
                schedule.months & (1 << month) != 0
                    && (1..=max_days_in_month(month))
                        .any(|day| schedule.days_of_month & (1 << day) != 0)
            });
            if !possible {
                return Err(err(format!(
                    "cron `{}` names a day of month none of its months has, so it never fires",
                    schedule.source
                )));
            }
        }
        Ok(schedule)
    }

    /// Normalized source text: the five fields joined by one space.
    ///
    /// This is the `cron` string a `scheduled` handler receives.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// True when the day starting at `day` (days since the epoch) matches.
    fn day_matches(&self, day: i64) -> bool {
        let (_, month, dom) = civil_from_days(day);
        if self.months & (1 << month) == 0 {
            return false;
        }
        let weekday = weekday_from_days(day);
        let dom_hit = self.days_of_month & (1 << dom) != 0;
        let dow_hit = self.days_of_week & (1 << weekday) != 0;
        if self.dom_restricted && self.dow_restricted {
            dom_hit || dow_hit
        } else {
            dom_hit && dow_hit
        }
    }

    /// True when the schedule fires at `minute`.
    ///
    /// # Arguments
    ///
    /// * `minute` - A minute on the UTC timeline.
    #[must_use]
    pub fn matches(&self, minute: UnixMinute) -> bool {
        let day = minute.0.div_euclid(MINUTES_PER_DAY);
        let of_day = minute.0.rem_euclid(MINUTES_PER_DAY);
        self.day_matches(day) && self.minute_of_day_matches(of_day)
    }

    /// True when minute-of-day `of_day` (0-1439) is in the hour and minute
    /// sets.
    fn minute_of_day_matches(&self, of_day: i64) -> bool {
        let hour = of_day / 60;
        let minute = of_day % 60;
        self.hours & (1 << hour) != 0 && self.minutes & (1 << minute) != 0
    }

    /// The first fire time strictly after `after`.
    ///
    /// # Arguments
    ///
    /// * `after` - Exclusive lower bound.
    ///
    /// # Returns
    ///
    /// `None` when the schedule does not fire in the next nine years (only
    /// possible near the end of the representable timeline, since
    /// [`Self::parse`] rejects schedules that never fire).
    #[must_use]
    pub fn next_after(&self, after: UnixMinute) -> Option<UnixMinute> {
        let start = after.0.checked_add(1)?;
        let first_day = start.div_euclid(MINUTES_PER_DAY);
        let first_of_day = start.rem_euclid(MINUTES_PER_DAY);
        for offset in 0..SEARCH_DAYS {
            let day = first_day.checked_add(offset)?;
            if !self.day_matches(day) {
                continue;
            }
            let from = if offset == 0 { first_of_day } else { 0 };
            if let Some(of_day) = (from..MINUTES_PER_DAY).find(|m| self.minute_of_day_matches(*m)) {
                return day
                    .checked_mul(MINUTES_PER_DAY)?
                    .checked_add(of_day)
                    .map(UnixMinute);
            }
        }
        None
    }

    /// The latest fire time at or before `at`.
    ///
    /// The scheduler uses this to coalesce missed fires: after downtime a
    /// schedule fires once, for the newest time it missed.
    ///
    /// # Arguments
    ///
    /// * `at` - Inclusive upper bound.
    ///
    /// # Returns
    ///
    /// `None` when the schedule did not fire in the nine years before `at`.
    #[must_use]
    pub fn latest_at_or_before(&self, at: UnixMinute) -> Option<UnixMinute> {
        let last_day = at.0.div_euclid(MINUTES_PER_DAY);
        let last_of_day = at.0.rem_euclid(MINUTES_PER_DAY);
        for offset in 0..SEARCH_DAYS {
            let day = last_day.checked_sub(offset)?;
            if !self.day_matches(day) {
                continue;
            }
            let to = if offset == 0 {
                last_of_day
            } else {
                MINUTES_PER_DAY - 1
            };
            if let Some(of_day) = (0..=to).rev().find(|m| self.minute_of_day_matches(*m)) {
                return day
                    .checked_mul(MINUTES_PER_DAY)?
                    .checked_add(of_day)
                    .map(UnixMinute);
            }
        }
        None
    }
}

impl fmt::Display for CronSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl std::str::FromStr for CronSchedule {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for CronSchedule {
    type Error = CronError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<CronSchedule> for String {
    fn from(value: CronSchedule) -> Self {
        value.source
    }
}

impl serde::Serialize for CronSchedule {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.source)
    }
}

impl<'de> serde::Deserialize<'de> for CronSchedule {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Weekday of `days` since the epoch (0 = Sunday); 1970-01-01 was a
/// Thursday.
fn weekday_from_days(days: i64) -> u32 {
    // rem_euclid(7) is in 0..7, so the conversion cannot fail.
    u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0)
}

/// Proleptic Gregorian `(year, month, day)` for `days` since the epoch.
///
/// Howard Hinnant's `civil_from_days`; exact for the whole `i64` day range
/// the searches above can reach.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    // Both are bounded by the comments above.
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    /// Minute at `yyyy-mm-ddThh:mmZ`, through `chrono` as an independent
    /// calendar.
    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> UnixMinute {
        let t = chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .and_then(|date| date.and_hms_opt(h, mi, 0))
            .map(|dt| dt.and_utc().timestamp())
            .unwrap_or_default();
        UnixMinute(t / 60)
    }

    /// Parses or panics with the message (tests only).
    fn cron(text: &str) -> CronSchedule {
        match CronSchedule::parse(text) {
            Ok(schedule) => schedule,
            Err(e) => panic!("`{text}`: {e}"),
        }
    }

    /// Slow oracle: first matching minute after `after`, one minute at a time.
    fn brute_next(schedule: &CronSchedule, after: UnixMinute, limit: i64) -> Option<UnixMinute> {
        (1..=limit)
            .map(|step| UnixMinute(after.0 + step))
            .find(|m| schedule.matches(*m))
    }

    #[test]
    fn every_thirty_minutes() {
        let s = cron("*/30 * * * *");
        assert_eq!(s.as_str(), "*/30 * * * *");
        assert_eq!(
            s.next_after(at(2026, 11, 4, 3, 0)),
            Some(at(2026, 11, 4, 3, 30))
        );
        assert_eq!(
            s.next_after(at(2026, 11, 4, 3, 59)),
            Some(at(2026, 11, 4, 4, 0))
        );
        assert_eq!(
            s.latest_at_or_before(at(2026, 11, 4, 3, 29)),
            Some(at(2026, 11, 4, 3, 0))
        );
        assert_eq!(
            s.latest_at_or_before(at(2026, 11, 4, 3, 30)),
            Some(at(2026, 11, 4, 3, 30))
        );
    }

    #[test]
    fn whitespace_is_normalized() {
        assert_eq!(cron("  0\t3  * * MON ").as_str(), "0 3 * * MON");
        assert_eq!(cron("0 3 * * MON"), cron("0  3 * * MON"));
    }

    #[test]
    fn names_ranges_lists_and_steps() {
        let s = cron("5,10-12 9-17/4 * jan-mar mon-fri");
        assert!(s.matches(at(2026, 1, 5, 9, 5))); // Monday
        assert!(s.matches(at(2026, 1, 5, 13, 11)));
        assert!(s.matches(at(2026, 1, 5, 17, 12)));
        assert!(!s.matches(at(2026, 1, 5, 15, 5))); // 15 not in 9/13/17
        assert!(!s.matches(at(2026, 1, 4, 9, 5))); // Sunday
        assert!(!s.matches(at(2026, 4, 6, 9, 5))); // April
    }

    #[test]
    fn value_with_step_runs_to_the_maximum() {
        let s = cron("50/5 * * * *");
        assert!(s.matches(at(2026, 1, 1, 0, 50)));
        assert!(s.matches(at(2026, 1, 1, 0, 55)));
        assert!(!s.matches(at(2026, 1, 1, 0, 0)));
    }

    #[test]
    fn sunday_is_zero_or_seven() {
        let zero = cron("0 0 * * 0");
        let seven = cron("0 0 * * 7");
        let sunday = at(2026, 11, 8, 0, 0);
        assert!(zero.matches(sunday));
        assert!(seven.matches(sunday));
        assert!(cron("0 0 * * 5-7").matches(sunday));
    }

    #[test]
    fn restricted_day_fields_match_either() {
        // The 13th, or any Friday.
        let s = cron("0 0 13 * FRI");
        assert!(s.matches(at(2026, 11, 13, 0, 0))); // Friday 13th
        assert!(s.matches(at(2026, 11, 6, 0, 0))); // Friday
        assert!(s.matches(at(2026, 10, 13, 0, 0))); // Tuesday 13th
        assert!(!s.matches(at(2026, 11, 7, 0, 0)));
        // A starred day-of-month keeps AND: only Fridays.
        let starred = cron("0 0 */1 * FRI");
        assert!(!starred.matches(at(2026, 10, 13, 0, 0)));
        assert!(starred.matches(at(2026, 11, 6, 0, 0)));
    }

    #[test]
    fn leap_day_waits_for_a_leap_year() {
        let s = cron("0 12 29 2 *");
        assert_eq!(
            s.next_after(at(2026, 3, 1, 0, 0)),
            Some(at(2028, 2, 29, 12, 0))
        );
        // 2100 is not a leap year.
        assert_eq!(
            s.next_after(at(2096, 3, 1, 0, 0)),
            Some(at(2104, 2, 29, 12, 0))
        );
        assert_eq!(
            s.latest_at_or_before(at(2104, 2, 28, 0, 0)),
            Some(at(2096, 2, 29, 12, 0))
        );
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "*/60 * * * *",
            "5-1 * * * *",
            "1,,2 * * * *",
            "* * * * MON-",
            "* * L * *",
            "* * * * 5#2",
            "@daily",
            "0 0 30 2 *",
            "0 0 31 4,6,9,11 *",
            "* * * FOO *",
            "* * * * JAN",
            "001 * * * *",
            "*/1/2 * * * *",
            "5-*/2 * * * *",
            "é * * * *",
        ] {
            assert!(
                CronSchedule::parse(bad).is_err(),
                "`{bad}` should be rejected"
            );
        }
        let long = format!("{} * * * *", "1,".repeat(70));
        assert!(CronSchedule::parse(&long).is_err());
    }

    #[test]
    fn impossible_month_day_with_weekday_still_fires() {
        // Restricted weekday makes it an OR, so Mondays in February fire.
        let s = cron("0 0 30 2 MON");
        assert!(s.next_after(at(2026, 1, 1, 0, 0)).is_some());
    }

    #[test]
    fn fast_search_agrees_with_minute_by_minute_scan() {
        let schedules = [
            "*/7 * * * *",
            "13 */5 * * *",
            "0 0 1,15 * *",
            "30 6 * * 1-5",
            "0 0 13 * 5",
            "59 23 31 * *",
            "0 0 * 2 0",
        ];
        let starts = [
            at(2026, 11, 4, 2, 59),
            at(2026, 12, 31, 23, 59),
            at(2028, 2, 28, 23, 0),
            UnixMinute(-1),
        ];
        for text in schedules {
            let s = cron(text);
            for start in starts {
                let fast = s.next_after(start);
                let slow = brute_next(&s, start, 400 * MINUTES_PER_DAY);
                assert_eq!(fast, slow, "{text} after {start:?}");
                if let Some(next) = fast {
                    assert_eq!(
                        s.latest_at_or_before(next),
                        Some(next),
                        "{text} latest at {next:?}"
                    );
                    assert!(s
                        .latest_at_or_before(UnixMinute(next.0 - 1))
                        .is_some_and(|prev| prev <= start));
                }
            }
        }
    }

    #[test]
    fn calendar_matches_chrono() {
        for days in (-800_000i64..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            let date = chrono::DateTime::from_timestamp(days * 86_400, 0)
                .map(|dt| dt.date_naive())
                .unwrap_or_default();
            use chrono::Datelike as _;
            assert_eq!(
                (y, m, d),
                (i64::from(date.year()), date.month(), date.day())
            );
            assert_eq!(
                weekday_from_days(days),
                date.weekday().num_days_from_sunday()
            );
        }
    }

    #[test]
    fn unix_minute_floors() {
        assert_eq!(UnixMinute::from_unix_ms(119_999), UnixMinute(1));
        assert_eq!(UnixMinute::from_unix_ms(-1), UnixMinute(-1));
        assert_eq!(UnixMinute(2).to_unix_ms(), Some(120_000));
        assert_eq!(UnixMinute(i64::MAX).to_unix_ms(), None);
    }

    #[test]
    fn serde_round_trips_the_normalized_source() {
        let s = cron("0  0 * * *");
        let json = serde_json::to_string(&s).unwrap_or_default();
        assert_eq!(json, "\"0 0 * * *\"");
        let back: CronSchedule = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, s);
        assert!(serde_json::from_str::<CronSchedule>("\"nope\"").is_err());
        assert_eq!(String::from(s.clone()), "0 0 * * *");
        assert_eq!(s.to_string(), "0 0 * * *");
        assert_eq!("0 0 * * *".parse::<CronSchedule>().ok(), Some(s));
    }
}
