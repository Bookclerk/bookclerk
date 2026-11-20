//! `bookclerk diagnostics` — show ring buffer / request upload / list sent
//! reports / read one back from the collector or delete its stored copy.

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
    ///
    /// The access code beside each ID reads the report back (`diagnostics
    /// view`), deletes the collector's copy (`diagnostics delete`), and with
    /// the ID opens the collector's public lookup page.
    Reports {
        /// Max reports to print (0 = all that are kept).
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
    },
    /// Read a sent report back from the collector, with the issues tracking
    /// it and its fix.
    ///
    /// Uses the access code kept in this install's report history.
    View {
        /// The report's ID, from `bookclerk diagnostics reports`.
        report_id: String,
    },
    /// Delete the collector's stored copy of a sent report.
    ///
    /// The collector keeps the report's one-line summary and issue links,
    /// so it still counts toward the issue it joined.
    Delete {
        /// The report's ID, from `bookclerk diagnostics reports`.
        report_id: String,
        /// Delete without asking (required when stdin is not a terminal).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// The report and its code from the install's history.
///
/// # Errors
///
/// When the ID is malformed, the report is not in the history, or it has no
/// access code.
fn history_target(
    files_dir: &std::path::Path,
    report_id: &str,
) -> anyhow::Result<(bookclerk_config::ReportId, bookclerk_config::AccessCode)> {
    let Some(id) = bookclerk_config::ReportId::parse(&report_id.trim().to_ascii_lowercase()) else {
        anyhow::bail!("`{report_id}` is not a report ID (a version-4 UUID)");
    };
    let Some(record) = bookclerk_config::find_report(files_dir, &id) else {
        anyhow::bail!("this install has not sent a report with ID {}", id.as_str());
    };
    let Some(code) = record.access_code else {
        anyhow::bail!(
            "the collector gave report {} no access code, so it cannot be read back",
            id.as_str()
        );
    };
    Ok((id, code))
}

/// Prints a lookup answer as text.
fn print_answer(answer: &bookclerk_config::LookupAnswer) {
    println!("report    {}", answer.report_id.as_str());
    println!("received  {}", answer.received_at_unix_ms);
    if let Some(at) = answer.deleted_at_unix_ms {
        println!("deleted   {at} (the stored copy is gone; the summary and links stay)");
    }
    for issue in &answer.issues {
        let state = match issue.state {
            bookclerk_config::IssueState::Open => "open",
            bookclerk_config::IssueState::Closed => "closed",
        };
        println!(
            "issue     #{} ({state}) {}  {}",
            issue.number, issue.title, issue.url
        );
    }
    if let Some(fix) = &answer.fix {
        let pr = fix
            .pr_url
            .clone()
            .or_else(|| fix.pr_number.map(|n| format!("#{n}")))
            .unwrap_or_else(|| "unknown pull request".into());
        match &fix.fixed_in {
            Some(release) => println!("fixed     in {release} by {pr}"),
            None => println!("fixed     by {pr}, not released yet"),
        }
    }
    if let Some(report) = &answer.report {
        println!(
            "sent by   {} {} ({} {} {})",
            report.version, report.trigger, report.os, report.arch, report.distro
        );
        for e in &report.events {
            let fields: Vec<String> = e.fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
            println!(
                "{} {} {} {} {}",
                e.ts_unix_ms,
                e.level.as_str(),
                e.target,
                e.message,
                fields.join(" ")
            );
        }
    }
}

/// Asks on the terminal before deleting, unless `yes`.
///
/// # Errors
///
/// When stdin is not a terminal and `yes` is not set, or the answer is not
/// yes.
fn confirm_delete(report_id: &bookclerk_config::ReportId, yes: bool) -> anyhow::Result<()> {
    use std::io::{self, IsTerminal, Write};
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        anyhow::bail!("stdin is not a terminal; pass --yes to delete the report");
    }
    eprint!(
        "Delete the collector's copy of report {}? Its summary and issue links stay. [y/N] ",
        report_id.as_str()
    );
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!("not deleted");
    }
    Ok(())
}

/// Dispatches a diagnostics subcommand against the process-global ring buffer.
pub fn run(
    command: DiagnosticsCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        DiagnosticsCommand::View { report_id } => {
            let (id, code) = history_target(&config.paths().files_dir, &report_id)?;
            let answer = bookclerk_config::fetch_report(&config.diagnostics, &id, &code)?;
            emit(format, &answer, || print_answer(&answer))
        }
        DiagnosticsCommand::Delete { report_id, yes } => {
            let files_dir = &config.paths().files_dir;
            let (id, code) = history_target(files_dir, &report_id)?;
            confirm_delete(&id, yes)?;
            let at = bookclerk_config::delete_report(&config.diagnostics, &id, &code)?;
            bookclerk_config::mark_report_deleted(files_dir, &id, at)?;
            let payload = json!({ "report_id": id.as_str(), "deleted_at_unix_ms": at });
            emit(format, &payload, || {
                println!(
                    "deleted the collector's copy of report {}; its summary and issue links stay",
                    id.as_str()
                );
            })
        }
        DiagnosticsCommand::Reports { limit } => {
            let reports = newest_reports(&config.paths().files_dir, limit);
            let payload = json!({ "reports": reports });
            emit(format, &payload, || {
                if reports.is_empty() {
                    println!("no diagnostics reports sent from this install");
                }
                for r in &reports {
                    let code = r
                        .access_code
                        .as_ref()
                        .map_or_else(|| "-".to_string(), bookclerk_config::AccessCode::display);
                    let deleted = if r.deleted_at_unix_ms.is_some() {
                        "  (deleted)"
                    } else {
                        ""
                    };
                    println!(
                        "{}  {}  {}  {}  {}{deleted}",
                        r.report_id.as_str(),
                        code,
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
                    access_code: bookclerk_config::AccessCode::parse("K7QM3XRDPW9TH2CF")
                        .filter(|_| n == 1),
                    deleted_at_unix_ms: None,
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
        assert!(history_target(dir.path(), "nope").is_err());
        assert!(history_target(dir.path(), "1e9ced79-df3c-4dae-872b-2e5772b00000").is_err());
        assert!(history_target(dir.path(), "0e9ced79-df3c-4dae-872b-2e5772b00000").is_err());
        let (id, code) =
            history_target(dir.path(), " 0E9CED79-DF3C-4DAE-872B-2E5772B00001 ").expect("target");
        assert_eq!(id.as_str(), "0e9ced79-df3c-4dae-872b-2e5772b00001");
        assert_eq!(code.as_str(), "K7QM3XRDPW9TH2CF");
        assert!(confirm_delete(&id, true).is_ok());
    }
}
