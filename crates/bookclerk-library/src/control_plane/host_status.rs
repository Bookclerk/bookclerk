//! Host heartbeats: capacity, budget, accounting, and availability.
//!
//! Hosts never talk to each other; each daemon writes its own row here every
//! [`HOST_HEARTBEAT_INTERVAL`] and the scheduler reads every row from the
//! shared database. A row carries:
//!
//! - **capacity** the host detects now: CPU cores and memory, the tighter of
//!   the machine and the daemon's cgroup;
//! - **budget**: the share of that capacity an operator lets the scheduler
//!   commit (`host.runtime`: `cpu_budget_percent`, `memory_budget_percent`,
//!   `schedulable`);
//! - **reservations**: the sum of the CPU and memory ceilings of the guests
//!   running on the host, the same numbers the jail enforces, so the budget is
//!   accounted against what the host has promised, not a guess;
//! - **measured use**: CPU busy and memory available on the machine (or the
//!   CPU-limited cgroup), so a desktop busy with other work reads as busy;
//! - **availability**: a decaying online fraction in parts per million (about
//!   a day's memory), so a
//!   desktop that is often off ranks below an always-on VPS with the same
//!   free capacity.
//!
//! A host is live while its last heartbeat is younger than three intervals.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::deployments::{
    list_deployments_for_host, list_deployments_for_instance, DeploymentStatus,
};
use super::instances::PluginInstanceId;
use crate::entities::{host_status, plugin_deployment_observations};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// How often a daemon heartbeats.
pub const HOST_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// A host whose last heartbeat is older than this many intervals is not live.
pub const HOST_LIVE_INTERVALS: i64 = 3;

/// Time constant of the availability average (about a day).
pub const AVAILABILITY_TAU: Duration = Duration::from_secs(24 * 3600);

/// Memory a guest is assumed to need when no ceiling is known (the grant
/// default).
pub const DEFAULT_GUEST_MEMORY_BYTES: i64 = 512 * 1024 * 1024;

/// CPU a guest is assumed to need when no ceiling is known (the grant
/// default, 80% of a core).
pub const DEFAULT_GUEST_CPU_MILLICORES: i64 = 800;

/// CPU and memory one guest may use at most: its grant's ceilings after the
/// host's `[plugins.jail]` limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct GuestReservation {
    /// Memory ceiling in bytes.
    pub memory_bytes: i64,
    /// CPU ceiling in millicores.
    pub cpu_millicores: i64,
}

impl GuestReservation {
    /// The grant defaults, used when nothing better is known.
    #[must_use]
    pub fn default_guest() -> Self {
        Self {
            memory_bytes: DEFAULT_GUEST_MEMORY_BYTES,
            cpu_millicores: DEFAULT_GUEST_CPU_MILLICORES,
        }
    }

    /// Component-wise sum.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
            cpu_millicores: self.cpu_millicores.saturating_add(other.cpu_millicores),
        }
    }

    /// Component-wise maximum.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self {
            memory_bytes: self.memory_bytes.max(other.memory_bytes),
            cpu_millicores: self.cpu_millicores.max(other.cpu_millicores),
        }
    }
}

/// One heartbeat's measurements, before budget and accounting are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHeartbeat {
    /// Daemon process incarnation.
    pub incarnation: String,
    /// RFC 3339 time the daemon started.
    pub started_at: String,
    /// Promised heartbeat interval.
    pub interval: Duration,
    /// Detected CPU capacity in millicores.
    pub cpu_millicores: i64,
    /// Detected memory in bytes.
    pub memory_bytes: Option<i64>,
    /// Measured CPU in use, in millicores.
    pub cpu_used_millicores: Option<i64>,
    /// Measured memory available to new work.
    pub memory_available_bytes: Option<i64>,
    /// Free bytes on the files directory's filesystem.
    pub files_dir_free_bytes: Option<i64>,
}

/// A host's latest heartbeat with budget, accounting, and availability.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostStatus {
    /// Reporting host.
    pub host_id: String,
    /// Daemon process incarnation.
    pub incarnation: String,
    /// RFC 3339 daemon start.
    pub started_at: String,
    /// RFC 3339 heartbeat time.
    pub heartbeat_at: String,
    /// Heartbeat time in Unix milliseconds.
    pub heartbeat_ms: i64,
    /// Promised interval in milliseconds.
    pub interval_ms: i64,
    /// Detected CPU capacity in millicores.
    pub cpu_millicores: i64,
    /// Detected memory.
    pub memory_bytes: Option<i64>,
    /// CPU the scheduler may commit, in millicores.
    pub cpu_budget_millicores: i64,
    /// Memory the scheduler may commit.
    pub memory_budget_bytes: Option<i64>,
    /// False when an operator drained the host.
    pub schedulable: bool,
    /// Measured CPU in use.
    pub cpu_used_millicores: Option<i64>,
    /// Measured memory available.
    pub memory_available_bytes: Option<i64>,
    /// Free bytes on the files directory's filesystem.
    pub files_dir_free_bytes: Option<i64>,
    /// Ceilings of the guests running here.
    pub reserved: GuestReservation,
    /// Guests running here.
    pub running_guests: i64,
    /// Recent online fraction in parts per million.
    pub availability_ppm: i64,
    /// Unix milliseconds the current online stretch began.
    pub online_since_ms: i64,
}

impl HostStatus {
    /// True while the last heartbeat is younger than
    /// [`HOST_LIVE_INTERVALS`] intervals.
    #[must_use]
    pub fn is_live(&self, now_ms: i64) -> bool {
        let window = self
            .interval_ms
            .max(1_000)
            .saturating_mul(HOST_LIVE_INTERVALS);
        now_ms.saturating_sub(self.heartbeat_ms) <= window
    }

    /// CPU still uncommitted under the budget, in millicores. Measured use
    /// counts when it is higher than the reservations (work outside
    /// Bookclerk on a shared machine).
    #[must_use]
    pub fn cpu_headroom_millicores(&self) -> i64 {
        let committed = self
            .reserved
            .cpu_millicores
            .max(self.cpu_used_millicores.unwrap_or(0));
        self.cpu_budget_millicores.saturating_sub(committed)
    }

    /// Memory still uncommitted under the budget and actually available on
    /// the machine; empty when the host could not detect its memory.
    #[must_use]
    pub fn memory_headroom_bytes(&self) -> Option<i64> {
        let budget = self.memory_budget_bytes?;
        let accounted = budget.saturating_sub(self.reserved.memory_bytes);
        Some(match self.memory_available_bytes {
            Some(available) => accounted.min(available),
            None => accounted,
        })
    }
}

/// Samples capacity and measured use for heartbeats. Keeps the previous CPU
/// counters so each sample reports utilization since the last one.
#[derive(Debug)]
pub struct HostSampler {
    /// Directory whose filesystem free space is reported.
    files_dir: PathBuf,
    /// The daemon's cgroup, when it runs in one.
    cgroup_dir: Option<PathBuf>,
    /// Previous CPU counters and when they were read.
    last_cpu: Option<(bookclerk_sandbox::CpuTimes, Instant)>,
    /// Daemon incarnation.
    incarnation: String,
    /// RFC 3339 daemon start.
    started_at: String,
}

impl HostSampler {
    /// A sampler for this process.
    #[must_use]
    pub fn new(files_dir: &Path) -> Self {
        Self::with_cgroup(files_dir, bookclerk_sandbox::process_cgroup_dir())
    }

    /// A sampler reading cgroup files from `cgroup_dir` (tests point it at
    /// fake controller files).
    #[must_use]
    pub fn with_cgroup(files_dir: &Path, cgroup_dir: Option<PathBuf>) -> Self {
        Self {
            files_dir: files_dir.to_path_buf(),
            cgroup_dir,
            last_cpu: None,
            incarnation: super::current_process_incarnation().to_string(),
            started_at: Utc::now().to_rfc3339(),
        }
    }

    /// Reads capacity and use now. The first sample has no CPU use yet.
    /// Blocking: small `/proc`, cgroup, and `statvfs` reads.
    pub fn sample(&mut self) -> HostHeartbeat {
        let cgroup = self
            .cgroup_dir
            .as_deref()
            .map(bookclerk_sandbox::read_cgroup_sample)
            .unwrap_or_default();
        let system = bookclerk_sandbox::read_system_memory();
        let logical = bookclerk_sandbox::logical_cpu_count().unwrap_or(1).max(1);
        let mut cpu_millicores = logical.saturating_mul(1000);
        if let (Some(quota), Some(period)) = (cgroup.cpu_max_quota_us, cgroup.cpu_max_period_us) {
            if let Some(quota_millis) = quota.saturating_mul(1000).checked_div(period) {
                let quota_millis = i64::try_from(quota_millis).unwrap_or(i64::MAX);
                cpu_millicores = cpu_millicores.min(quota_millis.max(1));
            }
        }
        let fit = |v: u64| i64::try_from(v).ok();
        let memory_bytes = match (system.total_bytes, cgroup.memory_max_bytes) {
            (Some(total), Some(max)) => fit(total.min(max)),
            (Some(total), None) => fit(total),
            (None, Some(max)) => fit(max),
            (None, None) => None,
        };
        let cgroup_available = cgroup
            .memory_max_bytes
            .zip(cgroup.memory_current_bytes)
            .map(|(max, current)| max.saturating_sub(current));
        let memory_available_bytes = match (system.available_bytes, cgroup_available) {
            (Some(system), Some(cgroup)) => fit(system.min(cgroup)),
            (Some(system), None) => fit(system),
            (None, Some(cgroup)) => fit(cgroup),
            (None, None) => None,
        };
        let now = Instant::now();
        let times = bookclerk_sandbox::read_cpu_times(self.cgroup_dir.as_deref(), &cgroup);
        let cpu_used_millicores = match (self.last_cpu, times) {
            (Some((before, at)), Some(after)) => bookclerk_sandbox::cpu_used_millicores(
                before,
                after,
                u64::try_from(logical.saturating_mul(1000)).unwrap_or(0),
                now.saturating_duration_since(at),
            )
            .and_then(fit)
            .map(|used| used.min(cpu_millicores)),
            _ => None,
        };
        if let Some(times) = times {
            self.last_cpu = Some((times, now));
        }
        HostHeartbeat {
            incarnation: self.incarnation.clone(),
            started_at: self.started_at.clone(),
            interval: HOST_HEARTBEAT_INTERVAL,
            cpu_millicores,
            memory_bytes,
            cpu_used_millicores,
            memory_available_bytes,
            files_dir_free_bytes: bookclerk_sandbox::filesystem_free_bytes(&self.files_dir)
                .and_then(fit),
        }
    }
}

/// Full availability in parts per million.
pub const AVAILABILITY_FULL_PPM: i64 = 1_000_000;

/// Next availability average (parts per million) after `gap_ms` since the
/// previous heartbeat.
///
/// The host counts as online for the whole gap when it is within the live
/// window, else for one interval (it stopped right after the last
/// heartbeat). The average decays with [`AVAILABILITY_TAU`].
#[must_use]
pub fn next_availability(previous_ppm: i64, gap_ms: i64, interval_ms: i64) -> i64 {
    if gap_ms <= 0 {
        return previous_ppm.clamp(0, AVAILABILITY_FULL_PPM);
    }
    let live_window = interval_ms.max(1_000).saturating_mul(HOST_LIVE_INTERVALS);
    let online_ms = if gap_ms <= live_window {
        gap_ms
    } else {
        interval_ms.min(gap_ms)
    };
    #[allow(clippy::cast_precision_loss)]
    let weight = (-(gap_ms as f64) / AVAILABILITY_TAU.as_millis() as f64).exp();
    #[allow(clippy::cast_precision_loss)]
    let online_fraction = online_ms as f64 / gap_ms as f64;
    #[allow(clippy::cast_precision_loss)]
    let next = previous_ppm as f64 * weight
        + AVAILABILITY_FULL_PPM as f64 * online_fraction * (1.0 - weight);
    #[allow(clippy::cast_possible_truncation)]
    let rounded = next.round() as i64;
    rounded.clamp(0, AVAILABILITY_FULL_PPM)
}

/// Records this host's heartbeat: its measurements, its budget from
/// `host.runtime`, the reservations of its running guests, and the updated
/// availability.
///
/// # Errors
///
/// Returns an error when a read or the upsert fails.
pub async fn record_host_heartbeat(
    store: &LibraryStore,
    host_id: &str,
    heartbeat: &HostHeartbeat,
) -> Result<HostStatus> {
    let now = Utc::now();
    let now_ms = now.timestamp_millis();
    let interval_ms = i64::try_from(heartbeat.interval.as_millis()).unwrap_or(i64::MAX);
    let runtime = match super::load_host_runtime(store, host_id).await {
        Ok(document) => document.body,
        Err(LibraryError::NotFound(_)) => super::HostRuntimeSettingsV1::new(),
        Err(err) => return Err(err),
    };
    let (reserved, running_guests) = host_reservations(store, host_id).await?;
    let previous = load_host_status(store, host_id).await?;
    let (availability_ppm, online_since_ms) = match &previous {
        Some(prev) => {
            let gap = now_ms.saturating_sub(prev.heartbeat_ms);
            let continuous = prev.is_live(now_ms);
            (
                next_availability(prev.availability_ppm, gap, prev.interval_ms),
                if continuous {
                    prev.online_since_ms
                } else {
                    now_ms
                },
            )
        }
        None => (AVAILABILITY_FULL_PPM, now_ms),
    };
    let budget = |value: i64, percent: u8| value.saturating_mul(i64::from(percent)) / 100;
    let status = HostStatus {
        host_id: host_id.to_string(),
        incarnation: heartbeat.incarnation.clone(),
        started_at: heartbeat.started_at.clone(),
        heartbeat_at: now.to_rfc3339(),
        heartbeat_ms: now_ms,
        interval_ms,
        cpu_millicores: heartbeat.cpu_millicores,
        memory_bytes: heartbeat.memory_bytes,
        cpu_budget_millicores: budget(heartbeat.cpu_millicores, runtime.cpu_budget_percent),
        memory_budget_bytes: heartbeat
            .memory_bytes
            .map(|bytes| budget(bytes, runtime.memory_budget_percent)),
        schedulable: runtime.schedulable,
        cpu_used_millicores: heartbeat.cpu_used_millicores,
        memory_available_bytes: heartbeat.memory_available_bytes,
        files_dir_free_bytes: heartbeat.files_dir_free_bytes,
        reserved,
        running_guests,
        availability_ppm,
        online_since_ms,
    };
    upsert_host_status(store, &status).await?;
    Ok(status)
}

/// Writes `status` as the host's row (insert or replace).
pub(crate) async fn upsert_host_status(store: &LibraryStore, status: &HostStatus) -> Result<()> {
    let model = host_status::ActiveModel {
        host_id: Set(status.host_id.clone()),
        incarnation: Set(status.incarnation.clone()),
        started_at: Set(status.started_at.clone()),
        heartbeat_at: Set(status.heartbeat_at.clone()),
        heartbeat_ms: Set(status.heartbeat_ms),
        interval_ms: Set(status.interval_ms),
        cpu_millicores: Set(status.cpu_millicores),
        memory_bytes: Set(status.memory_bytes),
        cpu_budget_millicores: Set(status.cpu_budget_millicores),
        memory_budget_bytes: Set(status.memory_budget_bytes),
        schedulable: Set(i64::from(status.schedulable)),
        cpu_used_millicores: Set(status.cpu_used_millicores),
        memory_available_bytes: Set(status.memory_available_bytes),
        files_dir_free_bytes: Set(status.files_dir_free_bytes),
        reserved_cpu_millicores: Set(status.reserved.cpu_millicores),
        reserved_memory_bytes: Set(status.reserved.memory_bytes),
        running_guests: Set(status.running_guests),
        availability_ppm: Set(status.availability_ppm),
        online_since_ms: Set(status.online_since_ms),
    };
    host_status::Entity::insert(model)
        .on_conflict(
            OnConflict::column(host_status::Column::HostId)
                .update_columns([
                    host_status::Column::Incarnation,
                    host_status::Column::StartedAt,
                    host_status::Column::HeartbeatAt,
                    host_status::Column::HeartbeatMs,
                    host_status::Column::IntervalMs,
                    host_status::Column::CpuMillicores,
                    host_status::Column::MemoryBytes,
                    host_status::Column::CpuBudgetMillicores,
                    host_status::Column::MemoryBudgetBytes,
                    host_status::Column::Schedulable,
                    host_status::Column::CpuUsedMillicores,
                    host_status::Column::MemoryAvailableBytes,
                    host_status::Column::FilesDirFreeBytes,
                    host_status::Column::ReservedCpuMillicores,
                    host_status::Column::ReservedMemoryBytes,
                    host_status::Column::RunningGuests,
                    host_status::Column::AvailabilityPpm,
                    host_status::Column::OnlineSinceMs,
                ])
                .to_owned(),
        )
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// Maps a stored row.
fn status_from_model(row: host_status::Model) -> HostStatus {
    HostStatus {
        host_id: row.host_id,
        incarnation: row.incarnation,
        started_at: row.started_at,
        heartbeat_at: row.heartbeat_at,
        heartbeat_ms: row.heartbeat_ms,
        interval_ms: row.interval_ms,
        cpu_millicores: row.cpu_millicores,
        memory_bytes: row.memory_bytes,
        cpu_budget_millicores: row.cpu_budget_millicores,
        memory_budget_bytes: row.memory_budget_bytes,
        schedulable: row.schedulable != 0,
        cpu_used_millicores: row.cpu_used_millicores,
        memory_available_bytes: row.memory_available_bytes,
        files_dir_free_bytes: row.files_dir_free_bytes,
        reserved: GuestReservation {
            memory_bytes: row.reserved_memory_bytes,
            cpu_millicores: row.reserved_cpu_millicores,
        },
        running_guests: row.running_guests,
        availability_ppm: row.availability_ppm,
        online_since_ms: row.online_since_ms,
    }
}

/// One host's latest heartbeat, if its daemon ever ran.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_host_status(store: &LibraryStore, host_id: &str) -> Result<Option<HostStatus>> {
    Ok(host_status::Entity::find_by_id(host_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(status_from_model))
}

/// Every host's latest heartbeat, by host id.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_host_statuses(store: &LibraryStore) -> Result<Vec<HostStatus>> {
    Ok(host_status::Entity::find()
        .order_by_asc(host_status::Column::HostId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(status_from_model)
        .collect())
}

/// Records the ceilings of the guest a host just started for a deployment.
///
/// # Errors
///
/// Returns an error when the update fails.
pub async fn record_deployment_reservation(
    store: &LibraryStore,
    deployment_id: &str,
    host_id: &str,
    reservation: GuestReservation,
) -> Result<()> {
    plugin_deployment_observations::Entity::update_many()
        .col_expr(
            plugin_deployment_observations::Column::ReservedMemoryBytes,
            sea_orm::sea_query::Expr::value(Some(reservation.memory_bytes)),
        )
        .col_expr(
            plugin_deployment_observations::Column::ReservedCpuMillicores,
            sea_orm::sea_query::Expr::value(Some(reservation.cpu_millicores)),
        )
        .filter(plugin_deployment_observations::Column::DeploymentId.eq(deployment_id))
        .filter(plugin_deployment_observations::Column::HostId.eq(host_id))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// True for an observation of a guest that is up.
fn guest_is_up(status: &str) -> bool {
    status == DeploymentStatus::Running.as_str() || status == DeploymentStatus::Healthy.as_str()
}

/// The reservation recorded on one observation row, if any.
fn row_reservation(row: &plugin_deployment_observations::Model) -> Option<GuestReservation> {
    Some(GuestReservation {
        memory_bytes: row.reserved_memory_bytes?,
        cpu_millicores: row.reserved_cpu_millicores?,
    })
}

/// Sum of the ceilings of the guests running on `host_id`, and how many run.
/// A running guest with no recorded ceiling counts as the grant defaults.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn host_reservations(
    store: &LibraryStore,
    host_id: &str,
) -> Result<(GuestReservation, i64)> {
    let running: std::collections::BTreeSet<String> = list_deployments_for_host(store, host_id)
        .await?
        .into_iter()
        .filter(|d| d.desired.runs())
        .map(|d| d.deployment_id)
        .collect();
    let rows = plugin_deployment_observations::Entity::find()
        .filter(plugin_deployment_observations::Column::HostId.eq(host_id))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut total = GuestReservation::default();
    let mut count = 0;
    for row in rows {
        if !running.contains(&row.deployment_id) || !guest_is_up(&row.status) {
            continue;
        }
        total = total.plus(row_reservation(&row).unwrap_or_else(GuestReservation::default_guest));
        count += 1;
    }
    Ok((total, count))
}

/// What one more guest of `instance_id` is expected to need: the largest
/// ceiling any host recorded for it, or `None` when no host ran it yet.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn instance_demand(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<Option<GuestReservation>> {
    let mut demand: Option<GuestReservation> = None;
    for deployment in list_deployments_for_instance(store, instance_id).await? {
        let rows = plugin_deployment_observations::Entity::find()
            .filter(
                plugin_deployment_observations::Column::DeploymentId.eq(&deployment.deployment_id),
            )
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        for reservation in rows.iter().filter_map(row_reservation) {
            demand = Some(demand.map_or(reservation, |d| d.max(reservation)));
        }
    }
    Ok(demand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_rises_while_online_and_decays_while_off() {
        let full = AVAILABILITY_FULL_PPM;
        // Continuous heartbeats keep a fully available host full.
        assert_eq!(next_availability(full, 15_000, 15_000), full);
        // Ten hours off costs about a third of a day-long average.
        let after_gap = next_availability(full, 10 * 3600 * 1000, 15_000);
        assert!((600_000..700_000).contains(&after_gap), "{after_gap}");
        // A host that keeps coming back climbs again.
        let mut a = after_gap;
        for _ in 0..(4 * 60 * 6) {
            a = next_availability(a, 15_000, 15_000);
        }
        assert!(a > after_gap, "{a} > {after_gap}");
        assert!(a < full);
        assert_eq!(next_availability(500_000, 0, 15_000), 500_000);
    }

    #[test]
    fn headroom_counts_reservations_measured_use_and_available_memory() {
        let status = HostStatus {
            host_id: "h".into(),
            incarnation: String::new(),
            started_at: String::new(),
            heartbeat_at: String::new(),
            heartbeat_ms: 100_000,
            interval_ms: 15_000,
            cpu_millicores: 4000,
            memory_bytes: Some(8 << 30),
            cpu_budget_millicores: 3200,
            memory_budget_bytes: Some(6 << 30),
            schedulable: true,
            cpu_used_millicores: Some(2500),
            memory_available_bytes: Some(1 << 30),
            files_dir_free_bytes: None,
            reserved: GuestReservation {
                memory_bytes: 2 << 30,
                cpu_millicores: 1600,
            },
            running_guests: 2,
            availability_ppm: 900_000,
            online_since_ms: 0,
        };
        // Measured use (2.5 cores) is above the reservations (1.6).
        assert_eq!(status.cpu_headroom_millicores(), 700);
        // 4 GiB of budget is left, but only 1 GiB is free on the machine.
        assert_eq!(status.memory_headroom_bytes(), Some(1 << 30));
        assert!(status.is_live(100_000 + 45_000));
        assert!(!status.is_live(100_000 + 45_001));
    }
}
