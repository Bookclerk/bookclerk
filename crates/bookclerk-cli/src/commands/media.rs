//! `bookclerk media`: what this build's media worker reads and writes.

use clap::Subcommand;
use serde::Serialize;

use crate::format_out::{self, OutputFormat};

/// Nested `media` verbs.
#[derive(Debug, Subcommand)]
pub enum MediaCommand {
    /// List the audio codecs, file types and output formats this build's
    /// media worker handles (the daemon's `GET /api/status/media`).
    Capabilities,
}

/// One output format in the `--format json` report.
#[derive(Debug, Serialize)]
struct EncodingRow {
    /// The output format id (`m4b`, `mp3` or `opus`).
    name: &'static str,
    /// True when settings offer it only under their advanced options.
    advanced: bool,
}

/// The `--format json` report: the same fields as the web API's.
#[derive(Debug, Serialize)]
struct Report {
    /// Audio codecs the worker decodes.
    decodes: Vec<&'static str>,
    /// Media types of files the worker opens.
    reads: Vec<&'static str>,
    /// Output formats the worker writes.
    encodes: Vec<EncodingRow>,
}

/// The report for this build, from [`bookclerk_media::capabilities`].
fn report() -> Report {
    let caps = bookclerk_media::capabilities::capabilities();
    Report {
        decodes: caps.decodes.iter().map(|codec| codec.as_str()).collect(),
        reads: caps.reads.to_vec(),
        encodes: caps
            .encodes
            .iter()
            .map(|encoding| EncodingRow {
                name: encoding.name(),
                advanced: encoding.is_advanced(),
            })
            .collect(),
    }
}

/// Runs a `media` verb.
///
/// # Errors
///
/// Returns an error when the JSON report cannot be written.
pub fn run(command: MediaCommand, format: OutputFormat) -> anyhow::Result<()> {
    match command {
        MediaCommand::Capabilities => {
            let report = report();
            format_out::emit(format, &report, || {
                format_out::line(format!("decodes: {}", report.decodes.join(", ")));
                format_out::line(format!("reads:   {}", report.reads.join(", ")));
                let encodes: Vec<String> = report
                    .encodes
                    .iter()
                    .map(|e| {
                        if e.advanced {
                            format!("{} (advanced)", e.name)
                        } else {
                            e.name.to_string()
                        }
                    })
                    .collect();
                format_out::line(format!("encodes: {}", encodes.join(", ")));
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_lists_opus_as_advanced() {
        let report = report();
        assert!(report.decodes.contains(&"vorbis"));
        assert!(report
            .encodes
            .iter()
            .any(|e| e.name == "opus" && e.advanced));
    }
}
