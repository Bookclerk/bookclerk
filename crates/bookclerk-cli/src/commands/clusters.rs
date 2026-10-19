//! `bookclerk clusters`: this install as a host node of owner clusters,
//! and this host operator's consent to the plugins they ask to run here
//! (H-4 design, section 5).
//!
//! Reads and writes the library database directly, like `nodes`. A
//! running daemon re-reads consent within a few seconds, so an approval
//! lets the plugin's jobs be claimed and a revoke cancels and gives back
//! the plugin's jobs held here.

use std::collections::BTreeSet;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    list_cluster_links, list_remote_plugin_consents, revoke_remote_plugin, RemoteApprover,
    RemotePluginConsent,
};
use bookclerk_plugin_host::{approve_remote_request, remote_consent_state, RemoteNarrowing};
use clap::Subcommand;
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

/// Audit identity of approvals made with the CLI.
const CLI_APPROVER: &str = "cli";

#[derive(Debug, Subcommand)]
/// `bookclerk clusters` verbs.
pub enum ClustersCommand {
    /// The owner clusters this install is enrolled in as a host.
    List,
    /// What each owner cluster asks to run here, and whether it may.
    Consents {
        /// Only this owner cluster.
        #[arg(long)]
        cluster: Option<String>,
    },
    /// Approve an owner plugin's request to run here, optionally narrowed.
    /// Name the request you reviewed by the digest `clusters consents`
    /// shows; it is refused when the cluster has changed the request since.
    /// Only what the request asks for can be approved; a later version that
    /// asks for more needs approval again.
    Approve {
        /// Canonical plugin key.
        plugin: String,
        /// Owner cluster id (default: the only enrolled cluster).
        #[arg(long)]
        cluster: Option<String>,
        /// The `request_digest` of the request you reviewed, from
        /// `clusters consents` (refused when the cluster now asks for
        /// anything else).
        #[arg(long = "request", value_name = "DIGEST")]
        request_digest: String,
        /// Take a domain out of the request. Repeatable.
        #[arg(long = "deny-domain", value_name = "DOMAIN")]
        deny_domains: Vec<String>,
        /// Take a binding out of the request (`secrets`, `kv`, ...).
        /// Repeatable.
        #[arg(long = "deny-binding", value_name = "BINDING")]
        deny_bindings: Vec<String>,
        /// Deny all network access.
        #[arg(long)]
        deny_network: bool,
    },
    /// Withdraw an approval: held jobs of the plugin are cancelled and
    /// given back, and nothing of it is claimed until this host's operator
    /// approves it again (a platform plugin is not approved automatically).
    Revoke {
        /// Canonical plugin key.
        plugin: String,
        /// Owner cluster id (default: the only enrolled cluster).
        #[arg(long)]
        cluster: Option<String>,
    },
}

/// JSON for one consent row.
fn consent_json(row: &RemotePluginConsent) -> serde_json::Value {
    let parse = |raw: Option<&str>| {
        raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    json!({
        "cluster_id": row.owner_cluster_id,
        "plugin_key": row.plugin_key,
        "requested_version": row.requested_version,
        "requested_grant": parse(Some(&row.requested_grant_json)),
        "request_digest": row.request_digest(),
        "approved_version": row.version,
        "grant": parse(row.grant_json.as_deref()),
        "approved_at": row.approved_at,
        "approved_by": row.approved_by,
        "revoked_at": row.revoked_at,
        "state": state(row),
    })
}

/// A row's consent state.
fn state(row: &RemotePluginConsent) -> &'static str {
    remote_consent_state(
        &row.plugin_key,
        &row.requested_grant_json,
        row.approved_request_json.as_deref(),
        row.grant_json.as_deref(),
    )
    .as_str()
}

/// The cluster a command acts on: the named one, or the only enrolled one.
async fn cluster_id(
    store: &bookclerk_library::LibraryStore,
    named: Option<String>,
) -> anyhow::Result<String> {
    if let Some(named) = named {
        return Ok(named.trim().to_string());
    }
    let links = list_cluster_links(store).await?;
    match links.as_slice() {
        [link] => Ok(link.owner_cluster_id.clone()),
        [] => anyhow::bail!("this install is not enrolled in any cluster"),
        _ => anyhow::bail!("enrolled in several clusters; pass --cluster"),
    }
}

/// Runs a `clusters` verb.
///
/// # Errors
///
/// Returns an error when the database cannot be opened, a read or write
/// fails, or an approval is refused.
pub async fn run(
    command: ClustersCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    match command {
        ClustersCommand::List => {
            let links = list_cluster_links(store).await?;
            let rows: Vec<_> = links
                .iter()
                .map(|link| {
                    json!({
                        "cluster_id": link.owner_cluster_id,
                        "owner_origin": link.owner_origin,
                        "node_id": link.remote_node_id,
                        "state": link.state,
                        "enrolled_at": link.enrolled_at,
                    })
                })
                .collect();
            emit(format, &json!({ "clusters": rows }), || {
                for link in &links {
                    println!(
                        "{} {} node={} {}",
                        link.owner_cluster_id, link.owner_origin, link.remote_node_id, link.state
                    );
                }
            })
        }
        ClustersCommand::Consents { cluster } => {
            let rows = list_remote_plugin_consents(store, cluster.as_deref()).await?;
            emit(
                format,
                &json!({ "consents": rows.iter().map(consent_json).collect::<Vec<_>>() }),
                || {
                    for row in &rows {
                        println!(
                            "{} {} {} {} request={}",
                            row.owner_cluster_id,
                            row.plugin_key,
                            row.requested_version,
                            state(row),
                            row.request_digest()
                        );
                    }
                },
            )
        }
        ClustersCommand::Approve {
            plugin,
            cluster,
            request_digest,
            deny_domains,
            deny_bindings,
            deny_network,
        } => {
            let cluster = cluster_id(store, cluster).await?;
            let row = approve_remote_request(
                store,
                &cluster,
                plugin.trim(),
                &request_digest,
                &RemoteNarrowing {
                    deny_domains: deny_domains.into_iter().collect::<BTreeSet<_>>(),
                    deny_bindings: deny_bindings.into_iter().collect::<BTreeSet<_>>(),
                    deny_network,
                },
                RemoteApprover::Operator(CLI_APPROVER),
            )
            .await?;
            emit(format, &json!({ "consent": consent_json(&row) }), || {
                println!(
                    "approved {} {} for cluster {} ({})",
                    row.plugin_key,
                    row.requested_version,
                    row.owner_cluster_id,
                    state(&row)
                );
            })
        }
        ClustersCommand::Revoke { plugin, cluster } => {
            let cluster = cluster_id(store, cluster).await?;
            let Some(row) = revoke_remote_plugin(store, &cluster, plugin.trim()).await? else {
                anyhow::bail!("cluster {cluster} has not asked to run {plugin} here");
            };
            emit(format, &json!({ "consent": consent_json(&row) }), || {
                println!(
                    "revoked {} for cluster {}",
                    row.plugin_key, row.owner_cluster_id
                );
            })
        }
    }
}
