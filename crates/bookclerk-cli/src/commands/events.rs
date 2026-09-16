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
                &delivery_action_url(&base, &id, "retry"),
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
                &delivery_action_url(&base, &id, "acknowledge"),
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
                &delivery_action_url(&base, &id, "cancel"),
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
                &delivery_action_url(&base, &id, "resume"),
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

/// `POST {base}/api/events/deliveries/{id}/{action}` with `{id}` as one path segment.
///
/// Delivery ids are `{event_id}:{plugin_id}`. Canonical PluginKey text uses `/`
/// and `#`, which must not split the path or start a URL fragment.
fn delivery_action_url(base: &str, id: &str, action: &str) -> String {
    format!(
        "{}/api/events/deliveries/{}/{action}",
        base.trim_end_matches('/'),
        encode_path_segment(id)
    )
}

/// Percent-encode a single path segment the same way as JS `encodeURIComponent`.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_segment_matches_encode_uri_component() {
        assert_eq!(encode_path_segment("plain"), "plain");
        assert_eq!(
            encode_path_segment("evt:platform:github.com/org/pkg#rev"),
            "evt%3Aplatform%3Agithub.com%2Forg%2Fpkg%23rev"
        );
        assert_eq!(encode_path_segment("a b"), "a%20b");
    }

    #[test]
    fn delivery_action_url_keeps_plugin_key_in_one_segment() {
        let id = "11111111-1111-1111-1111-111111111111:platform:github.com/org/pkg";
        let url = delivery_action_url("http://127.0.0.1:8787/", id, "retry");
        assert_eq!(
            url,
            "http://127.0.0.1:8787/api/events/deliveries/11111111-1111-1111-1111-111111111111%3Aplatform%3Agithub.com%2Forg%2Fpkg/retry"
        );
        assert!(
            !url.contains("/org/pkg/retry"),
            "slash must not add path segments"
        );
        let hashed = delivery_action_url(
            "http://127.0.0.1:8787",
            "evt:registry:https://crates.io#bookclerk-plugin-echo",
            "acknowledge",
        );
        assert!(
            hashed.contains("%23bookclerk-plugin-echo/acknowledge"),
            "hash must not start a URL fragment: {hashed}"
        );
    }
}
