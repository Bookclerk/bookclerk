//! Reading the level back out of a helper's or guest's stderr line.
//!
//! The host relays every line a jailed helper (`bookclerk-workerd`,
//! `bookclerk-jail`) or guest writes to stderr into its own log. Bookclerk's
//! helpers write tracing's text format (`<timestamp>  <LEVEL> <target>:
//! <message>`), so a relayed line used to carry a second timestamp and
//! level inside an INFO record of the host's, and a helper's ERROR never
//! counted as one.
//!
//! [`parse_relayed_line`] splits off that prefix. The bytes come from a
//! jailed process, so this is an untrusted-input boundary: the parser is
//! sans-IO, total (any input gives a result), and never raises a line above
//! [`RelayedLevel::Warn`], so a guest cannot forge an ERROR that triggers a
//! diagnostics upload. The `relayed_log_line` fuzz target checks it
//! (`docs/contributing/fuzzing.md`).

/// The level a relayed line is logged at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayedLevel {
    /// `TRACE`.
    Trace,
    /// `DEBUG`.
    Debug,
    /// `INFO`, and every line without a level prefix.
    Info,
    /// `WARN`, and also a line that said `ERROR` (relayed lines are capped
    /// here).
    Warn,
}

/// One relayed line: its level and its text without the timestamp and level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayedLine<'a> {
    /// The level to log it at.
    pub level: RelayedLevel,
    /// The text after the prefix (usually `<target>: <message> <fields>`),
    /// or the whole line, trimmed, when it had no prefix.
    pub text: &'a str,
}

/// Splits a tracing-format prefix (optional timestamp, then a level word)
/// off a relayed stderr line.
///
/// A line whose first word (after an optional timestamp, which is a word
/// starting with a digit and ending in `Z`) is not exactly one of `TRACE`,
/// `DEBUG`, `INFO`, `WARN` or `ERROR` is returned whole, at
/// [`RelayedLevel::Info`].
///
/// # Arguments
///
/// * `line` - One stderr line, ANSI escapes already stripped.
///
/// # Returns
///
/// The level and the remaining text, which is always a suffix of `line`
/// with its trailing whitespace removed.
#[must_use]
pub fn parse_relayed_line(line: &str) -> RelayedLine<'_> {
    let whole = RelayedLine {
        level: RelayedLevel::Info,
        text: line.trim(),
    };
    let mut rest = line.trim_start();
    let (first, after) = split_word(rest);
    if is_timestamp(first) {
        rest = after.trim_start();
    }
    let (word, after) = split_word(rest);
    let level = match word {
        "TRACE" => RelayedLevel::Trace,
        "DEBUG" => RelayedLevel::Debug,
        "INFO" => RelayedLevel::Info,
        "WARN" | "ERROR" => RelayedLevel::Warn,
        _ => return whole,
    };
    RelayedLine {
        level,
        text: after.trim(),
    }
}

/// The first whitespace-free word of `s` and what follows it.
fn split_word(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(end) => s.split_at(end),
        None => (s, ""),
    }
}

/// Whether `word` looks like tracing's RFC 3339 UTC timestamp.
fn is_timestamp(word: &str) -> bool {
    word.len() >= 11
        && word.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && word.ends_with('Z')
        && word.contains('T')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_helper_line_loses_its_timestamp_and_keeps_its_level() {
        let line =
            "2026-11-20T13:33:28.005681Z  INFO bookclerk_workerd: starting isolate plugin=covers";
        assert_eq!(
            parse_relayed_line(line),
            RelayedLine {
                level: RelayedLevel::Info,
                text: "bookclerk_workerd: starting isolate plugin=covers",
            }
        );
        let warn = parse_relayed_line("2026-11-20T13:33:28Z  WARN x: y");
        assert_eq!(warn.level, RelayedLevel::Warn);
        assert_eq!(warn.text, "x: y");
        assert_eq!(
            parse_relayed_line("DEBUG target: msg").level,
            RelayedLevel::Debug
        );
    }

    #[test]
    fn error_is_capped_at_warn() {
        let line = parse_relayed_line("2026-11-20T13:33:28Z ERROR guest: boom");
        assert_eq!(line.level, RelayedLevel::Warn);
        assert_eq!(line.text, "guest: boom");
    }

    #[test]
    fn other_lines_pass_whole_at_info() {
        for line in [
            "bookclerk-jail: plugin:pk [landlock+seccomp]: filesystem=enforced",
            "  plain text  ",
            "",
            "2026-11-20T13:33:28Z",
            "2026-11-20T13:33:28Z info lower-case is not a level",
            "INFORMATION is not INFO",
        ] {
            let parsed = parse_relayed_line(line);
            assert_eq!(parsed.level, RelayedLevel::Info, "{line}");
            assert_eq!(parsed.text, line.trim(), "{line}");
        }
    }
}
