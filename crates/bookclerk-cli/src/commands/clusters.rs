//! `bookclerk clusters`: this install as a host node of owner clusters,
//! and this host operator's consent to the plugins they ask to run here
//! (H-4 design, section 5).
//!
//! Reads and writes the library database directly, like `nodes`. A
//! running daemon re-reads consent within a few seconds, so an approval
//! lets the plugin's jobs be claimed and a revoke cancels and gives back
//! the plugin's jobs held here.
//!
//! `join --url <owner>` joins by signing in to the owner library in a
//! browser (an OAuth code with PKCE, sent back to a loopback listener this
//! command opens). Without `--url`, `join` reads an enrollment link (a
//! branch invite) from stdin or a prompt, never from argv: the link carries
//! a single-use secret. A running daemon dials a newly joined cluster
//! within a few seconds, and stops a left one's client as quickly. `leave`
//! tells the owner first (`node.leaving`), which revokes the node there.

use std::collections::BTreeSet;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    leave_cluster_link, list_cluster_links, list_remote_plugin_consents, load_cluster_link,
    load_cluster_link_keys, revoke_remote_plugin, ClusterLink, NodeSampler, RemotePluginConsent,
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
    /// Enroll this install as a host (a branch) of an owner library.
    ///
    /// With `--url`, prints a sign-in link (and opens a browser when it
    /// can): a user of that library signs in and approves, and the answer
    /// comes back to a loopback listener this command opens. Without it,
    /// reads a branch invite (`https://<owner>/enroll#bcn1...`) from stdin,
    /// or prompts for it; an invite is never taken from the command line.
    Join {
        /// The owner library's address (`https://library.example`).
        #[arg(long)]
        url: Option<String>,
        /// The name the owner sees for this branch (default: host name).
        #[arg(long, requires = "url")]
        name: Option<String>,
        /// The owner's certificate fingerprint (`sha256:...`), confirming
        /// a self-signed library without a prompt.
        #[arg(long, requires = "url")]
        fingerprint: Option<String>,
    },
    /// Leave an owner cluster: the owner is told (and revokes this node
    /// when it hears), the link stops being dialed and its node keys are
    /// deleted.
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

/// Tells an active cluster's owner this node is leaving
/// ([`bookclerk_node_client::announce_leaving`]); `false` when the link is
/// not active or the owner could not be told.
async fn announce_leaving(
    store: &bookclerk_library::LibraryStore,
    config: &Config,
    cluster: &str,
) -> bool {
    let Ok(Some(link)) = load_cluster_link(store, cluster).await else {
        return false;
    };
    if !link.is_active() {
        return false;
    }
    let Ok(keys) = load_cluster_link_keys(store, &link).await else {
        return false;
    };
    bookclerk_node_client::announce_leaving(
        link,
        keys,
        env!("CARGO_PKG_VERSION"),
        Box::new(NodeSampler::new(&config.paths().files_dir)),
        LEAVE_NOTICE_TIMEOUT,
    )
    .await
}

/// How long `leave` waits for the owner to hear `node.leaving`.
const LEAVE_NOTICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
        ClustersCommand::Join {
            url: Some(url),
            name,
            fingerprint,
        } => {
            let joined = super::clusters_join::join_with_sign_in(
                store,
                &url,
                name.as_deref(),
                fingerprint.as_deref(),
            )
            .await?;
            emit(format, &json!({ "cluster": link_json(&joined) }), || {
                println!(
                    "joined library {} at {} as node {}; a running daemon connects within a few seconds",
                    joined.owner_cluster_id, joined.owner_origin, joined.remote_node_id
                );
            })
        }
        ClustersCommand::Join { url: None, .. } => {
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
                    "joined cluster {} at {} as node {}; a running daemon connects within a few seconds",
                    joined.owner_cluster_id, joined.owner_origin, joined.remote_node_id
                );
            })
        }
        ClustersCommand::Leave { cluster } => {
            let cluster = cluster_id(store, cluster).await?;
            let notified = announce_leaving(store, config, &cluster).await;
            let Some(left) = leave_cluster_link(store, &cluster).await? else {
                anyhow::bail!("this install is not enrolled in cluster {cluster}");
            };
            emit(
                format,
                &json!({ "cluster": link_json(&left), "owner_notified": notified }),
                || {
                    if notified {
                        println!(
                            "left cluster {}; the owner revoked node {} and its node keys are deleted",
                            left.owner_cluster_id, left.remote_node_id
                        );
                    } else {
                        println!(
                            "left cluster {}; its node keys are deleted. The owner could not be told: revoke node {} there",
                            left.owner_cluster_id, left.remote_node_id
                        );
                    }
                },
            )
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
