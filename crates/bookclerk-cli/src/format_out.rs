//! Global CLI output format helpers.

use clap::ValueEnum;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
/// Global CLI `--format` choice for human text vs pretty JSON.
pub enum OutputFormat {
    #[default]
    /// Human-readable lines (default).
    Text,
    /// Pretty-printed JSON via [`emit`].
    Json,
}

impl OutputFormat {
    #[must_use]
    /// True when the caller asked for JSON instead of text.
    pub fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

/// Print a serializable value as JSON, or run `text` for human output.
pub fn emit<T: Serialize>(
    format: OutputFormat,
    value: &T,
    text: impl FnOnce(),
) -> anyhow::Result<()> {
    match format {
        OutputFormat::Json => {
            // Operator-facing CLI stdout, not a diagnostic log.
            // codeql[rust/cleartext-logging]
            println!("{}", serde_json::to_string_pretty(value)?);
        }
        OutputFormat::Text => text(),
    }
    Ok(())
}

/// One operator-facing stdout line (command output, not a diagnostic log).
pub fn line(text: impl AsRef<str>) {
    // codeql[rust/cleartext-logging]
    println!("{}", text.as_ref());
}

/// One operator-facing stderr line (warnings/progress, not a diagnostic log).
pub fn err_line(text: impl AsRef<str>) {
    // codeql[rust/cleartext-logging]
    eprintln!("{}", text.as_ref());
}
