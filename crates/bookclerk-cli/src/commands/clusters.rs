//! `bookclerk clusters`: this install as a host node of owner clusters,
//! and this host operator's consent to the plugins they ask to run here
//! (H-4 design, section 5).
//!
//! Reads and writes the library database directly, like `nodes`. A
//! running daemon re-reads consent within a few seconds, so an approval
//! lets the plugin's jobs be claimed and a revoke cancels and gives back
//! the plugin's jobs held here.
//!
//! `scan` is the exception: it asks this install's running daemon to send
//! a `scan.request` to the owner over its link (H-5), and polls the jobs
//! it queued.
//!
//! `join` reads the enrollment link from stdin (or a prompt), never from
//! argv: the link carries a single-use secret. A running daemon dials a
//! newly joined cluster at its next start, and stops a left one's client
//! within a few seconds.

use std::collections::BTreeSet;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    leave_cluster_link, list_cluster_links, list_remote_plugin_consents, revoke_remote_plugin,
    ClusterLink, RemotePluginConsent,
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
    /// Enroll this install as a host of an owner cluster. Reads the
    /// enrollment link (`https://<owner>/enroll#bcn1...`) from stdin, or
    /// prompts for it; it is never taken from the command line.
    Join,
    /// Leave an owner cluster: the link stops being dialed and its node
    /// keys are deleted. The owner is not told; revoke the node there.
    Leave {
        /// Owner cluster id (default: the only enrolled cluster).
        #[arg(long)]
        cluster: Option<String>,
    },
    /// What each owner cluster asks to run here, and whether it may.
    Consents {
        /// Only this owner cluster.
        #[arg(long)]
        cluster: Option<String>,
    },
    /// Approve an owner plugin's request to run here, optionally narrowed.
    /// Only what the request asks for can be approved; a later version that
    /// asks for more needs approval again.
    Approve {
        /// Canonical plugin key.
        plugin: String,
        /// Owner cluster id (default: the only enrolled cluster).
        #[arg(long)]
        cluster: Option<String>,
        /// The requested version you reviewed (refused when the cluster
        /// now asks for another).
        #[arg(long)]
        version: Option<String>,
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
    /// Ask an owner cluster to scan its library for this node's owner.
    ///
    /// Goes through this install's running daemon, over its link to the
    /// owner. The owner plans it as that user: an Owner or Administrator
    /// scanning every account gets one job per account, spread over the
    /// cluster's nodes and hosts; anyone else scans only their own
    /// accounts. Waits for the jobs unless `--no-wait`.
    Scan {
        /// Owner cluster id (default: the only enrolled cluster).
        #[arg(long)]
        cluster: Option<String>,
        /// Scan only this store account of the owner cluster.
        #[arg(long)]
        account: Option<String>,
        /// Pin the scan to one owner plugin instance.
        #[arg(long)]
        instance: Option<String>,
        /// Print the queued job ids without waiting for them.
        #[arg(long)]
        no_wait: bool,
    },
    /// Withdraw an approval: held jobs of the plugin are cancelled and
    /// given back, and nothing of it is claimed until approved again.
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
        "approved_version": row.version,
        "grant": parse(row.grant_json.as_deref()),
        "approved_at": row.approved_at,
        "approved_by": row.approved_by,
        "state": state(row),
    })
}

/// JSON for one cluster link (no key material).
fn link_json(link: &ClusterLink) -> serde_json::Value {
    json!({
        "cluster_id": link.owner_cluster_id,
        "owner_origin": link.owner_origin,
        "node_id": link.remote_node_id,
        "pinned": link.spki_pin.is_some(),
        "state": link.state,
        "enrolled_at": link.enrolled_at,
    })
}

/// Reads the enrollment link: the first line of stdin, or a prompt on a
/// terminal.
fn read_link_from_stdin() -> anyhow::Result<secrecy::SecretString> {
    use std::io::{IsTerminal, Read};
    // Zeroed on drop; the link lives on only as a secret.
    let mut text = zeroize::Zeroizing::new(String::new());
    if std::io::stdin().is_terminal() {
        eprint!("enrollment link: ");
        std::io::stdin().read_line(&mut text)?;
    } else {
        std::io::stdin().read_to_string(&mut text)?;
    }
    let link = text
        .split_once('\n')
        .map_or(text.as_str(), |(first, _)| first)
        .trim()
        .to_string();
    if link.is_empty() {
        anyhow::bail!("no enrollment link on stdin");
    }
    Ok(secrecy::SecretString::from(link))
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
            let rows: Vec<_> = links.iter().map(link_json).collect();
            emit(format, &json!({ "clusters": rows }), || {
                for link in &links {
                    println!(
                        "{} {} node={} {}",
                        link.owner_cluster_id, link.owner_origin, link.remote_node_id, link.state
                    );
                }
            })
        }
        ClustersCommand::Join => {
            use secrecy::ExposeSecret as _;
            let link = read_link_from_stdin()?;
            let joined = bookclerk_node_client::join_cluster(
                store,
                link.expose_secret(),
                env!("CARGO_PKG_VERSION"),
            )
            .await
            .map_err(|err| anyhow::anyhow!("could not join the cluster: {err}"))?;
            drop(link);
            emit(format, &json!({ "cluster": link_json(&joined) }), || {
                println!(
                    "joined cluster {} at {} as node {}; restart the daemon to connect",
                    joined.owner_cluster_id, joined.owner_origin, joined.remote_node_id
                );
            })
        }
        ClustersCommand::Leave { cluster } => {
            let cluster = cluster_id(store, cluster).await?;
            let Some(left) = leave_cluster_link(store, &cluster).await? else {
                anyhow::bail!("this install is not enrolled in cluster {cluster}");
            };
            emit(format, &json!({ "cluster": link_json(&left) }), || {
                println!(
                    "left cluster {}; its node keys are deleted. Revoke node {} on the owner",
                    left.owner_cluster_id, left.remote_node_id
                );
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
                            "{} {} {} {}",
                            row.owner_cluster_id,
                            row.plugin_key,
                            row.requested_version,
                            state(row)
                        );
                    }
                },
            )
        }
        ClustersCommand::Approve {
            plugin,
            cluster,
            version,
            deny_domains,
            deny_bindings,
            deny_network,
        } => {
            let cluster = cluster_id(store, cluster).await?;
            let row = approve_remote_request(
                store,
                &cluster,
                plugin.trim(),
                version.as_deref().map(str::trim),
                &RemoteNarrowing {
                    deny_domains: deny_domains.into_iter().collect::<BTreeSet<_>>(),
                    deny_bindings: deny_bindings.into_iter().collect::<BTreeSet<_>>(),
                    deny_network,
                },
                CLI_APPROVER,
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
        ClustersCommand::Scan {
            cluster,
            account,
            instance,
            no_wait,
        } => {
            let cluster = cluster_id(store, cluster).await?;
            let base = crate::commands::daemon_cmd::daemon_base_url(config);
            let token = crate::commands::daemon_cmd::operator_bearer(config).await?;
            let planned = crate::commands::daemon_cmd::post_json_async(
                &format!("{base}/api/config/clusters/{cluster}/scan"),
                json!({ "account": account, "plugin_instance_id": instance }),
                token.as_deref(),
            )
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "{err} (the daemon must be running and connected to cluster {cluster})"
                )
            })?;
            let ids: Vec<String> = ["created", "duplicates"]
                .iter()
                .flat_map(|field| planned[*field].as_array().cloned().unwrap_or_default())
                .filter_map(|id| id.as_str().map(str::to_string))
                .collect();
            let full = planned["queue_full"].as_u64().unwrap_or(0);
            if no_wait || ids.is_empty() {
                emit(format, &planned, || {
                    for id in &ids {
                        println!("{id}");
                    }
                })?;
            } else {
                wait_for_remote_jobs(&base, &cluster, token.as_deref(), &ids).await?;
            }
            if ids.is_empty() && full == 0 {
                anyhow::bail!("nothing to scan: no store account of this node's owner to scan");
            }
            if full > 0 {
                anyhow::bail!("{full} scan job(s) were not queued: the owner's queue is full");
            }
            Ok(())
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

/// Polls the owner (through this daemon) until every job in `ids` is
/// finished, printing state changes; fails when any did not succeed.
async fn wait_for_remote_jobs(
    base: &str,
    cluster: &str,
    token: Option<&str>,
    ids: &[String],
) -> anyhow::Result<()> {
    let mut seen: std::collections::BTreeMap<String, String> = Default::default();
    loop {
        let answer = crate::commands::daemon_cmd::post_json_async(
            &format!("{base}/api/config/clusters/{cluster}/jobs"),
            json!({ "job_ids": ids }),
            token,
        )
        .await?;
        let jobs = answer["jobs"].as_array().cloned().unwrap_or_default();
        let mut finished = 0usize;
        let mut failed = Vec::new();
        for job in &jobs {
            let id = job["id"].as_str().unwrap_or_default().to_string();
            let state = job["state"].as_str().unwrap_or_default();
            let progress = job["progress"].as_str().unwrap_or_default();
            let line = format!("{state} {progress}");
            if seen.get(&id) != Some(&line) {
                eprintln!("{id}: {}", line.trim());
                seen.insert(id.clone(), line);
            }
            match state {
                "succeeded" => finished += 1,
                "failed" | "cancelled" => {
                    finished += 1;
                    let detail = job["error_message"].as_str().unwrap_or(state);
                    failed.push(format!("{id} ({detail})"));
                }
                _ => {}
            }
        }
        // A job the owner no longer lets this node see counts as gone.
        if finished == jobs.len() {
            if !failed.is_empty() {
                anyhow::bail!("scan did not succeed: {}", failed.join(", "));
            }
            if jobs.len() < ids.len() {
                eprintln!(
                    "{} job(s) are not visible to this node",
                    ids.len() - jobs.len()
                );
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
