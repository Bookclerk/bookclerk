//! Tracing / logging initialization.
//!
//! Sinks:
//! - **stderr** (always): JSON for `bookclerkd`, text for the CLI when `-v` is
//!   set; filtered by `BOOKCLERK_LOG` / `RUST_LOG`
//! - **OS facility** (when available): journald / macOS os_log / Windows Event Log —
//!   same configured filter; Bookclerk does not manage log files or rotation
//! - **diagnostics ring buffer** (always): retains **all** levels through TRACE so
//!   crash / burst uploads include deep context even when stderr is quieter
//!
//! Records from the `log` crate reach the same sinks through a bridge that
//! drops what [`UNBRIDGED_LOG_TARGETS`] names: the WebSocket library traces
//! every frame's payload (in hex, which no registered secret matches) and
//! the request head of a handshake, and a frame or a header can carry a
//! bearer token.
//!
//! The CLI default filter is `off` so command tables stay human-readable.
//! `bookclerkd` defaults to JSON at `bookclerk=info,warn`.
//!
//! Stderr is written through a **non-blocking** worker thread. When the consumer
//! of stderr stalls (full pipe to a parent IDE/terminal capture), logging must
//! not park Tokio worker threads — that freezes accept and makes even `/health`
//! time out.

use std::borrow::Cow;
use std::io;
use std::io::IsTerminal;
use std::sync::Mutex;

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::diagnostics::{self, DiagnosticsHandle, DiagnosticsLayer};
use crate::journal::{OsLogFacility, OsLogLayer};
use crate::redact::RedactingWriter;
use crate::settings::DiagnosticsConfig;

/// `log` targets whose records reach no sink, by prefix: `tungstenite`
/// (and so `tungstenite::protocol` and `tungstenite::handshake`) writes
/// frame payloads, whole messages and handshake requests at TRACE.
pub const UNBRIDGED_LOG_TARGETS: [&str; 1] = ["tungstenite"];

/// True when records of `target` are dropped by the bridge.
fn is_unbridged(target: &str) -> bool {
    UNBRIDGED_LOG_TARGETS.iter().any(|prefix| {
        target
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// The `log` crate's logger: every record becomes a `tracing` event,
/// except those of [`UNBRIDGED_LOG_TARGETS`].
struct LogBridge(tracing_log::LogTracer);

impl log::Log for LogBridge {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        !is_unbridged(metadata.target()) && log::Log::enabled(&self.0, metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        if !is_unbridged(record.target()) {
            log::Log::log(&self.0, record);
        }
    }

    fn flush(&self) {}
}

/// Installs [`LogBridge`] as the process's `log` logger. A logger already
/// installed stays (a second call, or a host that set its own).
fn install_log_bridge() {
    if log::set_boxed_logger(Box::new(LogBridge(tracing_log::LogTracer::new()))).is_ok() {
        log::set_max_level(log::LevelFilter::Trace);
    }
}

/// Output format for the stderr sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-oriented text (CLI default).
    #[default]
    Text,
    /// Structured JSON (daemon default / log aggregators).
    Json,
}

/// Options for [`init_tracing_with`].
#[derive(Debug, Clone)]
pub struct TracingOptions {
    /// Stderr format (text vs JSON).
    pub format: LogFormat,
    /// Default `EnvFilter` directive when `BOOKCLERK_LOG` / `RUST_LOG` are unset.
    /// Affects stderr + OS facility only — the diagnostics ring always keeps TRACE.
    pub default_level: String,
    /// Identifier for journald / Event Log source / os_log category.
    pub syslog_identifier: String,
    /// Opt-in crash / error-burst upload settings.
    pub diagnostics: DiagnosticsConfig,
    /// Package version embedded in diagnostics uploads.
    pub version: String,
    /// Attempt to attach an OS log facility (no-op when unavailable).
    pub enable_journald: bool,
}

impl Default for TracingOptions {
    fn default() -> Self {
        Self {
            format: LogFormat::Text,
            default_level: String::from("bookclerk=info,warn"),
            syslog_identifier: String::from("bookclerk"),
            diagnostics: DiagnosticsConfig::default(),
            version: String::from(env!("CARGO_PKG_VERSION")),
            enable_journald: true,
        }
    }
}

/// Result of installing the global subscriber.
///
/// Keep this value alive for the process lifetime so the stderr worker thread
/// stays running ([`WorkerGuard`]).
pub struct LoggingHandle {
    /// Diagnostics ring buffer / upload handle.
    pub diagnostics: DiagnosticsHandle,
    /// Whether an OS facility sink was attached.
    pub journald: bool,
    /// Which facility was attached (if any).
    pub os_facility: Option<OsLogFacility>,
    /// Keeps the non-blocking stderr worker alive.
    _stderr_guard: WorkerGuard,
}

/// Install a global tracing subscriber (stderr + optional OS facility + diagnostics).
///
/// Filter precedence for stderr/OS facility: `BOOKCLERK_LOG` → `RUST_LOG` →
/// `opts.default_level`. The diagnostics ring buffer is **not** filtered by that
/// directive — it always records TRACE and above so uploads retain deep context.
/// Widen local verbosity with e.g. `BOOKCLERK_LOG=bookclerk=debug` when investigating.
///
/// Secrets are redacted on every sink via the public `register_secret` / `redact_str` helpers.
pub fn init_tracing_with(opts: TracingOptions) -> LoggingHandle {
    let filter = EnvFilter::try_from_env("BOOKCLERK_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(&opts.default_level));
    // Before the subscriber: `try_init` below would otherwise install a
    // bridge that carries every record.
    install_log_bridge();

    let diag_handle = DiagnosticsHandle::new(opts.diagnostics.clone(), opts.version.clone());
    diagnostics::install_global(diag_handle.clone());

    let os_layer = if opts.enable_journald {
        OsLogLayer::new(opts.syslog_identifier.clone()).ok()
    } else {
        None
    };
    let os_facility = os_layer.as_ref().map(OsLogLayer::facility);
    let journald_active = os_facility.is_some();

    // Ring buffer sees everything; stderr/OS facility honor EnvFilter.
    let diag_layer = DiagnosticsLayer::new(diag_handle.clone()).with_filter(LevelFilter::TRACE);

    // Non-blocking stderr: a full pipe to a parent process must not stall Tokio.
    let (nb_stderr, stderr_guard) = tracing_appender::non_blocking(std::io::stderr());
    let stderr_writer = SharedRedactingWriter::new(nb_stderr);
    let text_ansi = std::io::stderr().is_terminal();

    let result = match (opts.format, os_layer) {
        (LogFormat::Text, Some(os)) => {
            let fmt_layer = fmt::layer()
                .with_ansi(text_ansi)
                .with_target(false)
                .with_writer(stderr_writer.clone())
                .with_filter(filter.clone());
            let os = os.with_filter(filter);
            tracing_subscriber::registry()
                .with(diag_layer)
                .with(os)
                .with(fmt_layer)
                .try_init()
        }
        (LogFormat::Text, None) => {
            let fmt_layer = fmt::layer()
                .with_ansi(text_ansi)
                .with_target(false)
                .with_writer(stderr_writer)
                .with_filter(filter);
            tracing_subscriber::registry()
                .with(diag_layer)
                .with(fmt_layer)
                .try_init()
        }
        (LogFormat::Json, Some(os)) => {
            let fmt_layer = fmt::layer()
                .json()
                .with_ansi(false)
                .with_current_span(true)
                .with_writer(stderr_writer.clone())
                .with_filter(filter.clone());
            let os = os.with_filter(filter);
            tracing_subscriber::registry()
                .with(diag_layer)
                .with(os)
                .with(fmt_layer)
                .try_init()
        }
        (LogFormat::Json, None) => {
            let fmt_layer = fmt::layer()
                .json()
                .with_ansi(false)
                .with_current_span(true)
                .with_writer(stderr_writer)
                .with_filter(filter);
            tracing_subscriber::registry()
                .with(diag_layer)
                .with(fmt_layer)
                .try_init()
        }
    };
    let _ = result;

    if opts.diagnostics.share_reports {
        let url = opts.diagnostics.effective_submit_url();
        if url.is_empty() {
            tracing::warn!(
                "diagnostics.share_reports=true but collector_url is empty — \
                 set diagnostics.collector_url or bake BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL \
                 at cargo build"
            );
        } else {
            tracing::info!(
                url = %url,
                "diagnostics.share_reports=true — redacted reports will POST to collector"
            );
        }
    }

    LoggingHandle {
        diagnostics: diag_handle,
        journald: journald_active,
        os_facility,
        _stderr_guard: stderr_guard,
    }
}

/// Convenience wrapper matching the historical CLI/daemon call site.
pub fn init_tracing(format: LogFormat, default_level: &str) -> LoggingHandle {
    init_tracing_with(TracingOptions {
        format,
        default_level: default_level.to_string(),
        ..TracingOptions::default()
    })
}

/// `MakeWriter` adapter: redact then hand bytes to the non-blocking stderr worker.
#[derive(Clone)]
struct SharedRedactingWriter {
    /// Shared redacting stderr writer; secrets registered with the process are stripped.
    inner: std::sync::Arc<Mutex<RedactingWriter<NonBlocking>>>,
}

impl SharedRedactingWriter {
    /// Wraps a non-blocking stderr worker with the process-wide secret redactor.
    fn new(nb: NonBlocking) -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(RedactingWriter::new(nb))),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedRedactingWriter {
    type Writer = SharedRedactingWriterGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SharedRedactingWriterGuard { inner: &self.inner }
    }
}

/// `MakeWriter` guard that locks the redactor for one tracing event.
struct SharedRedactingWriterGuard<'a> {
    /// Mutex around the redacting non-blocking writer (poison is recovered).
    inner: &'a Mutex<RedactingWriter<NonBlocking>>,
}

impl io::Write for SharedRedactingWriterGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.flush()
    }
}

/// Remove ECMA-48/ANSI escape sequences so piped guest logs stay plain text.
///
/// `bookclerk-workerd` and other guests may emit colored `tracing_subscriber`
/// lines on stderr. The host re-logs those lines into JSON; leaving CSI codes
/// in place produces `\u001b[…` noise in `daemon.json_logs`.
#[must_use]
pub fn strip_ansi_escapes(input: &str) -> Cow<'_, str> {
    if !input.as_bytes().contains(&0x1b) {
        return Cow::Borrowed(input);
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'[' => {
                // CSI: ESC [ … final byte in 0x40..=0x7E
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i];
                    i += 1;
                    if (0x40..=0x7E).contains(&c) {
                        break;
                    }
                }
            }
            b']' => {
                // OSC: ESC ] … BEL or ESC \
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'(' | b')' => {
                i += 1;
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::strip_ansi_escapes;

    /// Lines a test subscriber wrote.
    #[derive(Clone, Default)]
    struct Lines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The bridge carries a `log` record to the subscriber, except one of
    /// the WebSocket library's, whose frame traces spell payloads in hex.
    #[test]
    fn the_log_bridge_drops_websocket_library_records() {
        use log::Log as _;

        for (target, dropped) in [
            ("tungstenite", true),
            ("tungstenite::protocol::frame", true),
            ("tungstenite::protocol", true),
            ("tungstenite::handshake::client", true),
            ("tungstenite_other", false),
            ("tokio_tungstenite", false),
            ("hyper::proto", false),
            ("", false),
        ] {
            assert_eq!(super::is_unbridged(target), dropped, "{target:?}");
        }

        let lines = Lines::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(lines.clone())
            .finish();
        let bridge = super::LogBridge(tracing_log::LogTracer::new());
        let record = |target: &'static str, text: &'static str| {
            bridge.log(
                &log::Record::builder()
                    .level(log::Level::Trace)
                    .target(target)
                    .args(format_args!("{text}"))
                    .build(),
            );
        };
        tracing::subscriber::with_default(subscriber, || {
            record("tungstenite::protocol::frame", "writing frame 0x746f6b656e");
            record("tungstenite::protocol", "Received message token");
            record("hyper::proto", "a carried record");
            let frame = log::Metadata::builder()
                .level(log::Level::Trace)
                .target("tungstenite::protocol::frame")
                .build();
            assert!(!bridge.enabled(&frame));
        });
        let written = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        assert!(written.contains("a carried record"), "{written}");
        assert!(!written.contains("746f6b656e"), "{written}");
        assert!(!written.contains("Received message"), "{written}");
    }

    #[test]
    fn strip_ansi_is_noop_without_escapes() {
        let line = "bookclerk-jail: plugin:sqlite [landlock+seccomp]: filesystem=enforced";
        assert!(matches!(
            strip_ansi_escapes(line),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(strip_ansi_escapes(line).as_ref(), line);
    }

    #[test]
    fn strip_ansi_removes_tracing_subscriber_colors() {
        let colored = "\u{1b}[2m2026-08-16T07:48:17.485269Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[2mbookclerk_workerd\u{1b}[0m\u{1b}[2m:\u{1b}[0m starting isolate \u{1b}[3mplugin\u{1b}[0m\u{1b}[2m=\u{1b}[0mlocal";
        let plain = strip_ansi_escapes(colored);
        assert_eq!(
            plain.as_ref(),
            "2026-08-16T07:48:17.485269Z  INFO bookclerk_workerd: starting isolate plugin=local"
        );
        assert!(!plain.contains('\u{1b}'));
    }
}
