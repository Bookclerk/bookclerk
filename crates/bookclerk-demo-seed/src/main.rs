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
    best_match, distinct_records, lookups, search_query, seed, Lookup, Seeded, StoreRecords,
    STOREFRONTS,
};
use bookclerk_library::control_plane::{
    default_instance_for_key, ensure_hand_managed, ensure_plugin_deployment,
    load_deployment_for_node, replace_plugin_deployment, ConfigActor, DeploymentReplace,
    DesiredState,
};
use bookclerk_plugin_host::{consent_request, HostControlPlane, PluginGrantStore, SessionServices};
use bookclerk_source::{CatalogHit, CatalogSearchOpts};
use chrono::Utc;

/// How long starting the storefronts and all catalog lookups together may
/// take. The visitor is waiting on this; a title whose search has not
/// answered by then keeps the seed's own fields.
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
    // The storefronts enroll without a grant, so none is deployed yet; the
    // Worker enables them through bookclerkd only after /health. Enable
    // them here the same way, then reopen so their deployments are present.
    let records = match enable_storefronts(&host).await {
        Ok(()) => match bookclerk_plugin_host::open_host_control_plane(&config).await {
            Ok(enabled) => store_records(&enabled).await,
            Err(err) => {
                eprintln!("bookclerk-demo-seed: could not reopen the library ({err}); using sample fields");
                StoreRecords::new()
            }
        },
        Err(err) => {
            eprintln!("bookclerk-demo-seed: could not enable the storefronts ({err:#}); using sample fields");
            StoreRecords::new()
        }
    };
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
    let started = tokio::time::Instant::now();
    let deadline = started + LOOKUP_BUDGET;
    let services = SessionServices::from_outbox(Some(&host.store));
    let loading = host.load_sources(&services);
    let sources = match tokio::time::timeout_at(deadline, loading).await {
        Ok(Ok(sources)) => sources,
        Ok(Err(err)) => {
            eprintln!(
                "bookclerk-demo-seed: storefronts did not start ({err}); using sample fields"
            );
            return StoreRecords::new();
        }
        Err(_) => {
            eprintln!(
                "bookclerk-demo-seed: storefronts took too long to start; using sample fields"
            );
            return StoreRecords::new();
        }
    };
    // Each search has the rest of the budget, so a slow store costs only
    // its own titles.
    let searches = lookups().into_iter().map(|lookup| {
        let source = sources.get(lookup.store);
        async move {
            let source = source?;
            let opts = CatalogSearchOpts {
                query: search_query(&lookup),
                region: "us".into(),
                limit: LOOKUP_HITS,
                ..CatalogSearchOpts::default()
            };
            let hits = match tokio::time::timeout_at(deadline, source.search_catalog(&opts)).await {
                Ok(Ok(hits)) => hits,
                Ok(Err(err)) => {
                    tracing::warn!(store = lookup.store, %err, "catalog search failed");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(store = lookup.store, "catalog search timed out");
                    return None;
                }
            };
            best_match(&lookup, &hits).map(|hit| (lookup, hit.clone()))
        }
    });
    let found: Vec<Option<(Lookup, CatalogHit)>> = futures::future::join_all(searches).await;
    distinct_records(
        found
            .into_iter()
            .flatten()
            .map(|(lookup, hit)| (lookup.key, hit))
            .collect(),
    )
}

/// Approves each enrolled storefront's requested grant and deploys its
/// default instance on this node: what the Worker does through bookclerkd's
/// operator API once the daemon is up (tools/demo/src/lifecycle.js), and
/// what `bookclerk plugins approve --yes` and `plugins deploy` do. The grant
/// is exactly what the plugin's manifest asks for, so the Worker's later
/// calls find both already in place.
///
/// # Errors
///
/// Returns an error when the grants file or a deployment cannot be written.
async fn enable_storefronts(host: &HostControlPlane) -> anyhow::Result<()> {
    let files_dir = &host.config.paths().files_dir;
    let actor = ConfigActor::Operator {
        id: "demo-seed".into(),
    };
    let mut grants = PluginGrantStore::load(files_dir)?;
    let mut fronts = Vec::new();
    for front in STOREFRONTS {
        if let Some(enrolled) = host.enrolled.iter().find(|e| e.plugin.alias() == front.id) {
            let plugin = &enrolled.plugin;
            grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
            fronts.push(plugin.plugin_key().canonical());
        }
    }
    grants.save(files_dir)?;
    for key in fronts {
        let instance = default_instance_for_key(&host.store, key)
            .await?
            .with_context(|| format!("no default instance for {key}"))?;
        // A scheduler-placed instance is not the seed's to deploy, as for
        // `plugins deploy`.
        ensure_hand_managed(&host.store, &instance.id).await?;
        let present = DesiredState::operator(true);
        let current = match load_deployment_for_node(&host.store, &instance.id, host.node_id())
            .await?
        {
            Some(current) => current,
            None => {
                ensure_plugin_deployment(&host.store, &actor, &instance.id, host.node_id(), present)
                    .await?
            }
        };
        if current.desired == present {
            continue;
        }
        let operation = format!(
            "demo-seed-deploy-{}-{}",
            instance.id,
            Utc::now().timestamp_micros()
        );
        match replace_plugin_deployment(
            &host.store,
            &actor,
            &current.deployment_id,
            current.revision,
            present,
            &operation,
        )
        .await?
        {
            DeploymentReplace::Applied(_) | DeploymentReplace::Replayed { .. } => {}
            DeploymentReplace::Conflict { current_revision } => {
                bail!("deployment of {key} changed concurrently (revision {current_revision})")
            }
        }
    }
    Ok(())
}
