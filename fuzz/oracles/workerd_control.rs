//! Shared oracle for the `workerd_control` fuzz target and the
//! `bookclerk-workerd` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! On Windows the launcher learns the bridge RPC port from workerd's stdout
//! (`--control-fd=1`), which plugin isolates write to as well. The input is
//! one stdout line. Invariants, checked against an independent reading with
//! `serde_json::Value`:
//!
//! 1. Whenever [`listen_port`] returns a port, the line is a JSON object with
//!    exactly the keys `event` (`"listen"`), `socket` (the asked name) and
//!    `port` (an integer from 1 to 65535), and the port matches.
//! 2. Every object the reading accepts is accepted by the parser once written
//!    out canonically (no duplicate keys, no extra whitespace).
//! 3. No structured log line ever yields a port: the input wrapped as a
//!    `console.*` message, the way workerd prints isolate output when
//!    structured logging is on, always reads as `None`.

use bookclerk_workerd::control::{listen_port, MAX_CONTROL_LINE_BYTES, RPC_SOCKET};
use serde_json::Value;

/// Largest input worth checking: a little past the parser's line cap.
pub const MAX_INPUT_BYTES: usize = MAX_CONTROL_LINE_BYTES + 64;

/// The port the independent reading finds in `value` for `socket`, if any.
fn oracle_port(value: &Value, socket: &str) -> Option<u16> {
    let object = value.as_object()?;
    if object.len() != 3 {
        return None;
    }
    if object.get("event")?.as_str()? != "listen" || object.get("socket")?.as_str()? != socket {
        return None;
    }
    let port = u16::try_from(object.get("port")?.as_u64()?).ok()?;
    (port != 0).then_some(port)
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - One workerd stdout line, without its terminator.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
// Oracle code: encoding a `serde_json::Value` cannot fail, and a panic here is
// the finding, never an input the parser must tolerate.
#[allow(clippy::expect_used)]
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let value: Option<Value> = serde_json::from_slice(data).ok();
    for socket in [RPC_SOCKET, "other"] {
        let parsed = listen_port(data, socket).map(std::num::NonZeroU16::get);
        let expected = value.as_ref().and_then(|v| oracle_port(v, socket));
        if let Some(port) = parsed {
            assert!(data.len() <= MAX_CONTROL_LINE_BYTES);
            assert_eq!(
                Some(port),
                expected,
                "parser accepted what the oracle refuses"
            );
        }
        if let (Some(port), Some(value)) = (expected, value.as_ref()) {
            let canonical = serde_json::to_vec(value).expect("re-encode a parsed value");
            if canonical.len() <= MAX_CONTROL_LINE_BYTES {
                assert_eq!(
                    listen_port(&canonical, socket).map(std::num::NonZeroU16::get),
                    Some(port),
                    "parser refused a canonical listen event"
                );
            }
        }
    }

    let message = String::from_utf8_lossy(data);
    for level in ["log", "info", "warn", "error", "debug"] {
        let wrapped = serde_json::json!({ "timestamp": 0, "level": level, "message": message });
        let line = serde_json::to_vec(&wrapped).expect("encode a log line");
        assert_eq!(
            listen_port(&line, RPC_SOCKET),
            None,
            "structured log line read as a control message"
        );
    }
}
