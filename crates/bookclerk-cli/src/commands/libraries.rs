//! `bookclerk libraries`: this install as a branch node of libraries,
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
//! `join --url <owner>` joins by signing in to the owner library in a
//! browser (an OAuth code with PKCE, sent back to a loopback listener this
//! command opens). `--as shared-branch` offers to join as a shared branch instead of a
//! branch (a shared node sharing the library's database; only an empty
//! install, and only when the library's Owner chooses it). Without `--url`, `join` reads an enrollment link (a
//! branch invite) from stdin or a prompt, never from argv: the link carries
//! a single-use secret. A running daemon dials a newly joined library
//! within a few seconds, and stops a left one's client as quickly. `leave`
//! tells the owner first (`node.leaving`), which revokes the node there.

use std::collections::BTreeSet;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    leave_library_link, list_library_links, list_remote_plugin_consents, load_library_link,
    load_library_link_keys, revoke_remote_plugin, LibraryLink, NodeSampler, RemoteApprover,
    RemotePluginConsent,
};
use bookclerk_plugin_host::{
    approve_remote_request, remote_consent_view, remote_row_changes, remote_row_consent_state,
    RemoteNarrowing,
};
use clap::Subcommand;
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

/// Audit identity of approvals made with the CLI.
const CLI_APPROVER: &str = "cli";

/// `libraries join --as`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum JoinAs {
    /// A branch (branch node) with its own database.
    Branch,
    /// A shared branch (shared node) sharing the library's database and key.
    SharedBranch,
}

#[derive(Debug, Subcommand)]
/// `bookclerk libraries` verbs.
pub enum LibrariesCommand {
    /// The libraries this install is enrolled in as a host.
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
        /// The library's certificate fingerprint as Settings → Branches
        /// shows it (a `sha256:` prefix is accepted), confirming a
        /// self-signed library without a prompt.
        #[arg(long, requires = "url")]
        fingerprint: Option<String>,
        /// How to join: `branch` (default; keeps this install's own
        /// database) or `shared_branch` (shares the library's database and key;
        /// only an empty install, and only when an Owner of the library
        /// chooses it after a fresh second factor).
        #[arg(long = "as", value_enum, default_value_t = JoinAs::Branch)]
        join_as: JoinAs,
        /// Anything given after `join` that is not a flag. Taken only to be
        /// refused with a fixed message: left to the parser, an unexpected
        /// argument (an invite pasted on the command line) is echoed in its
        /// error.
        #[arg(hide = true, num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<std::ffi::OsString>,
    },
    /// Leave a library: the owner is told (and revokes this node
    /// when it hears), the link stops being dialed and its node keys are
    /// deleted.
    Leave {
        /// Library id (default: the only enrolled library).
        #[arg(long)]
        library: Option<String>,
    },
    /// What each library asks to run here, and whether it may.
    Consents {
        /// Only this library.
        #[arg(long)]
        library: Option<String>,
    },
    /// Approve an owner plugin's request to run here, optionally narrowed.
    /// Name the request you reviewed by the digest `libraries consents`
    /// shows (it covers the version, the grant, the archive URL, the
    /// archive digest and the payload digest); it is refused when the
    /// library has changed the request since.
    /// Only what the request asks for can be approved; a later version that
    /// asks for more needs approval again.
    Approve {
        /// Canonical plugin key.
        plugin: String,
        /// Library id (default: the only enrolled library).
        #[arg(long)]
        library: Option<String>,
        /// The `request_digest` of the request you reviewed, from
        /// `libraries consents` (refused when the library now asks for
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
        /// Refuse the plugin's own User-Agent on a host it asks for; its
        /// requests there carry Bookclerk's agent. Repeatable.
        #[arg(long = "deny-own-user-agent", value_name = "HOST")]
        deny_own_user_agent_hosts: Vec<String>,
        /// Deny all network access.
        #[arg(long)]
        deny_network: bool,
    },
    /// Ask a library to scan its library for this node's owner.
    ///
    /// Goes through this install's running daemon, over its link to the
    /// owner. The owner plans it as that user: an Owner or Administrator
    /// scanning every account gets one job per account, spread over the
    /// library's nodes and hosts; anyone else scans only their own
    /// accounts. Waits for the jobs unless `--no-wait`.
    Scan {
        /// Library id (default: the only enrolled library).
        #[arg(long)]
        library: Option<String>,
        /// Scan only this store account of the library.
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
        /// Library id (default: the only enrolled library).
        #[arg(long)]
        library: Option<String>,
    },
}

/// JSON for one consent row ([`remote_consent_view`], the view the
/// daemon's API answers too).
fn consent_json(row: &RemotePluginConsent) -> serde_json::Value {
    remote_consent_view(row)
}

/// JSON for one library link (no key material).
fn link_json(link: &LibraryLink) -> serde_json::Value {
    json!({
        "library_id": link.parent_library_id,
        "owner_origin": link.owner_origin,
        "node_id": link.remote_node_id,
        "pinned": link.spki_pin.is_some(),
        "state": link.state,
        "enrolled_at": link.enrolled_at,
    })
}

/// Tells an active library's owner this node is leaving
/// ([`bookclerk_node_client::announce_leaving`]); `false` when the link is
/// not active or the owner could not be told.
async fn announce_leaving(
    store: &bookclerk_library::LibraryStore,
    config: &Config,
    link: &LibraryLink,
) -> bool {
    if !link.is_active() {
        return false;
    }
    let Ok(keys) = load_library_link_keys(store, link).await else {
        return false;
    };
    bookclerk_node_client::announce_leaving(
        link.clone(),
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
pub(super) fn printable(text: &str) -> String {
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

/// The library a command acts on: the named one, or the only one this
/// install is still enrolled in.
///
/// A left or revoked link is kept as a record but no longer counts as an
/// enrollment, so after leaving one library and joining another the new one
/// is the default. A named library is taken as given, whatever its state.
///
/// # Errors
///
/// Returns an error when the links cannot be read, or when no library is
/// named and there is not exactly one active link.
async fn library_id(
    store: &bookclerk_library::LibraryStore,
    named: Option<String>,
) -> anyhow::Result<String> {
    if let Some(named) = named {
        return Ok(named.trim().to_string());
    }
    let links: Vec<_> = list_library_links(store)
        .await?
        .into_iter()
        .filter(LibraryLink::is_active)
        .collect();
    match links.as_slice() {
        [link] => Ok(link.parent_library_id.clone()),
        [] => anyhow::bail!("this install is not enrolled in any library"),
        _ => anyhow::bail!("enrolled in several libraries; pass --library"),
    }
}

/// Runs a `libraries` verb.
///
/// # Errors
///
/// Returns an error when the database cannot be opened, a read or write
/// fails, or an approval is refused.
pub async fn run(
    command: LibrariesCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if matches!(&command, LibrariesCommand::Join { argv, .. } if !argv.is_empty()) {
        anyhow::bail!(
            "`libraries join` takes no arguments: paste the enrollment link at the prompt or pipe it on stdin"
        );
    }
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    match command {
        LibrariesCommand::List => {
            let links = list_library_links(store).await?;
            let rows: Vec<_> = links.iter().map(link_json).collect();
            emit(format, &json!({ "libraries": rows }), || {
                for link in &links {
                    println!(
                        "{} {} node={} {}",
                        printable(&link.parent_library_id),
                        printable(&link.owner_origin),
                        printable(&link.remote_node_id),
                        link.state
                    );
                }
            })
        }
        LibrariesCommand::Join {
            url: Some(url),
            name,
            fingerprint,
            join_as,
            ..
        } => {
            let joined = super::libraries_join::join_with_sign_in(
                store,
                config,
                &url,
                name.as_deref(),
                fingerprint.as_deref(),
                join_as == JoinAs::SharedBranch,
            )
            .await?;
            match joined {
                super::libraries_join::Joined::Branch(joined) => emit(
                    format,
                    &json!({ "library": link_json(&joined), "connection": "branch" }),
                    || {
                        println!(
                            "joined library {} at {} as node {}; a running daemon connects within a few seconds",
                            joined.parent_library_id, joined.owner_origin, joined.remote_node_id
                        );
                    },
                ),
                super::libraries_join::Joined::SharedBranch {
                    library_id,
                    node_id,
                    applied,
                } => emit(
                    format,
                    &json!({
                        "connection": "shared_branch",
                        "library_id": library_id,
                        "node_id": node_id,
                        "database": applied.database.as_str(),
                        "master_key_wrapped": applied.wrapped,
                        "restart_required": true,
                    }),
                    || {
                        println!(
                            "joined library {library_id} as shared branch node {node_id} ({} database); restart bookclerkd to start as a shared node of the library",
                            applied.database.as_str()
                        );
                    },
                ),
            }
        }
        LibrariesCommand::Join {
            url: None, join_as, ..
        } => {
            if join_as == JoinAs::SharedBranch {
                anyhow::bail!("a shared branch joins with a sign-in: pass --url <library>");
            }
            use secrecy::ExposeSecret as _;
            let link = read_link_from_stdin()?;
            let joined = bookclerk_node_client::join_library(
                store,
                link.expose_secret(),
                env!("CARGO_PKG_VERSION"),
            )
            .await
            // The error can carry the owner's own text (its error string, or
            // a node id it answered with), so it is escaped like any other.
            .map_err(|err| {
                anyhow::anyhow!(
                    "could not join the library: {}",
                    printable(&err.to_string())
                )
            })?;
            drop(link);
            emit(format, &json!({ "library": link_json(&joined) }), || {
                println!(
                    "joined library {} at {} as node {}; a running daemon connects within a few seconds",
                    joined.parent_library_id, joined.owner_origin, joined.remote_node_id
                );
            })
        }
        LibrariesCommand::Leave { library } => {
            let library = library_id(store, library).await?;
            let Some(link) = load_library_link(store, &library).await? else {
                anyhow::bail!("this install is not enrolled in library {library}");
            };
            let notified = announce_leaving(store, config, &link).await;
            // Only the node the owner was told about is left.
            let Some(left) =
                leave_library_link(store, &library, Some(&link.remote_node_id)).await?
            else {
                anyhow::bail!("this install is not enrolled in library {library}");
            };
            emit(
                format,
                &json!({ "library": link_json(&left), "owner_notified": notified }),
                || {
                    if notified {
                        println!(
                            "left library {}; the owner revoked node {} and its node keys are deleted",
                            left.parent_library_id, left.remote_node_id
                        );
                    } else {
                        println!(
                            "left library {}; its node keys are deleted. The owner could not be told: revoke node {} there",
                            left.parent_library_id, left.remote_node_id
                        );
                    }
                },
            )
        }
        LibrariesCommand::Consents { library } => {
            let rows = list_remote_plugin_consents(store, library.as_deref()).await?;
            emit(
                format,
                &json!({ "consents": rows.iter().map(consent_json).collect::<Vec<_>>() }),
                || {
                    for row in &rows {
                        println!(
                            "{} {} {} {} request={} artifact={} sha256={} payload={}",
                            printable(&row.parent_library_id),
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
                        // What changed since the approval, as the local
                        // consent flow and the web UI show it.
                        for line in remote_row_changes(row) {
                            println!("  {}", printable(&line));
                        }
                    }
                },
            )
        }
        LibrariesCommand::Approve {
            plugin,
            library,
            request_digest,
            deny_domains,
            deny_bindings,
            deny_own_user_agent_hosts,
            deny_network,
        } => {
            let library = library_id(store, library).await?;
            let row = approve_remote_request(
                store,
                &library,
                plugin.trim(),
                &request_digest,
                &RemoteNarrowing {
                    deny_domains: deny_domains.into_iter().collect::<BTreeSet<_>>(),
                    deny_bindings: deny_bindings.into_iter().collect::<BTreeSet<_>>(),
                    deny_own_user_agent_hosts: deny_own_user_agent_hosts
                        .into_iter()
                        .collect::<BTreeSet<_>>(),
                    deny_network,
                },
                RemoteApprover::Operator(CLI_APPROVER),
            )
            .await?;
            emit(format, &json!({ "consent": consent_json(&row) }), || {
                println!(
                    "approved {} {} for library {} ({})",
                    printable(&row.plugin_key),
                    printable(&row.requested_version),
                    printable(&row.parent_library_id),
                    state(&row)
                );
            })
        }
        LibrariesCommand::Scan {
            library,
            account,
            instance,
            no_wait,
        } => {
            let library = library_id(store, library).await?;
            let base = crate::commands::daemon_cmd::daemon_base_url(config);
            let token = crate::commands::daemon_cmd::operator_bearer(config).await?;
            let planned = crate::commands::daemon_cmd::post_json_async(
                &format!("{base}/api/config/libraries/{library}/scan"),
                json!({ "account": account, "plugin_instance_id": instance }),
                token.as_deref(),
            )
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "{err} (the daemon must be running and connected to library {library})"
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
                wait_for_remote_jobs(&base, &library, token.as_deref(), &ids).await?;
                // Progress went to stderr; a JSON caller still gets the plan.
                emit(format, &planned, || {})?;
            }
            if ids.is_empty() && full == 0 {
                anyhow::bail!("nothing to scan: no store account of this node's owner to scan");
            }
            if full > 0 {
                anyhow::bail!("{full} scan job(s) were not queued: the owner's queue is full");
            }
            Ok(())
        }
        LibrariesCommand::Revoke { plugin, library } => {
            let library = library_id(store, library).await?;
            let Some(row) = revoke_remote_plugin(store, &library, plugin.trim()).await? else {
                anyhow::bail!(
                    "library {} has not asked to run {} here",
                    printable(&library),
                    printable(&plugin)
                );
            };
            emit(format, &json!({ "consent": consent_json(&row) }), || {
                println!(
                    "revoked {} for library {}",
                    printable(&row.plugin_key),
                    printable(&row.parent_library_id)
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
    library: &str,
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
                &format!("{base}/api/config/libraries/{library}/jobs"),
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
    use super::*;
    use clap::Parser as _;

    #[derive(clap::Parser)]
    struct Wrapper {
        #[command(subcommand)]
        command: LibrariesCommand,
    }

    fn join_as(args: &[&str]) -> Result<JoinAs, clap::Error> {
        let parsed = Wrapper::try_parse_from(["libraries"].iter().chain(args))?;
        match parsed.command {
            LibrariesCommand::Join { join_as, .. } => Ok(join_as),
            other => panic!("not a join: {other:?}"),
        }
    }

    #[test]
    fn join_is_a_branch_unless_asked_for_a_shared_branch() {
        let url = ["join", "--url", "https://library.example"];
        assert_eq!(join_as(&url).unwrap(), JoinAs::Branch);
        assert_eq!(
            join_as(&[&url[..], &["--as", "shared-branch"]].concat()).unwrap(),
            JoinAs::SharedBranch
        );
        assert_eq!(
            join_as(&[&url[..], &["--as", "branch"]].concat()).unwrap(),
            JoinAs::Branch
        );
        assert!(join_as(&[&url[..], &["--as", "peer"]].concat()).is_err());
    }

    #[test]
    fn owner_text_cannot_steer_the_terminal() {
        assert_eq!(printable("plain {\"a\":1}"), "plain {\"a\":1}");
        assert_eq!(printable("a\u{1b}[2Jb"), "a\\u{1b}[2Jb");
        assert_eq!(printable("a\u{9b}2Jb"), "a\\u{9b}2Jb");
        assert_eq!(printable("x\u{202e}y\u{2066}z"), "x\\u{202e}y\\u{2066}z");
    }
}
