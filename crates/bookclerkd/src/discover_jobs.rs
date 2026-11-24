//! Discover's two queued jobs and how they are admitted.
//!
//! The page never builds shelves itself any more. A `discover_candidates`
//! job fetches one subject's storefront candidates
//! ([`bookclerk_discover::fetch_subject_candidates`]) and, when it finishes,
//! admits that subject's `discover_feed` job, which ranks them into stored
//! shelves ([`bookclerk_discover::rank_subject_feed`]) with no network.
//! Both are deduplicated per subject, so a burst of triggers folds into the
//! job already waiting. See `docs/internals/discovery.md`, *Precomputed shelves*.
//!
//! A fetch asks the storefronts deployed on the node that claims it. Each
//! storefront instance deployed only on other nodes gets a fetch of its own,
//! pinned to the instance so it is claimed where that instance runs; it
//! stores that store's rows and queues the ranking a moment later, so the
//! fetches finishing together rank once.

use std::time::Duration;

use bookclerk_library::{DiscoverSubject, EnqueueJobSpec, JobKind, JobPayload, JobTrigger};
use chrono::Utc;
use tracing::info;

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};

/// How old a subject's candidates may get before a read queues a refresh
/// (the daily refresh; prices on cards still load live).
pub const CANDIDATES_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);

/// Builds the ranking options the jobs use from the live `[discovery]` config.
///
/// # Arguments
///
/// * `cfg` - Live configuration.
/// * `vectors` - The vectors compared for similarity, or `None` to score
///   candidates without it.
pub(crate) fn discover_options(
    cfg: &bookclerk_config::Config,
    vectors: Option<bookclerk_discover::VectorSource>,
) -> bookclerk_discover::RecommendOptions {
    let similarity = vectors.is_some();
    bookclerk_discover::RecommendOptions {
        limit: cfg.discovery.recommend_limit.max(36),
        vectors: vectors.unwrap_or_else(bookclerk_discover::VectorSource::stand_in),
        region: String::from("us"),
        include_purchase_hints: false,
        external_user_id: None,
        include_listening: true,
        listening_providers: Vec::new(),
        fetch_storefront_candidates: cfg.discovery.storefront_candidates,
        storefront_seed_limit: cfg.discovery.storefront_seed_limit,
        storefront_max_remote_calls: cfg.discovery.storefront_max_remote_calls,
        exclude_graphicaudio_series_sets: cfg.discovery.exclude_graphicaudio_series_sets,
        disabled_shelves: Vec::new(),
        embeddings_enabled: similarity,
    }
}

/// Admits one Discover job for `subject`, claimable after `delay`.
///
/// # Errors
///
/// Returns the admission error.
async fn enqueue(
    state: &AppState,
    kind: JobKind,
    subject: &DiscoverSubject,
    trigger: JobTrigger,
    delay: Duration,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    let run_after = (!delay.is_zero())
        .then(|| chrono::Duration::from_std(delay).ok())
        .flatten()
        .map(|d| Utc::now() + d);
    admit(
        state,
        EnqueueJobSpec {
            kind,
            payload: JobPayload {
                trigger,
                subject: Some(subject.key()),
                ..JobPayload::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after,
        },
    )
    .await
}

/// Admits a storefront candidate fetch for `subject`.
///
/// # Errors
///
/// Returns the admission error.
pub async fn enqueue_discover_candidates(
    state: &AppState,
    subject: &DiscoverSubject,
    trigger: JobTrigger,
    delay: Duration,
) -> anyhow::Result<AdmitJob> {
    enqueue(state, JobKind::DiscoverCandidates, subject, trigger, delay).await
}

/// How long a pinned store fetch waits before queueing the subject's
/// ranking, so the other stores' fetches finishing around it fold into the
/// same ranking.
const PINNED_RANK_DELAY: Duration = Duration::from_secs(5);

/// One storefront instance some node is deployed to run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeployedStorefront {
    /// The instance id (`plugin_instances.plugin_instance_id`).
    instance_id: String,
    /// Its plugin key, which names its listings in the pool.
    plugin_key: String,
}

/// The storefront instances the library deploys, on any node.
///
/// A storefront is an installed plugin whose manifest exports the
/// `storefront` entrypoint (every node materializes the library's
/// installs, so this does not depend on which node asks). An instance
/// counts when some node has a deployment of it that should run.
///
/// # Returns
///
/// `None` when the plugins could not be discovered (or no plugin
/// directory is configured), so the caller asks only the local stores and
/// drops nothing.
async fn deployed_storefronts(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
) -> Option<Vec<DeployedStorefront>> {
    let config = state.config.read().await.clone();
    config.paths.as_ref()?;
    // Discovery walks and hashes every installed plugin: keep it off the
    // async workers.
    let discovered =
        tokio::task::spawn_blocking(move || bookclerk_plugin_host::discover_plugins(&config))
            .await
            .ok()?;
    let plugins = match discovered {
        Ok(plugins) => plugins,
        Err(err) => {
            tracing::warn!(error = %err, "plugin discovery failed; discover asks local stores only");
            return None;
        }
    };
    let keys: std::collections::BTreeSet<String> = plugins
        .iter()
        .filter(|plugin| {
            plugin
                .manifest
                .entrypoints
                .contains(&bookclerk_plugin_host::Entrypoint::Storefront)
        })
        .map(|plugin| plugin.plugin_key().to_string())
        .collect();
    deployed_instances_of(library, &keys).await.ok()
}

/// The library's instances of `keys` that some node is deployed to run.
///
/// # Errors
///
/// Returns an error when a read fails.
async fn deployed_instances_of(
    library: &bookclerk_library::LibraryStore,
    keys: &std::collections::BTreeSet<String>,
) -> anyhow::Result<Vec<DeployedStorefront>> {
    let instances = bookclerk_library::control_plane::list_all_plugin_instances(library).await?;
    let mut out = Vec::new();
    for instance in instances {
        if !keys.contains(&instance.plugin_key) {
            continue;
        }
        let id = instance.id.as_str().to_string();
        let deployed = bookclerk_library::control_plane::common_deployment_nodes(
            library,
            std::slice::from_ref(&id),
        )
        .await?;
        if !deployed.is_empty() {
            out.push(DeployedStorefront {
                instance_id: id,
                plugin_key: instance.plugin_key,
            });
        }
    }
    Ok(out)
}

/// Admits a candidate fetch of one storefront instance for `subject`,
/// claimable only where that instance runs.
///
/// # Errors
///
/// Returns the admission error.
async fn enqueue_pinned_candidates(
    state: &AppState,
    subject: &DiscoverSubject,
    instance_id: &str,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    admit(
        state,
        EnqueueJobSpec {
            kind: JobKind::DiscoverCandidates,
            payload: JobPayload {
                trigger,
                subject: Some(subject.key()),
                plugin_instance_id: Some(instance_id.to_string()),
                ..JobPayload::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Admits a feed ranking for `subject`.
///
/// # Errors
///
/// Returns the admission error.
pub async fn enqueue_discover_feed(
    state: &AppState,
    subject: &DiscoverSubject,
    trigger: JobTrigger,
    delay: Duration,
) -> anyhow::Result<AdmitJob> {
    enqueue(state, JobKind::DiscoverFeed, subject, trigger, delay).await
}

/// Parses the subject a Discover job carries.
fn job_subject(payload: &JobPayload) -> anyhow::Result<DiscoverSubject> {
    payload
        .subject
        .as_deref()
        .and_then(DiscoverSubject::parse)
        .ok_or_else(|| anyhow::anyhow!("discover job has no valid subject"))
}

/// Drops a deleted subject's Discover rows when it no longer exists.
///
/// Called before a job does any work and again after it wrote, so a job
/// queued (or still running) when its user or portal identity was deleted
/// leaves nothing behind for the daily refresh to pick up again.
///
/// # Returns
///
/// `true` when the subject is gone (and its rows were deleted).
///
/// # Errors
///
/// Returns the library error.
async fn forget_if_removed(
    library: &bookclerk_library::LibraryStore,
    subject: &DiscoverSubject,
) -> anyhow::Result<bool> {
    if library.discover_subject_exists(subject).await? {
        return Ok(false);
    }
    library.forget_discover_subject(subject).await?;
    Ok(true)
}

/// Runs a `discover_candidates` job, then admits the subject's ranking.
///
/// # Errors
///
/// Fails on a missing subject, cancellation, or a library error. A store
/// that fails is reported in the summary, not as a job failure.
pub async fn run_discover_candidates(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let subject = job_subject(payload)?;
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    if forget_if_removed(&library, &subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    let opts = discover_options(&cfg, None);
    if let Some(instance) = payload
        .plugin_instance_id
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        return run_pinned_candidates(state, &library, &subject, instance, payload, opts).await;
    }
    let deployed = if opts.fetch_storefront_candidates {
        deployed_storefronts(state, &library).await
    } else {
        None
    };
    run_subject_candidates(state, &library, &subject, payload, opts, deployed).await
}

/// The subject's own `discover_candidates` fetch: asks this node's stores,
/// queues a pinned fetch for each storefront in `deployed` that no local
/// source serves, drops the rows of stores no node deploys, then queues the
/// ranking.
///
/// # Arguments
///
/// * `deployed` - The storefront instances deployed on any node, or `None`
///   when they could not be listed (only local stores are asked and nothing
///   is dropped).
///
/// # Errors
///
/// Fails on a library error.
async fn run_subject_candidates(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    subject: &DiscoverSubject,
    payload: &JobPayload,
    mut opts: bookclerk_discover::RecommendOptions,
    deployed: Option<Vec<DeployedStorefront>>,
) -> anyhow::Result<String> {
    let local = state.sources.read().await.clone();
    // Storefront instances no local source serves get fetches of their own.
    let local_instances: std::collections::BTreeSet<String> = local
        .all()
        .iter()
        .filter_map(|source| source.plugin_instance_id().map(str::to_string))
        .collect();
    let remote: Vec<&DeployedStorefront> = deployed
        .iter()
        .flatten()
        .filter(|store| !local_instances.contains(&store.instance_id))
        .collect();
    // This node's stores get their share of the call budget; each remote
    // store gets its own in its pinned fetch.
    let local_stores = local.all().len();
    opts.storefront_max_remote_calls = budget_share(
        opts.storefront_max_remote_calls,
        local_stores,
        local_stores + remote.len(),
    );
    let report = if opts.fetch_storefront_candidates {
        bookclerk_discover::fetch_subject_candidates(library, &local, subject, &opts).await?
    } else {
        // Nothing to fetch, but the fetch counts as done for the day, and
        // the event that queued it still refreshes the stored taste.
        bookclerk_discover::refresh_subject_taste(library, subject, &opts).await?;
        library.mark_discover_fetch(subject, Utc::now()).await?;
        bookclerk_discover::FetchReport::default()
    };
    if forget_if_removed(library, subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    // Pinned fetches the full job queue turned away. The job fails after
    // its other work so a retry admits them; the ones already admitted
    // fold into their pending rows.
    let mut unqueued = 0usize;
    for store in &remote {
        match enqueue_pinned_candidates(state, subject, &store.instance_id, payload.trigger).await {
            Ok(AdmitJob::Created(_) | AdmitJob::Duplicate(_)) => {}
            Ok(AdmitJob::QueueFull) => unqueued = unqueued.saturating_add(1),
            Err(err) => tracing::warn!(
                subject = %subject.key(),
                instance = %store.instance_id,
                error = %err,
                "could not queue a discover fetch for a storefront on another node"
            ),
        }
    }
    let dropped = match &deployed {
        Some(deployed) => drop_undeployed_stores(library, subject, &local, deployed).await?,
        None => 0,
    };
    // The ranking reads what was just stored; a failure to admit it leaves
    // the next read to queue it.
    if let Err(err) = enqueue_discover_feed(state, subject, payload.trigger, Duration::ZERO).await {
        tracing::warn!(subject = %subject.key(), error = %err, "could not queue the discover ranking");
    }
    let pruned = library
        .prune_discover_store_calls(Utc::now() - bookclerk_discover::STORE_CALL_MAX_AGE)
        .await
        .unwrap_or(0);
    // Catalog listings no candidate, cached call, holding or override
    // refers to any more, and no store has reported for a while. Every
    // subject's fetch ends here, so the scan runs at most once an hour per
    // daemon rather than once per subject.
    let pruned_listings = if catalog_prune_due(Utc::now()) {
        library
            .prune_catalog_listings(Utc::now() - CATALOG_LISTING_RETENTION)
            .await
            .unwrap_or(0)
    } else {
        0
    };
    // Ask the branches what they can supply of what people here look at.
    crate::catalog_lookup::ask_branches_if_due(library).await;
    if unqueued > 0 {
        anyhow::bail!(
            "subject={}: the job queue is full; {unqueued} storefront fetch(es) on other nodes not queued",
            subject.key()
        );
    }
    Ok(format!(
        "subject={} seeds={} stores={} candidates={} failed={} remote_stores={} \
         dropped_stores={dropped} pruned_calls={pruned} pruned_listings={pruned_listings}",
        subject.key(),
        report.seeds,
        report.stores_saved,
        report.candidates,
        report.stores_failed.join(","),
        remote.len(),
    ))
}

/// `share` of `of` stores' part of a `total` call budget, rounded down but
/// at least one call for a non-empty share (the whole budget when `of` is
/// zero).
fn budget_share(total: usize, share: usize, of: usize) -> usize {
    if of == 0 || share >= of {
        return total;
    }
    if share == 0 {
        return 0;
    }
    total
        .saturating_mul(share)
        .checked_div(of)
        .unwrap_or(total)
        .max(1)
}

/// Runs a `discover_candidates` job pinned to one storefront instance:
/// fetches that store's candidates on this node, then queues the subject's
/// ranking a moment later.
///
/// # Errors
///
/// Fails on cancellation or a library error.
async fn run_pinned_candidates(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    subject: &DiscoverSubject,
    instance: &str,
    payload: &JobPayload,
    mut opts: bookclerk_discover::RecommendOptions,
) -> anyhow::Result<String> {
    if !opts.fetch_storefront_candidates {
        return Ok(format!(
            "subject={} instance={instance} storefront candidates off",
            subject.key()
        ));
    }
    let Some(source) = state
        .sources
        .read()
        .await
        .all()
        .into_iter()
        .find(|source| source.plugin_instance_id() == Some(instance))
    else {
        // Routing sends the job where the instance runs; an instance no
        // longer deployed anywhere has nothing to fetch.
        return Ok(format!(
            "subject={} instance={instance} not served on this node",
            subject.key()
        ));
    };
    let stores = deployed_storefronts(state, library)
        .await
        .map_or(1, |deployed| deployed.len().max(1));
    opts.storefront_max_remote_calls = budget_share(opts.storefront_max_remote_calls, 1, stores);
    let mut one = bookclerk_source::SourceRegistry::new();
    one.register(source);
    let report =
        bookclerk_discover::fetch_subject_candidates(library, &one, subject, &opts).await?;
    if forget_if_removed(library, subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    if let Err(err) =
        enqueue_discover_feed(state, subject, payload.trigger, PINNED_RANK_DELAY).await
    {
        tracing::warn!(subject = %subject.key(), error = %err, "could not queue the discover ranking");
    }
    Ok(format!(
        "subject={} instance={instance} candidates={} failed={}",
        subject.key(),
        report.candidates,
        report.stores_failed.join(","),
    ))
}

/// Drops `subject`'s stored candidates of storefronts no node deploys any
/// more, so a retired store's titles leave the shelves.
///
/// A row stays when its listing's plugin is a deployed storefront or one of
/// this node's sources.
///
/// # Returns
///
/// How many stores' rows were dropped.
///
/// # Errors
///
/// Returns the library error.
async fn drop_undeployed_stores(
    library: &bookclerk_library::LibraryStore,
    subject: &DiscoverSubject,
    local: &bookclerk_source::SourceRegistry,
    deployed: &[DeployedStorefront],
) -> anyhow::Result<usize> {
    let mut live: std::collections::BTreeSet<&str> = deployed
        .iter()
        .map(|store| store.plugin_key.as_str())
        .collect();
    let local_sources = local.all();
    live.extend(local_sources.iter().map(|source| source.plugin_key()));
    let mut stale: std::collections::BTreeMap<String, bookclerk_library::SeedScope> =
        std::collections::BTreeMap::new();
    for row in library.list_discover_candidates(subject).await? {
        if !live.contains(row.listing.plugin_key.as_str()) {
            stale.entry(row.source).or_insert(row.seed_scope);
        }
    }
    for (source, scope) in &stale {
        library
            .replace_discover_candidates(subject, source, *scope, &[])
            .await?;
    }
    Ok(stale.len())
}

/// How long an unreferenced catalog listing is kept after a store last
/// reported it, so a title that drops off one subject's shelves and comes
/// back within the week is not resolved again.
const CATALOG_LISTING_RETENTION: chrono::Duration = chrono::Duration::days(7);

/// Least time between two catalog listing prunes on one daemon.
const CATALOG_PRUNE_INTERVAL: chrono::Duration = chrono::Duration::hours(1);

/// Unix seconds of this daemon's last catalog listing prune (0: never).
static LAST_CATALOG_PRUNE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Whether this daemon should prune catalog listings now, claiming the
/// slot when it should so concurrent jobs do not prune together.
///
/// # Arguments
///
/// * `now` - The current time.
fn catalog_prune_due(now: chrono::DateTime<Utc>) -> bool {
    use std::sync::atomic::Ordering;
    let now_secs = now.timestamp();
    let last = LAST_CATALOG_PRUNE.load(Ordering::Relaxed);
    now_secs.saturating_sub(last) >= CATALOG_PRUNE_INTERVAL.num_seconds()
        && LAST_CATALOG_PRUNE
            .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// The vectors a ranking on this node compares.
///
/// The space is the one that serves the library now: this node's model
/// once it has embedded every work, the model another node embedded with,
/// or the hash stand-in. The stand-in's work vectors are made here, since
/// they cost next to nothing; a model's are made by a `discover_embed`
/// job, queued here when this node's model does not serve yet.
///
/// # Errors
///
/// Returns the library error.
async fn ranking_vectors(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
) -> anyhow::Result<bookclerk_discover::VectorSource> {
    let node = crate::embedding::node_embedder(state).await;
    let current = node.space().clone();
    let mut source = crate::embedding::vector_source(library, node).await?;
    if bookclerk_discover::is_stand_in(&source.space) {
        // Embedding with the stand-in can give a model its cover back
        // only by never having lost it, so the space stays the stand-in's.
        let mut stand_in = bookclerk_discover::open_embedder();
        if let Err(err) =
            bookclerk_discover::embed_dirty_works(library, stand_in.as_mut(), None).await
        {
            tracing::warn!(error = %err, "could not embed the library with the stand-in");
        }
        source = bookclerk_discover::VectorSource::new(source.space, Some(stand_in));
    }
    crate::embedding::embed_if_needed(state, &current, &source.space).await;
    Ok(source)
}

/// Most rankings one `discover_feed` job runs when candidate fetches keep
/// finishing under it; past this the next read or event queues another.
const MAX_RANK_PASSES: u32 = 3;

/// Runs a `discover_feed` job: ranks stored candidates into stored shelves,
/// again when a candidate fetch finished during the ranking.
///
/// # Errors
///
/// Fails on a missing subject, cancellation, or a library error.
pub async fn run_discover_feed(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let subject = job_subject(payload)?;
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    if forget_if_removed(&library, &subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    let has_books = library.has_held_titles().await?;
    // One embedder at a time, as the request-queue ranking job does.
    let _gate = state
        .discover_gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| anyhow::anyhow!("discover gate closed"))?;
    let vectors = if has_books {
        Some(ranking_vectors(state, &library).await?)
    } else {
        None
    };
    // Named in the job's summary, so a reader of the job list can tell a
    // ranking on a model's vectors from one on the stand-in's.
    let ranked_with = vectors.as_ref().map_or_else(
        || "none".to_owned(),
        |source| source.space.model.to_string(),
    );
    let opts = discover_options(&cfg, vectors);
    let sources = state.sources.read().await.clone();
    // A candidate fetch that finishes while this ranking runs folds its
    // follow-up ranking into this job (the dedup key covers running jobs),
    // so rank again until the candidates read are the newest stored.
    let mut passes = 0;
    let report = loop {
        passes += 1;
        let read = library.newest_discover_candidate_fetch(&subject).await?;
        let report =
            bookclerk_discover::rank_subject_feed(&library, &sources, &subject, &opts).await?;
        if passes >= MAX_RANK_PASSES
            || library.newest_discover_candidate_fetch(&subject).await? == read
        {
            break report;
        }
    };
    if forget_if_removed(&library, &subject).await? {
        return Ok(format!("subject={} removed", subject.key()));
    }
    info!(
        subject = %subject.key(),
        scope = report.scope.as_str(),
        shelves = report.shelves,
        cards = report.cards,
        bytes = report.stored_bytes,
        "stored discover feed"
    );
    warm_feed_covers(state, &library, &subject).await;
    // Open Discover pages on this node re-read at once; other nodes see the
    // new `computed_at` on their next revision read.
    crate::live::poke(bookclerk_library::control_plane::LiveTopic::Discover);
    Ok(format!(
        "subject={} scope={} shelves={} candidates={} passes={passes} vectors={ranked_with}",
        subject.key(),
        report.scope.as_str(),
        report.shelves,
        report.candidates
    ))
}

/// Starts downloading the covers of the feed just stored, first shelves
/// first, so the page that opens next finds them cached.
///
/// Without this, the first page load asked for every cover at once, and each
/// one waited on its download and then on the media worker, which a small
/// machine runs one job at a time (the hosted demo took 0.8 to 1.7 s per
/// cover). The page's own request still prefetches what this left out; a
/// cover already cached or downloading is skipped.
///
/// # Arguments
///
/// * `state` - Daemon state holding the cover service.
/// * `library` - Store the feed was written to.
/// * `subject` - Whose feed it is.
async fn warm_feed_covers(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    subject: &DiscoverSubject,
) {
    match bookclerk_discover::stored_subject_feed(library, subject, &[]).await {
        Ok(Some(stored)) => crate::covers::prefetch(
            state,
            stored
                .feed
                .shelves
                .iter()
                .flat_map(|shelf| shelf.items.iter().filter_map(|item| item.cover_url.clone())),
        ),
        Ok(None) => {}
        Err(err) => {
            tracing::debug!(error = %err, "could not read the stored feed to warm its covers")
        }
    }
}

/// True when a fetch or ranking for `subject` is waiting or running, so the
/// page can say its shelves are being rebuilt.
///
/// # Errors
///
/// Returns the library read error.
pub async fn subject_refresh_active(
    library: &bookclerk_library::LibraryStore,
    subject: &DiscoverSubject,
) -> anyhow::Result<bool> {
    let payload = JobPayload {
        subject: Some(subject.key()),
        ..JobPayload::default()
    };
    for kind in [JobKind::DiscoverCandidates, JobKind::DiscoverFeed] {
        if library.job_is_active(&kind.dedup_key(&payload)).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What a read of a subject's stored feed should queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedRefresh {
    /// Nothing: the feed is current.
    None,
    /// Rank again from stored candidates (they are newer than the feed).
    Rank,
    /// Fetch candidates (none yet, or older than [`CANDIDATES_MAX_AGE`]).
    Fetch,
}

/// Decides what a read should queue for a subject.
///
/// # Arguments
///
/// * `feed` - The stored feed, when one decodes.
/// * `newest_candidates` - Newest stored candidate fetch, when any.
/// * `now` - The current time.
#[must_use]
pub fn refresh_needed(
    feed: Option<&bookclerk_discover::StoredSubjectFeed>,
    newest_candidates: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> FeedRefresh {
    let Some(fetched) = newest_candidates else {
        return FeedRefresh::Fetch;
    };
    if fetched + CANDIDATES_MAX_AGE <= now {
        return FeedRefresh::Fetch;
    }
    match feed {
        None => FeedRefresh::Rank,
        Some(feed) if feed.candidates_fetched_at.is_none_or(|used| used < fetched) => {
            FeedRefresh::Rank
        }
        Some(_) => FeedRefresh::None,
    }
}

/// Queues whatever [`refresh_needed`] asks for, logging (not failing) on error.
pub async fn queue_refresh(state: &AppState, subject: &DiscoverSubject, refresh: FeedRefresh) {
    let result = match refresh {
        FeedRefresh::None => return,
        FeedRefresh::Rank => {
            enqueue_discover_feed(state, subject, JobTrigger::Api, Duration::ZERO).await
        }
        FeedRefresh::Fetch => {
            enqueue_discover_candidates(state, subject, JobTrigger::Api, Duration::ZERO).await
        }
    };
    if let Err(err) = result {
        tracing::warn!(subject = %subject.key(), error = %err, "could not queue a discover refresh");
    }
}

/// Queues a candidate fetch for every subject with a stored feed whose
/// candidates are older than [`CANDIDATES_MAX_AGE`] (the daily refresh).
///
/// # Returns
///
/// How many fetches were admitted. Failures are logged, not returned.
pub async fn queue_daily_refresh(state: &AppState) -> usize {
    let library = state.library_snapshot().await;
    let subjects = match library.list_discover_feed_subjects().await {
        Ok(subjects) => subjects,
        Err(err) => {
            tracing::warn!(error = %err, "discover refresh could not list subjects");
            return 0;
        }
    };
    let now = Utc::now();
    let mut queued = 0;
    for subject in subjects {
        match forget_if_removed(&library, &subject).await {
            Ok(false) => {}
            Ok(true) => continue,
            Err(err) => {
                tracing::warn!(subject = %subject.key(), error = %err, "discover refresh could not check a subject");
                continue;
            }
        }
        let newest = library
            .newest_discover_candidate_fetch(&subject)
            .await
            .ok()
            .flatten();
        if newest.is_some_and(|at| at + CANDIDATES_MAX_AGE > now) {
            continue;
        }
        match enqueue_discover_candidates(state, &subject, JobTrigger::Scheduler, Duration::ZERO)
            .await
        {
            Ok(_) => queued += 1,
            Err(err) => {
                tracing::warn!(subject = %subject.key(), error = %err, "daily discover refresh enqueue failed");
            }
        }
    }
    queued
}

#[cfg(test)]
mod tests {
    use bookclerk_library::SeedScope;

    use super::*;

    #[test]
    fn the_catalog_prune_runs_at_most_once_an_interval() {
        // Far in the future, so no other test's real-time claim interferes.
        let t = Utc::now() + chrono::Duration::days(3650);
        assert!(catalog_prune_due(t));
        assert!(!catalog_prune_due(t + chrono::Duration::minutes(5)));
        assert!(catalog_prune_due(t + CATALOG_PRUNE_INTERVAL));
    }

    fn feed(used: Option<chrono::DateTime<Utc>>) -> bookclerk_discover::StoredSubjectFeed {
        bookclerk_discover::StoredSubjectFeed {
            feed: bookclerk_discover::DiscoverFeed {
                shelves: Vec::new(),
                shelf_kinds: Vec::new(),
                state: None,
                computed_at: None,
            },
            scope: SeedScope::Own,
            candidates_fetched_at: used,
            computed_at: Utc::now(),
        }
    }

    #[test]
    fn the_call_budget_is_split_by_store_count() {
        // No remote stores: the whole budget stays here.
        assert_eq!(budget_share(40, 3, 3), 40);
        assert_eq!(budget_share(40, 0, 0), 40);
        // Two local stores of four get half.
        assert_eq!(budget_share(40, 2, 4), 20);
        // One store of three rounds down.
        assert_eq!(budget_share(40, 1, 3), 13);
        // A share never starves a store it covers.
        assert_eq!(budget_share(2, 1, 5), 1);
        // No stores here, no calls here.
        assert_eq!(budget_share(40, 0, 4), 0);
    }

    /// Stores `product` as `plugin_key`'s one candidate for `subject`.
    async fn store_candidate(
        store: &bookclerk_library::LibraryStore,
        subject: &DiscoverSubject,
        plugin_key: &str,
        product: &str,
    ) {
        let listing = bookclerk_library::CatalogListing {
            draft: bookclerk_resolve::ListingDraft {
                plugin_key: plugin_key.to_string(),
                marketplace: Some(String::from("us")),
                product_id: product.to_string(),
                title: product.to_string(),
                work_kind: Some(bookclerk_resolve::WorkKind::Book),
                expression_kind: Some(bookclerk_resolve::ExpressionKind::Narration),
                media_kind: Some(bookclerk_resolve::MediaKind::Audio),
                ..bookclerk_resolve::ListingDraft::default()
            },
            detail: bookclerk_library::ListingDetail::default(),
            offers: Vec::new(),
        };
        let ids = store
            .record_catalog_listings(std::slice::from_ref(&listing))
            .await
            .unwrap()
            .ids;
        let rows: Vec<bookclerk_library::CandidateRef> = ids
            .into_iter()
            .map(|id| bookclerk_library::CandidateRef {
                listing_id: id.unwrap(),
                origin: String::from("deals"),
                seed_title: None,
                seed_categories: None,
                starter_genre: None,
            })
            .collect();
        store
            .replace_discover_candidates(subject, plugin_key, SeedScope::Own, &rows)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_store_no_node_deploys_leaves_the_shelves() {
        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let subject = DiscoverSubject::User(3);
        store_candidate(&store, &subject, "audible", "kept").await;
        store_candidate(&store, &subject, "libro", "retired").await;
        let deployed = [DeployedStorefront {
            instance_id: String::from("i-audible"),
            plugin_key: String::from("audible"),
        }];
        let local = bookclerk_source::SourceRegistry::new();
        let dropped = drop_undeployed_stores(&store, &subject, &local, &deployed)
            .await
            .unwrap();
        assert_eq!(dropped, 1);
        let left: Vec<String> = store
            .list_discover_candidates(&subject)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.listing.product_id)
            .collect();
        assert_eq!(left, ["kept"]);
        // Nothing more to drop on the next run.
        let again = drop_undeployed_stores(&store, &subject, &local, &deployed)
            .await
            .unwrap();
        assert_eq!(again, 0);
    }

    #[test]
    fn a_read_queues_only_what_is_stale() {
        let now = Utc::now();
        let fresh = now - chrono::Duration::hours(1);
        let old = now - chrono::Duration::hours(25);
        assert_eq!(refresh_needed(None, None, now), FeedRefresh::Fetch);
        assert_eq!(
            refresh_needed(Some(&feed(Some(old))), Some(old), now),
            FeedRefresh::Fetch
        );
        assert_eq!(refresh_needed(None, Some(fresh), now), FeedRefresh::Rank);
        assert_eq!(
            refresh_needed(Some(&feed(Some(old))), Some(fresh), now),
            FeedRefresh::Rank
        );
        assert_eq!(
            refresh_needed(Some(&feed(None)), Some(fresh), now),
            FeedRefresh::Rank
        );
        assert_eq!(
            refresh_needed(Some(&feed(Some(fresh))), Some(fresh), now),
            FeedRefresh::None
        );
    }

    #[test]
    fn job_subjects_must_parse() {
        let payload = JobPayload {
            subject: Some(String::from("user:3")),
            ..JobPayload::default()
        };
        assert_eq!(job_subject(&payload).ok(), Some(DiscoverSubject::User(3)));
        assert!(job_subject(&JobPayload::default()).is_err());
        let bad = JobPayload {
            subject: Some(String::from("user:x")),
            ..JobPayload::default()
        };
        assert!(job_subject(&bad).is_err());
    }

    /// Answers one GET on the daemon's router as JSON.
    async fn get(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Sends an empty JSON `POST` to `uri` and decodes the JSON answer.
    async fn post(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Sends a JSON `POST` of `body` to `uri` and decodes the JSON answer.
    async fn post_json(
        app: &axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// The uuid of the one library title stored.
    async fn book_uuid(library: &bookclerk_library::LibraryStore) -> String {
        library
            .all_acquire_titles(None)
            .await
            .unwrap()
            .remove(0)
            .holding_id
    }

    /// Pending Discover jobs as `(kind, subject)` pairs.
    async fn queued(library: &bookclerk_library::LibraryStore) -> Vec<(JobKind, String)> {
        let mut jobs: Vec<(JobKind, String)> = library
            .list_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|j| matches!(j.kind, JobKind::DiscoverCandidates | JobKind::DiscoverFeed))
            .filter(|j| j.state == bookclerk_library::JobState::Pending)
            .map(|j| (j.kind, j.payload.subject.unwrap_or_default()))
            .collect();
        jobs.sort_by_key(|(kind, subject)| (kind.as_str().to_string(), subject.clone()));
        jobs
    }

    /// A subject's fetch queues a pinned fetch for a storefront deployed only
    /// on another node, drops the rows of a store no node deploys, and the
    /// pinned fetch skips a store this node does not serve.
    #[tokio::test]
    async fn a_subject_fetch_fans_out_to_stores_on_other_nodes() {
        use bookclerk_library::control_plane as cp;
        const STORE_KEY: &str = "path:file:///tmp/far-store#farstore";
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state_with(
            &files,
            "[discovery]\nembeddings_enabled = false\n",
        )
        .await;
        let instance =
            cp::create_plugin_instance(&library, &cp::ConfigActor::Bootstrap, STORE_KEY, "")
                .await
                .unwrap();
        cp::ensure_plugin_deployment(
            &library,
            &cp::ConfigActor::Bootstrap,
            &instance.id,
            "node-b",
            cp::DesiredState::Present,
        )
        .await
        .unwrap();
        let subject = DiscoverSubject::Operator;
        store_candidate(&library, &subject, "retired", "gone").await;

        let keys: std::collections::BTreeSet<String> = [STORE_KEY.to_string()].into();
        let deployed = deployed_instances_of(&library, &keys).await.unwrap();
        assert_eq!(
            deployed,
            vec![DeployedStorefront {
                instance_id: instance.id.to_string(),
                plugin_key: STORE_KEY.to_string(),
            }]
        );
        let mut opts = discover_options(&state.config.read().await.clone(), None);
        opts.fetch_storefront_candidates = true;
        let payload = JobPayload {
            subject: Some(subject.key()),
            ..JobPayload::default()
        };
        let summary =
            run_subject_candidates(&state, &library, &subject, &payload, opts, Some(deployed))
                .await
                .unwrap();
        assert!(summary.contains("remote_stores=1"), "{summary}");
        assert!(summary.contains("dropped_stores=1"), "{summary}");
        assert!(library
            .list_discover_candidates(&subject)
            .await
            .unwrap()
            .is_empty());
        let pinned: Vec<Option<String>> = library
            .list_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == JobKind::DiscoverCandidates)
            .map(|job| job.payload.plugin_instance_id)
            .collect();
        assert_eq!(pinned, vec![Some(instance.id.to_string())]);
        assert!(queued(&library)
            .await
            .contains(&(JobKind::DiscoverFeed, subject.key())));

        // A full job queue fails the fetch so a retry queues the pinned one.
        {
            use sea_orm::ConnectionTrait;
            library
                .db()
                .execute_unprepared("UPDATE jobs SET state = 'succeeded'")
                .await
                .unwrap();
        }
        let max_pending = std::mem::replace(&mut state.config.write().await.jobs.max_pending, 0);
        let mut full = discover_options(&state.config.read().await.clone(), None);
        full.fetch_storefront_candidates = true;
        let again = deployed_instances_of(&library, &keys).await.unwrap();
        let err = run_subject_candidates(&state, &library, &subject, &payload, full, Some(again))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("job queue is full"), "{err}");
        state.config.write().await.jobs.max_pending = max_pending;

        // The pinned fetch, run on a node without that store, asks nothing.
        let pinned_payload = JobPayload {
            subject: Some(subject.key()),
            plugin_instance_id: Some(instance.id.to_string()),
            ..JobPayload::default()
        };
        let mut on = discover_options(&state.config.read().await.clone(), None);
        on.fetch_storefront_candidates = true;
        let summary = run_pinned_candidates(
            &state,
            &library,
            &subject,
            instance.id.as_str(),
            &pinned_payload,
            on.clone(),
        )
        .await
        .unwrap();
        assert!(summary.ends_with("not served on this node"), "{summary}");
        let mut off = on.clone();
        off.fetch_storefront_candidates = false;
        let summary = run_pinned_candidates(
            &state,
            &library,
            &subject,
            instance.id.as_str(),
            &pinned_payload,
            off,
        )
        .await
        .unwrap();
        assert!(summary.ends_with("storefront candidates off"), "{summary}");

        // On the node that serves it, the store is asked and the ranking
        // queued.
        state
            .sources
            .write()
            .await
            .register(std::sync::Arc::new(PinnedStore {
                instance: instance.id.to_string(),
            }));
        let summary = run_pinned_candidates(
            &state,
            &library,
            &subject,
            instance.id.as_str(),
            &pinned_payload,
            on,
        )
        .await
        .unwrap();
        assert!(
            summary.starts_with(&format!("subject=operator instance={}", instance.id)),
            "{summary}"
        );
        assert!(summary.contains("candidates="), "{summary}");
    }

    /// A storefront double owned by one plugin instance that answers nothing.
    struct PinnedStore {
        /// The instance id it reports.
        instance: String,
    }

    #[async_trait::async_trait]
    impl bookclerk_source::ContentSource for PinnedStore {
        fn id(&self) -> &str {
            "farstore"
        }

        fn plugin_instance_id(&self) -> Option<&str> {
            Some(&self.instance)
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            bookclerk_source::PortalAuthMode::Oauth
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            bookclerk_source::SourceBrand {
                id: "farstore",
                name: "Far store",
                bg: "#000000",
                fg: "#ffffff",
                accent: "#111111",
                icon_url: "https://example.invalid/icon",
            }
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("not in this test"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Ok(Vec::new())
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Err(bookclerk_source::SourceError::api("not in this test"))
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            _opts: &bookclerk_source::FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            Err(bookclerk_source::SourceError::api("not in this test"))
        }
    }

    /// With no model on the node, a ranking makes the stand-in's work
    /// vectors itself and queues no embedding job.
    #[tokio::test]
    async fn a_ranking_without_a_model_embeds_with_the_stand_in() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        library
            .upsert_account("libro", "reader@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut book =
            bookclerk_library::NewBook::minimal("p1", "reader@example.com", "us", "Dune");
        book.source = "libro".into();
        book.plugin_key = "libro".into();
        library.upsert_book(&book).await.unwrap();

        let source = ranking_vectors(&state, &library).await.unwrap();
        assert!(bookclerk_discover::is_stand_in(&source.space));
        assert!(source.embedder.is_some());
        let spaces = library.work_vector_spaces().await.unwrap();
        assert_eq!(spaces.len(), 1);
        assert!(bookclerk_discover::is_stand_in(&spaces[0].space));
        assert_eq!(spaces[0].works, 1);
        let embeds = library
            .list_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == JobKind::DiscoverEmbed)
            .count();
        assert_eq!(embeds, 0);
    }

    /// A wish records the library work it resolves to, and a sequel of an
    /// owned book gets a catalog work of its own.
    #[tokio::test]
    async fn a_wish_stores_the_work_it_resolves_to() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        library
            .upsert_account("libro", "reader@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut book =
            bookclerk_library::NewBook::minimal("p1", "reader@example.com", "us", "Dune");
        book.source = "libro".into();
        book.plugin_key = "libro".into();
        book.authors = Some("Frank Herbert".into());
        library.upsert_book(&book).await.unwrap();
        let app = crate::api::router(std::sync::Arc::clone(&state), None);

        let wish = |title: &str| serde_json::json!({ "title": title, "authors": "Frank Herbert" });
        let (status, owned) = post_json(&app, "/api/wishlist", wish("Dune")).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{owned}");
        let dune_work = library.work_of_holding(&book_uuid(&library).await).await;
        assert_eq!(
            owned["work_id"].as_str(),
            dune_work.unwrap().as_deref(),
            "{owned}"
        );
        let (status, sequel) = post_json(&app, "/api/wishlist", wish("Dune Messiah")).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{sequel}");
        assert!(sequel["work_id"].is_string(), "{sequel}");
        assert_ne!(sequel["work_id"], owned["work_id"], "{sequel}");
    }

    /// A wish whose title is over the listing limits names no work, so it is a
    /// bad request.
    #[tokio::test]
    async fn an_overlong_wish_title_is_a_bad_request() {
        let files = tempfile::tempdir().unwrap();
        let (state, _library) = crate::node_api::tests::test_state(&files).await;
        let app = crate::api::router(std::sync::Arc::clone(&state), None);
        let long = "x".repeat(10_000);
        let (status, body) =
            post_json(&app, "/api/wishlist", serde_json::json!({ "title": long })).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn the_page_reads_stored_shelves_and_queues_what_is_stale() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state_with(
            &files,
            "[discovery]\nembeddings_enabled = false\n",
        )
        .await;
        let app = crate::api::router(std::sync::Arc::clone(&state), None);

        // An empty library answers at once and queues nothing.
        let (status, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{feed}");
        assert_eq!(feed["state"], "ready");
        assert!(queued(&library).await.is_empty());

        library
            .upsert_account("libro", "reader@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut book =
            bookclerk_library::NewBook::minimal("p1", "reader@example.com", "us", "Dune");
        book.source = "libro".into();
        book.plugin_key = "libro".into();
        library.upsert_book(&book).await.unwrap();

        // No feed yet: placeholders, and the fetch is queued once.
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "preparing");
        assert!(feed["shelves"].as_array().unwrap().is_empty());
        get(&app, "/api/discover/recommendations").await;
        assert_eq!(
            queued(&library).await,
            vec![(JobKind::DiscoverCandidates, String::from("operator"))]
        );

        // The fetch runs (no stores deployed) and admits the ranking.
        let payload = JobPayload {
            subject: Some(String::from("operator")),
            ..JobPayload::default()
        };
        let summary = run_discover_candidates(&state, &payload, None)
            .await
            .unwrap();
        assert!(summary.starts_with("subject=operator"), "{summary}");
        assert!(queued(&library)
            .await
            .contains(&(JobKind::DiscoverFeed, String::from("operator"))));
        let summary = run_discover_feed(&state, &payload, None).await.unwrap();
        assert!(summary.contains("scope=household"), "{summary}");
        assert!(
            summary.ends_with("passes=1 vectors=local-hash-v1"),
            "{summary}"
        );

        // The handlers ran here, not under the worker, so their rows are
        // still pending: the page reports the rebuild as in progress.
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "refreshing");
        let (_, report) = get(&app, "/api/discover/status").await;
        assert_eq!(report["updating"], true);
        {
            use sea_orm::ConnectionTrait;
            library
                .db()
                .execute_unprepared(
                    "UPDATE jobs SET state = 'succeeded' \
                     WHERE kind IN ('discover_candidates', 'discover_feed')",
                )
                .await
                .unwrap();
        }

        // A fetch that found nothing still counts as fresh: the feed is ready
        // and nothing more is queued.
        let jobs_before = queued(&library).await;
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "ready");
        assert!(feed["computed_at"].is_string());
        assert_eq!(queued(&library).await, jobs_before);

        let (status, report) = get(&app, "/api/discover/status").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{report}");
        assert_eq!(report["subject"], "operator");
        assert_eq!(report["ready"], true);
        assert_eq!(report["scope"], "household");
        assert!(report["candidates_fetched_at"].is_string());
        assert_eq!(report["updating"], false);

        // The Refresh button queues a fetch now and reports it.
        let (status, report) = post(&app, "/api/discover/refresh").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{report}");
        assert_eq!(report["updating"], true);
        assert_eq!(
            queued(&library).await,
            vec![(JobKind::DiscoverCandidates, String::from("operator"))]
        );
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "refreshing");
        // A second press finds the fetch waiting, which counts as queued.
        let (status, report) = post(&app, "/api/discover/refresh").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{report}");
        assert_eq!(queued(&library).await.len(), 1);
        {
            use sea_orm::ConnectionTrait;
            library
                .db()
                .execute_unprepared("UPDATE jobs SET state = 'succeeded'")
                .await
                .unwrap();
        }
        // A full job queue is reported, not answered as queued.
        let max_pending = std::mem::replace(&mut state.config.write().await.jobs.max_pending, 0);
        let (status, _) = post(&app, "/api/discover/refresh").await;
        assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert!(queued(&library).await.is_empty());
        state.config.write().await.jobs.max_pending = max_pending;

        // A feed another release wrote, which does not decode, counts as
        // missing, so the page shows placeholders until a ranking replaces it.
        library
            .put_discover_feed(
                &DiscoverSubject::Operator,
                "not a feed",
                SeedScope::Household,
                Some(Utc::now()),
            )
            .await
            .unwrap();
        let (_, report) = get(&app, "/api/discover/status").await;
        assert_eq!(report["ready"], false);
        let (_, feed) = get(&app, "/api/discover/recommendations").await;
        assert_eq!(feed["state"], "preparing");

        // A job without a subject fails instead of guessing one.
        assert!(run_discover_feed(&state, &JobPayload::default(), None)
            .await
            .is_err());
        assert!(
            run_discover_candidates(&state, &JobPayload::default(), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_job_for_a_deleted_user_writes_nothing_back() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        let gone = DiscoverSubject::User(9_999);
        library
            .put_discover_feed(&gone, "{}", SeedScope::Own, None)
            .await
            .unwrap();
        let payload = JobPayload {
            subject: Some(gone.key()),
            ..JobPayload::default()
        };
        let summary = run_discover_candidates(&state, &payload, None)
            .await
            .unwrap();
        assert!(summary.ends_with("removed"), "{summary}");
        let summary = run_discover_feed(&state, &payload, None).await.unwrap();
        assert!(summary.ends_with("removed"), "{summary}");
        assert!(library.get_discover_feed(&gone).await.unwrap().is_none());
        assert!(library
            .newest_discover_candidate_fetch(&gone)
            .await
            .unwrap()
            .is_none());
        assert!(queued(&library).await.is_empty());
    }

    #[tokio::test]
    async fn the_daily_refresh_queues_only_stale_subjects() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        assert_eq!(queue_daily_refresh(&state).await, 0);
        let mut ids = Vec::new();
        for name in ["Fresh", "Stale"] {
            let user = library
                .create_user(bookclerk_library::UserRole::Member, Some(name), None)
                .await
                .unwrap();
            ids.push(user.id);
        }
        let fresh = DiscoverSubject::User(ids[0]);
        let stale = DiscoverSubject::User(ids[1]);
        // A deleted user's leftover feed is dropped, not refreshed.
        let deleted = DiscoverSubject::User(9_999);
        for subject in [&fresh, &stale, &deleted] {
            library
                .put_discover_feed(subject, "{}", SeedScope::Own, None)
                .await
                .unwrap();
        }
        library
            .mark_discover_fetch(&fresh, Utc::now())
            .await
            .unwrap();
        library
            .mark_discover_fetch(&stale, Utc::now() - chrono::Duration::hours(25))
            .await
            .unwrap();
        assert_eq!(queue_daily_refresh(&state).await, 1);
        assert_eq!(
            queued(&library).await,
            vec![(JobKind::DiscoverCandidates, stale.key())]
        );
        assert!(library.get_discover_feed(&deleted).await.unwrap().is_none());
    }
}
