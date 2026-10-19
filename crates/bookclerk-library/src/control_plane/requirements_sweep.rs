//! Re-resolving the requirements of queued jobs after deployments change.
//!
//! [`super::resolve_job_requirements`] fixes a job's required instances at
//! admission and refuses a job no node could run. Deployments keep moving
//! after that: a `local` output is deployed or retired, or the scheduler
//! moves a replica. A pending job whose recorded requirements no longer
//! match would either run without writing a newly deployed node-bound output
//! or wait forever, holding a `max_pending` slot and its dedup key, because
//! claims page past it.
//!
//! [`sweep_job_requirements`] re-resolves every pending job that is or could
//! be routed and compares the result with what is recorded:
//!
//! - **Settled** (the requirement set is unchanged or only gains instances,
//!   and some node is deployed to run all of them): a new set is written at
//!   once, and any unsettled mark is cleared.
//! - **Unsettled** (no node runs all of them, or a recorded instance is no
//!   longer deployed anywhere): no recorded instance is dropped yet, but
//!   instances the job gained are recorded at once (the recorded set becomes
//!   the union), so the job cannot be claimed and run without a newly
//!   deployed node-bound output while it waits. The first sweep that sees it
//!   stamps [`JobPayload::requires_unsettled_since_ms`]; later sweeps only
//!   record further gains until it has been unsettled for the grace period
//!   ([`REQUIREMENTS_GRACE`]). A placement move retires the old replica and
//!   adds the new one in separate writes, so for a moment no node may run
//!   everything; the grace period keeps such a move from failing queued work.
//! - **Grace expired**: when some node can run the new set (an instance was
//!   only dropped, for example a retired `local` output), the new set is
//!   written, exactly what admission would record now. Otherwise the job is
//!   failed with `error_kind = "unroutable"` and a reason naming the
//!   instances, which frees its pending slot and dedup key.
//!
//! A grace period on a wall-clock stamp was chosen over counting consecutive
//! unsettled sweeps because sweeps run both on a timer and on every
//! deployment wakeup, so a count would expire faster on a busy cluster.
//!
//! Every write is a compare-and-swap on `state = 'pending'` and the lease
//! generation, which it bumps ([`LibraryStore::rewrite_pending_job_payload`],
//! [`LibraryStore::fail_pending_job`]); a job claimed meanwhile is left alone.
//! The daemon runs the sweep under the [`REQUIREMENTS_SWEEP_LEASE`] cluster
//! lease so one node sweeps at a time.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::job_routing::{desired_requirements, no_common_node_message, requirements_routable};
use crate::error::Result;
use crate::models::{JobKind, JobPayload, JobRecord};
use crate::store::LibraryStore;

/// Cluster lease the daemon holds while it sweeps.
pub const REQUIREMENTS_SWEEP_LEASE: &str = "jobs:requirements-sweep";

/// Lease length; the holder renews it on every pass.
pub const REQUIREMENTS_SWEEP_LEASE_TTL: Duration = Duration::from_secs(90);

/// How long a pending job's requirements may stay unsettled before the
/// sweep acts on them. Several scheduler passes (20 s apart) and guest
/// restarts fit inside it.
pub const REQUIREMENTS_GRACE: Duration = Duration::from_secs(10 * 60);

/// `error_kind` of a job the sweep failed.
pub const UNROUTABLE_ERROR_KIND: &str = "unroutable";

/// Pending rows read per page.
const SWEEP_PAGE: u64 = 64;

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequirementsSweepReport {
    /// Pending jobs whose requirements were re-resolved.
    pub examined: u32,
    /// Jobs whose requirement set was rewritten.
    pub rewritten: u32,
    /// Jobs newly marked, or still, unsettled within the grace period.
    pub unsettled: u32,
    /// Jobs whose unsettled mark was cleared without another change.
    pub settled: u32,
    /// Jobs failed as unroutable after the grace period.
    pub failed: u32,
    /// Writes that lost their compare-and-swap (the job was claimed).
    pub lost: u32,
}

impl RequirementsSweepReport {
    /// True when the sweep wrote anything.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.rewritten + self.settled + self.failed > 0
    }
}

/// Re-resolves the requirements of every pending routed job (see the module
/// docs for the rules).
///
/// `node_bound_plugin_keys` are the plugin keys of node-bound outputs. A
/// recorded instance whose key is missing from them is treated as no longer
/// required, so the caller must pass a set that does not depend on what the
/// sweeping node has installed, and must not sweep with a set it could not
/// read. `grace` is normally [`REQUIREMENTS_GRACE`]; `now` is the sweep's
/// clock.
///
/// # Errors
///
/// Returns an error when a read or write fails; jobs handled before the
/// failure keep their writes.
pub async fn sweep_job_requirements(
    store: &LibraryStore,
    node_bound_plugin_keys: &BTreeSet<String>,
    grace: Duration,
    now: DateTime<Utc>,
) -> Result<RequirementsSweepReport> {
    let mut report = RequirementsSweepReport::default();
    let now_ms = now.timestamp_millis();
    let grace_ms = i64::try_from(grace.as_millis()).unwrap_or(i64::MAX);
    let mut after: Option<String> = None;
    loop {
        let (jobs, next) = store
            .list_pending_jobs_page(after.as_deref(), SWEEP_PAGE)
            .await?;
        for job in jobs {
            if !sweepable(&job) {
                continue;
            }
            report.examined += 1;
            sweep_one(
                store,
                &job,
                node_bound_plugin_keys,
                now_ms,
                grace_ms,
                &mut report,
            )
            .await?;
        }
        match next {
            Some(next) => after = Some(next),
            None => return Ok(report),
        }
    }
}

/// True when admission could have given `job` requirements: it is pinned,
/// is an acquire (node-bound outputs), or already has some.
fn sweepable(job: &JobRecord) -> bool {
    job.kind != JobKind::Invalid
        && (job.kind == JobKind::Acquire
            || job.payload.plugin_instance_id.is_some()
            || !job.payload.requires.is_empty()
            || job.payload.requires_unsettled_since_ms.is_some())
}

/// Re-resolves one pending job and writes, holds, or fails it.
async fn sweep_one(
    store: &LibraryStore,
    job: &JobRecord,
    keys: &BTreeSet<String>,
    now_ms: i64,
    grace_ms: i64,
    report: &mut RequirementsSweepReport,
) -> Result<()> {
    let current = &job.payload.requires;
    let desired = desired_requirements(store, job.kind, &job.payload, keys).await?;
    let routable = requirements_routable(store, &desired).await?;
    let drops = current.iter().any(|id| !desired.contains(id));
    let since = job.payload.requires_unsettled_since_ms;

    if routable && !drops {
        if desired == *current && since.is_none() {
            return Ok(());
        }
        let changed = desired != *current;
        let payload = JobPayload {
            requires: desired,
            requires_unsettled_since_ms: None,
            ..job.payload.clone()
        };
        if write(store, job, &payload, report).await? {
            if changed {
                report.rewritten += 1;
            } else {
                report.settled += 1;
            }
        }
        return Ok(());
    }

    // Inside the grace period nothing recorded is dropped, but instances the
    // job gained are recorded at once: the old set may still be claimable,
    // and a job run on it would skip a newly deployed node-bound output.
    let in_grace = since.is_none_or(|since| now_ms.saturating_sub(since) < grace_ms);
    if in_grace {
        let held: Vec<String> = current
            .iter()
            .chain(desired.iter())
            .cloned()
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect();
        if since.is_some() && held == *current {
            report.unsettled += 1;
            return Ok(());
        }
        let payload = JobPayload {
            requires: held,
            requires_unsettled_since_ms: Some(since.unwrap_or(now_ms)),
            ..job.payload.clone()
        };
        if write(store, job, &payload, report).await? {
            report.unsettled += 1;
        }
        return Ok(());
    }
    if routable {
        let payload = JobPayload {
            requires: desired,
            requires_unsettled_since_ms: None,
            ..job.payload.clone()
        };
        if write(store, job, &payload, report).await? {
            report.rewritten += 1;
        }
        return Ok(());
    }
    let reason = format!(
        "{} for over {} minutes",
        no_common_node_message(job.kind, &desired),
        grace_ms / 60_000
    );
    if store
        .fail_pending_job(
            &job.id,
            job.lease_generation,
            UNROUTABLE_ERROR_KIND,
            &reason,
        )
        .await?
    {
        report.failed += 1;
    } else {
        report.lost += 1;
    }
    Ok(())
}

/// CAS-writes `payload` onto `job`, counting a lost race.
async fn write(
    store: &LibraryStore,
    job: &JobRecord,
    payload: &JobPayload,
    report: &mut RequirementsSweepReport,
) -> Result<bool> {
    let won = store
        .rewrite_pending_job_payload(&job.id, job.lease_generation, payload)
        .await?;
    if !won {
        report.lost += 1;
    }
    Ok(won)
}
