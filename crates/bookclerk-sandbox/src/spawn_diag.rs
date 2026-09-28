//! Bounded, secret-free startup lines for spawn deadlines.
//!
//! Recording is separate from emission. [`record_spawn_diagnostic`] always
//! keeps a byte-capped copy and emits a `bookclerk::spawn` tracing event.
//! Raw stderr is only for an explicit diagnostic mode, or for a jailed child
//! whose stderr is a pipe the host forwarder turns back into tracing.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::SPEC_ENV;

/// Maximum bytes stored for one diagnostic line, cut on a UTF-8 boundary.
pub const SPAWN_DIAG_RECORD_BYTES: usize = 512;

/// Maximum bytes retained in the process-wide diagnostic ring.
pub const SPAWN_DIAG_TOTAL_BYTES: usize = 16 * 1024;

/// Maximum lines in that ring. The byte cap is the budget; this only stops a
/// flood of tiny lines.
pub const SPAWN_DIAG_MAX_LINES: usize = 80;

/// Process-wide stage ring. Guest plugin stderr is not written here.
static SPAWN_DIAG: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// `true` when `BOOKCLERK_SPAWN_DIAG` is `1`, `true`, or `stderr`.
#[must_use]
pub fn spawn_diag_stderr_enabled() -> bool {
    diagnostic_stderr(std::env::var("BOOKCLERK_SPAWN_DIAG").ok().as_deref())
}

/// `true` when raw stage lines should be written to this process's stderr.
///
/// Diagnostic mode is explicit. A jailed child also prints, because its
/// stderr is the host's capture pipe rather than the daemon log. The host
/// forwarder re-emits those lines as tracing and does not treat them as a
/// second daemon sink.
#[must_use]
pub fn spawn_diag_raw_stderr() -> bool {
    spawn_diag_stderr_enabled() || std::env::var_os(SPEC_ENV).is_some()
}

/// Copy `text` up to `max_bytes` without splitting a scalar value.
///
/// A 4-byte cap of `café` is `caf`. The truncated text never reinterprets the
/// leftover bytes of `é` as Latin-1.
#[must_use]
pub fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Session-challenge environment name replaced in diagnostic text.
const CHALLENGE_ENV_NAME: &str = "BOOKCLERK_SESSION_CHALLENGE";

/// Replacement for one 64-hex session challenge.
const REDACTED_CHALLENGE: &str = "[redacted]";

/// Replacement for [`CHALLENGE_ENV_NAME`].
const REDACTED_ENV: &str = "[redacted-env]";

/// A session challenge is exactly this many ASCII hex digits.
const CHALLENGE_HEX_LEN: usize = 64;

/// Replace a 64-hex session challenge and the challenge env name.
///
/// Non-hex bytes are copied as Unicode scalar values. A multibyte character
/// is never widened by casting each byte to `char`. Diagnostic retention uses
/// [`redact_capped`] so a token that crosses the byte cap is recognized before
/// any of its prefix is stored.
#[must_use]
pub fn redact_diagnostic_text(message: &str) -> String {
    redact_capped(message, message.len())
}

/// Redact secrets and keep at most `max_bytes` of the result.
///
/// A 64-hex challenge or `BOOKCLERK_SESSION_CHALLENGE` that starts inside the cap and
/// continues past it is classified with a bounded lookahead, then replaced.
/// No prefix of that token is emitted. Bytes after the cap are not copied.
/// The cut stays on a UTF-8 boundary.
#[must_use]
pub fn redact_capped(message: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    if max_bytes == 0 {
        return out;
    }
    out.reserve(max_bytes.min(message.len()));
    let bytes = message.as_bytes();
    let mut index = 0;
    while index < bytes.len() && index < max_bytes && out.len() < max_bytes {
        if message[index..].starts_with(CHALLENGE_ENV_NAME) {
            push_marker(&mut out, REDACTED_ENV, max_bytes);
            index += CHALLENGE_ENV_NAME.len();
            if index > max_bytes {
                break;
            }
            continue;
        }
        if bytes[index].is_ascii_hexdigit() {
            let run = hex_run_len(bytes, index);
            if run == CHALLENGE_HEX_LEN {
                push_marker(&mut out, REDACTED_CHALLENGE, max_bytes);
                index += CHALLENGE_HEX_LEN;
                if index > max_bytes {
                    break;
                }
                continue;
            }
            let emit_end = (index + run).min(max_bytes);
            let room = max_bytes - out.len();
            let take = (emit_end - index).min(room);
            out.push_str(&message[index..index + take]);
            index += take;
            continue;
        }
        let Some(ch) = message[index..].chars().next() else {
            break;
        };
        let len = ch.len_utf8();
        if index + len > max_bytes || out.len() + len > max_bytes {
            break;
        }
        out.push(ch);
        index += len;
    }
    out
}

/// Append as much of `marker` as fits in `max_bytes` without splitting it.
fn push_marker(out: &mut String, marker: &str, max_bytes: usize) {
    let room = max_bytes.saturating_sub(out.len());
    out.push_str(truncate_utf8(marker, room));
}

/// Length of the ASCII hex run at `start`, capped at 65.
///
/// Sixty-five means the run is longer than a session challenge, so it is not
/// redacted. The scan does not copy the run.
fn hex_run_len(bytes: &[u8], start: usize) -> usize {
    let end = (start + CHALLENGE_HEX_LEN + 1).min(bytes.len());
    let mut index = start;
    while index < end && bytes[index].is_ascii_hexdigit() {
        index += 1;
    }
    index - start
}

/// Remember one already-formatted line and emit it.
///
/// The ring and the tracing event are capped. Raw stderr follows
/// [`spawn_diag_raw_stderr`].
pub fn record_spawn_diagnostic(message: &str) {
    let line = bounded_line(message);
    remember_line(&line);
    emit_line(&line);
}

/// Push `line` onto `ring`, dropping the oldest entries until both caps hold.
pub fn push_capped_line(
    ring: &mut VecDeque<String>,
    line: &str,
    record_bytes: usize,
    total_bytes: usize,
    max_lines: usize,
) {
    let line = truncate_utf8(line, record_bytes.min(total_bytes)).to_string();
    ring.push_back(line);
    while ring.len() > max_lines || byte_len(ring) > total_bytes {
        if ring.pop_front().is_none() {
            break;
        }
    }
}

/// Join `ring` and cap the copy at `total_bytes`.
#[must_use]
pub fn join_capped_lines(ring: &VecDeque<String>, total_bytes: usize) -> String {
    let joined = ring.iter().cloned().collect::<Vec<_>>().join("\n");
    truncate_utf8(&joined, total_bytes).to_string()
}

/// Byte-capped snapshot of [`record_spawn_diagnostic`] lines.
#[must_use]
pub fn snapshot_spawn_diagnostics() -> String {
    let ring = SPAWN_DIAG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    join_capped_lines(&ring, SPAWN_DIAG_TOTAL_BYTES)
}

/// Redact and cap one line before it is stored or emitted.
fn bounded_line(message: &str) -> String {
    redact_capped(message, SPAWN_DIAG_RECORD_BYTES)
}

/// Push `line` onto the process ring, dropping oldest entries past the caps.
fn remember_line(line: &str) {
    let mut ring = SPAWN_DIAG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    push_capped_line(
        &mut ring,
        line,
        SPAWN_DIAG_RECORD_BYTES,
        SPAWN_DIAG_TOTAL_BYTES,
        SPAWN_DIAG_MAX_LINES,
    );
}

/// Emit `line` as a tracing event, and to stderr only in diagnostic mode
/// or from a captured jail child.
fn emit_line(line: &str) {
    tracing::info!(target: "bookclerk::spawn", "{line}");
    if spawn_diag_raw_stderr() {
        eprintln!("{line}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

/// `true` for the explicit `BOOKCLERK_SPAWN_DIAG` values.
fn diagnostic_stderr(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true") | Some("stderr"))
}

/// Sum of stored line lengths, excluding the newlines a snapshot inserts.
fn byte_len(ring: &VecDeque<String>) -> usize {
    ring.iter().map(String::len).sum()
}

#[cfg(test)]
#[allow(clippy::missing_docs_in_private_items)]
mod tests {
    use super::*;

    #[test]
    fn truncate_and_redact_keep_multibyte_text() {
        assert_eq!(truncate_utf8("café", 5), "café");
        assert_eq!(truncate_utf8("café", 4), "caf");
        assert!(!truncate_utf8("café", 4).contains('Ã'));
        assert!(!truncate_utf8("café", 4).contains('©'));
        let challenge = "ab".repeat(32);
        let line = redact_diagnostic_text(&format!("café BOOKCLERK_SESSION_CHALLENGE={challenge}"));
        assert!(line.contains("café"));
        assert!(line.contains("[redacted]"));
        assert!(line.contains("[redacted-env]"));
        assert!(!line.contains(&challenge));
        assert!(!line.contains('Ã'));
    }

    #[test]
    fn diagnostic_stderr_is_opt_in() {
        assert!(!diagnostic_stderr(None));
        assert!(!diagnostic_stderr(Some("0")));
        assert!(!diagnostic_stderr(Some("")));
        assert!(diagnostic_stderr(Some("1")));
        assert!(diagnostic_stderr(Some("true")));
        assert!(diagnostic_stderr(Some("stderr")));
    }

    #[test]
    fn redact_capped_recognizes_tokens_that_cross_the_cutoff() {
        let secret = "ab".repeat(32);
        assert_eq!(secret.len(), 64);
        let across = format!("{}{secret}", "x".repeat(449));
        assert_eq!(across.len(), 513);
        let across_out = redact_capped(&across, SPAWN_DIAG_RECORD_BYTES);
        assert_eq!(
            across_out,
            format!("{}[redacted]", "x".repeat(449)),
            "a 64-hex challenge starting at byte 449 must not keep a 63-char prefix"
        );
        assert!(across_out.len() <= SPAWN_DIAG_RECORD_BYTES);
        assert!(!across_out.contains(&secret[..63]));

        let before = format!("before {secret} after");
        assert_eq!(
            redact_capped(&before, SPAWN_DIAG_RECORD_BYTES),
            "before [redacted] after"
        );

        let after = format!("{}{secret}", "x".repeat(SPAWN_DIAG_RECORD_BYTES));
        let after_out = redact_capped(&after, SPAWN_DIAG_RECORD_BYTES);
        assert_eq!(after_out, "x".repeat(SPAWN_DIAG_RECORD_BYTES));
        assert!(!after_out.contains(&secret[..16]));
        assert!(!after_out.contains("[redacted]"));

        let hex63 = format!("{}c", "ab".repeat(31));
        assert_eq!(hex63.len(), 63);
        let not_a_challenge = format!("{}{hex63}", "x".repeat(449));
        assert_eq!(
            redact_capped(&not_a_challenge, SPAWN_DIAG_RECORD_BYTES),
            not_a_challenge
        );

        let hex65 = format!("{}e", "ab".repeat(32));
        assert_eq!(hex65.len(), 65);
        let longer = format!("{}{hex65}", "q".repeat(449));
        let longer_out = redact_capped(&longer, SPAWN_DIAG_RECORD_BYTES);
        assert!(longer_out.contains(&hex65[..16]), "{longer_out}");
        assert!(!longer_out.contains("[redacted]"));
        assert!(longer_out.len() <= SPAWN_DIAG_RECORD_BYTES);

        let multibyte = format!("{}{secret}", "café".repeat(100));
        assert_eq!("café".len() * 100, 500);
        let multibyte_out = redact_capped(&multibyte, SPAWN_DIAG_RECORD_BYTES);
        assert!(multibyte_out.contains("café"));
        assert!(multibyte_out.contains("[redacted]"));
        assert!(!multibyte_out.contains(&secret));
        assert!(!multibyte_out.contains('Ã'));
        assert!(multibyte_out.len() <= SPAWN_DIAG_RECORD_BYTES);

        let env_at = format!("{}{CHALLENGE_ENV_NAME}", "x".repeat(500));
        let env_out = redact_capped(&env_at, SPAWN_DIAG_RECORD_BYTES);
        assert!(env_out.starts_with(&"x".repeat(500)), "{env_out}");
        assert!(env_out.contains("[redacted"));
        assert!(!env_out.contains("BOOKCLERK"));
        assert!(env_out.len() <= SPAWN_DIAG_RECORD_BYTES);

        let split_char = format!("{}é", "x".repeat(511));
        let split_out = redact_capped(&split_char, SPAWN_DIAG_RECORD_BYTES);
        assert_eq!(split_out, "x".repeat(511));
        assert!(!split_out.contains('Ã'));
        assert_eq!(redact_capped("café secret", 0), "");
    }

    #[test]
    fn recorded_diagnostics_redact_tokens_around_the_cutoff() {
        let cases = [
            ("before", format!("before {} after", "ab".repeat(32)), true),
            (
                "across",
                format!("{}{}", "x".repeat(449), "cd".repeat(32)),
                true,
            ),
            (
                "after",
                format!("{}{}", "x".repeat(SPAWN_DIAG_RECORD_BYTES), "ef".repeat(32)),
                false,
            ),
            (
                "env",
                format!("{}{CHALLENGE_ENV_NAME}", "n".repeat(500)),
                true,
            ),
            (
                "multibyte",
                format!("{}{}", "café".repeat(100), "12".repeat(32)),
                true,
            ),
        ];
        for (label, message, expect_marker) in cases {
            let secret = secret_in(&message);
            let (event, snap) = capture_recorded(&message);
            let direct = redact_capped(&message, SPAWN_DIAG_RECORD_BYTES);
            assert!(
                direct.len() <= SPAWN_DIAG_RECORD_BYTES,
                "{label} redaction grew past the cap: {}",
                direct.len()
            );
            assert!(
                event.contains(&direct),
                "{label} emission did not contain the capped redaction\nevent: {event}\ndirect: {direct}"
            );
            assert!(snap.len() <= SPAWN_DIAG_TOTAL_BYTES, "{label}");
            assert!(!event.contains('Ã'), "{label}: {event}");
            assert!(!snap.contains('Ã'), "{label}");
            if expect_marker {
                assert!(
                    event.contains("[redacted"),
                    "{label} emission dropped the marker: {event}"
                );
            }
            if let Some(secret) = secret {
                let prefix = &secret[..secret.len().min(16)];
                assert!(
                    !event.contains(prefix),
                    "{label} emission kept a challenge prefix ({} bytes)",
                    event.len()
                );
                assert!(
                    !snap.contains(prefix),
                    "{label} snapshot kept a challenge prefix ({} bytes)",
                    snap.len()
                );
                assert!(
                    !event.contains(&secret),
                    "{label} emission kept the challenge ({} bytes)",
                    event.len()
                );
                assert!(
                    !snap.contains(&secret),
                    "{label} snapshot kept the challenge ({} bytes)",
                    snap.len()
                );
            }
        }

        let body63 = format!("{}c", "ab".repeat(31));
        let hex63 = format!("{}{body63}", "x".repeat(449));
        let (event, snap) = capture_recorded(&hex63);
        assert!(event.contains(&body63), "{event}");
        assert!(!event.contains("[redacted]"), "{event}");
        assert!(
            !snap.contains(&"ab".repeat(32)),
            "snapshot stored a 64-hex challenge while recording a 63-hex run"
        );
    }

    /// 64-hex run, or the challenge env name, when that token is in `message`.
    fn secret_in(message: &str) -> Option<String> {
        if let Some(start) = message.find(CHALLENGE_ENV_NAME) {
            return Some(message[start..start + CHALLENGE_ENV_NAME.len()].to_string());
        }
        let bytes = message.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index].is_ascii_hexdigit() {
                let run = hex_run_len(bytes, index);
                if run == 64 {
                    return Some(message[index..index + 64].to_string());
                }
                index += run.max(1);
                continue;
            }
            index += 1;
        }
        None
    }

    fn capture_recorded(message: &str) -> (String, String) {
        let mut event = String::new();
        for _ in 0..40 {
            let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let subscriber = SpawnCapture(std::sync::Arc::clone(&lines));
            tracing::subscriber::with_default(subscriber, || {
                tracing::callsite::rebuild_interest_cache();
                record_spawn_diagnostic(message);
            });
            event = lines
                .lock()
                .expect("spawn events")
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            if !event.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (event, snapshot_spawn_diagnostics())
    }

    struct SpawnCapture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    struct SpawnVisit<'a>(&'a mut String);

    impl tracing::field::Visit for SpawnVisit<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.0.clear();
                self.0.push_str(value);
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" && self.0.is_empty() {
                use std::fmt::Write;
                let _ = write!(self.0, "{value:?}");
            }
        }
    }

    impl tracing::Subscriber for SpawnCapture {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == "bookclerk::spawn"
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut message = String::new();
            event.record(&mut SpawnVisit(&mut message));
            self.0.lock().expect("spawn events").push(message);
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[test]
    fn ring_snapshot_stays_inside_the_byte_budget() {
        for index in 0..100 {
            record_spawn_diagnostic(&format!("café-{index}-{}", "y".repeat(4_000)));
        }
        let snap = snapshot_spawn_diagnostics();
        assert!(snap.len() <= SPAWN_DIAG_TOTAL_BYTES);
        assert!(snap.contains("café"));
        assert!(!snap.contains(&"y".repeat(600)));
        assert!(!snap.contains('Ã'));
    }
}
