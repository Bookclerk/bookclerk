//! Tracing / logging initialization.

use tracing_subscriber::fmt;
use tracing_subscriber::EnvFilter;

/// Output format for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-oriented text (CLI default).
    #[default]
    Text,
    /// Structured JSON (daemon / journald-friendly).
    Json,
}

/// Install a global tracing subscriber.
///
/// Respects `RUST_LOG` / `LIBATION_LOG` (e.g. `libation=debug,info`).
pub fn init_tracing(format: LogFormat, default_level: &str) {
    let filter = EnvFilter::try_from_env("LIBATION_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    match format {
        LogFormat::Text => {
            let _ = fmt()
                .with_env_filter(filter)
                .with_target(false)
                .with_writer(std::io::stderr)
                .try_init();
        }
        LogFormat::Json => {
            let _ = fmt()
                .json()
                .with_env_filter(filter)
                .with_current_span(true)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }
}
