//! Shared oracle for the `relayed_log_line` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A jailed helper's or guest's stderr line reaches
//! [`parse_relayed_line`] when the host relays it into its log. The
//! invariants, checked against an independent token reading: the result's
//! text is a suffix of the line (trailing whitespace removed); the level is
//! never above WARN; a level other than INFO comes only from a line whose
//! first word, or second word after a timestamp-shaped first word, is
//! exactly that level's name (or `ERROR` for WARN); a line with no such word
//! is returned whole and trimmed at INFO; and parsing is deterministic.

use bookclerk_config::{parse_relayed_line, RelayedLevel};

/// Largest input worth checking (relayed lines are truncated to a few KiB).
pub const MAX_INPUT_BYTES: usize = 4096;

/// The level word the oracle reads at the start of `line`, independently of
/// the parser: the first word, or the second when the first starts with a
/// digit, ends in `Z`, contains `T` and is at least 11 bytes long.
fn level_word(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    let first = words.next()?;
    let candidate = if first.len() >= 11
        && first.starts_with(|c: char| c.is_ascii_digit())
        && first.ends_with('Z')
        && first.contains('T')
    {
        words.next()?
    } else {
        first
    };
    ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"]
        .contains(&candidate)
        .then_some(candidate)
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The stderr line's bytes (non-UTF-8 input is skipped, as the
///   host decodes lines before relaying them).
///
/// # Panics
///
/// When the parser breaks an invariant.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    // The host relays single lines.
    let line = line.split('\n').next().unwrap_or_default();
    let parsed = parse_relayed_line(line);
    assert!(
        line.trim_end().ends_with(parsed.text),
        "text is not a suffix of the trimmed line"
    );
    assert_eq!(parsed.text, parsed.text.trim(), "text is not trimmed");
    match level_word(line) {
        None => {
            assert_eq!(parsed.level, RelayedLevel::Info, "level from no level word");
            assert_eq!(parsed.text, line.trim(), "a line without a level lost text");
        }
        Some(word) => {
            let want = match word {
                "TRACE" => RelayedLevel::Trace,
                "DEBUG" => RelayedLevel::Debug,
                "INFO" => RelayedLevel::Info,
                _ => RelayedLevel::Warn,
            };
            assert_eq!(parsed.level, want, "level differs from the line's word");
            assert!(
                parsed.text.len() < line.trim().len(),
                "the level prefix was not removed"
            );
        }
    }
    assert_eq!(parse_relayed_line(line), parsed, "parsing twice differs");
}
