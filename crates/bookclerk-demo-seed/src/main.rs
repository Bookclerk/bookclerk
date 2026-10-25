//! `bookclerk-demo-seed`: fills a fresh hosted-demo library with sample data
//! (see the library docs), then exits. The demo image runs it once before
//! bookclerkd, with the same environment, so it opens the same library the
//! same way the CLI does.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context};
use bookclerk_config::Config;
use bookclerk_demo_seed::{
    best_match, distinct_records, lookups, seed, Lookup, Seeded, StoreRecords, STOREFRONTS,
};
use bookclerk_plugin_host::{HostControlPlane, SessionServices};
use bookclerk_source::{CatalogHit, CatalogSearchOpts};
use chrono::Utc;

/// How long all catalog lookups together may take. The visitor is waiting
/// on this, and the seed falls back to its own fields for whatever is left.
const LOOKUP_BUDGET: Duration = Duration::from_secs(15);

/// Search hits asked of a storefront per title.
const LOOKUP_HITS: usize = 10;

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
    if host
        .store
        .get_user_by_login_name(bookclerk_demo_seed::SAMPLE_LOGIN)
        .await?
        .is_some()
    {
        eprintln!("bookclerk-demo-seed: already seeded");
        return Ok(());
    }
    let records = store_records(&host).await;
    let missing = lookups().len().saturating_sub(records.len());
    if missing > 0 {
        eprintln!(
            "bookclerk-demo-seed: {missing} title(s) had no storefront record; using sample fields"
        );
    }
    match seed(
        &host.store,
        &host.session.cluster_id,
        &plugin_keys,
        &records,
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

/// Looks every sample title up in its storefront's public catalog, through
/// the storefront plugins, within [`LOOKUP_BUDGET`].
///
/// Never fails: a storefront that does not start, a search that errors or
/// times out, or a title with no confident match just has no record.
async fn store_records(host: &HostControlPlane) -> StoreRecords {
    let sources = match host
        .load_sources(&SessionServices::from_outbox(Some(&host.store)))
        .await
    {
        Ok(sources) => sources,
        Err(err) => {
            eprintln!(
                "bookclerk-demo-seed: storefronts did not start ({err}); using sample fields"
            );
            return StoreRecords::new();
        }
    };
    let searches = lookups().into_iter().map(|lookup| {
        let source = sources.get(lookup.store);
        async move {
            let source = source?;
            let opts = CatalogSearchOpts {
                query: format!("{} {}", lookup.title, lookup.author),
                region: "us".into(),
                limit: LOOKUP_HITS,
                ..CatalogSearchOpts::default()
            };
            let hits = match source.search_catalog(&opts).await {
                Ok(hits) => hits,
                Err(err) => {
                    tracing::warn!(store = lookup.store, %err, "catalog search failed");
                    return None;
                }
            };
            best_match(&lookup, &hits).map(|hit| (lookup, hit.clone()))
        }
    });
    let found: Vec<Option<(Lookup, CatalogHit)>> =
        match tokio::time::timeout(LOOKUP_BUDGET, futures::future::join_all(searches)).await {
            Ok(found) => found,
            Err(_) => {
                eprintln!("bookclerk-demo-seed: storefront lookups timed out; using sample fields");
                Vec::new()
            }
        };
    distinct_records(
        found
            .into_iter()
            .flatten()
            .map(|(lookup, hit)| (lookup.key, hit))
            .collect(),
    )
}
