//! `bookclerk diagnostics` — show ring buffer / request upload.

use bookclerk_config::Config;
use clap::Subcommand;
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk diagnostics` subcommands: print the ring buffer or request an upload.
pub enum DiagnosticsCommand {
    /// Print recent diagnostics ring-buffer events.
    Show {
        /// Max events to print (0 = all).
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
    },
    /// Upload the current diagnostics ring (requires share_reports + collector URL).
    Upload {
        /// Trigger label recorded with the report.
        #[arg(long, default_value = "cli_manual")]
        trigger: String,
    },
}

/// Dispatches a diagnostics subcommand against the process-global ring buffer.
pub fn run(
    command: DiagnosticsCommand,
    _config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        DiagnosticsCommand::Show { limit } => {
            let Some(diag) = bookclerk_config::diagnostics_global() else {
                anyhow::bail!("diagnostics not initialized");
            };
            let mut events = diag.snapshot_events();
            if limit > 0 && events.len() > limit {
                events = events.split_off(events.len() - limit);
            }
            let payload = json!({
                "version": diag.version(),
                "upload_enabled": diag.upload_enabled(),
                "events": events,
            });
            emit(format, &payload, || {
                println!(
                    "version={} upload_enabled={} events={}",
                    diag.version(),
                    diag.upload_enabled(),
                    events.len()
                );
                for e in &events {
                    println!("{} {} {} {}", e.ts_unix_ms, e.level, e.target, e.message);
                }
            })
        }
        DiagnosticsCommand::Upload { trigger } => {
            let Some(diag) = bookclerk_config::diagnostics_global() else {
                anyhow::bail!("diagnostics not initialized");
            };
            if !diag.upload_enabled() {
                anyhow::bail!(
                    "diagnostics upload not configured (set diagnostics.share_reports=true and collector URL)"
                );
            }
            diag.upload_blocking(&trigger);
            let payload = json!({
                "uploaded": true,
                "trigger": trigger,
                "uploads_attempted": diag.uploads_attempted(),
            });
            emit(format, &payload, || {
                println!(
                    "upload requested trigger={trigger} attempts={}",
                    diag.uploads_attempted()
                );
            })
        }
    }
}
