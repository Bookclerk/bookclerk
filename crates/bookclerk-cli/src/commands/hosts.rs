//! `bookclerk hosts`: the cluster's hosts and their placement labels.
//!
//! Reads and writes the library database directly, like `plugins`. Label
//! changes wake the cluster scheduler through every host's queue.

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    effective_host_labels, list_hosts, live_host_ids, load_host, set_host_labels, ConfigActor,
};
use clap::Subcommand;
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk hosts` verbs.
pub enum HostsCommand {
    /// Every registered host: liveness, artifact target, and labels.
    List,
    /// Add or remove placement labels on a host (`nas`, `zone=eu`).
    Label {
        /// Host id (default: this host).
        host: Option<String>,
        /// Label to add. Repeatable.
        #[arg(long = "add", value_name = "LABEL")]
        add: Vec<String>,
        /// Label to remove. Repeatable.
        #[arg(long = "remove", value_name = "LABEL")]
        remove: Vec<String>,
        /// Start from no labels (then apply `--add`).
        #[arg(long)]
        clear: bool,
    },
}

/// Dispatches a hosts verb.
///
/// # Errors
///
/// Returns an error when the library cannot be opened, the host is unknown,
/// or a label is invalid.
pub async fn run(
    command: HostsCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let this_host = plane.host_id().to_string();
    let live = live_host_ids(store).await?;
    match command {
        HostsCommand::List => {
            let hosts = list_hosts(store).await?;
            let rows: Vec<_> = hosts
                .iter()
                .map(|host| {
                    json!({
                        "host_id": host.host_id,
                        "this_host": host.host_id == this_host,
                        "live": live.contains(&host.host_id),
                        "target": host.target,
                        "labels": host.labels,
                        "implicit_labels": effective_host_labels(host)
                            .into_iter()
                            .filter(|label| !host.labels.contains(label))
                            .collect::<Vec<_>>(),
                        "software_version": host.software_version,
                        "heartbeat_at": host.heartbeat_at,
                    })
                })
                .collect();
            emit(format, &json!({ "hosts": rows }), || {
                for host in &hosts {
                    println!(
                        "{}{} {} target={} labels={}",
                        host.host_id,
                        if host.host_id == this_host {
                            " (this host)"
                        } else {
                            ""
                        },
                        if live.contains(&host.host_id) {
                            "live"
                        } else {
                            "not-live"
                        },
                        if host.target.is_empty() {
                            "-"
                        } else {
                            host.target.as_str()
                        },
                        if host.labels.is_empty() {
                            "-".to_string()
                        } else {
                            host.labels.join(",")
                        }
                    );
                }
            })
        }
        HostsCommand::Label {
            host,
            add,
            remove,
            clear,
        } => {
            let host_id = host.unwrap_or_else(|| this_host.clone());
            let current = load_host(store, &host_id).await?;
            let mut labels: Vec<String> = if clear { Vec::new() } else { current.labels };
            labels.retain(|label| !remove.iter().any(|r| r.trim() == label));
            labels.extend(add);
            let updated = set_host_labels(
                store,
                &ConfigActor::Operator { id: "cli".into() },
                &host_id,
                &labels,
            )
            .await?;
            let payload = json!({
                "host_id": updated.host_id,
                "labels": updated.labels,
            });
            emit(format, &payload, || {
                println!(
                    "host {} labels: {}",
                    updated.host_id,
                    if updated.labels.is_empty() {
                        "-".to_string()
                    } else {
                        updated.labels.join(",")
                    }
                );
            })
        }
    }
}
