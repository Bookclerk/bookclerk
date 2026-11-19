//! `bookclerk diagnostics` — show ring buffer / request upload / list sent reports.

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
    /// Replace this install's random usage ID (diagnostics.share_usage) with a new one.
    ///
    /// The next check-in then counts as a new install. Restart the daemon so
    /// its reports carry the new ID.
    ResetId,
    /// List the diagnostics reports this install sent, newest first.
    ///
    /// Each report ID is what the collector stored and what the GitHub
    /// issue for it mentions: search the issues for it, or give it to a
    /// maintainer.
    Reports {
        /// Max reports to print (0 = all that are kept).
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
    },
}

/// Dispatches a diagnostics subcommand against the process-global ring buffer.
pub fn run(
    command: DiagnosticsCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        DiagnosticsCommand::Reports { limit } => {
            let reports = newest_reports(&config.paths().files_dir, limit);
            let payload = json!({ "reports": reports });
            emit(format, &payload, || {
                if reports.is_empty() {
                    println!("no diagnostics reports sent from this install");
                }
                for r in &reports {
                    println!(
                        "{}  {}  {}  {}",
                        r.report_id.as_str(),
                        r.sent_at_unix_ms,
                        r.trigger,
                        r.version
                    );
                }
            })
        }
        DiagnosticsCommand::ResetId => {
            bookclerk_config::InstallId::reset(&config.paths().files_dir)?;
            let payload = json!({ "reset": true, "share_usage": config.diagnostics.share_usage });
            emit(format, &payload, || {
                println!("install ID replaced; restart bookclerkd so reports use it");
                if !config.diagnostics.share_usage {
                    println!("diagnostics.share_usage is off, so the ID is not sent anywhere");
                }
            })
        }
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

/// The install's sent reports, newest first.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `limit` - How many to return; `0` returns every kept report.
fn newest_reports(
    files_dir: &std::path::Path,
    limit: usize,
) -> Vec<bookclerk_config::ReportRecord> {
    let mut reports = bookclerk_config::load_report_history(files_dir);
    reports.reverse();
    if limit > 0 {
        reports.truncate(limit);
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_list_newest_first_and_honor_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..3_u64 {
            bookclerk_config::record_report(
                dir.path(),
                bookclerk_config::ReportRecord {
                    report_id: bookclerk_config::ReportId::parse(&format!(
                        "0e9ced79-df3c-4dae-872b-2e5772b0000{n}"
                    ))
                    .expect("id"),
                    trigger: "crash".into(),
                    version: "1".into(),
                    sent_at_unix_ms: n,
                },
            )
            .expect("record");
        }
        let all = newest_reports(dir.path(), 0);
        assert_eq!(
            all.iter().map(|r| r.sent_at_unix_ms).collect::<Vec<_>>(),
            vec![2, 1, 0]
        );
        assert_eq!(newest_reports(dir.path(), 2).len(), 2);
    }
}
