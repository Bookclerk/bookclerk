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

/// Replace a 64-hex session challenge and the challenge env name.
///
/// Non-hex bytes are copied as Unicode scalar values. A multibyte character
/// is never widened by casting each byte to `char`.
#[must_use]
pub fn redact_diagnostic_text(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let bytes = message.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_hexdigit() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
                index += 1;
            }
            if index - start == 64 {
                out.push_str("[redacted]");
            } else {
                out.push_str(&message[start..index]);
            }
            continue;
        }
        let Some(ch) = message[index..].chars().next() else {
            break;
        };
        out.push(ch);
        index += ch.len_utf8();
    }
    out.replace("BOOKCLERK_SESSION_CHALLENGE", "[redacted-env]")
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
    let capped = truncate_utf8(message, SPAWN_DIAG_RECORD_BYTES);
    let redacted = redact_diagnostic_text(capped);
    truncate_utf8(&redacted, SPAWN_DIAG_RECORD_BYTES).to_string()
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
