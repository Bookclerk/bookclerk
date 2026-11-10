//! Control messages workerd writes on `--control-fd`: the port each socket bound.
//!
//! Where the launcher cannot hand workerd an already-bound socket (Windows has
//! no `--socket-fd`), workerd binds the bridge RPC socket on `127.0.0.1:0`
//! itself and reports the port the operating system picked with one JSON line
//! per socket:
//!
//! ```text
//! {"event":"listen","socket":"rpc","port":53117}
//! ```
//!
//! The control channel is workerd's stdout, which isolate `console.*` output
//! shares. The generated config turns on structured logging whenever workerd
//! reports its port ([`crate::config::ListenSpec::ReportedTcp`]), so every
//! line an isolate can produce is a JSON object with `timestamp`, `level` and
//! `message` keys. [`listen_port`] accepts only an object with exactly the
//! three control keys, so isolate output can never be read as a control
//! message. The `workerd_control` fuzz target checks both properties.

use std::num::NonZeroU16;

use serde::Deserialize;

/// Name of the bridge RPC socket in the generated workerd config.
pub const RPC_SOCKET: &str = "rpc";

/// Longest control line worth parsing, in bytes; a `listen` message is under 100.
pub const MAX_CONTROL_LINE_BYTES: usize = 1024;

/// Event name workerd writes once a socket is listening.
const LISTEN_EVENT: &str = "listen";

/// One control message, with no key beyond the three workerd writes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlMessage {
    /// Event kind; only [`LISTEN_EVENT`] carries a port.
    event: String,
    /// Config name of the socket the event is about.
    socket: String,
    /// TCP port the socket bound.
    port: u16,
}

/// Reads the port `socket` is listening on from one workerd control line.
///
/// # Arguments
///
/// * `line` - One line of workerd's stdout, without the line terminator
///   (trailing ASCII whitespace, such as a Windows `\r`, is ignored).
/// * `socket` - Socket name from the generated config, usually [`RPC_SOCKET`].
///
/// # Returns
///
/// The bound port when `line` is a JSON object holding exactly `event`
/// (`"listen"`), `socket` (equal to `socket`) and a non-zero `port`, each key
/// once. `None` for every other line, including structured log lines, lines
/// over [`MAX_CONTROL_LINE_BYTES`] and other sockets' events.
#[must_use]
pub fn listen_port(line: &[u8], socket: &str) -> Option<NonZeroU16> {
    if line.len() > MAX_CONTROL_LINE_BYTES {
        return None;
    }
    // serde also reads a struct from a JSON array; workerd only writes objects.
    if line.trim_ascii_start().first() != Some(&b'{') {
        return None;
    }
    let message: ControlMessage = serde_json::from_slice(line).ok()?;
    if message.event != LISTEN_EVENT || message.socket != socket {
        return None;
    }
    NonZeroU16::new(message.port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_listen_event_for_the_named_socket() {
        let line = br#"{"event":"listen","socket":"rpc","port":53117}"#;
        assert_eq!(
            listen_port(line, RPC_SOCKET).map(NonZeroU16::get),
            Some(53117)
        );
        assert_eq!(listen_port(line, "other"), None);
    }

    #[test]
    fn ignores_a_trailing_carriage_return() {
        let line = b"{\"event\":\"listen\",\"socket\":\"rpc\",\"port\":80}\r";
        assert_eq!(listen_port(line, RPC_SOCKET).map(NonZeroU16::get), Some(80));
    }

    #[test]
    fn refuses_structured_log_lines_that_quote_a_listen_event() {
        // What workerd prints for `console.log` of a forged control line.
        let line = br#"{"timestamp":0,"level":"log","message":"{\"event\":\"listen\",\"socket\":\"rpc\",\"port\":1}"}"#;
        assert_eq!(listen_port(line, RPC_SOCKET), None);
    }

    #[test]
    fn refuses_extra_duplicate_or_wrong_keys() {
        for line in [
            &br#"{"event":"listen","socket":"rpc","port":1,"level":"log"}"#[..],
            br#"{"event":"listen","socket":"rpc","port":1,"port":2}"#,
            br#"{"event":"close","socket":"rpc","port":1}"#,
            br#"{"event":"listen","socket":"rpc","port":0}"#,
            br#"{"event":"listen","socket":"rpc","port":65536}"#,
            br#"{"event":"listen","socket":"rpc","port":1.0}"#,
            br#"{"event":"listen","socket":"rpc"}"#,
            br#"["listen","rpc",1]"#,
            b"Listening on 127.0.0.1:1",
        ] {
            assert_eq!(
                listen_port(line, RPC_SOCKET),
                None,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn refuses_overlong_lines() {
        let mut line = br#"{"event":"listen","socket":"rpc","port":1}"#.to_vec();
        line.resize(MAX_CONTROL_LINE_BYTES + 1, b' ');
        assert_eq!(listen_port(&line, RPC_SOCKET), None);
    }
}
