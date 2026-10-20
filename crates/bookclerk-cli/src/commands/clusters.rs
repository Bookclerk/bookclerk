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
    ClusterLink, RemoteApprover, RemotePluginConsent,
};
use bookclerk_plugin_host::{
    approve_remote_request, remote_consent_view, remote_row_consent_state, RemoteNarrowing,
};
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
    Join {
        /// Anything given after `join`. Taken only to be refused with a
        /// fixed message: left to the parser, an unexpected argument (a
        /// link pasted on the command line) is echoed in its error.
        #[arg(hide = true, num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<std::ffi::OsString>,
    },
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
    /// Name the request you reviewed by the digest `clusters consents`
    /// shows (it covers the version, the grant, the archive URL, the
    /// archive digest and the payload digest); it is refused when the
    /// cluster has changed the request since.
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

/// JSON for one consent row ([`remote_consent_view`], the view the
/// daemon's API answers too).
fn consent_json(row: &RemotePluginConsent) -> serde_json::Value {
    remote_consent_view(row)
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
/// terminal with echo off.
fn read_link_from_stdin() -> anyhow::Result<secrecy::SecretString> {
    // Zeroed on drop; the link lives on only as a secret.
    let text = crate::secret_input::read_secret_stdin("enrollment link: ")?;
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
    remote_row_consent_state(row).as_str()
}

/// `text` from an owner, safe to print to a terminal: control characters
/// (C0 and C1, which can start escape sequences) and bidirectional
/// formatting characters (which can reorder what the operator reads) are
/// written as `\u{..}` escapes.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            let bidi = matches!(
                c,
                '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            );
            if c.is_control() || bidi {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// The cluster a command acts on: the named one, or the only one this
/// install is still enrolled in.
///
/// A left or revoked link is kept as a record but no longer counts as an
/// enrollment, so after leaving one cluster and joining another the new one
/// is the default. A named cluster is taken as given, whatever its state.
///
/// # Errors
///
/// Returns an error when the links cannot be read, or when no cluster is
/// named and there is not exactly one active link.
async fn cluster_id(
    store: &bookclerk_library::LibraryStore,
    named: Option<String>,
) -> anyhow::Result<String> {
    if let Some(named) = named {
        return Ok(named.trim().to_string());
    }
    let links: Vec<_> = list_cluster_links(store)
        .await?
        .into_iter()
        .filter(ClusterLink::is_active)
        .collect();
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
    if matches!(&command, ClustersCommand::Join { argv } if !argv.is_empty()) {
        anyhow::bail!(
            "`clusters join` takes no arguments: paste the enrollment link at the prompt or pipe it on stdin"
        );
    }
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
                        printable(&link.owner_cluster_id),
                        printable(&link.owner_origin),
                        printable(&link.remote_node_id),
                        link.state
                    );
                }
            })
        }
        ClustersCommand::Join { .. } => {
            use secrecy::ExposeSecret as _;
            let link = read_link_from_stdin()?;
            let joined = bookclerk_node_client::join_cluster(
                store,
                link.expose_secret(),
                env!("CARGO_PKG_VERSION"),
            )
            .await
            // The error can carry the owner's own text (its error string, or
            // a node id it answered with), so it is escaped like any other.
            .map_err(|err| {
                anyhow::anyhow!(
                    "could not join the cluster: {}",
                    printable(&err.to_string())
                )
            })?;
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
                            "{} {} {} {} request={} artifact={} sha256={} payload={}",
                            printable(&row.owner_cluster_id),
                            printable(&row.plugin_key),
                            printable(&row.requested_version),
                            state(row),
                            row.request_digest(),
                            printable(&row.requested_artifact_url),
                            printable(&row.requested_artifact_sha256),
                            printable(&row.requested_artifact_payload_sha256)
                        );
                        // The capabilities the digest covers, so an approval
                        // by digest names something the operator saw.
                        println!("  grant: {}", printable(&row.requested_grant_json));
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
                    printable(&row.plugin_key),
                    printable(&row.requested_version),
                    printable(&row.owner_cluster_id),
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
                anyhow::bail!(
                    "cluster {} has not asked to run {} here",
                    printable(&cluster),
                    printable(&plugin)
                );
            };
            emit(format, &json!({ "consent": consent_json(&row) }), || {
                println!(
                    "revoked {} for cluster {}",
                    printable(&row.plugin_key),
                    printable(&row.owner_cluster_id)
                );
            })
        }
    }
}

/// Most job ids one status request may carry; matches the owner's
/// `MAX_STATUS_JOBS` cap on the node protocol's job-status call.
const STATUS_BATCH: usize = 64;

/// Polls the owner (through this daemon) until every job in `ids` it lets
/// this node see is finished, printing state changes; fails when any did
/// not succeed or is not visible, since its outcome is then unknown.
///
/// Ids go out in batches of [`STATUS_BATCH`], and a finished or hidden
/// job is dropped from later polls, so a scan of many accounts never
/// exceeds the owner's per-request cap.
async fn wait_for_remote_jobs(
    base: &str,
    cluster: &str,
    token: Option<&str>,
    ids: &[String],
) -> anyhow::Result<()> {
    let mut pending: std::collections::BTreeSet<String> = ids.iter().cloned().collect();
    let mut seen: std::collections::BTreeMap<String, String> = Default::default();
    let mut failed = Vec::new();
    let mut hidden = Vec::new();
    while !pending.is_empty() {
        let batch: Vec<String> = pending.iter().cloned().collect();
        for chunk in batch.chunks(STATUS_BATCH) {
            let answer = crate::commands::daemon_cmd::post_json_async(
                &format!("{base}/api/config/clusters/{cluster}/jobs"),
                json!({ "job_ids": chunk }),
                token,
            )
            .await?;
            let jobs = answer["jobs"].as_array().cloned().unwrap_or_default();
            let mut visible = std::collections::BTreeSet::new();
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
                    "succeeded" => {
                        pending.remove(&id);
                    }
                    "failed" | "cancelled" => {
                        let detail = job["error_message"].as_str().unwrap_or(state);
                        failed.push(format!("{id} ({detail})"));
                        pending.remove(&id);
                    }
                    _ => {}
                }
                visible.insert(id);
            }
            // A job the owner does not let this node see (gone, or no
            // longer an account of this node's owner) has no known outcome.
            for id in chunk {
                if !visible.contains(id) && pending.remove(id) {
                    hidden.push(id.clone());
                }
            }
        }
        if !pending.is_empty() {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
    if !failed.is_empty() {
        anyhow::bail!("scan did not succeed: {}", failed.join(", "));
    }
    if !hidden.is_empty() {
        anyhow::bail!(
            "the outcome of {} scan job(s) is unknown: not visible to this node: {}",
            hidden.len(),
            hidden.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::printable;

    #[test]
    fn owner_text_cannot_steer_the_terminal() {
        assert_eq!(printable("plain {\"a\":1}"), "plain {\"a\":1}");
        assert_eq!(printable("a\u{1b}[2Jb"), "a\\u{1b}[2Jb");
        assert_eq!(printable("a\u{9b}2Jb"), "a\\u{9b}2Jb");
        assert_eq!(printable("x\u{202e}y\u{2066}z"), "x\\u{202e}y\\u{2066}z");
    }
}
