//! Named cluster leases: singletons over the shared database.
//!
//! A lease is one `cluster_leases` row: a holder (a node id, or a process
//! id for schema migration), a fencing token, and an expiry in Unix
//! milliseconds. Acquiring a free or expired lease bumps the token;
//! renewing a held one keeps it. Every write is a compare-and-swap on
//! `(name, holder, token)`, so two contenders never both win.
//!
//! A lease only says who *should* act. A holder that stalls past its expiry
//! can still be running, so destructive work checks the lease first with
//! [`renew_lease`] (or a [`LeaseFence`]): that write succeeds only while the
//! row still names this holder and token and has not expired, and it
//! extends the expiry. A holder that lost the lease is refused there.
//!
//! [`SingletonDuty`] turns a lease into "one live node runs this every
//! interval": every daemon ticks it, only the holder is told to run, the
//! holder reruns once its interval has passed, and a new holder runs at
//! once. A dead holder's lease expires and the next node to tick takes it.
//!
//! Expiry is compared against each node's wall clock; nodes are assumed to
//! keep their clocks within a small fraction of the lease TTL.

use std::time::Duration;

use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder,
};

use crate::entities::cluster_leases;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Lease of the job retention duty (terminal `jobs` rows).
pub const JOB_RETENTION_LEASE: &str = "maintenance.jobs";
/// Lease of the event retention duty (terminal deliveries, empty events,
/// stale subscriber catalog rows).
pub const EVENT_RETENTION_LEASE: &str = "maintenance.events";
/// Lease of the plugin KV expiry duty.
pub const KV_EXPIRY_LEASE: &str = "maintenance.kv";
/// Lease of the session duty (expired operator sessions, OIDC RP states,
/// WebAuthn challenges).
pub const SESSION_EXPIRY_LEASE: &str = "maintenance.sessions";
/// Lease a schema migration runs under (see
/// [`crate::apply_host_schema_with_batch_opts`]).
pub const MIGRATION_LEASE: &str = "schema.migrate";

/// A held cluster lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterLease {
    /// Lease name.
    pub name: String,
    /// Holder (a node id).
    pub holder: String,
    /// Fencing token; a new holder always gets a larger one.
    pub token: i64,
    /// Expiry in Unix milliseconds.
    pub expires_at_ms: i64,
}

impl ClusterLease {
    /// True when the lease has not expired at `now_ms`.
    #[must_use]
    pub fn is_held_at(&self, now_ms: i64) -> bool {
        self.expires_at_ms > now_ms
    }
}

/// Unix milliseconds `ttl` from now.
fn expiry_after(ttl: Duration) -> i64 {
    Utc::now()
        .timestamp_millis()
        .saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX))
}

/// Acquires or renews `name` for `holder` for `ttl`.
///
/// Returns `None` when another holder's lease has not expired, or when a
/// concurrent acquire won. Renewing keeps the token; taking over an expired
/// lease bumps it.
///
/// # Errors
///
/// Returns an error when the read or write fails.
pub async fn acquire_lease(
    store: &LibraryStore,
    name: &str,
    holder: &str,
    ttl: Duration,
) -> Result<Option<ClusterLease>> {
    acquire_lease_on(store.db(), name, holder, ttl).await
}

/// [`acquire_lease`] on a bare connection (schema migration runs before a
/// [`LibraryStore`] exists).
///
/// # Errors
///
/// Returns an error when the read or write fails.
pub async fn acquire_lease_on<C: ConnectionTrait>(
    db: &C,
    name: &str,
    holder: &str,
    ttl: Duration,
) -> Result<Option<ClusterLease>> {
    let now_ms = Utc::now().timestamp_millis();
    let expires_at_ms = expiry_after(ttl);
    let renewed_at = Utc::now().to_rfc3339();
    let lease = |token| ClusterLease {
        name: name.to_string(),
        holder: holder.to_string(),
        token,
        expires_at_ms,
    };
    let Some(row) = cluster_leases::Entity::find_by_id(name.to_string())
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
    else {
        let model = cluster_leases::ActiveModel {
            name: Set(name.to_string()),
            holder: Set(holder.to_string()),
            token: Set(1),
            expires_at_ms: Set(expires_at_ms),
            renewed_at: Set(renewed_at),
        };
        return match cluster_leases::Entity::insert(model).exec(db).await {
            Ok(_) => Ok(Some(lease(1))),
            // A concurrent first acquire inserted the row; this one lost.
            Err(err) => match load_lease_on(db, name).await? {
                Some(_) => Ok(None),
                None => Err(LibraryError::Orm(err)),
            },
        };
    };
    let token = if row.holder == holder {
        row.token
    } else if row.expires_at_ms <= now_ms {
        row.token + 1
    } else {
        return Ok(None);
    };
    let res = cluster_leases::Entity::update_many()
        .col_expr(
            cluster_leases::Column::Holder,
            Expr::value(holder.to_string()),
        )
        .col_expr(cluster_leases::Column::Token, Expr::value(token))
        .col_expr(
            cluster_leases::Column::ExpiresAtMs,
            Expr::value(expires_at_ms),
        )
        .col_expr(cluster_leases::Column::RenewedAt, Expr::value(renewed_at))
        .filter(cluster_leases::Column::Name.eq(name))
        .filter(cluster_leases::Column::Holder.eq(row.holder))
        .filter(cluster_leases::Column::Token.eq(row.token))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok((res.rows_affected == 1).then(|| lease(token)))
}

/// Fencing check: extends a lease this holder still holds.
///
/// Succeeds only while the row names `lease.holder` and `lease.token` and
/// has not expired. Returns the extended lease, or `None` when the lease
/// expired or another holder took it; the caller must then stop before any
/// further destructive work.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn renew_lease(
    store: &LibraryStore,
    lease: &ClusterLease,
    ttl: Duration,
) -> Result<Option<ClusterLease>> {
    renew_lease_on(store.db(), lease, ttl).await
}

/// [`renew_lease`] on a bare connection.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn renew_lease_on<C: ConnectionTrait>(
    db: &C,
    lease: &ClusterLease,
    ttl: Duration,
) -> Result<Option<ClusterLease>> {
    let now_ms = Utc::now().timestamp_millis();
    let expires_at_ms = expiry_after(ttl);
    let res = cluster_leases::Entity::update_many()
        .col_expr(
            cluster_leases::Column::ExpiresAtMs,
            Expr::value(expires_at_ms),
        )
        .col_expr(
            cluster_leases::Column::RenewedAt,
            Expr::value(Utc::now().to_rfc3339()),
        )
        .filter(cluster_leases::Column::Name.eq(lease.name.as_str()))
        .filter(cluster_leases::Column::Holder.eq(lease.holder.as_str()))
        .filter(cluster_leases::Column::Token.eq(lease.token))
        .filter(cluster_leases::Column::ExpiresAtMs.gt(now_ms))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok((res.rows_affected == 1).then(|| ClusterLease {
        expires_at_ms,
        ..lease.clone()
    }))
}

/// Gives up a held lease so another holder may take it at once.
///
/// The row stays (holder and token remain visible to status reads) with an
/// expiry of zero.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn release_lease(store: &LibraryStore, lease: &ClusterLease) -> Result<()> {
    release_lease_on(store.db(), lease).await
}

/// [`release_lease`] on a bare connection.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn release_lease_on<C: ConnectionTrait>(db: &C, lease: &ClusterLease) -> Result<()> {
    cluster_leases::Entity::update_many()
        .col_expr(cluster_leases::Column::ExpiresAtMs, Expr::value(0_i64))
        .filter(cluster_leases::Column::Name.eq(lease.name.as_str()))
        .filter(cluster_leases::Column::Holder.eq(lease.holder.as_str()))
        .filter(cluster_leases::Column::Token.eq(lease.token))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// The current row of lease `name`, held or expired.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_lease(store: &LibraryStore, name: &str) -> Result<Option<ClusterLease>> {
    load_lease_on(store.db(), name).await
}

/// [`load_lease`] on a bare connection.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_lease_on<C: ConnectionTrait>(db: &C, name: &str) -> Result<Option<ClusterLease>> {
    Ok(cluster_leases::Entity::find_by_id(name.to_string())
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
        .map(lease_from_row))
}

/// Every lease row, held or expired, by name.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_leases(store: &LibraryStore) -> Result<Vec<ClusterLease>> {
    Ok(cluster_leases::Entity::find()
        .order_by_asc(cluster_leases::Column::Name)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(lease_from_row)
        .collect())
}

/// Maps a `cluster_leases` row to a [`ClusterLease`].
fn lease_from_row(row: cluster_leases::Model) -> ClusterLease {
    ClusterLease {
        name: row.name,
        holder: row.holder,
        token: row.token,
        expires_at_ms: row.expires_at_ms,
    }
}

/// Proof, checked against the database, that this holder still holds a
/// lease. Call [`LeaseFence::check`] before each destructive step.
#[derive(Debug)]
pub struct LeaseFence<'a> {
    /// Store the lease lives in.
    store: &'a LibraryStore,
    /// The lease as of the last successful check.
    lease: ClusterLease,
    /// How far each check extends the lease.
    ttl: Duration,
}

impl<'a> LeaseFence<'a> {
    /// A fence for a lease acquired with `ttl`.
    #[must_use]
    pub fn new(store: &'a LibraryStore, lease: ClusterLease, ttl: Duration) -> Self {
        Self { store, lease, ttl }
    }

    /// The lease as of the last successful check.
    #[must_use]
    pub fn lease(&self) -> &ClusterLease {
        &self.lease
    }

    /// Renews the lease, or fails when this holder no longer holds it.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Conflict`] when the lease expired or another
    /// holder took it, or the write error.
    pub async fn check(&mut self) -> Result<()> {
        match renew_lease(self.store, &self.lease, self.ttl).await? {
            Some(lease) => {
                self.lease = lease;
                Ok(())
            }
            None => Err(LibraryError::Conflict(format!(
                "lease `{}` (token {}) is no longer held by `{}`",
                self.lease.name, self.lease.token, self.lease.holder
            ))),
        }
    }
}

/// One periodic duty that exactly one live node runs per interval.
///
/// Every node keeps one per duty and calls [`SingletonDuty::poll`] more
/// often than the lease TTL. `poll` acquires or renews the lease and hands
/// back a [`LeaseFence`] only when this node holds it and the duty is due:
/// at once after taking the lease, then every `interval`.
#[derive(Debug, Clone)]
pub struct SingletonDuty {
    /// Lease the duty runs under.
    lease_name: String,
    /// This node's holder id.
    holder: String,
    /// Lease TTL; poll more often than this.
    ttl: Duration,
    /// Time between runs on the holder.
    interval: Duration,
    /// Token of the lease this node last held, if it holds it.
    held_token: Option<i64>,
    /// When this node last ran the duty under `held_token`.
    last_run_ms: Option<i64>,
}

impl SingletonDuty {
    /// A duty under lease `lease_name` for `holder`.
    #[must_use]
    pub fn new(
        lease_name: impl Into<String>,
        holder: impl Into<String>,
        ttl: Duration,
        interval: Duration,
    ) -> Self {
        Self {
            lease_name: lease_name.into(),
            holder: holder.into(),
            ttl,
            interval,
            held_token: None,
            last_run_ms: None,
        }
    }

    /// The lease name.
    #[must_use]
    pub fn lease_name(&self) -> &str {
        &self.lease_name
    }

    /// Takes or renews the lease; returns a fence when the duty should run
    /// now on this node.
    ///
    /// # Errors
    ///
    /// Returns an error when the lease read or write fails.
    pub async fn poll<'a>(&mut self, store: &'a LibraryStore) -> Result<Option<LeaseFence<'a>>> {
        let Some(lease) = acquire_lease(store, &self.lease_name, &self.holder, self.ttl).await?
        else {
            self.held_token = None;
            self.last_run_ms = None;
            return Ok(None);
        };
        if self.held_token != Some(lease.token) {
            // Newly taken (or retaken after losing it): run at once.
            self.held_token = Some(lease.token);
            self.last_run_ms = None;
        }
        let now_ms = Utc::now().timestamp_millis();
        let interval_ms = i64::try_from(self.interval.as_millis()).unwrap_or(i64::MAX);
        let due = self
            .last_run_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= interval_ms);
        if !due {
            return Ok(None);
        }
        Ok(Some(LeaseFence::new(store, lease, self.ttl)))
    }

    /// Records that the duty ran (successfully or not) so the next run waits
    /// a full interval.
    pub fn mark_ran(&mut self) {
        self.last_run_ms = Some(Utc::now().timestamp_millis());
    }
}
