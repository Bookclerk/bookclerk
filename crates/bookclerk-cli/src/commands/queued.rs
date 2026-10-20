//! Library work the CLI queues through the database (H-5).
//!
//! `bookclerk library scan` and `library acquire` no longer run storefront
//! or destination guests in the CLI process. They plan their jobs with the
//! same planners as the daemon's HTTP API (`control_plane::plan_scan` and
//! `plan_acquire`, as operator authority, since the CLI holds the library
//! database), record each job's routing requirements
//! (`control_plane::resolve_job_requirements`), and write the rows with
//! `enqueue_job`. Any live `bookclerkd` node of the cluster, or a host node
//! trusted with the job, claims them through the routed claim. The CLI then
//! waits for every job, printing progress as it changes; Ctrl-C asks each
//! unfinished job to cancel.
//!
//! Live-store rules still hold: `library scan --account <id>` queues exactly
//! one job for that account (an explicit account bypasses `scan_enabled`),
//! and `library acquire --asin <ASIN>` exactly one job for that title.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    default_instance_for_key, list_all_plugin_instances, live_node_ids, plan_acquire, plan_scan,
    PluginInstanceId, ScanRequester,
};
use bookclerk_library::{
    EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, JobRecord, JobState, JobTrigger,
    LibraryStore,
};

/// How often a waiting CLI re-reads its jobs.
const POLL: Duration = Duration::from_millis(750);

/// What admitting one planned job did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Queued {
    /// A new job.
    Created(String),
    /// The same work was already pending or running.
    Duplicate(String),
    /// The queue had no room.
    QueueFull,
}

impl Queued {
    /// The job id to wait for, when there is one.
    pub(crate) fn job_id(&self) -> Option<&str> {
        match self {
            Self::Created(id) | Self::Duplicate(id) => Some(id),
            Self::QueueFull => None,
        }
    }
}

/// Plugin keys of node-bound outputs (the verified first-party `local`
/// destination), as the daemon routes acquires with. A discovery failure
/// is an error: an acquire queued without its node-bound outputs could run
/// on a node that does not hold them.
///
/// # Errors
///
/// Returns an error when plugin discovery fails.
pub(crate) fn node_bound_output_keys(config: &Config) -> anyhow::Result<BTreeSet<String>> {
    if config.paths.is_none() {
        return Ok(BTreeSet::new());
    }
    Ok(bookclerk_plugin_host::discover_plugins(config)?
        .iter()
        .filter(|plugin| bookclerk_plugin_host::is_first_party_local_output(plugin))
        .map(|plugin| plugin.plugin_key().to_string())
        .collect())
}

/// Admits `plan` as jobs of `kind`, each with its routing requirements.
///
/// # Errors
///
/// Returns an error when requirements cannot be met by any node (a
/// conflict) or a read or write fails. Jobs admitted before the failure
/// stay queued.
pub(crate) async fn admit(
    store: &LibraryStore,
    config: &Config,
    kind: JobKind,
    plan: Vec<JobPayload>,
    node_bound: &BTreeSet<String>,
) -> anyhow::Result<Vec<Queued>> {
    let mut out = Vec::with_capacity(plan.len());
    for mut payload in plan {
        payload.trigger = JobTrigger::Api;
        let bound = if kind == JobKind::Acquire {
            node_bound.clone()
        } else {
            BTreeSet::new()
        };
        payload.requires = bookclerk_library::control_plane::resolve_job_requirements(
            store, kind, &payload, &bound,
        )
        .await?;
        let outcome = store
            .enqueue_job(EnqueueJobSpec {
                kind,
                payload,
                priority: 0,
                max_attempts: i64::from(config.jobs.max_attempts),
                max_pending: i64::from(config.jobs.max_pending),
                run_after: None,
            })
            .await?;
        out.push(match outcome {
            EnqueueOutcome::Created { id } => Queued::Created(id),
            EnqueueOutcome::Duplicate { existing_id } => Queued::Duplicate(existing_id),
            EnqueueOutcome::QueueFull => Queued::QueueFull,
        });
    }
    Ok(out)
}

/// Fails closed when no daemon of the cluster is live to run queued work.
///
/// # Errors
///
/// Returns an error naming what to do when no node heartbeated recently.
pub(crate) async fn require_live_node(store: &LibraryStore) -> anyhow::Result<()> {
    if live_node_ids(store).await?.is_empty() {
        anyhow::bail!(
            "no bookclerkd node of this cluster is running to execute queued jobs; \
             start bookclerkd (`cargo dev` in development), or pass --no-wait to only queue"
        );
    }
    Ok(())
}

/// The plugin instance a `--source` / `--instance` pair pins work to.
///
/// `--instance` wins. Otherwise `--source` is matched against every plugin
/// instance in the database: an instance id, a full plugin key, or the
/// plugin's alias (the key's `#<alias>` part, any case); a plugin key that
/// matches resolves to its default instance. No guest is spawned.
///
/// # Errors
///
/// Returns an error when the source matches nothing or several plugins.
pub(crate) async fn resolve_pin(
    store: &LibraryStore,
    source: Option<&str>,
    instance: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if let Some(pin) = instance.map(str::trim).filter(|p| !p.is_empty()) {
        PluginInstanceId::parse(pin).map_err(|err| anyhow::anyhow!("--instance {pin}: {err}"))?;
        return Ok(Some(pin.to_string()));
    }
    let Some(needle) = source.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let instances = list_all_plugin_instances(store).await?;
    if let Some(found) = instances.iter().find(|i| i.id.as_str() == needle) {
        return Ok(Some(found.id.as_str().to_string()));
    }
    // A store name recorded on account rows (`audible`) names its plugin too.
    let by_store: BTreeSet<String> = store
        .list_accounts()
        .await?
        .into_iter()
        .filter(|a| a.source.eq_ignore_ascii_case(needle))
        .map(|a| a.plugin_key)
        .collect();
    let keys: BTreeSet<String> = instances
        .iter()
        .map(|i| i.plugin_key.clone())
        .filter(|key| {
            key == needle
                || by_store.contains(key)
                || key
                    .rsplit_once('#')
                    .is_some_and(|(_, alias)| alias.eq_ignore_ascii_case(needle))
        })
        .collect();
    match keys.len() {
        0 => {
            let known: BTreeSet<&str> = instances
                .iter()
                .map(|i| {
                    i.plugin_key
                        .rsplit_once('#')
                        .map_or(i.plugin_key.as_str(), |(_, a)| a)
                })
                .collect();
            anyhow::bail!(
                "content source `{needle}` has no plugin instance (known: {})",
                known.into_iter().collect::<Vec<_>>().join(", ")
            )
        }
        1 => {
            let key = keys.into_iter().next().unwrap_or_default();
            let default = default_instance_for_key(store, &key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("plugin {key} has no default instance"))?;
            Ok(Some(default.id.as_str().to_string()))
        }
        _ => anyhow::bail!(
            "content source `{needle}` is ambiguous; use --instance or a plugin key. candidates: {}",
            keys.into_iter().collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The account id an operator named: an exact `accounts.account_id`, or
/// else the id of the one account whose label matches (any case). Anything
/// else is passed on unchanged and matches nothing when the job runs.
///
/// # Errors
///
/// Returns an error when the read fails or a label names several accounts.
pub(crate) async fn resolve_account(store: &LibraryStore, needle: &str) -> anyhow::Result<String> {
    let needle = needle.trim();
    let accounts = store.list_accounts().await?;
    if accounts.iter().any(|a| a.account_id == needle) {
        return Ok(needle.to_string());
    }
    let ids: BTreeSet<&str> = accounts
        .iter()
        .filter(|a| {
            a.label
                .as_deref()
                .is_some_and(|label| label.eq_ignore_ascii_case(needle))
        })
        .map(|a| a.account_id.as_str())
        .collect();
    match ids.len() {
        0 => Ok(needle.to_string()),
        1 => Ok(ids.into_iter().next().unwrap_or(needle).to_string()),
        _ => anyhow::bail!(
            "account name `{needle}` matches several accounts ({}); use the account id",
            ids.into_iter().collect::<Vec<_>>().join(", ")
        ),
    }
}

/// `library scan` arguments.
pub(crate) struct ScanArgs<'a> {
    /// Accounts named (ids or labels); empty scans every scan-enabled one.
    pub accounts: &'a [String],
    /// `--source`.
    pub source: Option<&'a str>,
    /// `--instance`.
    pub instance: Option<&'a str>,
    /// Wait for the jobs.
    pub wait: bool,
}

/// `library acquire` arguments.
pub(crate) struct AcquireArgs<'a> {
    /// Title ids; empty acquires every pending title.
    pub titles: &'a [String],
    /// `--account`.
    pub account: Option<&'a str>,
    /// `--instance`.
    pub instance: Option<&'a str>,
    /// `--force`.
    pub force: bool,
    /// `--pdf`.
    pub pdf_only: bool,
    /// Wait for the jobs.
    pub wait: bool,
}

/// Queues `library scan`'s jobs and, with `wait`, waits for them.
///
/// # Returns
///
/// `true` when the jobs were waited for and all succeeded; `false` when
/// they were only queued.
///
/// # Errors
///
/// Returns an error when planning or admission fails, no node is live to
/// wait on, the queue was full, or a job did not succeed.
pub(crate) async fn scan(
    store: &LibraryStore,
    config: &Config,
    args: ScanArgs<'_>,
) -> anyhow::Result<bool> {
    let pin = resolve_pin(store, args.source, args.instance).await?;
    if args.wait {
        require_live_node(store).await?;
    }
    let mut plan = Vec::new();
    if args.accounts.is_empty() {
        plan.extend(plan_scan(store, ScanRequester::Operator, None, pin.as_deref()).await?);
    } else {
        for needle in args.accounts {
            let account = resolve_account(store, needle).await?;
            plan.extend(
                plan_scan(
                    store,
                    ScanRequester::Operator,
                    Some(&account),
                    pin.as_deref(),
                )
                .await?,
            );
        }
    }
    let queued = admit(store, config, JobKind::Scan, plan, &BTreeSet::new()).await?;
    settle("scan", store, queued, args.wait).await
}

/// Queues `library acquire`'s jobs and, with `wait`, waits for them.
///
/// # Returns
///
/// `true` when the jobs were waited for and all succeeded; `false` when
/// they were only queued.
///
/// # Errors
///
/// As [`scan`].
pub(crate) async fn acquire(
    store: &LibraryStore,
    config: &Config,
    args: AcquireArgs<'_>,
) -> anyhow::Result<bool> {
    let pin = resolve_pin(store, None, args.instance).await?;
    if args.wait {
        require_live_node(store).await?;
    }
    let account = match args.account {
        Some(needle) => Some(resolve_account(store, needle).await?),
        None => None,
    };
    let mut plan = plan_acquire(
        store,
        ScanRequester::Operator,
        account.as_deref(),
        args.titles,
        pin.as_deref(),
    )
    .await?;
    for payload in &mut plan {
        payload.force = args.force;
        payload.pdf_only = args.pdf_only;
    }
    let node_bound = node_bound_output_keys(config)?;
    let queued = admit(store, config, JobKind::Acquire, plan, &node_bound).await?;
    settle("acquire", store, queued, args.wait).await
}

/// Reports `queued`, then waits (with `wait`) and reports each outcome.
async fn settle(
    kind: &str,
    store: &LibraryStore,
    queued: Vec<Queued>,
    wait: bool,
) -> anyhow::Result<bool> {
    if queued.is_empty() {
        anyhow::bail!("nothing to {kind}: no store account matches");
    }
    report_queued(kind, &queued);
    let full = queued.iter().filter(|q| **q == Queued::QueueFull).count();
    let ids: Vec<String> = queued
        .iter()
        .filter_map(|q| q.job_id().map(str::to_string))
        .collect();
    if !wait {
        for id in &ids {
            println!("{id}");
        }
    } else {
        let jobs = wait_for_jobs(store, &ids).await?;
        finish(kind, &jobs)?;
        // A job whose row vanished while waited on has no known outcome.
        let finished: std::collections::BTreeSet<&str> =
            jobs.iter().map(|job| job.id.as_str()).collect();
        let vanished: std::collections::BTreeSet<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|id| !finished.contains(id))
            .collect();
        if !vanished.is_empty() {
            anyhow::bail!(
                "the outcome of {} {kind} job(s) is unknown: they no longer exist: {}",
                vanished.len(),
                vanished.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
    }
    if full > 0 {
        anyhow::bail!("{full} {kind} job(s) were not queued: the job queue is full");
    }
    Ok(wait)
}

/// Prints what admission did, one line per planned job, to stderr.
pub(crate) fn report_queued(kind: &str, queued: &[Queued]) {
    for item in queued {
        match item {
            Queued::Created(id) => eprintln!("queued {kind} job {id}"),
            Queued::Duplicate(id) => eprintln!("{kind} job {id} was already queued; waiting on it"),
            Queued::QueueFull => eprintln!("{kind} not queued: the job queue is full"),
        }
    }
}

/// True when the job will not change state again.
fn terminal(state: JobState) -> bool {
    matches!(
        state,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled
    )
}

/// Waits until every job in `ids` is terminal, printing progress changes
/// to stderr. Ctrl-C asks each unfinished job to cancel and keeps waiting
/// until they stop; a second Ctrl-C stops waiting.
///
/// An id listed more than once (a plan that admitted the same job twice) is
/// waited on once.
///
/// # Returns
///
/// The final record of each job, in `ids` order and once per job (a job
/// that vanished is left out).
///
/// # Errors
///
/// Returns an error when a read fails, or when interrupted twice.
pub(crate) async fn wait_for_jobs(
    store: &LibraryStore,
    ids: &[String],
) -> anyhow::Result<Vec<JobRecord>> {
    let mut unique = std::collections::BTreeSet::new();
    let ids: Vec<String> = ids
        .iter()
        .filter(|id| unique.insert(id.as_str()))
        .cloned()
        .collect();
    let mut last: BTreeMap<String, (JobState, Option<String>)> = BTreeMap::new();
    // `None` marks a job whose row disappeared while it was waited on.
    let mut done: BTreeMap<String, Option<JobRecord>> = BTreeMap::new();
    let mut interrupted = false;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        for id in &ids {
            if done.contains_key(id) {
                continue;
            }
            let Some(job) = store.get_job(id).await? else {
                eprintln!("job {id} no longer exists");
                done.insert(id.clone(), None);
                continue;
            };
            let seen = (job.state, job.progress.clone());
            if last.get(id) != Some(&seen) {
                match (&job.state, job.progress.as_deref()) {
                    (JobState::Pending, _) if job.attempt_count == 0 => {
                        eprintln!("{id}: waiting for a node to claim it");
                    }
                    (JobState::Running, Some(progress)) => eprintln!("{id}: {progress}"),
                    (JobState::Running, None) => eprintln!("{id}: running"),
                    (JobState::Pending, _) => eprintln!(
                        "{id}: retrying (attempt {} of {})",
                        job.attempt_count + 1,
                        job.max_attempts
                    ),
                    _ => {}
                }
                last.insert(id.clone(), seen);
            }
            if terminal(job.state) {
                done.insert(id.clone(), Some(job));
            }
        }
        if done.len() == ids.len() {
            return Ok(ids
                .iter()
                .filter_map(|id| done.remove(id).flatten())
                .collect());
        }
        tokio::select! {
            () = tokio::time::sleep(POLL) => {}
            _ = &mut ctrl_c => {
                if interrupted {
                    anyhow::bail!("stopped waiting; the jobs keep their state in the queue");
                }
                interrupted = true;
                eprintln!("interrupted: asking unfinished jobs to cancel (Ctrl-C again to stop waiting)");
                for id in ids.iter().filter(|id| !done.contains_key(*id)) {
                    let _ = store.request_job_cancel(id).await;
                }
                ctrl_c.set(tokio::signal::ctrl_c());
            }
        }
    }
}

/// Prints each finished job's outcome and fails when any did not succeed.
///
/// # Errors
///
/// Returns an error naming the failed or cancelled jobs.
pub(crate) fn finish(kind: &str, jobs: &[JobRecord]) -> anyhow::Result<()> {
    let mut failed = Vec::new();
    for job in jobs {
        match job.state {
            JobState::Succeeded => println!(
                "{kind} {} succeeded: {}",
                job.id,
                job.progress.as_deref().unwrap_or("done")
            ),
            state => {
                eprintln!(
                    "{kind} {} {}: {}",
                    job.id,
                    state.as_str(),
                    job.error_message
                        .as_deref()
                        .or(job.error_kind.as_deref())
                        .unwrap_or("no detail")
                );
                failed.push(job.id.clone());
            }
        }
    }
    if !failed.is_empty() {
        anyhow::bail!("{kind} did not succeed: {}", failed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bookclerk_config::Config;
    use bookclerk_library::{JobKind, LibraryStore};

    use super::{acquire, resolve_account, resolve_pin, scan, AcquireArgs, ScanArgs};

    async fn store() -> LibraryStore {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        for (account, label, scan) in [
            ("a1", Some("Riley"), true),
            ("a2", None, true),
            ("a3", Some("Off"), false),
        ] {
            store
                .upsert_account("pk", account, "us", label, scan, "teststore")
                .await
                .unwrap();
        }
        store
    }

    /// Jobs of `kind`, as `(account, title, force)`, sorted.
    async fn pending(
        store: &LibraryStore,
        kind: JobKind,
    ) -> Vec<(Option<String>, Option<String>, bool)> {
        let mut jobs: Vec<_> = store
            .list_jobs(50)
            .await
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == kind)
            .map(|job| (job.payload.account, job.payload.title, job.payload.force))
            .collect();
        jobs.sort();
        jobs
    }

    #[tokio::test]
    async fn a_named_account_queues_exactly_one_scan() {
        let store = store().await;
        let config = Config::default();
        // An explicit account bypasses scan_enabled, and a label names it.
        let queued = scan(
            &store,
            &config,
            ScanArgs {
                accounts: &["off".to_string()],
                source: None,
                instance: None,
                wait: false,
            },
        )
        .await
        .unwrap();
        assert!(!queued, "only queued");
        assert_eq!(
            pending(&store, JobKind::Scan).await,
            vec![(Some("a3".into()), None, false)]
        );
        // A bare scan is one job per scan-enabled account.
        scan(
            &store,
            &config,
            ScanArgs {
                accounts: &[],
                source: None,
                instance: None,
                wait: false,
            },
        )
        .await
        .unwrap();
        let accounts: Vec<_> = pending(&store, JobKind::Scan)
            .await
            .into_iter()
            .map(|(account, _, _)| account.unwrap())
            .collect();
        assert_eq!(accounts, vec!["a1", "a2", "a3"]);
    }

    #[tokio::test]
    async fn waiting_needs_a_live_node_and_nothing_is_queued_without_one() {
        let store = store().await;
        let err = scan(
            &store,
            &Config::default(),
            ScanArgs {
                accounts: &["a1".to_string()],
                source: None,
                instance: None,
                wait: true,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("--no-wait"), "{err}");
        assert!(pending(&store, JobKind::Scan).await.is_empty());
    }

    #[tokio::test]
    async fn an_acquire_queues_one_job_per_title() {
        let store = store().await;
        let titles = [
            "B0ONE".to_string(),
            "B0TWO".to_string(),
            "b0one".to_string(),
        ];
        acquire(
            &store,
            &Config::default(),
            AcquireArgs {
                titles: &titles,
                account: Some("Riley"),
                instance: None,
                force: true,
                pdf_only: false,
                wait: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            pending(&store, JobKind::Acquire).await,
            vec![
                (Some("a1".into()), Some("B0ONE".into()), true),
                (Some("a1".into()), Some("B0TWO".into()), true),
            ]
        );
    }

    #[tokio::test]
    async fn pins_and_accounts_resolve_or_fail_closed() {
        let store = store().await;
        assert_eq!(resolve_pin(&store, None, None).await.unwrap(), None);
        assert!(resolve_pin(&store, None, Some("not an id")).await.is_err());
        let err = resolve_pin(&store, Some("teststore"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no plugin instance"), "{err}");
        assert_eq!(resolve_account(&store, "a2").await.unwrap(), "a2");
        assert_eq!(resolve_account(&store, "riley").await.unwrap(), "a1");
        assert_eq!(resolve_account(&store, "nobody").await.unwrap(), "nobody");
        store
            .upsert_account("pk", "a4", "us", Some("Riley"), true, "teststore")
            .await
            .unwrap();
        assert!(resolve_account(&store, "Riley").await.is_err());
    }
}
