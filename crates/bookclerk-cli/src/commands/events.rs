//! `bookclerk events` — operator client for the durable domain-event outbox.

use bookclerk_config::Config;
use clap::Subcommand;
use serde_json::Value;

use crate::commands::daemon_cmd;
use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk events` verbs that call bookclerkd's `/api/events` surface.
pub enum EventsCommand {
    /// GET /api/events — recent outbox envelopes.
    List,
    /// GET /api/events/deliveries?state=dead_letter
    #[command(name = "dead-letters")]
    DeadLetters,
    /// POST /api/events/deliveries/{id}/retry
    Retry {
        /// Delivery id (`{event_id}:{plugin_id}`).
        id: String,
    },
    /// POST /api/events/deliveries/{id}/acknowledge
    Ack {
        /// Delivery id (`{event_id}:{plugin_id}`).
        id: String,
    },
    /// POST /api/events/deliveries/{id}/cancel
    Cancel {
        /// Delivery id (`{event_id}:{plugin_id}`).
        id: String,
    },
    /// POST /api/events/deliveries/{id}/resume
    Resume {
        /// Delivery id (`{event_id}:{plugin_id}`).
        id: String,
    },
}

/// Dispatches an events verb against a running bookclerkd.
pub async fn run(
    command: EventsCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let base = daemon_cmd::daemon_base_url(config);
    let token = daemon_cmd::operator_bearer(config).await?;
    match command {
        EventsCommand::List => {
            let v =
                daemon_cmd::get_json_async(&format!("{base}/api/events"), token.as_deref()).await?;
            emit(format, &v, || print_events(&v))
        }
        EventsCommand::DeadLetters => {
            let v = daemon_cmd::get_json_async(
                &format!("{base}/api/events/deliveries?state=dead_letter"),
                token.as_deref(),
            )
            .await?;
            emit(format, &v, || print_deliveries(&v))
        }
        EventsCommand::Retry { id } => {
            let v = daemon_cmd::post_json_async(
                &format!("{base}/api/events/deliveries/{id}/retry"),
                serde_json::json!({}),
                token.as_deref(),
            )
            .await?;
            emit(format, &v, || {
                println!("ok={} {}", v["ok"], v["message"].as_str().unwrap_or(""));
            })
        }
        EventsCommand::Ack { id } => {
            let v = daemon_cmd::post_json_async(
                &format!("{base}/api/events/deliveries/{id}/acknowledge"),
                serde_json::json!({}),
                token.as_deref(),
            )
            .await?;
            emit(format, &v, || {
                println!("ok={} {}", v["ok"], v["message"].as_str().unwrap_or(""));
            })
        }
        EventsCommand::Cancel { id } => {
            let v = daemon_cmd::post_json_async(
                &format!("{base}/api/events/deliveries/{id}/cancel"),
                serde_json::json!({}),
                token.as_deref(),
            )
            .await?;
            emit(format, &v, || {
                println!("ok={} {}", v["ok"], v["message"].as_str().unwrap_or(""));
            })
        }
        EventsCommand::Resume { id } => {
            let v = daemon_cmd::post_json_async(
                &format!("{base}/api/events/deliveries/{id}/resume"),
                serde_json::json!({}),
                token.as_deref(),
            )
            .await?;
            emit(format, &v, || {
                println!("ok={} {}", v["ok"], v["message"].as_str().unwrap_or(""));
            })
        }
    }
}

/// Print outbox envelopes in the text format.
fn print_events(v: &Value) {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no events");
        return;
    }
    for row in rows {
        println!(
            "{} type={} schema={} dispatch={} {}",
            row["id"].as_str().unwrap_or("-"),
            row["eventType"].as_str().unwrap_or("-"),
            row["schemaVersion"],
            row["dispatchState"].as_str().unwrap_or("-"),
            row["dedupKey"].as_str().unwrap_or("")
        );
    }
}

/// Print delivery rows in the text format.
fn print_deliveries(v: &Value) {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no dead letters");
        return;
    }
    for row in rows {
        println!(
            "{} plugin={} state={} attempts={} {}",
            row["id"].as_str().unwrap_or("-"),
            row["pluginId"].as_str().unwrap_or("-"),
            row["state"].as_str().unwrap_or("-"),
            row["attemptCount"],
            row["errorMessage"].as_str().unwrap_or("")
        );
    }
}
