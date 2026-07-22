//! Tracing / logging initialization.
//!
//! Sinks:
//! - **stderr** (always): text or JSON, filtered by `LIBATION_LOG` / `RUST_LOG`
//! - **OS facility** (when available): journald / macOS os_log / Windows Event Log —
//!   same configured filter; Libation does not manage log files or rotation
//! - **diagnostics ring buffer** (always): retains **all** levels through TRACE so
//!   crash / burst uploads include deep context even when stderr is quieter

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
    /// Default `EnvFilter` directive when `LIBATION_LOG` / `RUST_LOG` are unset.
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
            default_level: String::from("libation=info,warn"),
            syslog_identifier: String::from("libation"),
            diagnostics: DiagnosticsConfig::default(),
            version: String::from(env!("CARGO_PKG_VERSION")),
            enable_journald: true,
        }
    }
}

/// Result of installing the global subscriber.
#[derive(Clone)]
pub struct LoggingHandle {
    /// Diagnostics ring buffer / upload handle.
    pub diagnostics: DiagnosticsHandle,
    /// Whether an OS facility sink was attached.
    pub journald: bool,
    /// Which facility was attached (if any).
    pub os_facility: Option<OsLogFacility>,
}

/// Install a global tracing subscriber (stderr + optional OS facility + diagnostics).
///
/// Filter precedence for stderr/OS facility: `LIBATION_LOG` → `RUST_LOG` →
/// `opts.default_level`. The diagnostics ring buffer is **not** filtered by that
/// directive — it always records TRACE and above so uploads retain deep context.
/// Widen local verbosity with e.g. `LIBATION_LOG=libation=debug` when investigating.
///
/// Secrets are redacted on every sink. See [`crate::redact`].
pub fn init_tracing_with(opts: TracingOptions) -> LoggingHandle {
    let filter = EnvFilter::try_from_env("LIBATION_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(&opts.default_level));

    let diag_handle = DiagnosticsHandle::new(opts.diagnostics.clone(), opts.version.clone());
    diagnostics::install_global(diag_handle.clone());
    if opts.diagnostics.share_reports {
        let url = opts.diagnostics.effective_submit_url();
        if url.is_empty() {
            eprintln!(
                "libation: diagnostics.share_reports=true but collector_url is empty — \
                 set diagnostics.collector_url or bake LIBATION_DIAGNOSTICS_COLLECTOR_URL \
                 at cargo build"
            );
        } else {
            eprintln!(
                "libation: diagnostics.share_reports=true — redacted reports will POST to {url}"
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

    let result = match (opts.format, os_layer) {
        (LogFormat::Text, Some(os)) => {
            let fmt_layer = fmt::layer()
                .with_target(false)
                .with_writer(|| RedactingWriter::new(std::io::stderr()))
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
                .with_writer(|| RedactingWriter::new(std::io::stderr()))
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
                .with_writer(|| RedactingWriter::new(std::io::stderr()))
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
                .with_writer(|| RedactingWriter::new(std::io::stderr()))
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
