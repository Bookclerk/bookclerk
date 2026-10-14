//! `bookclerk nodes`: the cluster's nodes and their placement labels.
//!
//! Reads and writes the library database directly, like `plugins`. Label
//! changes wake the cluster scheduler through every node's queue.

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    effective_node_labels, list_node_statuses, list_nodes, load_node, load_node_runtime,
    set_node_labels, update_node_runtime, ConfigActor, NodeRuntimeSettingsV1, NodeStatus,
};
use clap::Subcommand;
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk nodes` verbs.
pub enum NodesCommand {
    /// Every registered node: liveness, artifact target, and labels.
    List,
    /// Set a node's scheduling budget or drain it. Applies at that node's
    /// next heartbeat.
    Set {
        /// Node id (default: this node).
        node: Option<String>,
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
        /// Operator label for the node.
        #[arg(long)]
        label: Option<String>,
    },
    /// Add or remove placement labels on a node (`nas`, `zone=eu`).
    Label {
        /// Node id (default: this node).
        node: Option<String>,
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

/// Dispatches a nodes verb.
///
/// # Errors
///
/// Returns an error when the library cannot be opened, the node is unknown,
/// or a label is invalid.
pub async fn run(
    command: NodesCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let this_node = plane.node_id().to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let statuses: std::collections::HashMap<String, NodeStatus> = list_node_statuses(store)
        .await?
        .into_iter()
        .map(|status| (status.node_id.clone(), status))
        .collect();
    match command {
        NodesCommand::List => {
            let nodes = list_nodes(store).await?;
            let mut rows = Vec::new();
            for node in &nodes {
                let status = statuses.get(&node.node_id);
                let runtime = match load_node_runtime(store, &node.node_id).await {
                    Ok(document) => document.body,
                    Err(bookclerk_library::LibraryError::NotFound(_)) => {
                        NodeRuntimeSettingsV1::new()
                    }
                    Err(err) => return Err(err.into()),
                };
                rows.push(json!({
                    "node_id": node.node_id,
                    "this_node": node.node_id == this_node,
                    "live": status.is_some_and(|s| s.is_live(now_ms)),
                    "target": node.target,
                    "labels": node.labels,
                    "implicit_labels": effective_node_labels(node)
                        .into_iter()
                        .filter(|label| !node.labels.contains(label))
                        .collect::<Vec<_>>(),
                    "runtime": runtime,
                    "status": status,
                    "software_version": node.software_version,
                }));
            }
            emit(format, &json!({ "nodes": rows }), || {
                for node in &nodes {
                    let status = statuses.get(&node.node_id);
                    println!(
                        "{}{} {} target={} labels={}",
                        node.node_id,
                        if node.node_id == this_node {
                            " (this node)"
                        } else {
                            ""
                        },
                        if status.is_some_and(|s| s.is_live(now_ms)) {
                            "live"
                        } else {
                            "not-live"
                        },
                        if node.target.is_empty() {
                            "-"
                        } else {
                            node.target.as_str()
                        },
                        if node.labels.is_empty() {
                            "-".to_string()
                        } else {
                            node.labels.join(",")
                        }
                    );
                    match status {
                        Some(status) => println!("  {}", status_line(status)),
                        None => println!("  no daemon heartbeat yet"),
                    }
                }
            })
        }
        NodesCommand::Set {
            node,
            cpu_budget,
            memory_budget,
            drain,
            undrain,
            label,
        } => {
            let node_id = node.unwrap_or_else(|| this_node.clone());
            load_node(store, &node_id).await?;
            let document = update_node_runtime(
                store,
                &ConfigActor::Operator { id: "cli".into() },
                &node_id,
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
                "node_id": node_id,
                "revision": document.revision,
                "runtime": document.body,
            });
            emit(format, &payload, || {
                println!(
                    "node {node_id}: cpu budget {}%, memory budget {}%, {} (revision {}); applies at its next heartbeat",
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
        NodesCommand::Label {
            node,
            add,
            remove,
            clear,
        } => {
            let node_id = node.unwrap_or_else(|| this_node.clone());
            let current = load_node(store, &node_id).await?;
            let mut labels: Vec<String> = if clear { Vec::new() } else { current.labels };
            labels.retain(|label| !remove.iter().any(|r| r.trim() == label));
            labels.extend(add);
            let updated = set_node_labels(
                store,
                &ConfigActor::Operator { id: "cli".into() },
                &node_id,
                &labels,
            )
            .await?;
            let payload = json!({
                "node_id": updated.node_id,
                "labels": updated.labels,
            });
            emit(format, &payload, || {
                println!(
                    "node {} labels: {}",
                    updated.node_id,
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
fn status_line(status: &NodeStatus) -> String {
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
