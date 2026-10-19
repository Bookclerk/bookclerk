//! Counted cluster semaphores built from fenced `cluster_leases` rows.
//!
//! A semaphore `name` with limit `N` is the lease rows `sem:<name>#0` …
//! `sem:<name>#<N-1>`. A permit is one of those slot leases: a holder, a
//! fencing token, and an expiry. Taking a free or expired slot is the same
//! compare-and-swap the singleton leases use, so no two holders ever own one
//! slot, and since only slots below the limit are taken, no more than `N`
//! permits are live at once on any engine and any number of connections.
//!
//! Recovery after a holder fails is expiry plus fencing: an unrenewed slot
//! expires and the next acquirer takes it with a larger token, after which
//! the old holder's renewal and release match zero rows. A caller that
//! renews with [`renew_permits`] and gets `false` has lost its permit and
//! must stop the work it gates.
//!
//! Lowering a limit does not revoke slots at or above the new limit; their
//! holders keep them until they release or stop renewing, and no new permit
//! is granted until the live count (every slot, including those at or above
//! the new limit) is under the new limit. That count is read before the
//! slot's compare-and-swap, so while old high slots are still live, racing
//! acquirers can briefly overshoot the new limit (never the old one); once
//! the high slots drain, the slot CAS alone enforces the new limit exactly.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::{acquire_lease, release_lease, ClusterLease};
use crate::entities::cluster_leases;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Prefix of every semaphore slot lease name.
pub const SEMAPHORE_LEASE_PREFIX: &str = "sem:";

/// Largest limit a semaphore honours; larger limits are clamped.
pub const MAX_SEMAPHORE_LIMIT: u32 = 256;

/// One semaphore a caller wants a permit of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemaphoreRequest {
    /// Semaphore name (for example `jobs:network` or `account:<key>:<id>`).
    pub name: String,
    /// Permits the semaphore grants cluster-wide (at least 1).
    pub limit: u32,
}

impl SemaphoreRequest {
    /// A request for one permit of `name` under `limit`.
    #[must_use]
    pub fn new(name: impl Into<String>, limit: u32) -> Self {
        Self {
            name: name.into(),
            limit,
        }
    }
}

/// One held permit: a slot lease of a semaphore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemaphorePermit {
    /// Semaphore name.
    pub semaphore: String,
    /// Slot index below the limit the permit was taken under.
    pub slot: u32,
    /// The slot lease (holder, fencing token, expiry).
    pub lease: ClusterLease,
}

/// Outcome of [`acquire_permits`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermitAcquire {
    /// Every requested permit is held.
    Granted(Vec<SemaphorePermit>),
    /// The named semaphore had no free slot; nothing is held.
    Blocked(String),
}

/// Lease name of slot `slot` of semaphore `name`.
#[must_use]
pub fn semaphore_slot_lease_name(name: &str, slot: u32) -> String {
    format!("{SEMAPHORE_LEASE_PREFIX}{name}#{slot}")
}

/// Effective slot count for `limit` (at least 1, at most [`MAX_SEMAPHORE_LIMIT`]).
fn slots(limit: u32) -> u32 {
    limit.clamp(1, MAX_SEMAPHORE_LIMIT)
}

/// Slot lease names read per statement, within every backend's bind limit.
const SLOT_READ_CHUNK: usize = bookclerk_plugin_abi::HOST_MIN_BINDS as usize;

/// Current slot rows of `name` below `limit`, by slot.
async fn load_slots(
    store: &LibraryStore,
    name: &str,
    limit: u32,
) -> Result<BTreeMap<u32, cluster_leases::Model>> {
    let names: Vec<String> = (0..slots(limit))
        .map(|slot| semaphore_slot_lease_name(name, slot))
        .collect();
    // Read in chunks every backend binds in one statement (D1 caps a
    // statement at 100 parameters; a semaphore has up to 256 slots).
    let mut rows = Vec::new();
    for chunk in names.chunks(SLOT_READ_CHUNK) {
        rows.extend(
            cluster_leases::Entity::find()
                .filter(cluster_leases::Column::Name.is_in(chunk.to_vec()))
                .all(store.db())
                .await
                .map_err(LibraryError::Orm)?,
        );
    }
    let mut by_slot = BTreeMap::new();
    for row in rows {
        if let Some(slot) = names.iter().position(|n| *n == row.name) {
            by_slot.insert(u32::try_from(slot).unwrap_or(u32::MAX), row);
        }
    }
    Ok(by_slot)
}

/// Every slot row of `name` (any slot index, including those at or above
/// the current limit), by slot.
async fn load_all_slots(
    store: &LibraryStore,
    name: &str,
) -> Result<BTreeMap<u32, cluster_leases::Model>> {
    let prefix = format!("{SEMAPHORE_LEASE_PREFIX}{name}#");
    // The LIKE is a coarse pre-filter that may match more rows than the
    // semaphore's (`%` / `_` in a name, case-insensitive engines); the exact
    // prefix check below drops the extras. A backslash (an escape on some
    // engines) could make it match fewer, so the pattern stops before one.
    let pattern_prefix = prefix.split('\\').next().unwrap_or_default();
    let rows = cluster_leases::Entity::find()
        .filter(cluster_leases::Column::Name.like(format!("{pattern_prefix}%")))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut by_slot = BTreeMap::new();
    for row in rows {
        let Some(slot) = row
            .name
            .strip_prefix(prefix.as_str())
            .filter(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|rest| rest.parse::<u32>().ok())
        else {
            continue;
        };
        by_slot.insert(slot, row);
    }
    Ok(by_slot)
}

/// Takes one permit of `name` for `holder`, valid for `ttl`.
///
/// Tries the free or expired slots below `limit` in order; each attempt is
/// a fenced compare-and-swap, so a concurrent acquirer that wins a slot
/// makes this one move on. `holder` should be unique per piece of work
/// (it is also how a renewal proves ownership).
///
/// Live permits other holders keep on slots at or above `limit` (left over
/// from a higher limit) count against it, so after a limit drop no permit
/// is granted until the live count is under the new limit.
///
/// # Returns
///
/// The permit, or `None` when `limit` live permits are held by others.
///
/// # Errors
///
/// Returns an error when a read or write fails.
pub async fn acquire_permit(
    store: &LibraryStore,
    name: &str,
    limit: u32,
    holder: &str,
    ttl: Duration,
) -> Result<Option<SemaphorePermit>> {
    let now_ms = Utc::now().timestamp_millis();
    let current = load_all_slots(store, name).await?;
    let held_by_others = current
        .values()
        .filter(|row| row.holder != holder && row.expires_at_ms > now_ms)
        .count();
    if held_by_others >= usize::try_from(slots(limit)).unwrap_or(usize::MAX) {
        return Ok(None);
    }
    for slot in 0..slots(limit) {
        if let Some(row) = current.get(&slot) {
            if row.holder != holder && row.expires_at_ms > now_ms {
                continue;
            }
        }
        let lease_name = semaphore_slot_lease_name(name, slot);
        if let Some(lease) = acquire_lease(store, &lease_name, holder, ttl).await? {
            return Ok(Some(SemaphorePermit {
                semaphore: name.to_string(),
                slot,
                lease,
            }));
        }
    }
    Ok(None)
}

/// Takes one permit of every semaphore in `requests`, or none.
///
/// Requests are taken in name order (duplicates merge to the smallest
/// limit). When one semaphore is full, the permits already taken are
/// released and the blocked name is returned.
///
/// # Errors
///
/// Returns an error when a read or write fails; permits taken before the
/// failure are released on a best-effort basis.
pub async fn acquire_permits(
    store: &LibraryStore,
    requests: &[SemaphoreRequest],
    holder: &str,
    ttl: Duration,
) -> Result<PermitAcquire> {
    let mut merged: BTreeMap<&str, u32> = BTreeMap::new();
    for request in requests {
        merged
            .entry(request.name.as_str())
            .and_modify(|limit| *limit = (*limit).min(request.limit))
            .or_insert(request.limit);
    }
    let mut held = Vec::with_capacity(merged.len());
    for (name, limit) in merged {
        match acquire_permit(store, name, limit, holder, ttl).await {
            Ok(Some(permit)) => held.push(permit),
            Ok(None) => {
                release_permits(store, &held).await?;
                return Ok(PermitAcquire::Blocked(name.to_string()));
            }
            Err(err) => {
                let _ = release_permits(store, &held).await;
                return Err(err);
            }
        }
    }
    Ok(PermitAcquire::Granted(held))
}

/// Extends every permit in `permits` by `ttl` while its holder and token
/// still own the slot.
///
/// # Returns
///
/// `false` when any permit was taken over (its fence is lost); the
/// remaining permits are still renewed.
///
/// # Errors
///
/// Returns an error when a write fails.
pub async fn renew_permits(
    store: &LibraryStore,
    permits: &[SemaphorePermit],
    ttl: Duration,
) -> Result<bool> {
    let mut all = true;
    for permit in permits {
        all &= renew_fenced_lease(store, &permit.lease, ttl).await?;
    }
    Ok(all)
}

/// Gives back every permit in `permits` so the slots are free at once.
///
/// A permit whose fence was lost is left alone (the release matches no row).
///
/// # Errors
///
/// Returns an error when a write fails.
pub async fn release_permits(store: &LibraryStore, permits: &[SemaphorePermit]) -> Result<()> {
    for permit in permits {
        release_lease(store, &permit.lease).await?;
    }
    Ok(())
}

/// Live permits of `name` among its first `max_slots` slots (expired slots
/// are not counted).
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn live_permits(
    store: &LibraryStore,
    name: &str,
    max_slots: u32,
) -> Result<Vec<SemaphorePermit>> {
    let now_ms = Utc::now().timestamp_millis();
    Ok(load_slots(store, name, max_slots)
        .await?
        .into_iter()
        .filter(|(_, row)| row.expires_at_ms > now_ms)
        .map(|(slot, row)| SemaphorePermit {
            semaphore: name.to_string(),
            slot,
            lease: ClusterLease {
                name: row.name,
                holder: row.holder,
                token: row.token,
                expires_at_ms: row.expires_at_ms,
            },
        })
        .collect())
}

/// Permits of `name` among its first `max_slots` slots whose holder starts
/// with `holder_prefix`, expired ones included.
///
/// A worker that holds permits only by name (a host that reconnected to
/// another owner node) finds them again with this. An expired slot nobody
/// took over is still returned, since [`renew_fenced_lease`] still renews
/// it; a slot another holder took is not.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn permits_held_by(
    store: &LibraryStore,
    name: &str,
    max_slots: u32,
    holder_prefix: &str,
) -> Result<Vec<SemaphorePermit>> {
    Ok(load_slots(store, name, max_slots)
        .await?
        .into_iter()
        .filter(|(_, row)| row.holder.starts_with(holder_prefix))
        .map(|(slot, row)| SemaphorePermit {
            semaphore: name.to_string(),
            slot,
            lease: ClusterLease {
                name: row.name,
                holder: row.holder,
                token: row.token,
                expires_at_ms: row.expires_at_ms,
            },
        })
        .collect())
}

/// Extends `lease` by `ttl` only while the row still carries its holder and
/// token. Unlike [`acquire_lease`], it never takes a lease over.
///
/// A lease that has expired but that nobody has taken is still renewed: the
/// unchanged token proves no other holder ran in between.
///
/// # Returns
///
/// `false` when another holder took the lease (the fence is lost).
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn renew_fenced_lease(
    store: &LibraryStore,
    lease: &ClusterLease,
    ttl: Duration,
) -> Result<bool> {
    let now = Utc::now();
    let expires_at_ms = now
        .timestamp_millis()
        .saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX));
    let res = cluster_leases::Entity::update_many()
        .col_expr(
            cluster_leases::Column::ExpiresAtMs,
            Expr::value(expires_at_ms),
        )
        .col_expr(
            cluster_leases::Column::RenewedAt,
            Expr::value(now.to_rfc3339()),
        )
        .filter(cluster_leases::Column::Name.eq(lease.name.as_str()))
        .filter(cluster_leases::Column::Holder.eq(lease.holder.as_str()))
        .filter(cluster_leases::Column::Token.eq(lease.token))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(res.rows_affected == 1)
}
