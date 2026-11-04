//! Shared oracle for the `cron_schedule` fuzz target and the
//! `bookclerk-plugin-manifest` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is a cron string from an untrusted `plugin.toml`, optionally
//! preceded by a start minute: when the input contains a `|`, the bytes
//! before it are a decimal minute on the Unix timeline and the rest is the
//! cron text. The invariants, checked against a minute-by-minute scan of
//! [`CronSchedule::matches`] as the independent oracle:
//!
//! - parsing never panics, and an accepted schedule's normalized source
//!   parses back to an equal schedule;
//! - `next_after(start)` is after `start`, fires, and no minute between
//!   them fires (scanned up to [`SCAN_MINUTES`]);
//! - `latest_at_or_before` of that fire is the fire itself, and of the
//!   minute before it is at or before `start`.

use bookclerk_plugin_manifest::{CronSchedule, UnixMinute, MAX_CRON_SOURCE_BYTES};

/// Largest input worth parsing: a start minute plus the longest cron.
pub const MAX_INPUT_BYTES: usize = MAX_CRON_SOURCE_BYTES + 32;

/// Minutes the slow oracle scans between `start` and the next fire.
pub const SCAN_MINUTES: i64 = 3 * 1440;

/// Start minute used when the input carries none (2026-11-04T00:00Z).
const DEFAULT_START: i64 = 29_896_560;

/// Bound on the start minute so the scans stay far from `i64` overflow.
const START_LIMIT: i64 = 1 << 40;

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (start, cron) = match text.split_once('|') {
        Some((minute, cron)) => match minute.trim().parse::<i64>() {
            Ok(minute) => (minute.clamp(-START_LIMIT, START_LIMIT), cron),
            Err(_) => return,
        },
        None => (DEFAULT_START, text),
    };
    let Ok(schedule) = CronSchedule::parse(cron) else {
        return;
    };
    let reparsed = CronSchedule::parse(schedule.as_str());
    assert_eq!(
        reparsed.as_ref().ok(),
        Some(&schedule),
        "normalized source `{}` did not parse back equal",
        schedule.as_str()
    );

    let start = UnixMinute(start);
    let next = schedule.next_after(start);
    let scan_end = start.0 + SCAN_MINUTES;
    let first_scanned = (start.0 + 1..=scan_end)
        .map(UnixMinute)
        .find(|m| schedule.matches(*m));
    match next {
        Some(next) => {
            assert!(next > start, "next_after went backwards");
            assert!(schedule.matches(next), "next_after returned a non-fire");
            if next.0 <= scan_end {
                assert_eq!(first_scanned, Some(next), "next_after skipped a fire");
            } else {
                assert_eq!(first_scanned, None, "next_after skipped a fire");
            }
            assert_eq!(
                schedule.latest_at_or_before(next),
                Some(next),
                "latest_at_or_before missed an exact fire"
            );
            if let Some(prev) = schedule.latest_at_or_before(UnixMinute(next.0 - 1)) {
                assert!(prev <= start, "a fire lies between start and next_after");
                assert!(
                    schedule.matches(prev),
                    "latest_at_or_before returned a non-fire"
                );
            }
        }
        None => assert_eq!(
            first_scanned, None,
            "next_after found nothing but a fire exists"
        ),
    }
}
