//! Durable job admission and scan / acquire / listen-sync executors.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_acquire::{acquire_book_indexed, AcquireRequest, MatchStorageOptions};
use bookclerk_config::BadBookAction;
use bookclerk_library::{
    is_downloadable, AcquireStatus, BookRecord, EnqueueJobSpec, EnqueueOutcome, JobKind,
    JobPayload, JobTrigger,
};
use bookclerk_source::ScanOptions;
use tracing::{error, info, warn};

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::registry::registry_for_job;

/// Result of admitting work into the durable queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitJob {
    /// A new row was inserted.
    Created(String),
    /// An equivalent pending/running job already exists.
    Duplicate(String),
    /// The pending+running cap was reached.
    QueueFull,
}

impl From<EnqueueOutcome> for AdmitJob {
    fn from(value: EnqueueOutcome) -> Self {
        match value {
            EnqueueOutcome::Created { id } => Self::Created(id),
            EnqueueOutcome::Duplicate { existing_id } => Self::Duplicate(existing_id),
            EnqueueOutcome::QueueFull => Self::QueueFull,
        }
    }
}

/// Wakes the event dispatcher after a successful acquire or storage match.
async fn notify_integrations(state: &AppState) {
    state.job_notify.notify_waiters();
}

/// Builds an [`EnqueueJobSpec`] for scan, acquire, or listen-sync admission.
fn enqueue_spec(
    kind: JobKind,
    account: Option<String>,
    title: Option<String>,
    plugin_instance_id: Option<String>,
    trigger: JobTrigger,
    max_pending: i64,
    max_attempts: i64,
) -> EnqueueJobSpec {
    EnqueueJobSpec {
        kind,
        payload: JobPayload {
            account,
            title,
            plugin_instance_id: plugin_instance_id
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty()),
            trigger,
            ..Default::default()
        },
        priority: 0,
        max_attempts,
        max_pending,
        run_after: None,
    }
}

/// Admission refused a job that no node could ever claim.
///
/// The job's required plugin instances share no node where all of them are
/// deployed to run, so the row would wait forever. Callers report it as a
/// conflict the operator fixes by deploying the instances to a common node,
/// not as a server failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobUnroutable(pub String);

impl std::fmt::Display for JobUnroutable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JobUnroutable {}

/// Types a requirement-resolution failure for admission callers.
///
/// [`bookclerk_library::LibraryError::Conflict`] from
/// `resolve_job_requirements` means no common deployment node and becomes
/// [`JobUnroutable`]; every other error passes through unchanged.
pub(crate) fn requirements_error(err: bookclerk_library::LibraryError) -> anyhow::Error {
    match err {
        bookclerk_library::LibraryError::Conflict(message) => JobUnroutable(message).into(),
        other => other.into(),
    }
}

/// Writes a job row and wakes the worker when a new row is created.
///
/// Takes a shared [`AppState::job_runtime`] permit so a database swap cannot
/// interleave with admission.
async fn admit(state: &AppState, mut spec: EnqueueJobSpec) -> anyhow::Result<AdmitJob> {
    let _permit = tokio::time::timeout(Duration::from_secs(15), state.job_runtime.read())
        .await
        .map_err(|_| anyhow::anyhow!("job admission paused (database reload in progress)"))?;
    let library = state.library_snapshot().await;
    if spec.payload.requires.is_empty() {
        let bound = if spec.kind == JobKind::Acquire {
            // Discovered keys plus instance keys, like the sweep: a node
            // without the `local` plugin installed must not admit an
            // acquire as claimable everywhere.
            sweep_node_bound_output_keys(state, &library).await?
        } else {
            std::collections::BTreeSet::new()
        };
        spec.payload.requires = bookclerk_library::control_plane::resolve_job_requirements(
            &library,
            spec.kind,
            &spec.payload,
            &bound,
        )
        .await
        .map_err(requirements_error)?;
    }
    let outcome = library.enqueue_job(spec).await?;
    if matches!(outcome, EnqueueOutcome::Created { .. }) {
        state.job_notify.notify_one();
    }
    Ok(outcome.into())
}

/// Plugin keys of the outputs that live on one node's disk.
///
/// Today that is the verified first-party `local` destination. An acquire
/// writes to every deployed one, so it is routed to a node running them
/// (see `control_plane::resolve_job_requirements`).
async fn node_bound_output_keys(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let config = state.config.read().await.clone();
    if config.paths.is_none() {
        // A config that was not loaded from disk has no plugin directories.
        return Ok(std::collections::BTreeSet::new());
    }
    node_bound_output_keys_from(bookclerk_plugin_host::discover_plugins(&config), library).await
}

/// Node-bound output keys from a discovery result.
///
/// A failed discovery (an unrelated plugin with a duplicate alias, a missing
/// binary, an I/O error) must not admit an acquire that any node may claim:
/// that node would skip `local` outputs it does not run. Instead every
/// instance whose key names the first-party `local` destination is treated
/// as node-bound. Without discovery the install's provenance cannot be
/// checked, so this may route more than the verified path would, which only
/// narrows the nodes that can claim the acquire; on a single node that runs
/// its outputs nothing changes.
///
/// # Errors
///
/// Returns an error when discovery failed and the instance list cannot be
/// read.
async fn node_bound_output_keys_from(
    discovered: bookclerk_plugin_host::Result<Vec<bookclerk_plugin_host::DiscoveredPlugin>>,
    library: &bookclerk_library::LibraryStore,
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    match discovered {
        Ok(plugins) => Ok(plugins
            .iter()
            .filter(|plugin| bookclerk_plugin_host::is_first_party_local_output(plugin))
            .map(|plugin| plugin.plugin_key().to_string())
            .collect()),
        Err(err) => {
            warn!(
                error = %err,
                "plugin discovery failed; routing acquire by the instances' `local` plugin keys"
            );
            local_output_keys_from_instances(library).await
        }
    }
}

/// Plugin keys of the instances whose key names the first-party `local`
/// destination, read from the cluster's instance list.
///
/// This does not depend on what is installed on this node, so it is the
/// fallback when discovery fails and the floor of what the requirements
/// sweep treats as node-bound.
///
/// # Errors
///
/// Returns an error when the instance list cannot be read.
async fn local_output_keys_from_instances(
    library: &bookclerk_library::LibraryStore,
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let instances = bookclerk_library::control_plane::list_all_plugin_instances(library).await?;
    Ok(instances
        .into_iter()
        .filter(|instance| {
            bookclerk_plugin_catalog::PluginKey::parse(&instance.plugin_key)
                .is_ok_and(|key| bookclerk_plugin_host::is_first_party_local_output_key(&key))
        })
        .map(|instance| instance.plugin_key)
        .collect())
}

/// Node-bound output keys for acquire admission and the cluster-wide
/// requirements sweep.
///
/// Admission and the sweep must agree, or an acquire admitted on a node
/// without the `local` plugin would be claimable by any node until the next
/// sweep. The sweep runs on whichever node holds its lease, and it drops recorded
/// requirements that its keys no longer cover, so a sweeping node that
/// lacks the `local` plugin, or whose discovery failed, must not read as
/// "no node-bound outputs". The result is this node's discovered keys
/// ([`node_bound_output_keys`]) plus every instance key that names the
/// first-party `local` destination, whatever is installed here.
///
/// # Errors
///
/// Returns an error when the instance list cannot be read; the caller skips
/// the sweep pass rather than sweeping with a partial set.
pub(crate) async fn sweep_node_bound_output_keys(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let mut keys = node_bound_output_keys(state, library).await?;
    keys.extend(local_output_keys_from_instances(library).await?);
    Ok(keys)
}

/// True when the fence asked to stop or the row is flagged cancelled.
async fn job_cancelled(
    ctx: Option<&JobExecCtx>,
    library: &bookclerk_library::LibraryStore,
) -> bool {
    let Some(ctx) = ctx else {
        return false;
    };
    ctx.is_cancelled()
        || library
            .job_cancel_requested(&ctx.fence.job_id)
            .await
            .unwrap_or(false)
}

/// Returns `(max_pending, max_attempts)` from the live `[jobs]` config.
async fn queue_limits(state: &AppState) -> (i64, i64) {
    let cfg = state.config.read().await;
    (
        i64::from(cfg.jobs.max_pending),
        i64::from(cfg.jobs.max_attempts),
    )
}

/// Admit a library scan. Does not spawn a per-request task.
///
/// `plugin_instance_id` pins the scan to one deployed instance; otherwise
/// every plugin is scanned once through its default instance.
pub async fn enqueue_scan(
    state: Arc<AppState>,
    account: Option<String>,
    plugin_instance_id: Option<String>,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        enqueue_spec(
            JobKind::Scan,
            account,
            None,
            plugin_instance_id,
            trigger,
            max_pending,
            max_attempts,
        ),
    )
    .await
}

/// Admit acquire for pending titles (optional title / account filter).
///
/// `plugin_instance_id` pins every fetch to one deployed instance of the
/// book's plugin; otherwise the plugin's default instance is used.
pub async fn enqueue_acquire(
    state: Arc<AppState>,
    title: Option<String>,
    account: Option<String>,
    plugin_instance_id: Option<String>,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        enqueue_spec(
            JobKind::Acquire,
            account,
            title,
            plugin_instance_id,
            trigger,
            max_pending,
            max_attempts,
        ),
    )
    .await
}

/// Admit a listening-progress sync.
pub async fn enqueue_listen_sync(
    state: Arc<AppState>,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        enqueue_spec(
            JobKind::ListenSync,
            None,
            None,
            None,
            trigger,
            max_pending,
            max_attempts,
        ),
    )
    .await
}

/// Admit a remote integration library scan.
pub async fn enqueue_integration_scan(
    state: Arc<AppState>,
    integration_id: String,
    force: bool,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        EnqueueJobSpec {
            kind: JobKind::IntegrationScan,
            payload: JobPayload {
                trigger,
                integration_id: Some(integration_id),
                force,
                ..Default::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Admit an ABI `JobHandler` stream-copy invocation.
#[allow(dead_code)] // API/CLI enqueue lands with the #118 broker follow-up.
pub async fn enqueue_plugin_copy(
    state: Arc<AppState>,
    plugin_id: String,
    source_key: String,
    dest_key: String,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let config = state.config.read().await.clone();
    let plugin = bookclerk_plugin_host::discover_plugins(&config)
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .into_iter()
        .find(|p| p.manifest.id == plugin_id)
        .ok_or_else(|| anyhow::anyhow!("plugin `{plugin_id}` is not installed"))?;
    let grant = bookclerk_plugin_host::require_grant(&config.paths().files_dir, &plugin)
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    if !grant.allows_job("stream_copy") {
        anyhow::bail!("plugin `{plugin_id}` grant does not authorize job trigger `stream_copy`");
    }
    let (max_pending, max_attempts) = queue_limits(&state).await;
    admit(
        &state,
        EnqueueJobSpec {
            kind: JobKind::PluginCopy,
            payload: JobPayload {
                trigger,
                plugin_id: Some(plugin_id),
                source_key: Some(source_key),
                dest_key: Some(dest_key),
                ..Default::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Progress/detail prefix after a fenced [`bookclerk_plugin_host::JobOutcome::Suspended`] commit.
///
/// [`crate::job_worker`] must not call `complete_job` for this outcome: suspend already moved
/// the row to `pending`.
pub const JOB_SUSPENDED_DETAIL_PREFIX: &str = "suspended until ";

/// Opens the consented named database bindings for one plugin invocation.
///
/// These are plugin-owned units, not the host job queue. `jobs` stays on the
/// library; guests never receive SQL against it. Reads the stored operator
/// grant by the session's canonical PluginKey (`database:<NAME>` entries)
/// and asks the active database adapter to open (provisioning on first use)
/// one isolated session per binding. Fails closed: a missing PluginKey
/// grant, or a granted binding that cannot be provisioned, fails the job
/// rather than running without isolation or inheriting another plugin's grant.
async fn open_granted_binding_databases(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    owner: &bookclerk_plugin_host::PluginSession,
) -> anyhow::Result<Vec<(String, bookclerk_plugin_host::GuestDatabaseFactory)>> {
    let plugin_key = owner.id();
    let config = state.config.read().await.clone();
    let files_dir = config.paths().files_dir.clone();
    let names = {
        let store = bookclerk_plugin_host::PluginGrantStore::load(&files_dir)
            .map_err(|err| anyhow::anyhow!("plugin grant store could not be loaded: {err}"))?;
        let grant = store.get_by_plugin_key(plugin_key).ok_or_else(|| {
            anyhow::anyhow!(
                "plugin `{plugin_key}` has no PluginKey grant; database bindings will not open"
            )
        })?;
        bookclerk_plugin_host::granted_database_bindings(grant)
    };
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let registry = state.database_registry.read().await;
    let Some(active) = registry.active() else {
        anyhow::bail!(
            "plugin `{plugin_key}` has granted database bindings {names:?} but no active \
             database plugin is loaded"
        );
    };
    active
        .open_binding_databases(&config, library, plugin_key, &names, owner)
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

/// Runs `plugin.worker().handle(stream_copy)` on a loaded destination guest.
pub async fn run_plugin_copy(
    state: &AppState,
    plugin_id: Option<&str>,
    source_key: Option<&str>,
    dest_key: Option<&str>,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let plugin_id = plugin_id.unwrap_or("local");
    let from = source_key.ok_or_else(|| anyhow::anyhow!("plugin_copy missing source_key"))?;
    let to = dest_key.ok_or_else(|| anyhow::anyhow!("plugin_copy missing dest_key"))?;
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    if let Some(ctx) = ctx {
        let _ = library.set_job_progress(&ctx.fence, "copying").await;
    }
    let destinations = state.destinations.read().await;
    let session = destinations
        .require_plugin_session(plugin_id, bookclerk_plugin_host::OPERATOR_ACCOUNT)
        .map_err(|err| anyhow::anyhow!(err))?;
    let lease = match ctx {
        Some(ctx) => bookclerk_plugin_host::JobInvocationLease {
            job_id: ctx.fence.job_id.clone(),
            attempt: u32::try_from(ctx.attempt_count.max(1)).unwrap_or(1),
            generation: ctx.fence.generation,
            dedup_key: ctx.dedup_key.clone(),
            deadline_unix_ms: ctx
                .lease_expires_at
                .map(|t| u64::try_from(t.timestamp_millis()).unwrap_or(u64::MAX / 2))
                .unwrap_or(u64::MAX / 2),
            checkpoint: ctx.checkpoint.clone(),
            invocation_sequence: ctx.invocation_sequence,
        },
        None => bookclerk_plugin_host::JobInvocationLease {
            job_id: "plugin_copy".into(),
            attempt: 1,
            generation: 1,
            dedup_key: "plugin_copy".into(),
            deadline_unix_ms: u64::MAX / 2,
            checkpoint: None,
            invocation_sequence: 1,
        },
    };
    let cancel = ctx
        .map(|c| std::sync::Arc::clone(&c.cancel))
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let databases = open_granted_binding_databases(state, &library, &session).await?;
    let outcome = tokio::select! {
        () = async {
            loop {
                if job_cancelled(ctx, &library).await {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => {
            anyhow::bail!("cancelled");
        }
        outcome = session.stream_copy_with_databases(
            lease,
            from,
            to,
            cancel,
            ctx.map(|c| (library.clone(), c.fence.clone())),
            databases,
        ) => outcome?,
    };
    match outcome {
        bookclerk_plugin_host::JobOutcome::Completed {
            message,
            bytes_copied,
        } => Ok(format!(
            "copied {from} -> {to} ({bytes_copied} bytes) {message}"
        )),
        bookclerk_plugin_host::JobOutcome::Cancelled { message } => {
            anyhow::bail!("cancelled: {message}")
        }
        bookclerk_plugin_host::JobOutcome::Retryable { message, .. } => {
            anyhow::bail!("plugin_copy retryable: {message}")
        }
        bookclerk_plugin_host::JobOutcome::Rejected { message } => {
            anyhow::bail!("plugin_copy rejected: {message}")
        }
        bookclerk_plugin_host::JobOutcome::Suspended {
            checkpoint,
            wake_at_unix_ms,
        } => {
            let Some(ctx) = ctx else {
                anyhow::bail!("plugin_copy suspended without a job fence");
            };
            let wake = chrono::DateTime::from_timestamp_millis(
                i64::try_from(wake_at_unix_ms).unwrap_or(i64::MAX),
            )
            .ok_or_else(|| anyhow::anyhow!("plugin_copy suspend wake_at out of range"))?;
            let committed = library.suspend_job(&ctx.fence, &checkpoint, wake).await?;
            if !committed {
                anyhow::bail!("plugin_copy suspend lost the fence; checkpoint was not committed");
            }
            Ok(format!("{JOB_SUSPENDED_DETAIL_PREFIX}{wake}"))
        }
    }
}

/// Run a scan synchronously (worker / tests).
///
/// Cancel is checked between sources. An in-flight source page fetch finishes
/// before the next source starts.
///
/// `plugin_instance_id` pins the scan to one deployed instance; otherwise
/// every plugin is scanned once through its default instance.
pub async fn run_scan(
    state: &AppState,
    account: Option<&str>,
    plugin_instance_id: Option<&str>,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let _guard = state.work_lock.lock().await;
    let cfg = state.config.read().await.clone();
    let paths = cfg.paths();
    paths.ensure_dirs()?;
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let registry = registry_for_job(state).await;
    let options = ScanOptions {
        accounts: account.map(|a| vec![a.to_string()]).unwrap_or_default(),
        page_size: 50,
        import_episodes: cfg.library.import_episodes,
        import_plus_titles: cfg.library.import_plus_titles,
        cancel: ctx.map(|c| c.cancel.clone()),
    };
    let summary = match plugin_instance_id {
        Some(instance) => {
            let source = registry.require_routed(instance, Some(instance))?;
            registry.scan_one(&library, source, options).await?
        }
        None => registry.scan_all(&library, options).await?,
    };
    info!(
        account = account.unwrap_or("*"),
        accounts = summary.accounts,
        books_upserted = summary.books_upserted,
        pages = summary.pages,
        skipped_disabled = summary.skipped_disabled,
        ambiguous = summary.warnings.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "run_scan finished"
    );
    if !summary.warnings.is_empty() {
        warn!(
            warnings = %summary.warnings.join("; "),
            "scan skipped ambiguous plugin keys"
        );
    }
    if cfg.library.enrich_from_audible {
        if let Err(err) =
            bookclerk_enrich::enrich_books_from_audible(&library, cfg.library.enrich_min_confidence)
                .await
        {
            warn!(error = %err, "Audible enrichment failed");
        }
    }
    let mut message = format!(
        "{} account(s), {} book upsert(s), {} page(s), {} skipped (scan disabled)",
        summary.accounts, summary.books_upserted, summary.pages, summary.skipped_disabled
    );
    if !summary.warnings.is_empty() {
        message.push_str("; skipped ambiguous keys: ");
        message.push_str(&summary.warnings.join("; "));
    }
    Ok(message)
}

/// Acquire pending titles synchronously.
///
/// When `ctx` is set, progress is fenced and cancel is checked between titles.
///
/// `plugin_instance_id` pins every fetch to that deployed instance of the
/// book's plugin; otherwise the plugin's default instance is used.
pub async fn run_acquire(
    state: &AppState,
    title: Option<&str>,
    account: Option<&str>,
    plugin_instance_id: Option<&str>,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let _guard = state.work_lock.lock().await;
    let cfg = state.config.read().await.clone();
    let paths = cfg.paths();
    paths.ensure_dirs()?;
    let library = state.library.read().await.clone();
    let destinations =
        bookclerk_plugin_host::build_acquire_destinations(&cfg, &*state.destinations.read().await)?;
    let storage = destinations.listing_backend()?;
    // The primary destination's options carry its naming overrides and
    // prefix, so match, layout, and acquire agree on keys.
    let options = destinations.primary_destination().options.clone();
    let registry = registry_for_job(state).await;

    let mut index = bookclerk_acquire::scan_storage(
        &library,
        storage.as_ref(),
        ctx.map(|c| &c.fence),
        ctx.and_then(|c| c.checkpoint.as_ref()),
        true,
    )
    .await?;
    let _ = bookclerk_acquire::match_storage_using_index(
        &library,
        storage.as_ref(),
        MatchStorageOptions {
            account: account.map(str::to_string),
            clear_missing: true,
            fix_layout: cfg.library.fix_storage_layout,
            download: options.clone(),
            ..Default::default()
        },
        &index,
    )
    .await?;

    let books = library.list_books(account).await?;
    let targets: Vec<_> = books
        .into_iter()
        .filter(|b| {
            title.is_none_or(|a| {
                a.eq_ignore_ascii_case(&b.uuid)
                    || a.eq_ignore_ascii_case(&b.product_id)
                    || b.isbn.as_deref().is_some_and(|i| a.eq_ignore_ascii_case(i))
                    || b.asin.as_deref().is_some_and(|x| a.eq_ignore_ascii_case(x))
            })
        })
        .filter(|b| {
            acquire_job_includes_book(b, options.download_pdf, cfg.library.download_episodes)
        })
        .collect();
    // A pinned instance serves its own plugin's books only; the rest are
    // skipped, not failed.
    let (targets, foreign): (Vec<_>, Vec<_>) = targets.into_iter().partition(|b| {
        plugin_instance_id.is_none_or(|pin| registry.route(&b.plugin_key, Some(pin)).is_some())
    });
    if !foreign.is_empty() {
        info!(
            plugin_instance_id = plugin_instance_id.unwrap_or(""),
            skipped = foreign.len(),
            "run_acquire: titles owned by other plugins are skipped for a pinned instance"
        );
    }

    if targets.is_empty() {
        info!(
            title = title.unwrap_or("*"),
            account = account.unwrap_or("*"),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "run_acquire finished: nothing to acquire"
        );
        if let Some(scan_id) = index.scan_id() {
            let _ = library.storage_scan_delete(scan_id).await;
        }
        return Ok("nothing to acquire".into());
    }

    let mut ok = 0u32;
    let mut matched = 0u32;
    let mut failed = 0u32;
    let bad_book = cfg.output.bad_book_action;
    let total = targets.len();
    for (idx, book) in targets.into_iter().enumerate() {
        if let Some(ctx) = ctx {
            if ctx.is_cancelled()
                || library
                    .job_cancel_requested(&ctx.fence.job_id)
                    .await
                    .unwrap_or(false)
            {
                anyhow::bail!("cancelled after {idx}/{total} titles");
            }
            let progress = format!("{}/{} acquiring {}", idx + 1, total, book.title);
            if !library
                .set_job_progress(&ctx.fence, &progress)
                .await
                .unwrap_or(false)
            {
                ctx.request_cancel();
                anyhow::bail!("cancelled after {idx}/{total} titles (lease fence lost)");
            }
        }
        let content_source = registry
            .require_routed(&book.plugin_key, plugin_instance_id)
            .map_err(|err| anyhow::anyhow!("{err} (title {})", book.asin_or_isbn()))?;
        let req = AcquireRequest {
            asin: book.download_product_id().to_string(),
            book_uuid: Some(book.uuid.clone()),
            source: book.source.clone(),
            account_id: book.account_id.clone(),
            title: book.title.clone(),
            authors: book.authors.clone(),
            narrators: book.narrators.clone(),
            series: book.series.clone(),
            series_index: book.series_index.clone(),
            options: options.clone(),
            files_dir: paths.files_dir.clone(),
            cache_dir: cfg.download_cache_dir(),
            force: false,
            write_destinations: None,
            job_id: ctx.map(|c| c.fence.job_id.clone()),
            temp_quota_bytes: Some(cfg.jobs.temp_quota_bytes),
        };
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            match acquire_book_indexed(
                &library,
                &destinations,
                req.clone(),
                Some(&mut index),
                content_source.as_ref(),
            )
            .await
            {
                Ok(result) if result.matched_existing => {
                    info!(asin = %result.asin, key = %result.storage_key, "matched existing");
                    notify_integrations(state).await;
                    matched += 1;
                    break;
                }
                Ok(result) => {
                    info!(asin = %result.asin, key = %result.storage_key, "acquired");
                    notify_integrations(state).await;
                    ok += 1;
                    break;
                }
                Err(err) => {
                    if bad_book == BadBookAction::Retry && attempts < 2 {
                        warn!(asin = %book.asin_or_isbn(), error = %err, "acquire failed; retrying");
                        continue;
                    }
                    warn!(asin = %book.asin_or_isbn(), error = %err, "acquire failed");
                    failed += 1;
                    if matches!(bad_book, BadBookAction::Ask | BadBookAction::Abort) {
                        anyhow::bail!("acquire aborted on {}: {err}", book.asin_or_isbn());
                    }
                    break;
                }
            }
        }
    }
    let detail = format!("acquired={ok} matched={matched} failed={failed}");
    info!(
        title = title.unwrap_or("*"),
        account = account.unwrap_or("*"),
        acquired = ok,
        matched,
        failed,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "run_acquire finished"
    );
    if failed > 0 && bad_book == BadBookAction::Retry {
        anyhow::bail!("{detail}");
    }
    if let Some(scan_id) = index.scan_id() {
        let _ = library.storage_scan_delete(scan_id).await;
    }
    Ok(detail)
}

/// Run a remote integration library scan synchronously.
///
/// The remote `scan_library` RPC is not interruptible. Cancel is checked
/// before the call; a repeat scan is idempotent (remote upsert / rescan).
pub async fn run_integration_scan(
    state: &AppState,
    integration_id: &str,
    force: bool,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let integrations = state.integrations.read().await.clone();
    let Some(integration) = integrations.get(integration_id) else {
        anyhow::bail!("integration `{integration_id}` not found");
    };
    if !integration.supports_library_scan() {
        anyhow::bail!("integration `{integration_id}` does not support library scan");
    }
    integration.scan_library(force).await?;
    info!(
        integration = integration_id,
        force,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "run_integration_scan finished"
    );
    Ok(format!("scanned integration {integration_id}"))
}

/// Sync listening progress from capable integrations.
///
/// Cancel is checked between providers. An in-flight provider RPC finishes.
pub async fn run_listen_sync(state: &AppState, ctx: Option<&JobExecCtx>) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let library = state.library.read().await.clone();
    let integrations = state.integrations.read().await.clone();
    let mut summary = bookclerk_integrations::SyncListeningSummary::default();
    for integration in integrations.listening_sync_providers() {
        if job_cancelled(ctx, &library).await {
            anyhow::bail!("cancelled");
        }
        match integration.sync_listening_progress(&library).await {
            Ok(n) => {
                summary.upserted += n;
                summary
                    .by_provider
                    .push(bookclerk_integrations::SyncListeningProviderResult {
                        id: integration.id().to_string(),
                        upserted: n,
                        error: None,
                    });
            }
            Err(err) => {
                warn!(id = %integration.id(), %err, "listening sync provider failed");
                summary
                    .by_provider
                    .push(bookclerk_integrations::SyncListeningProviderResult {
                        id: integration.id().to_string(),
                        upserted: 0,
                        error: Some(err.to_string()),
                    });
            }
        }
    }
    if summary.by_provider.is_empty() {
        info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "listen_sync skipped (no capable integrations)"
        );
        return Ok("no capable integrations".into());
    }
    for p in &summary.by_provider {
        if let Some(err) = &p.error {
            warn!(id = %p.id, %err, "listening sync provider failed");
        }
    }
    let detail = format!(
        "upserted={} providers={}",
        summary.upserted,
        summary.by_provider.len()
    );
    info!(
        upserted = summary.upserted,
        providers = summary.by_provider.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "listen_sync finished"
    );
    if summary.by_provider.iter().any(|p| p.error.is_some()) {
        anyhow::bail!("{detail}");
    }
    Ok(detail)
}

/// Whether [`run_acquire`] should visit this library row.
///
/// Already-acquired audio is still included when companion PDF download is
/// enabled and the PDF is not stored, so a later job can resume the sidecar
/// through [`bookclerk_acquire::acquire_book_indexed`].
fn acquire_job_includes_book(
    book: &BookRecord,
    download_pdf: bool,
    download_episodes: bool,
) -> bool {
    if !is_downloadable(&book.content_kind) {
        return false;
    }
    if !download_episodes && book.content_kind == "episode" {
        return false;
    }
    if book.acquire_status != AcquireStatus::Acquired {
        return true;
    }
    download_pdf && companion_pdf_needs_resume(book)
}

/// True when audio is acquired but the companion PDF is missing or incomplete.
fn companion_pdf_needs_resume(book: &BookRecord) -> bool {
    book.pdf_status != AcquireStatus::Acquired
        || book
            .pdf_storage_key
            .as_deref()
            .is_none_or(|key| key.is_empty())
}

/// Log a handler failure and request a diagnostics upload.
pub fn note_job_failure(job_id: &str, err: &anyhow::Error) {
    error!(%job_id, error = %err, "job failed");
    if let Some(diag) = bookclerk_config::diagnostics_global() {
        diag.request_upload("job_failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{LibraryStore, NewBook};

    async fn test_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    async fn book_with(
        store: &LibraryStore,
        acquire: AcquireStatus,
        audio_key: Option<&str>,
        pdf: AcquireStatus,
        pdf_key: Option<&str>,
        content_kind: &str,
    ) -> BookRecord {
        store
            .upsert_account("audible", "user-1", "us", None, true, "audible")
            .await
            .unwrap();
        let mut new_book = NewBook::minimal("B00TEST", "user-1", "us", "Test");
        new_book.content_kind = content_kind.to_string();
        store.upsert_book(&new_book).await.unwrap();
        store
            .set_acquire_status("B00TEST", "user-1", acquire, audio_key, None)
            .await
            .unwrap();
        store
            .set_pdf_status("B00TEST", "user-1", pdf, pdf_key)
            .await
            .unwrap();
        store
            .get_book("B00TEST", "user-1")
            .await
            .unwrap()
            .expect("book")
    }

    #[tokio::test]
    async fn acquire_job_keeps_acquired_audio_when_pdf_needs_resume() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::Acquired,
            Some("audio.m4b"),
            AcquireStatus::Error,
            None,
            "book",
        )
        .await;
        assert!(
            acquire_job_includes_book(&book, true, true),
            "job path must visit acquired audio so companion PDF can resume"
        );
        assert!(
            !acquire_job_includes_book(&book, false, true),
            "without download_pdf the acquired title stays skipped"
        );
    }

    #[tokio::test]
    async fn acquire_job_keeps_acquired_audio_when_pdf_key_is_missing() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::Acquired,
            Some("audio.m4b"),
            AcquireStatus::Acquired,
            Some(""),
            "book",
        )
        .await;
        assert!(acquire_job_includes_book(&book, true, true));
    }

    #[tokio::test]
    async fn acquire_job_skips_fully_acquired_title() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::Acquired,
            Some("audio.m4b"),
            AcquireStatus::Acquired,
            Some("audio.pdf"),
            "book",
        )
        .await;
        assert!(!acquire_job_includes_book(&book, true, true));
    }

    #[tokio::test]
    async fn acquire_job_includes_pending_audio() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::NotAcquired,
            None,
            AcquireStatus::NotAcquired,
            None,
            "book",
        )
        .await;
        assert!(acquire_job_includes_book(&book, false, true));
    }

    #[tokio::test]
    async fn acquire_job_skips_podcast_parent() {
        let store = test_store().await;
        let book = book_with(
            &store,
            AcquireStatus::NotAcquired,
            None,
            AcquireStatus::Error,
            None,
            "podcast",
        )
        .await;
        assert!(!acquire_job_includes_book(&book, true, true));
    }

    /// A failed plugin discovery still routes an acquire to the node that runs
    /// the `local` output, instead of admitting it for any node to claim.
    #[tokio::test]
    async fn discovery_failure_still_routes_acquire_to_local_outputs() {
        use bookclerk_library::control_plane::{
            create_plugin_instance, ensure_plugin_deployment, resolve_job_requirements,
            ConfigActor, DesiredState,
        };

        const LOCAL: &str = "platform:bookclerk/bookclerk-plugin-destination-local";
        const S3: &str = "platform:bookclerk/bookclerk-plugin-destination-s3";
        const LOOKALIKE: &str = "path:file:///tmp/out#bookclerk-plugin-destination-local";
        let store = test_store().await;
        let operator = ConfigActor::Operator {
            id: "operator".into(),
        };
        let mut local = None;
        for key in [LOCAL, S3, LOOKALIKE] {
            let instance = create_plugin_instance(&store, &operator, key, "")
                .await
                .unwrap();
            ensure_plugin_deployment(
                &store,
                &ConfigActor::Bootstrap,
                &instance.id,
                "node-a",
                DesiredState::Present,
            )
            .await
            .unwrap();
            if key == LOCAL {
                local = Some(instance.id);
            }
        }

        let failed = Err(bookclerk_plugin_host::PluginError::message(
            "duplicate plugin alias `echo`",
        ));
        let bound = node_bound_output_keys_from(failed, &store).await.unwrap();
        assert_eq!(bound, std::collections::BTreeSet::from([LOCAL.to_string()]));

        let payload = JobPayload {
            title: Some("one book".into()),
            ..Default::default()
        };
        let requires = resolve_job_requirements(&store, JobKind::Acquire, &payload, &bound)
            .await
            .unwrap();
        assert_eq!(requires, vec![local.unwrap().to_string()]);
    }

    #[test]
    fn grant_store_read_error_fails_the_job_instead_of_dropping_bindings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin-grants.json"), "{not-json").unwrap();
        let err = bookclerk_plugin_host::PluginGrantStore::load(dir.path())
            .map_err(|err| anyhow::anyhow!("plugin grant store could not be loaded: {err}"))
            .expect_err("corrupt grant file must fail closed");
        assert!(err.to_string().contains("plugin grant store"), "{err}");
    }
}
