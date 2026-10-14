//! `bookclerk hosts`: the cluster's hosts and their placement labels.
//!
//! Reads and writes the library database directly, like `plugins`. Label
//! changes wake the cluster scheduler through every host's queue.

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    effective_host_labels, list_host_statuses, list_hosts, load_host, load_host_runtime,
    set_host_labels, update_host_runtime, ConfigActor, HostRuntimeSettingsV1, HostStatus,
};
use clap::Subcommand;
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk hosts` verbs.
pub enum HostsCommand {
    /// Every registered host: liveness, artifact target, and labels.
    List,
    /// Set a host's scheduling budget or drain it. Applies at that host's
    /// next heartbeat.
    Set {
        /// Host id (default: this host).
        host: Option<String>,
        /// Share of detected CPU the scheduler may commit here (1-100).
        #[arg(long, value_name = "PERCENT")]
        cpu_budget: Option<u8>,
        /// Share of detected memory the scheduler may commit here (1-100).
        #[arg(long, value_name = "PERCENT")]
        memory_budget: Option<u8>,
        /// Place nothing new here and move scheduled replicas away.
        #[arg(long, conflicts_with = "undrain")]
        drain: bool,
        /// Accept scheduled replicas again.
        #[arg(long)]
        undrain: bool,
        /// Operator label for the host.
        #[arg(long)]
        label: Option<String>,
    },
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
    let now_ms = chrono::Utc::now().timestamp_millis();
    let statuses: std::collections::HashMap<String, HostStatus> = list_host_statuses(store)
        .await?
        .into_iter()
        .map(|status| (status.host_id.clone(), status))
        .collect();
    match command {
        HostsCommand::List => {
            let hosts = list_hosts(store).await?;
            let mut rows = Vec::new();
            for host in &hosts {
                let status = statuses.get(&host.host_id);
                let runtime = match load_host_runtime(store, &host.host_id).await {
                    Ok(document) => document.body,
                    Err(bookclerk_library::LibraryError::NotFound(_)) => {
                        HostRuntimeSettingsV1::new()
                    }
                    Err(err) => return Err(err.into()),
                };
                rows.push(json!({
                    "host_id": host.host_id,
                    "this_host": host.host_id == this_host,
                    "live": status.is_some_and(|s| s.is_live(now_ms)),
                    "target": host.target,
                    "labels": host.labels,
                    "implicit_labels": effective_host_labels(host)
                        .into_iter()
                        .filter(|label| !host.labels.contains(label))
                        .collect::<Vec<_>>(),
                    "runtime": runtime,
                    "status": status,
                    "software_version": host.software_version,
                }));
            }
            emit(format, &json!({ "hosts": rows }), || {
                for host in &hosts {
                    let status = statuses.get(&host.host_id);
                    println!(
                        "{}{} {} target={} labels={}",
                        host.host_id,
                        if host.host_id == this_host {
                            " (this host)"
                        } else {
                            ""
                        },
                        if status.is_some_and(|s| s.is_live(now_ms)) {
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
                    match status {
                        Some(status) => println!("  {}", status_line(status)),
                        None => println!("  no daemon heartbeat yet"),
                    }
                }
            })
        }
        HostsCommand::Set {
            host,
            cpu_budget,
            memory_budget,
            drain,
            undrain,
            label,
        } => {
            let host_id = host.unwrap_or_else(|| this_host.clone());
            load_host(store, &host_id).await?;
            let document = update_host_runtime(
                store,
                &ConfigActor::Operator { id: "cli".into() },
                &host_id,
                |runtime| {
                    if let Some(percent) = cpu_budget {
                        runtime.cpu_budget_percent = percent;
                    }
                    if let Some(percent) = memory_budget {
                        runtime.memory_budget_percent = percent;
                    }
                    if drain {
                        runtime.schedulable = false;
                    }
                    if undrain {
                        runtime.schedulable = true;
                    }
                    if let Some(label) = &label {
                        runtime.label.clone_from(label);
                    }
                },
            )
            .await?;
            let payload = json!({
                "host_id": host_id,
                "revision": document.revision,
                "runtime": document.body,
            });
            emit(format, &payload, || {
                println!(
                    "host {host_id}: cpu budget {}%, memory budget {}%, {} (revision {}); applies at its next heartbeat",
                    document.body.cpu_budget_percent,
                    document.body.memory_budget_percent,
                    if document.body.schedulable {
                        "schedulable"
                    } else {
                        "drained"
                    },
                    document.revision
                );
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

/// One-line summary of a heartbeat: capacity, budget, committed, measured,
/// availability.
fn status_line(status: &HostStatus) -> String {
    let cores = |millis: i64| format!("{:.1}", millis as f64 / 1000.0);
    let gib = |bytes: i64| format!("{:.1}G", bytes as f64 / f64::from(1u32 << 30));
    let opt_gib = |bytes: Option<i64>| bytes.map_or_else(|| "?".to_string(), gib);
    format!(
        "cpu {} cores (budget {}, reserved {}, busy {}) | memory {} (budget {}, reserved {}, available {}) | guests {} | availability {:.0}%{}",
        cores(status.cpu_millicores),
        cores(status.cpu_budget_millicores),
        cores(status.reserved.cpu_millicores),
        status
            .cpu_used_millicores
            .map_or_else(|| "?".to_string(), cores),
        opt_gib(status.memory_bytes),
        opt_gib(status.memory_budget_bytes),
        gib(status.reserved.memory_bytes),
        opt_gib(status.memory_available_bytes),
        status.running_guests,
        status.availability_ppm as f64 / 10_000.0,
        if status.schedulable { "" } else { " | drained" }
    )
}
