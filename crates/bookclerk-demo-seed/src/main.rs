//! `bookclerk-demo-seed`: fills a fresh hosted-demo library with sample data
//! (see the library docs), then exits. The demo image runs it once before
//! bookclerkd, with the same environment, so it opens the same library the
//! same way the CLI does.

use std::process::ExitCode;

use std::collections::BTreeMap;

use anyhow::{bail, Context};
use bookclerk_config::Config;
use bookclerk_demo_seed::{seed, Seeded, STOREFRONTS};
use chrono::Utc;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BOOKCLERK_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bookclerk-demo-seed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Opens the library and its control plane (enrolling the staged plugins,
/// as bookclerkd will), then seeds it.
///
/// # Errors
///
/// Returns an error when the library cannot be opened, no storefront is
/// enrolled, or a write fails.
async fn run() -> anyhow::Result<()> {
    let config = Config::load(None, None).context("load config")?;
    let host = bookclerk_plugin_host::open_host_control_plane(&config)
        .await
        .context("open the library")?;
    let plugin_keys: BTreeMap<&str, String> = STOREFRONTS
        .iter()
        .filter_map(|front| {
            host.enrolled
                .iter()
                .find(|e| e.plugin.alias() == front.id)
                .map(|e| (front.id, e.plugin.plugin_key().to_string()))
        })
        .collect();
    if plugin_keys.is_empty() {
        bail!("no storefront plugin is enrolled");
    }
    for front in STOREFRONTS {
        if !plugin_keys.contains_key(front.id) {
            eprintln!(
                "bookclerk-demo-seed: the `{}` plugin is not enrolled; leaving its titles out",
                front.id
            );
        }
    }
    match seed(
        &host.store,
        &host.session.cluster_id,
        &plugin_keys,
        Utc::now(),
    )
    .await?
    {
        // No ids or counts: they come from the library and stay out of logs.
        Seeded::Wrote(_) => {
            eprintln!("bookclerk-demo-seed: added the sample member, books, branch and wishlist")
        }
        Seeded::AlreadySeeded => eprintln!("bookclerk-demo-seed: already seeded"),
    }
    Ok(())
}
