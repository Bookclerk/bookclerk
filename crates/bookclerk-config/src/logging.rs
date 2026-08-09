//! Tracing / logging initialization.
//!
//! Sinks:
//! - **stderr** (always): text or JSON, filtered by `BOOKCLERK_LOG` / `RUST_LOG`
//! - **OS facility** (when available): journald / macOS os_log / Windows Event Log —
//!   same configured filter; Bookclerk does not manage log files or rotation
//! - **diagnostics ring buffer** (always): retains **all** levels through TRACE so
//!   crash / burst uploads include deep context even when stderr is quieter
//!
//! Stderr is written through a **non-blocking** worker thread. When the consumer
//! of stderr stalls (full pipe to a parent IDE/terminal capture), logging must
//! not park Tokio worker threads — that freezes accept and makes even `/health`
//! time out.

use std::io;
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
/// Secrets are redacted on every sink. See [`crate::redact`].
pub fn init_tracing_with(opts: TracingOptions) -> LoggingHandle {
    let filter = EnvFilter::try_from_env("BOOKCLERK_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(&opts.default_level));

    let diag_handle = DiagnosticsHandle::new(opts.diagnostics.clone(), opts.version.clone());
    diagnostics::install_global(diag_handle.clone());
    if opts.diagnostics.share_reports {
        let url = opts.diagnostics.effective_submit_url();
        if url.is_empty() {
            eprintln!(
                "bookclerk: diagnostics.share_reports=true but collector_url is empty — \
                 set diagnostics.collector_url or bake BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL \
                 at cargo build"
            );
        } else {
            eprintln!(
                "bookclerk: diagnostics.share_reports=true — redacted reports will POST to {url}"
            );
        }
    }

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

    let result = match (opts.format, os_layer) {
        (LogFormat::Text, Some(os)) => {
            let fmt_layer = fmt::layer()
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
    inner: std::sync::Arc<Mutex<RedactingWriter<NonBlocking>>>,
}

impl SharedRedactingWriter {
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

struct SharedRedactingWriterGuard<'a> {
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
