//! `bookclerk-demo-seed`: fills a fresh hosted-demo library with sample data
//! (see the library docs), then exits. The demo image runs it once before
//! bookclerkd, with the same environment, so it opens the same library the
//! same way the CLI does.

use std::process::ExitCode;

use anyhow::Context;
use bookclerk_config::Config;
use bookclerk_demo_seed::{seed, Seeded, SAMPLE_STORE};

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
/// Returns an error when the library cannot be opened, the sample
/// storefront is not enrolled, or a write fails.
async fn run() -> anyhow::Result<()> {
    let config = Config::load(None, None).context("load config")?;
    let host = bookclerk_plugin_host::open_host_control_plane(&config)
        .await
        .context("open the library")?;
    let plugin_key = host
        .enrolled
        .iter()
        .find(|e| e.plugin.alias() == SAMPLE_STORE)
        .map(|e| e.plugin.plugin_key().to_string())
        .with_context(|| format!("the `{SAMPLE_STORE}` plugin is not enrolled"))?;
    match seed(&host.store, &host.session.cluster_id, &plugin_key).await? {
        Seeded::Wrote(report) => eprintln!(
            "bookclerk-demo-seed: added user {} with {} books, {} branch titles and {} wishlist items",
            report.user_id, report.books, report.branch_titles, report.wishlist
        ),
        Seeded::AlreadySeeded => eprintln!("bookclerk-demo-seed: already seeded"),
    }
    Ok(())
}
