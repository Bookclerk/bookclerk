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
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader, Lines};

use crate::config::ListenSpec;

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

/// Learns the bridge RPC port of one workerd child and forwards its stdout to tracing.
///
/// # Arguments
///
/// * `listen` - Listen mode the child's config was generated with.
/// * `stdout` - The child's stdout; it carries the control channel when
///   `listen` is [`ListenSpec::ReportedTcp`].
/// * `timeout` - How long workerd gets to report its port.
///
/// # Returns
///
/// The port from `listen` when it is fixed, otherwise the port workerd
/// reported for [`RPC_SOCKET`]. Every other stdout line, before and after the
/// report, is logged under the `workerd` target.
///
/// # Errors
///
/// For [`ListenSpec::ReportedTcp`]: when `stdout` is `None`, when it ends or
/// fails before the report (workerd exited, for example because its bind
/// failed), or when nothing arrives within `timeout`.
pub async fn bridge_port<R>(
    listen: &ListenSpec,
    stdout: Option<R>,
    timeout: Duration,
) -> Result<u16>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    if let Some(port) = listen.port() {
        forward_lines(stdout);
        return Ok(port);
    }
    let stdout = stdout.context("workerd stdout is not piped")?;
    let mut lines = BufReader::new(stdout).lines();
    let port = tokio::time::timeout(timeout, read_listen_port(&mut lines))
        .await
        .context("timeout waiting for workerd to report the bridge RPC port")??;
    tokio::spawn(forward_remaining(lines));
    Ok(port)
}

/// Forwards one workerd output stream to tracing, line by line, on a spawned task.
///
/// # Arguments
///
/// * `stream` - The child's stdout or stderr; `None` when it was not piped.
pub fn forward_lines<R>(stream: Option<R>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    if let Some(stream) = stream {
        tokio::spawn(forward_remaining(BufReader::new(stream).lines()));
    }
}

/// Logs every remaining line of `lines` under the `workerd` target until it ends.
async fn forward_remaining<R>(mut lines: Lines<BufReader<R>>)
where
    R: AsyncRead + Unpin,
{
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(target: "workerd", "{line}");
    }
}

/// Reads lines until one reports the [`RPC_SOCKET`] port, logging the others.
///
/// # Errors
///
/// When the stream ends or fails first.
async fn read_listen_port<R>(lines: &mut Lines<BufReader<R>>) -> Result<u16>
where
    R: AsyncRead + Unpin,
{
    while let Some(line) = lines
        .next_line()
        .await
        .context("read workerd control output")?
    {
        if let Some(port) = listen_port(line.as_bytes(), RPC_SOCKET) {
            return Ok(port.get());
        }
        tracing::info!(target: "workerd", "{line}");
    }
    bail!("workerd exited before reporting the bridge RPC port; its log above says why")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generous bound for tests whose stream already holds the report.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn stream(text: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(text.as_bytes().to_vec())
    }

    #[tokio::test]
    async fn fixed_ports_need_no_report() {
        let port = bridge_port(
            &ListenSpec::InheritedTcp { port: 9 },
            Some(stream("not a control line\n")),
            TEST_TIMEOUT,
        )
        .await
        .expect("fixed port");
        assert_eq!(port, 9);
        let port = bridge_port(
            &ListenSpec::TcpLoopback(10),
            None::<std::io::Cursor<Vec<u8>>>,
            TEST_TIMEOUT,
        )
        .await
        .expect("fixed port without stdout");
        assert_eq!(port, 10);
    }

    #[tokio::test]
    async fn reported_port_comes_after_log_lines() {
        let output = concat!(
            r#"{"timestamp":0,"level":"log","message":"{\"event\":\"listen\",\"socket\":\"rpc\",\"port\":1}"}"#,
            "\n",
            r#"{"event":"listen","socket":"granted","port":2}"#,
            "\r\n",
            r#"{"event":"listen","socket":"rpc","port":53117}"#,
            "\n",
            "trailing log line\n",
        );
        let port = bridge_port(&ListenSpec::ReportedTcp, Some(stream(output)), TEST_TIMEOUT)
            .await
            .expect("reported port");
        assert_eq!(port, 53117);
    }

    #[tokio::test]
    async fn exit_before_the_report_fails_at_once() {
        let output = "Socket \"rpc\": bind(): forbidden (os error 10013)\n";
        let err = bridge_port(&ListenSpec::ReportedTcp, Some(stream(output)), TEST_TIMEOUT)
            .await
            .expect_err("no report");
        assert!(
            format!("{err:#}").contains("exited before reporting"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn silence_times_out() {
        // The write half stays open, so the read neither ends nor yields.
        let (_writer, reader) = tokio::io::duplex(64);
        let err = bridge_port(
            &ListenSpec::ReportedTcp,
            Some(reader),
            Duration::from_millis(20),
        )
        .await
        .expect_err("timeout");
        assert!(format!("{err:#}").contains("timeout waiting"), "{err:#}");
    }

    #[tokio::test]
    async fn reported_port_needs_stdout() {
        let err = bridge_port(
            &ListenSpec::ReportedTcp,
            None::<std::io::Cursor<Vec<u8>>>,
            TEST_TIMEOUT,
        )
        .await
        .expect_err("no stdout");
        assert!(format!("{err:#}").contains("not piped"), "{err:#}");
    }

    #[tokio::test]
    async fn forward_lines_drains_a_stream() {
        forward_lines(Some(stream("one\ntwo\n")));
        forward_lines(None::<std::io::Cursor<Vec<u8>>>);
        tokio::task::yield_now().await;
    }

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
