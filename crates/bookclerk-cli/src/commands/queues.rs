//! `bookclerk queues` — operator client for plugin-owned queues on the outbox.

use bookclerk_config::Config;
use clap::Subcommand;
use serde_json::Value;

use crate::commands::daemon_cmd;
use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk queues` verbs that call bookclerkd's `/api/queues` surface.
pub enum QueuesCommand {
    /// GET /api/queues — every queue with delivery counters and consumers.
    List,
    /// GET /api/queues/{name} — counters, consumers, recent messages and deliveries.
    Inspect {
        /// Queue name (`core.events` is the domain-event bus).
        name: String,
    },
}

/// Dispatches a queues verb against a running bookclerkd.
///
/// # Errors
///
/// Returns an error when the daemon is unreachable or answers with a failure.
pub async fn run(
    command: QueuesCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let base = daemon_cmd::daemon_base_url(config);
    let token = daemon_cmd::operator_bearer(config).await?;
    match command {
        QueuesCommand::List => {
            let v =
                daemon_cmd::get_json_async(&format!("{base}/api/queues"), token.as_deref()).await?;
            emit(format, &v, || print_queues(&v))
        }
        QueuesCommand::Inspect { name } => {
            let v =
                daemon_cmd::get_json_async(&format!("{base}/api/queues/{name}"), token.as_deref())
                    .await?;
            emit(format, &v, || print_queue_detail(&v))
        }
    }
}

/// One queue summary line in the text format.
fn queue_line(row: &Value) -> String {
    format!(
        "{} pending={} running={} acked={} dead_letter={} consumers={}",
        row["name"].as_str().unwrap_or("-"),
        row["pending"],
        row["running"],
        row["acked"],
        row["deadLetter"],
        row["consumers"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".into())
    )
}

/// Print queue summaries in the text format.
fn print_queues(v: &Value) {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no queues");
        return;
    }
    for row in rows {
        println!("{}", queue_line(&row));
    }
}

/// Print one queue's summary, messages, and deliveries in the text format.
fn print_queue_detail(v: &Value) {
    println!("{}", queue_line(v));
    let messages = v["messages"].as_array().cloned().unwrap_or_default();
    println!("messages: {}", messages.len());
    for row in messages {
        println!(
            "  {} source={} type={} content={} dispatch={} created={}",
            row["id"].as_str().unwrap_or("-"),
            row["source"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("host"),
            row["eventType"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("-"),
            row["contentType"].as_str().unwrap_or("-"),
            row["dispatchState"].as_str().unwrap_or("-"),
            row["createdAt"].as_str().unwrap_or("-")
        );
    }
    let deliveries = v["deliveries"].as_array().cloned().unwrap_or_default();
    println!("deliveries: {}", deliveries.len());
    for row in deliveries {
        println!(
            "  {} plugin={} state={} attempts={} {}",
            row["id"].as_str().unwrap_or("-"),
            row["pluginId"].as_str().unwrap_or("-"),
            row["state"].as_str().unwrap_or("-"),
            row["attemptCount"],
            row["errorMessage"].as_str().unwrap_or("")
        );
    }
}
