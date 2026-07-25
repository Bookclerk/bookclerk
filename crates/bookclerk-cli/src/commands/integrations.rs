//! `bookclerk integrations` — status, test, tickets, library scan.
//!
//! Host commands are integration-agnostic: they go through
//! [`bookclerk_integrations::IntegrationRegistry`] capabilities. Adapter-specific
//! clients (e.g. AbsApiClient) stay inside the integrations crate.

use bookclerk_config::Config;
use bookclerk_integrations::{mint_for_external_user, ExternalUser};
use bookclerk_library::LibraryStore;
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum IntegrationsCommand {
    /// Show health of configured integrations.
    Status,
    /// Probe connectivity for one (or all) enabled integrations.
    Test {
        /// Integration id (default: all enabled).
        #[arg(long)]
        integration: Option<String>,
    },
    /// Claim ticket management.
    Tickets {
        #[command(subcommand)]
        command: TicketsCommand,
    },
    /// Trigger a remote library scan on an integration that supports it.
    Scan {
        /// Integration id (`audiobookshelf`, …).
        #[arg(long)]
        integration: String,
        /// Force full rescan when the integration supports it.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TicketsCommand {
    /// Mint a claim ticket for an external identity.
    Create {
        /// Integration provider id (`audiobookshelf`).
        #[arg(long, default_value = "audiobookshelf")]
        provider: String,
        /// External user id from the provider.
        #[arg(long)]
        external_user_id: String,
        /// Optional display label.
        #[arg(long)]
        label: Option<String>,
    },
    /// List open (unredeemed, unexpired) claim tickets.
    List,
}

pub async fn run(command: IntegrationsCommand, config: &Config) -> anyhow::Result<()> {
    let paths = config.paths().clone();
    paths.ensure_dirs()?;
    let library = LibraryStore::open(&paths.library_db)?;
    let mut registry = bookclerk_integrations::from_config(config)?;
    bookclerk_plugin::load_external_integrations(config, &mut registry).await?;

    match command {
        IntegrationsCommand::Status => {
            let health = registry.health_all().await;
            if health.is_empty() {
                println!("no integrations enabled");
            }
            for h in health {
                println!(
                    "{} enabled={} ok={} {}",
                    h.id,
                    h.enabled,
                    h.ok,
                    h.detail.unwrap_or_default()
                );
            }
            Ok(())
        }
        IntegrationsCommand::Test { integration } => {
            let targets: Vec<_> = if let Some(id) = integration {
                let Some(i) = registry.get(&id) else {
                    anyhow::bail!("integration `{id}` is not enabled / registered");
                };
                vec![i]
            } else {
                registry.all().to_vec()
            };
            if targets.is_empty() {
                println!("no integrations enabled");
                return Ok(());
            }
            for i in targets {
                println!("== {} ({})", i.display_name(), i.id());
                for line in i.diagnose().await? {
                    println!("{line}");
                }
            }
            Ok(())
        }
        IntegrationsCommand::Tickets { command } => match command {
            TicketsCommand::Create {
                provider,
                external_user_id,
                label,
            } => {
                let user = ExternalUser {
                    provider,
                    external_user_id,
                    display_name: label,
                    access_token: None,
                };
                let minted = mint_for_external_user(&library, config, &user, "cli")?;
                println!("ticket={}", minted.token);
                if let Some(url) = minted.portal_url {
                    println!("url={url}");
                }
                println!(
                    "identity={} expires={}",
                    minted.identity.id,
                    minted.record.expires_at.to_rfc3339()
                );
                Ok(())
            }
            TicketsCommand::List => {
                let tickets = library.list_open_claim_tickets()?;
                if tickets.is_empty() {
                    println!("no open claim tickets");
                }
                for t in tickets {
                    println!(
                        "id={} identity={:?} expires={} created_by={} hash={}…",
                        t.id,
                        t.identity_id,
                        t.expires_at.to_rfc3339(),
                        t.created_by,
                        &t.token_hash[..8.min(t.token_hash.len())]
                    );
                }
                Ok(())
            }
        },
        IntegrationsCommand::Scan { integration, force } => {
            let Some(i) = registry.get(&integration) else {
                anyhow::bail!("integration `{integration}` is not enabled / registered");
            };
            if !i.supports_library_scan() {
                anyhow::bail!(
                    "integration `{}` does not support library scan (missing config?)",
                    i.id()
                );
            }
            i.scan_library(force).await?;
            println!("scan started for {}", i.id());
            Ok(())
        }
    }
}
