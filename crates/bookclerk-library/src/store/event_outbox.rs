//! Durable domain-event outbox and per-subscriber deliveries.

#![allow(clippy::missing_docs_in_private_items)]

use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::anyhow;
use chrono::{Duration, Utc};
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::{NotSet, Set},
    ColumnTrait, Condition, ConnectionTrait, DatabaseBackend, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use uuid::Uuid;

use super::{now_str, parse_dt, parse_dt_opt, LibraryStore};
use crate::entities::{
    books, domain_events, event_deliveries, event_outbox_stats, event_subscriber_nodes,
};
use crate::error::{LibraryError, Result};
use crate::models::{
    catalog_subscribers_for_event, collapse_live_subscriber_nodes, event_matches_wake_grants,
    job_backoff_run_after, subscription_matches_event, AcquireStatus, DomainEventRecord,
    EventCatalogSubscription, EventDeliveryFence, EventDeliveryMetrics, EventDeliveryRecord,
    EventSubscriber, EventSubscriberCatalogRecord, EventSubscriberNodeRecord, PendingWakeProgress,
    PublishDomainEventOutcome, PublishDomainEventSpec, EVENT_DELIVERY_MAX_ATTEMPTS,
    EVENT_RESOURCE_CLASS_NETWORK, EVENT_SUBSCRIBER_HEARTBEAT_TTL_SECS,
};

const STATE_PENDING: &str = "pending";
const STATE_DISPATCHED: &str = "dispatched";
const STATE_RUNNING: &str = "running";
const STATE_ACKED: &str = "acked";
const STATE_REJECTED: &str = "rejected";
const STATE_DEAD_LETTER: &str = "dead_letter";
const RECONCILE_PAGE: u64 = 200;
/// Pending-delivery claim page. Tests may override via [`set_claim_page_for_test`].
const EVENT_CLAIM_PAGE: u64 = 64;

#[cfg(test)]
thread_local! {
    static CLAIM_PAGE_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// Override the claim candidate page size (library tests only).
#[cfg(test)]
pub(crate) fn set_claim_page_for_test(page: Option<u64>) {
    CLAIM_PAGE_OVERRIDE.with(|c| c.set(page));
}

fn claim_page_size() -> u64 {
    #[cfg(test)]
    {
        if let Some(n) = CLAIM_PAGE_OVERRIDE.with(std::cell::Cell::get) {
            return n.max(1);
        }
    }
    EVENT_CLAIM_PAGE
}

/// Keyset after `(created_at, id)` so concurrent claims cannot skip rows via OFFSET.
fn claim_keyset_after(created_at: &str, id: &str) -> Condition {
    Condition::any()
        .add(event_deliveries::Column::CreatedAt.gt(created_at.to_string()))
        .add(
            Condition::all()
                .add(event_deliveries::Column::CreatedAt.eq(created_at.to_string()))
                .add(event_deliveries::Column::Id.gt(id.to_string())),
        )
}
/// SET `run_after` / `updated_at` plus EXISTS `event_id` / `wake_lease_owner`.
const WAKE_UPDATE_FIXED_BINDS: usize = 4;
const EVENT_OUTBOX_STATS_ID: i64 = 1;
const EVENT_WAKE_FAR_FUTURE: &str = "9999-12-31T23:59:59+00:00";

/// Rewrite SQLite `?` placeholders to Postgres `$1`…`$n` (sqlx does not).
fn rewrite_sql_placeholders(backend: DatabaseBackend, sql: &str) -> String {
    if backend != DatabaseBackend::Postgres {
        return sql.to_string();
    }
    let mut n = 0u32;
    let mut out = String::with_capacity(sql.len() + 16);
    for ch in sql.chars() {
        if ch == '?' {
            n += 1;
            out.push('$');
            out.push_str(&n.to_string());
        } else {
            out.push(ch);
        }
    }
    out
}

fn statement_for_backend(
    backend: DatabaseBackend,
    sql: impl Into<String>,
    values: impl IntoIterator<Item = sea_orm::Value>,
) -> Statement {
    Statement::from_sql_and_values(
        backend,
        rewrite_sql_placeholders(backend, &sql.into()),
        values,
    )
}

/// Fenced sleeper UPDATE: one statement so a stale owner cannot clear a later registration.
fn wake_fenced_update_sql(id_count: usize) -> String {
    let placeholders = vec!["?"; id_count].join(", ");
    format!(
        "UPDATE event_deliveries SET \
            run_after = ?, resume_pending = 1, \
            wake_event_type = '', wake_filter_json = '', wake_grants_json = '', \
            updated_at = ? \
         WHERE id IN ({placeholders}) \
           AND state = 'pending' \
           AND EXISTS ( \
             SELECT 1 FROM domain_events \
             WHERE id = ? AND wake_pending = 1 AND wake_lease_owner = ? \
           )"
    )
}

fn wake_delivery_update_bind_count(id_count: usize) -> usize {
    WAKE_UPDATE_FIXED_BINDS + id_count
}

fn wake_in_chunk_size(max_binds: usize) -> usize {
    max_binds.saturating_sub(WAKE_UPDATE_FIXED_BINDS).max(1)
}

/// Wake matching pending deliveries when `owner` still holds the event's wake lease.
pub(crate) async fn wake_deliveries_fenced_on<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    owner: &str,
    ids: &[String],
    now: &str,
    max_binds: usize,
) -> Result<u32> {
    if ids.is_empty() {
        return Ok(0);
    }
    let backend = db.get_database_backend();
    let chunk_size = wake_in_chunk_size(max_binds);
    let mut woken = 0u32;
    for chunk in ids.chunks(chunk_size) {
        let sql = wake_fenced_update_sql(chunk.len());
        let mut values = Vec::with_capacity(wake_delivery_update_bind_count(chunk.len()));
        values.push(now.to_string().into());
        values.push(now.to_string().into());
        for id in chunk {
            values.push(id.clone().into());
        }
        values.push(event_id.to_string().into());
        values.push(owner.to_string().into());
        let res = db
            .execute_raw(statement_for_backend(backend, sql, values))
            .await
            .map_err(LibraryError::Orm)?;
        woken = woken.saturating_add(u32::try_from(res.rows_affected()).unwrap_or(0));
    }
    Ok(woken)
}

thread_local! {
    static DISPATCH_EVENT_CALLS: AtomicU32 = const { AtomicU32::new(0) };
    static DISPATCH_PAGE_FAULTS: AtomicU32 = const { AtomicU32::new(0) };
    static DISPATCH_CHUNK_OVERRIDE: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
    /// Remaining injected `publish_domain_event_on` failures (library tests only).
    static PUBLISH_FAULTS: AtomicU32 = const { AtomicU32::new(0) };
}

#[cfg(test)]
static SNAPSHOT_CLAIM_BARRIER: std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Barrier>>> =
    std::sync::Mutex::new(None);

/// Fail the next `n` outbox inserts (used to prove acquire+publish rollback).
#[cfg(test)]
pub(crate) fn inject_event_publish_failures(n: u32) {
    PUBLISH_FAULTS.with(|c| c.store(n, Ordering::SeqCst));
}

/// Take and reset the dispatch-call counter (library tests only).
#[cfg(test)]
pub(crate) fn take_dispatch_event_calls() -> u32 {
    DISPATCH_EVENT_CALLS.with(|c| c.swap(0, Ordering::SeqCst))
}

/// Fail after the next `n` successful dispatch pages (tests only).
pub fn inject_dispatch_page_failures(n: u32) {
    DISPATCH_PAGE_FAULTS.with(|c| c.store(n, Ordering::SeqCst));
}

/// Override subscribers-per-plan for dispatch paging tests.
pub fn set_dispatch_chunk_for_test(chunk: Option<usize>) {
    DISPATCH_CHUNK_OVERRIDE.with(|c| c.set(chunk));
}

/// Two-store tests wait here after observing an empty snapshot, before CAS.
#[cfg(test)]
pub(crate) fn set_snapshot_claim_barrier(barrier: Option<std::sync::Arc<tokio::sync::Barrier>>) {
    *SNAPSHOT_CLAIM_BARRIER
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = barrier;
}

#[cfg(test)]
tokio::task_local! {
    static SNAPSHOT_CLAIM_WAIT: ();
}

/// Runs `fut` as a snapshot-CAS participant.
///
/// The rendezvous barrier is process-global so two multi-thread tasks can meet,
/// but every dispatch calls [`wait_snapshot_claim_barrier`]. Only tasks inside
/// this scope wait; unrelated tests skip it so the suite cannot deadlock.
#[cfg(test)]
pub(crate) async fn with_snapshot_claim_wait<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    SNAPSHOT_CLAIM_WAIT.scope((), fut).await
}

#[cfg(test)]
async fn wait_snapshot_claim_barrier() {
    if SNAPSHOT_CLAIM_WAIT.try_with(|_| ()).is_err() {
        return;
    }
    let barrier = SNAPSHOT_CLAIM_BARRIER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(b) = barrier {
        b.wait().await;
    }
}

#[cfg(not(test))]
async fn wait_snapshot_claim_barrier() {}

fn parse_dispatch_snapshot(event_id: &str, json: &str) -> Result<Vec<EventSubscriber>> {
    let ids: Vec<String> = serde_json::from_str(json).map_err(|err| {
        LibraryError::Other(anyhow!(
            "domain event {event_id} has invalid dispatch_snapshot_json: {err}"
        ))
    })?;
    Ok(ids.into_iter().map(EventSubscriber::plugin).collect())
}

/// Current dispatch chunk override, if any.
pub(crate) fn dispatch_chunk_override() -> Option<usize> {
    DISPATCH_CHUNK_OVERRIDE.with(|c| c.get())
}

fn take_dispatch_page_fault() -> bool {
    DISPATCH_PAGE_FAULTS.with(|c| {
        let n = c.load(Ordering::SeqCst);
        if n == 0 {
            false
        } else {
            c.store(n - 1, Ordering::SeqCst);
            true
        }
    })
}

fn take_publish_fault() -> bool {
    PUBLISH_FAULTS.with(|c| {
        let n = c.load(Ordering::SeqCst);
        if n == 0 {
            false
        } else {
            c.store(n - 1, Ordering::SeqCst);
            true
        }
    })
}

impl LibraryStore {
    /// Persist a domain event. Duplicate `(account_id, source, event_type, dedup_key)`
    /// coalesces. Wake processing is left to [`Self::process_pending_wakes`].
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails, or
    /// [`LibraryError::Other`] when the payload exceeds 64 KiB.
    pub async fn publish_domain_event(
        &self,
        spec: PublishDomainEventSpec,
    ) -> Result<PublishDomainEventOutcome> {
        let spec = prepare_publish_domain_event(spec)?;
        if let Some(atomic) = &self.atomic {
            return atomic.publish_domain_event(spec).await;
        }
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        match publish_domain_event_on(&txn, spec).await {
            Ok(outcome) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                Ok(outcome)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// Snapshot matching subscribers and create one delivery row per plugin.
    ///
    /// Idempotent under replay: existing `(event_id, plugin_id)` rows are kept.
    /// Marks the outbox row `dispatched` in the same transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    pub async fn dispatch_event_deliveries(
        &self,
        event_id: &str,
        subscribers: &[EventSubscriber],
        operation_id: &str,
    ) -> Result<u32> {
        DISPATCH_EVENT_CALLS.with(|c| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        if let Some(atomic) = &self.atomic {
            let snapshot = self
                .load_or_init_dispatch_snapshot(event_id, subscribers)
                .await?;
            let chunk = self.dispatch_chunk_size().max(1);
            if snapshot.is_empty() {
                return atomic
                    .dispatch_event_deliveries(event_id, &[], operation_id, true)
                    .await;
            }
            let mut total = 0u32;
            let mut start = 0usize;
            let mut page = 0usize;
            while start < snapshot.len() {
                let end = (start + chunk).min(snapshot.len());
                let last = end == snapshot.len();
                let op = format!("{operation_id}:p{page}");
                total += atomic
                    .dispatch_event_deliveries(event_id, &snapshot[start..end], &op, last)
                    .await?;
                if take_dispatch_page_fault() {
                    return Err(LibraryError::Other(anyhow!(
                        "injected dispatch page failure after page {page}"
                    )));
                }
                start = end;
                page += 1;
            }
            return Ok(total);
        }
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        match dispatch_event_deliveries_on(&txn, event_id, subscribers).await {
            Ok(n) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                Ok(n)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// Returns the frozen dispatch subscriber snapshot, writing it on first use.
    async fn load_or_init_dispatch_snapshot(
        &self,
        event_id: &str,
        subscribers: &[EventSubscriber],
    ) -> Result<Vec<EventSubscriber>> {
        let row = domain_events::Entity::find_by_id(event_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("domain event {event_id}")))?;
        if !row.dispatch_snapshot_json.trim().is_empty() {
            return parse_dispatch_snapshot(event_id, &row.dispatch_snapshot_json);
        }
        let json = serde_json::to_string(
            &subscribers
                .iter()
                .map(|s| s.plugin_id.as_str())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into());
        wait_snapshot_claim_barrier().await;
        let cas = domain_events::Entity::update_many()
            .col_expr(
                domain_events::Column::DispatchSnapshotJson,
                sea_orm::sea_query::Expr::value(json.clone()),
            )
            .filter(domain_events::Column::Id.eq(event_id))
            .filter(domain_events::Column::DispatchSnapshotJson.eq(""))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if cas.rows_affected == 0 {
            let row = domain_events::Entity::find_by_id(event_id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
                .ok_or_else(|| LibraryError::NotFound(format!("domain event {event_id}")))?;
            return parse_dispatch_snapshot(event_id, &row.dispatch_snapshot_json);
        }
        Ok(subscribers.to_vec())
    }

    /// Next undispatched outbox row (oldest first).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn next_undispatched_event(&self) -> Result<Option<DomainEventRecord>> {
        let row = domain_events::Entity::find()
            .filter(domain_events::Column::DispatchState.eq(STATE_PENDING))
            .order_by_asc(domain_events::Column::CreatedAt)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        row.map(map_event).transpose()
    }

    /// Claim up to `limit` `wake_pending` events and process one delivery page each.
    ///
    /// Each claimed event is leased with a unique UUID token so two processors
    /// cannot own the same slice (process `node_id` is not the fence — it is ABA
    /// across restarts). Cursor release and finish require that token.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read or wake write fails.
    pub async fn process_pending_wakes(
        &self,
        limit: u64,
        _owner: &str,
        lease_secs: u64,
    ) -> Result<PendingWakeProgress> {
        let page = limit.max(1);
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let lease_expires =
            (now + Duration::seconds(i64::try_from(lease_secs).unwrap_or(60))).to_rfc3339();
        let candidates = domain_events::Entity::find()
            .filter(domain_events::Column::WakePending.eq(1i64))
            .filter(
                Condition::any()
                    .add(domain_events::Column::WakeLeaseExpiresAt.is_null())
                    .add(domain_events::Column::WakeLeaseExpiresAt.eq(""))
                    .add(domain_events::Column::WakeLeaseExpiresAt.lte(now_s.clone())),
            )
            .order_by_asc(domain_events::Column::CreatedAt)
            .order_by_asc(domain_events::Column::Id)
            .limit(page)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut still_pending = u64::try_from(candidates.len()).unwrap_or(0) >= page;
        let mut claimed = 0u32;
        for row in candidates {
            let token = Uuid::new_v4().to_string();
            let cas = domain_events::Entity::update_many()
                .col_expr(
                    domain_events::Column::WakeLeaseOwner,
                    sea_orm::sea_query::Expr::value(Some(token.clone())),
                )
                .col_expr(
                    domain_events::Column::WakeLeaseExpiresAt,
                    sea_orm::sea_query::Expr::value(Some(lease_expires.clone())),
                )
                .filter(domain_events::Column::Id.eq(&row.id))
                .filter(domain_events::Column::WakePending.eq(1i64))
                .filter(
                    Condition::any()
                        .add(domain_events::Column::WakeLeaseExpiresAt.is_null())
                        .add(domain_events::Column::WakeLeaseExpiresAt.eq(""))
                        .add(domain_events::Column::WakeLeaseExpiresAt.lte(now_s.clone())),
                )
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if cas.rows_affected != 1 {
                continue;
            }
            claimed = claimed.saturating_add(1);
            let Some(fresh) = domain_events::Entity::find_by_id(&row.id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            else {
                continue;
            };
            self.wake_one_claimed_page(&fresh, &token).await?;
            if let Some(after) = domain_events::Entity::find_by_id(&row.id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                if after.wake_pending != 0 {
                    still_pending = true;
                }
            }
        }
        Ok(PendingWakeProgress {
            claimed,
            still_pending,
        })
    }

    async fn wake_one_claimed_page(&self, row: &domain_events::Model, owner: &str) -> Result<u32> {
        let page = self.wake_page();
        let max_binds =
            usize::try_from(self.max_binds).unwrap_or(bookclerk_plugin_abi::D1_MAX_BINDS as usize);
        if self.atomic.is_some() {
            return wake_one_page_on(&self.db, row, owner, page, max_binds).await;
        }
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        match wake_one_page_on(&txn, row, owner, page, max_binds).await {
            Ok(n) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                Ok(n)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// Claim the next ready delivery with a fenced `pending` → `running` mutation.
    ///
    /// When `node_id` is non-empty, this node’s own catalog must match the parent
    /// event’s type, schema version, and filter. The host loads the catalog and
    /// filters candidates in Rust, then CAS-updates one row (portable slot +
    /// `UPDATE … WHERE id = ? AND state = pending`). An empty `node_id` keeps
    /// plugin-id-only filtering (tests).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the claim write fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn claim_next_event_delivery(
        &self,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
        plugin_ids: &[String],
        max_in_flight: u32,
        node_id: &str,
    ) -> Result<Option<EventDeliveryRecord>> {
        if plugin_ids.is_empty() {
            return Ok(None);
        }
        if self.atomic.is_some() {
            return self
                .claim_next_event_delivery_host_cas(
                    owner,
                    lease_secs,
                    operation_id,
                    plugin_ids,
                    max_in_flight,
                    node_id,
                )
                .await;
        }
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        match claim_next_event_delivery_on(
            &txn,
            owner,
            lease_secs,
            plugin_ids,
            max_in_flight,
            node_id,
        )
        .await
        {
            Ok(row) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                Ok(row)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// Host eligibility + per-row CAS through the guest atomic backend.
    async fn claim_next_event_delivery_host_cas(
        &self,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
        plugin_ids: &[String],
        max_in_flight: u32,
        node_id: &str,
    ) -> Result<Option<EventDeliveryRecord>> {
        let Some(atomic) = &self.atomic else {
            return Ok(None);
        };
        let page = claim_page_size();
        let now_s = Utc::now().to_rfc3339();
        sanitize_unknown_event_resource_class_on(&self.db, &now_s).await?;
        let mut cursor: Option<(String, String)> = None;
        loop {
            let mut query = event_deliveries::Entity::find()
                .filter(event_deliveries::Column::State.eq(STATE_PENDING))
                .filter(event_deliveries::Column::RunAfter.lte(now_s.clone()))
                .filter(event_deliveries::Column::PluginId.is_in(plugin_ids.to_vec()))
                .filter(event_deliveries::Column::ResourceClass.eq(EVENT_RESOURCE_CLASS_NETWORK));
            if let Some((created_at, id)) = &cursor {
                query = query.filter(claim_keyset_after(created_at, id));
            }
            let candidates = query
                .order_by_asc(event_deliveries::Column::CreatedAt)
                .order_by_asc(event_deliveries::Column::Id)
                .limit(page)
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if candidates.is_empty() {
                return Ok(None);
            }
            cursor = candidates
                .last()
                .map(|m| (m.created_at.clone(), m.id.clone()));
            for model in candidates {
                if fifo_blocked(&self.db, &model).await? {
                    continue;
                }
                if !node_id.trim().is_empty()
                    && !delivery_model_matches_node_catalog(&self.db, node_id, &model).await?
                {
                    continue;
                }
                let op = format!("{operation_id}:{}", model.id);
                if let Some(row) = atomic
                    .claim_event_delivery(
                        &model.id,
                        owner,
                        lease_secs,
                        &op,
                        &model.plugin_id,
                        &model.resource_class,
                        max_in_flight,
                    )
                    .await?
                {
                    return Ok(Some(row));
                }
            }
        }
    }

    /// Release a claimed row when this node’s catalog cannot handle it.
    ///
    /// Kept for tests that exercise catalog mismatch after a local SeaORM claim.
    /// Production claim filters eligibility in the host before CAS.
    #[cfg(test)]
    pub(crate) async fn release_if_incompatible_local_catalog(
        &self,
        node_id: &str,
        claimed: Option<EventDeliveryRecord>,
    ) -> Result<Option<EventDeliveryRecord>> {
        let Some(row) = claimed else {
            return Ok(None);
        };
        if node_id.trim().is_empty() {
            return Ok(Some(row));
        }
        if self.delivery_matches_node_catalog(node_id, &row).await? {
            return Ok(Some(row));
        }
        let restore_resume = row.checkpoint_json.is_some();
        self.release_unexecuted_event_delivery(&row.fence(), restore_resume)
            .await?;
        Ok(None)
    }

    #[cfg(test)]
    async fn delivery_matches_node_catalog(
        &self,
        node_id: &str,
        row: &EventDeliveryRecord,
    ) -> Result<bool> {
        let Some(event) = self.get_domain_event(&row.event_id).await? else {
            return Ok(false);
        };
        let Some(subs) = node_plugin_subscriptions(&self.db, node_id, &row.plugin_id).await? else {
            return Ok(false);
        };
        Ok(subs.iter().any(|s| subscription_matches_event(s, &event)))
    }

    /// Return a claimed delivery to `pending` without consuming an attempt.
    ///
    /// Used when this worker cannot execute the subscriber (plugin not loaded).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn release_unexecuted_event_delivery(
        &self,
        fence: &EventDeliveryFence,
        restore_resume: bool,
    ) -> Result<bool> {
        let now = now_str();
        let mut update = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::State,
                sea_orm::sea_query::Expr::value(STATE_PENDING),
            )
            .col_expr(
                event_deliveries::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            );
        if restore_resume {
            update = update.col_expr(
                event_deliveries::Column::ResumePending,
                sea_orm::sea_query::Expr::value(1i64),
            );
        } else {
            update = update.col_expr(
                event_deliveries::Column::AttemptCount,
                sea_orm::sea_query::Expr::cust(
                    "CASE WHEN attempt_count > 0 THEN attempt_count - 1 ELSE 0 END",
                ),
            );
        }
        let res = update
            .filter(event_deliveries::Column::Id.eq(&fence.delivery_id))
            .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
            .filter(event_deliveries::Column::LeaseOwner.eq(&fence.owner))
            .filter(event_deliveries::Column::LeaseGeneration.eq(fence.generation))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Refresh the lease when `fence` still owns the running generation.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn heartbeat_event_delivery(
        &self,
        fence: &EventDeliveryFence,
        lease_secs: u64,
    ) -> Result<bool> {
        let now = Utc::now();
        let res = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Some(
                    (now + Duration::seconds(i64::try_from(lease_secs).unwrap_or(60))).to_rfc3339(),
                )),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now.to_rfc3339()),
            )
            .filter(event_deliveries::Column::Id.eq(&fence.delivery_id))
            .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
            .filter(event_deliveries::Column::LeaseOwner.eq(&fence.owner))
            .filter(event_deliveries::Column::LeaseGeneration.eq(fence.generation))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Mark a fenced delivery `acked`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn ack_event_delivery(&self, fence: &EventDeliveryFence) -> Result<bool> {
        finalize_delivery(&self.db, fence, STATE_ACKED, Some("ack"), None).await
    }

    /// Mark a fenced delivery permanently `rejected`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn reject_event_delivery(
        &self,
        fence: &EventDeliveryFence,
        reason: &str,
    ) -> Result<bool> {
        finalize_delivery(
            &self.db,
            fence,
            STATE_REJECTED,
            Some("reject"),
            Some(reason),
        )
        .await
    }

    /// Mark a fenced delivery `dead_letter`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn dead_letter_event_delivery(
        &self,
        fence: &EventDeliveryFence,
        reason: &str,
    ) -> Result<bool> {
        let ok = finalize_delivery(
            &self.db,
            fence,
            STATE_DEAD_LETTER,
            Some("dead_letter"),
            Some(reason),
        )
        .await?;
        if ok {
            bump_event_stats(&self.db, 0, 0, 1, None, None).await?;
        }
        Ok(ok)
    }

    /// Return a fenced delivery to `pending` with backoff (retryable handler).
    ///
    /// When `attempt_count` has already reached `max_attempts`, the delivery is
    /// dead-lettered instead of looping forever on a guest that keeps returning
    /// retry.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn retry_event_delivery(
        &self,
        fence: &EventDeliveryFence,
        run_after: chrono::DateTime<Utc>,
        reason: &str,
    ) -> Result<bool> {
        let Some(row) = event_deliveries::Entity::find_by_id(&fence.delivery_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        if row.state != STATE_RUNNING
            || row.lease_owner.as_deref() != Some(fence.owner.as_str())
            || row.lease_generation != fence.generation
        {
            return Ok(false);
        }
        if row.attempt_count >= row.max_attempts {
            return self
                .dead_letter_event_delivery(
                    fence,
                    &format!(
                        "retry exhausted after {} attempts: {reason}",
                        row.attempt_count
                    ),
                )
                .await;
        }
        let now = now_str();
        let res = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::State,
                sea_orm::sea_query::Expr::value(STATE_PENDING),
            )
            .col_expr(
                event_deliveries::Column::RunAfter,
                sea_orm::sea_query::Expr::value(run_after.to_rfc3339()),
            )
            .col_expr(
                event_deliveries::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::ErrorMessage,
                sea_orm::sea_query::Expr::value(Some(reason.to_string())),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(event_deliveries::Column::Id.eq(&fence.delivery_id))
            .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
            .filter(event_deliveries::Column::LeaseOwner.eq(&fence.owner))
            .filter(event_deliveries::Column::LeaseGeneration.eq(fence.generation))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected == 1 {
            bump_event_stats(&self.db, 1, 0, 0, None, None).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Suspend a fenced delivery (checkpoint + wake) without burning an attempt.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn suspend_event_delivery(
        &self,
        fence: &EventDeliveryFence,
        checkpoint_json: &str,
        checkpoint_schema_version: i64,
        wake_at: chrono::DateTime<Utc>,
        wake_event_type: &str,
        wake_filter_json: &str,
        wake_grants_json: &str,
    ) -> Result<bool> {
        if checkpoint_json.len() > 65_536 {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "event checkpoint of {} bytes exceeds 65536",
                checkpoint_json.len()
            )));
        }
        let wake_event_type = wake_event_type.trim();
        if !wake_event_type.is_empty() {
            validate_wake_event_type(wake_event_type)?;
        }
        let wake_filter_json = wake_filter_json.trim();
        if !wake_filter_json.is_empty() {
            let parsed: serde_json::Value =
                serde_json::from_str(wake_filter_json).map_err(|err| {
                    LibraryError::Other(anyhow::anyhow!("wake filter JSON is invalid: {err}"))
                })?;
            if !parsed.is_object() {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "wake filter JSON must be an object"
                )));
            }
        }
        let now = Utc::now();
        let Some(row) = event_deliveries::Entity::find_by_id(&fence.delivery_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        if row.state != STATE_RUNNING
            || row.lease_owner.as_deref() != Some(fence.owner.as_str())
            || row.lease_generation != fence.generation
        {
            return Ok(false);
        }
        let run_after = if !wake_event_type.is_empty() && wake_at.timestamp_millis() <= 0 {
            EVENT_WAKE_FAR_FUTURE.to_string()
        } else {
            wake_at.to_rfc3339()
        };
        let res = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::State,
                sea_orm::sea_query::Expr::value(STATE_PENDING),
            )
            .col_expr(
                event_deliveries::Column::RunAfter,
                sea_orm::sea_query::Expr::value(run_after),
            )
            .col_expr(
                event_deliveries::Column::CheckpointJson,
                sea_orm::sea_query::Expr::value(Some(checkpoint_json.to_string())),
            )
            .col_expr(
                event_deliveries::Column::CheckpointSchemaVersion,
                sea_orm::sea_query::Expr::value(checkpoint_schema_version),
            )
            .col_expr(
                event_deliveries::Column::InvocationSequence,
                sea_orm::sea_query::Expr::value(row.invocation_sequence + 1),
            )
            .col_expr(
                event_deliveries::Column::ResumePending,
                sea_orm::sea_query::Expr::value(1i64),
            )
            .col_expr(
                event_deliveries::Column::WakeEventType,
                sea_orm::sea_query::Expr::value(wake_event_type.to_string()),
            )
            .col_expr(
                event_deliveries::Column::WakeFilterJson,
                sea_orm::sea_query::Expr::value(wake_filter_json.to_string()),
            )
            .col_expr(
                event_deliveries::Column::WakeGrantsJson,
                sea_orm::sea_query::Expr::value(wake_grants_json.to_string()),
            )
            .col_expr(
                event_deliveries::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now.to_rfc3339()),
            )
            .filter(event_deliveries::Column::Id.eq(&fence.delivery_id))
            .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
            .filter(event_deliveries::Column::LeaseOwner.eq(&fence.owner))
            .filter(event_deliveries::Column::LeaseGeneration.eq(fence.generation))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected == 1 {
            bump_event_stats(&self.db, 0, 1, 0, None, None).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Fail a running delivery: retry with backoff or dead-letter at max attempts.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn fail_event_delivery(
        &self,
        fence: &EventDeliveryFence,
        reason: &str,
    ) -> Result<bool> {
        let Some(row) = event_deliveries::Entity::find_by_id(&fence.delivery_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        if row.state != STATE_RUNNING
            || row.lease_owner.as_deref() != Some(fence.owner.as_str())
            || row.lease_generation != fence.generation
        {
            return Ok(false);
        }
        if row.attempt_count >= row.max_attempts {
            return self.dead_letter_event_delivery(fence, reason).await;
        }
        let run_after = job_backoff_run_after(row.attempt_count, Utc::now());
        self.retry_event_delivery(fence, run_after, reason).await
    }

    /// Reclaim expired running deliveries back to `pending`.
    ///
    /// Rows with `cancel_requested` become `rejected` instead of pending so an
    /// operator cancel survives a worker crash. A stored checkpoint restores
    /// `resume_pending` so a crash during resume does not consume an attempt.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn reclaim_expired_event_deliveries(&self) -> Result<u32> {
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let expired = Condition::any()
            .add(event_deliveries::Column::LeaseExpiresAt.is_null())
            .add(event_deliveries::Column::LeaseExpiresAt.lte(now_s.clone()));
        let cancelled = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::State,
                sea_orm::sea_query::Expr::value(STATE_REJECTED),
            )
            .col_expr(
                event_deliveries::Column::Outcome,
                sea_orm::sea_query::Expr::value(Some("reject".to_string())),
            )
            .col_expr(
                event_deliveries::Column::ErrorMessage,
                sea_orm::sea_query::Expr::value(Some("cancelled by operator".to_string())),
            )
            .col_expr(
                event_deliveries::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::CancelRequested,
                sea_orm::sea_query::Expr::value(0i64),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now_s.clone()),
            )
            .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
            .filter(event_deliveries::Column::CancelRequested.eq(1i64))
            .filter(expired.clone())
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let res = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::State,
                sea_orm::sea_query::Expr::value(STATE_PENDING),
            )
            .col_expr(
                event_deliveries::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now_s),
            )
            .col_expr(
                event_deliveries::Column::ResumePending,
                sea_orm::sea_query::Expr::cust(
                    "CASE WHEN checkpoint_json IS NOT NULL THEN 1 ELSE resume_pending END",
                ),
            )
            .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
            .filter(expired)
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected > 0 {
            bump_event_stats(
                &self.db,
                i64::try_from(res.rows_affected).unwrap_or(0),
                0,
                0,
                None,
                None,
            )
            .await?;
        }
        Ok(u32::try_from(cancelled.rows_affected + res.rows_affected).unwrap_or(u32::MAX))
    }

    /// Load one event envelope.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn get_domain_event(&self, id: &str) -> Result<Option<DomainEventRecord>> {
        domain_events::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_event)
            .transpose()
    }

    /// Load one delivery.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn get_event_delivery(&self, id: &str) -> Result<Option<EventDeliveryRecord>> {
        event_deliveries::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_delivery)
            .transpose()
    }

    /// Recent outbox events, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_domain_events(&self, limit: u64) -> Result<Vec<DomainEventRecord>> {
        domain_events::Entity::find()
            .order_by_desc(domain_events::Column::CreatedAt)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_event)
            .collect()
    }

    /// Deliveries filtered by optional state, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_event_deliveries(
        &self,
        state: Option<&str>,
        limit: u64,
    ) -> Result<Vec<EventDeliveryRecord>> {
        let mut q = event_deliveries::Entity::find();
        if let Some(state) = state {
            q = q.filter(event_deliveries::Column::State.eq(state));
        }
        q.order_by_desc(event_deliveries::Column::UpdatedAt)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_delivery)
            .collect()
    }

    /// Re-queue a dead-lettered delivery for operator retry.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn retry_dead_letter_delivery(&self, id: &str) -> Result<bool> {
        let now = now_str();
        let res = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::State,
                sea_orm::sea_query::Expr::value(STATE_PENDING),
            )
            .col_expr(
                event_deliveries::Column::RunAfter,
                sea_orm::sea_query::Expr::value(now.clone()),
            )
            .col_expr(
                event_deliveries::Column::Outcome,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::ErrorMessage,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::AttemptCount,
                sea_orm::sea_query::Expr::value(0i64),
            )
            .col_expr(
                event_deliveries::Column::ResumePending,
                sea_orm::sea_query::Expr::value(0i64),
            )
            .col_expr(
                event_deliveries::Column::InvocationSequence,
                sea_orm::sea_query::Expr::value(0i64),
            )
            .col_expr(
                event_deliveries::Column::CheckpointJson,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                event_deliveries::Column::CheckpointSchemaVersion,
                sea_orm::sea_query::Expr::value(0i64),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(event_deliveries::Column::Id.eq(id))
            .filter(event_deliveries::Column::State.eq(STATE_DEAD_LETTER))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Operator acknowledge: mark a dead-letter as rejected (no further retry).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn acknowledge_dead_letter_delivery(&self, id: &str) -> Result<bool> {
        let now = now_str();
        let res = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::State,
                sea_orm::sea_query::Expr::value(STATE_REJECTED),
            )
            .col_expr(
                event_deliveries::Column::Outcome,
                sea_orm::sea_query::Expr::value(Some("ack".to_string())),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(event_deliveries::Column::Id.eq(id))
            .filter(event_deliveries::Column::State.eq(STATE_DEAD_LETTER))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Count deliveries in `state`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn count_event_deliveries(&self, state: &str) -> Result<i64> {
        let n = event_deliveries::Entity::find()
            .filter(event_deliveries::Column::State.eq(state))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        i64::try_from(n).map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))
    }

    /// Operator-visible delivery-queue counters (pending vs suspended split).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn event_delivery_metrics(&self) -> Result<EventDeliveryMetrics> {
        let pending_all = self.count_event_deliveries(STATE_PENDING).await?;
        let suspended = i64::try_from(
            event_deliveries::Entity::find()
                .filter(event_deliveries::Column::State.eq(STATE_PENDING))
                .filter(event_deliveries::Column::CheckpointJson.is_not_null())
                .count(&self.db)
                .await
                .map_err(LibraryError::Orm)?,
        )
        .map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
        let oldest = event_deliveries::Entity::find()
            .filter(event_deliveries::Column::State.eq(STATE_PENDING))
            .order_by_asc(event_deliveries::Column::CreatedAt)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let oldest_pending_age_secs = oldest.map(|row| {
            let created = parse_dt(&row.created_at);
            (Utc::now() - created).num_seconds().max(0)
        });
        let stats = ensure_event_outbox_stats(&self.db).await?;
        let dispatch_latency_ms_avg = if stats.dispatch_count > 0 {
            Some(stats.dispatch_latency_ms_sum / stats.dispatch_count)
        } else {
            None
        };
        let handler_latency_ms_avg = if stats.handler_count > 0 {
            Some(stats.handler_latency_ms_sum / stats.handler_count)
        } else {
            None
        };
        Ok(EventDeliveryMetrics {
            pending: (pending_all - suspended).max(0),
            running: self.count_event_deliveries(STATE_RUNNING).await?,
            suspended,
            dead_letter: self.count_event_deliveries(STATE_DEAD_LETTER).await?,
            acked: self.count_event_deliveries(STATE_ACKED).await?,
            oldest_pending_age_secs,
            retries_total: stats.retries_total,
            suspensions_total: stats.suspensions_total,
            dead_letters_total: stats.dead_letters_total,
            dispatch_latency_ms_avg,
            handler_latency_ms_avg,
        })
    }

    /// Record one `onEvent` duration sample.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    pub async fn record_event_handler_latency(&self, duration_ms: i64) -> Result<()> {
        bump_event_stats(&self.db, 0, 0, 0, None, Some(duration_ms.max(0))).await
    }

    /// Outbox envelopes in `dispatch_state`, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_domain_events_by_dispatch_state(
        &self,
        dispatch_state: &str,
        limit: u64,
    ) -> Result<Vec<DomainEventRecord>> {
        domain_events::Entity::find()
            .filter(domain_events::Column::DispatchState.eq(dispatch_state))
            .order_by_asc(domain_events::Column::CreatedAt)
            .order_by_asc(domain_events::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_event)
            .collect()
    }

    /// Dispatched events newer than `created_after`, after optional `(created_at, id)` cursor.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_dispatched_events_page(
        &self,
        created_after: &str,
        after: Option<(&str, &str)>,
        limit: u64,
    ) -> Result<Vec<DomainEventRecord>> {
        let mut q = domain_events::Entity::find()
            .filter(domain_events::Column::DispatchState.eq(STATE_DISPATCHED))
            .filter(domain_events::Column::CreatedAt.gt(created_after.to_string()));
        if let Some((created_at, id)) = after {
            q = q.filter(
                Condition::any()
                    .add(domain_events::Column::CreatedAt.gt(created_at.to_string()))
                    .add(
                        Condition::all()
                            .add(domain_events::Column::CreatedAt.eq(created_at.to_string()))
                            .add(domain_events::Column::Id.gt(id.to_string())),
                    ),
            );
        }
        q.order_by_asc(domain_events::Column::CreatedAt)
            .order_by_asc(domain_events::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_event)
            .collect()
    }

    /// Heartbeat one node's plugin registration (does not delete other nodes).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    pub async fn upsert_event_subscriber(
        &self,
        node_id: &str,
        plugin_id: &str,
        subscriptions: &[EventCatalogSubscription],
        enabled: bool,
    ) -> Result<()> {
        self.upsert_event_subscriber_at(node_id, plugin_id, subscriptions, enabled, Utc::now())
            .await
    }

    /// Heartbeat one node's plugin registration at an explicit timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    pub async fn upsert_event_subscriber_at(
        &self,
        node_id: &str,
        plugin_id: &str,
        subscriptions: &[EventCatalogSubscription],
        enabled: bool,
        heartbeat_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let node_id = node_id.trim();
        let plugin_id = plugin_id.trim();
        if node_id.is_empty() || plugin_id.is_empty() {
            return Ok(());
        }
        let json = serde_json::to_string(subscriptions).unwrap_or_else(|_| "[]".into());
        let model = event_subscriber_nodes::ActiveModel {
            node_id: Set(node_id.to_string()),
            plugin_id: Set(plugin_id.to_string()),
            subscriptions_json: Set(json),
            enabled: Set(i64::from(enabled)),
            heartbeat_at: Set(heartbeat_at.to_rfc3339()),
        };
        event_subscriber_nodes::Entity::insert(model)
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    event_subscriber_nodes::Column::NodeId,
                    event_subscriber_nodes::Column::PluginId,
                ])
                .update_columns([
                    event_subscriber_nodes::Column::SubscriptionsJson,
                    event_subscriber_nodes::Column::Enabled,
                    event_subscriber_nodes::Column::HeartbeatAt,
                ])
                .to_owned(),
            )
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Live catalog collapsed by `plugin_id` using the default heartbeat TTL.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_live_event_subscribers(&self) -> Result<Vec<EventSubscriberCatalogRecord>> {
        self.list_live_event_subscribers_with_ttl(EVENT_SUBSCRIBER_HEARTBEAT_TTL_SECS)
            .await
    }

    /// Live catalog collapsed by `plugin_id` for a custom heartbeat TTL.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_live_event_subscribers_with_ttl(
        &self,
        ttl_secs: i64,
    ) -> Result<Vec<EventSubscriberCatalogRecord>> {
        let rows = self.list_live_event_subscriber_nodes(ttl_secs).await?;
        Ok(collapse_live_subscriber_nodes(&rows))
    }

    /// Live per-node rows whose heartbeat is within `ttl_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_live_event_subscriber_nodes(
        &self,
        ttl_secs: i64,
    ) -> Result<Vec<EventSubscriberNodeRecord>> {
        let cutoff = (Utc::now() - Duration::seconds(ttl_secs.max(1))).to_rfc3339();
        event_subscriber_nodes::Entity::find()
            .filter(event_subscriber_nodes::Column::HeartbeatAt.gte(cutoff))
            .order_by_asc(event_subscriber_nodes::Column::PluginId)
            .order_by_asc(event_subscriber_nodes::Column::NodeId)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_node_row)
            .collect()
    }

    /// Create deliveries from the live catalog for one event.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    pub async fn dispatch_catalog_matches(
        &self,
        event: &DomainEventRecord,
        operation_id: &str,
    ) -> Result<u32> {
        let catalog = self.list_live_event_subscribers().await?;
        let subs = catalog_subscribers_for_event(&catalog, event);
        self.dispatch_event_deliveries(&event.id, &subs, operation_id)
            .await
    }

    /// Dispatched events in the retention window missing a delivery for `plugin_id`.
    ///
    /// Restricted to `event_types` the plugin actually subscribes to so unrelated
    /// types do not occupy the page.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    async fn list_missing_dispatched_events_for_plugin(
        &self,
        plugin_id: &str,
        event_types: &[String],
        created_after: &str,
        after: Option<(&str, &str)>,
        limit: u64,
    ) -> Result<Vec<DomainEventRecord>> {
        if event_types.is_empty() || plugin_id.trim().is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = event_types
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!(
            "SELECT e.id FROM domain_events e \
             WHERE e.dispatch_state = 'dispatched' \
               AND e.created_at > ? \
               AND e.event_type IN ({placeholders}) \
               AND NOT EXISTS ( \
                 SELECT 1 FROM event_deliveries d \
                 WHERE d.event_id = e.id AND d.plugin_id = ? \
               )"
        );
        let mut values: Vec<sea_orm::Value> = vec![created_after.to_string().into()];
        for event_type in event_types {
            values.push(event_type.clone().into());
        }
        values.push(plugin_id.to_string().into());
        if let Some((created_at, id)) = after {
            sql.push_str(" AND (e.created_at > ? OR (e.created_at = ? AND e.id > ?))");
            values.push(created_at.to_string().into());
            values.push(created_at.to_string().into());
            values.push(id.to_string().into());
        }
        sql.push_str(" ORDER BY e.created_at ASC, e.id ASC LIMIT ?");
        values.push(i64::try_from(limit).unwrap_or(200).into());
        let backend = self.db.get_database_backend();
        let rows = self
            .db
            .query_all_raw(statement_for_backend(backend, sql, values))
            .await
            .map_err(LibraryError::Orm)?;
        let mut ids = Vec::new();
        for row in rows {
            if let Ok(id) = row.try_get_by_index::<String>(0) {
                if !id.is_empty() {
                    ids.push(id);
                }
            }
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let models = domain_events::Entity::find()
            .filter(domain_events::Column::Id.is_in(ids.clone()))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut by_id = std::collections::HashMap::new();
        for model in models {
            by_id.insert(model.id.clone(), model);
        }
        ids.into_iter()
            .filter_map(|id| by_id.remove(&id))
            .map(map_event)
            .collect()
    }

    /// Late-join catalog deliveries for missing `(event_id, plugin_id)` pairs.
    ///
    /// Pages an anti-join of dispatched events in the retention window that
    /// lack a delivery for each live plugin. Unchanged catalogs with no missing
    /// pairs issue bounded empty `SELECT`s and **zero** dispatch writes.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read or write fails.
    pub async fn reconcile_catalog_deliveries(
        &self,
        created_after: chrono::DateTime<Utc>,
    ) -> Result<u32> {
        let catalog = self.list_live_event_subscribers().await?;
        let cutoff = created_after.to_rfc3339();
        let mut total = 0u32;
        for row in &catalog {
            if !row.enabled {
                continue;
            }
            let mut types: Vec<String> = row
                .subscriptions
                .iter()
                .map(|s| s.event_type.clone())
                .collect();
            types.sort();
            types.dedup();
            if types.is_empty() {
                continue;
            }
            let mut after: Option<(String, String)> = None;
            loop {
                let page = self
                    .list_missing_dispatched_events_for_plugin(
                        &row.plugin_id,
                        &types,
                        &cutoff,
                        after.as_ref().map(|(c, i)| (c.as_str(), i.as_str())),
                        RECONCILE_PAGE,
                    )
                    .await?;
                if page.is_empty() {
                    break;
                }
                for event in &page {
                    let subs = catalog_subscribers_for_event(std::slice::from_ref(row), event);
                    let Some(sub) = subs.into_iter().find(|s| s.plugin_id == row.plugin_id) else {
                        continue;
                    };
                    let op = format!("reconcile-{}-{}", event.id, sub.plugin_id);
                    total += self
                        .dispatch_event_deliveries(&event.id, std::slice::from_ref(&sub), &op)
                        .await?;
                }
                let Some(last) = page.last() else {
                    break;
                };
                after = Some((last.created_at.to_rfc3339(), last.id.clone()));
                if u64::try_from(page.len()).unwrap_or(RECONCILE_PAGE) < RECONCILE_PAGE {
                    break;
                }
            }
        }
        Ok(total)
    }

    /// Cancel a pending delivery or flag a running one for cooperative stop.
    ///
    /// Pending (including suspended) rows become `rejected`. Running rows set
    /// `cancel_requested` so the heartbeat loop aborts `onEvent`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn request_event_delivery_cancel(
        &self,
        id: &str,
    ) -> Result<Option<EventDeliveryRecord>> {
        for _ in 0..16 {
            let Some(model) = event_deliveries::Entity::find_by_id(id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            else {
                return Ok(None);
            };
            if model.state != STATE_PENDING && model.state != STATE_RUNNING {
                return Ok(Some(map_delivery(model)?));
            }
            if model.state == STATE_PENDING {
                let now = now_str();
                let res = event_deliveries::Entity::update_many()
                    .col_expr(
                        event_deliveries::Column::State,
                        sea_orm::sea_query::Expr::value(STATE_REJECTED),
                    )
                    .col_expr(
                        event_deliveries::Column::Outcome,
                        sea_orm::sea_query::Expr::value(Some("reject".to_string())),
                    )
                    .col_expr(
                        event_deliveries::Column::ErrorMessage,
                        sea_orm::sea_query::Expr::value(Some("cancelled by operator".to_string())),
                    )
                    .col_expr(
                        event_deliveries::Column::CancelRequested,
                        sea_orm::sea_query::Expr::value(0i64),
                    )
                    .col_expr(
                        event_deliveries::Column::UpdatedAt,
                        sea_orm::sea_query::Expr::value(now),
                    )
                    .filter(event_deliveries::Column::Id.eq(id))
                    .filter(event_deliveries::Column::State.eq(STATE_PENDING))
                    .exec(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?;
                if res.rows_affected == 1 {
                    return self.get_event_delivery(id).await;
                }
                continue;
            }
            let res = event_deliveries::Entity::update_many()
                .col_expr(
                    event_deliveries::Column::CancelRequested,
                    sea_orm::sea_query::Expr::value(1i64),
                )
                .col_expr(
                    event_deliveries::Column::UpdatedAt,
                    sea_orm::sea_query::Expr::value(now_str()),
                )
                .filter(event_deliveries::Column::Id.eq(id))
                .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if res.rows_affected == 1 {
                return self.get_event_delivery(id).await;
            }
        }
        self.get_event_delivery(id).await
    }

    /// True when a running delivery worker should abort after the current step.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn event_delivery_cancel_requested(&self, id: &str) -> Result<bool> {
        let Some(model) = event_deliveries::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        Ok(model.cancel_requested != 0)
    }

    /// Wake a suspended delivery: `run_after = now` and `resume_pending = 1`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn resume_event_delivery(&self, id: &str) -> Result<bool> {
        let now = now_str();
        let res = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::RunAfter,
                sea_orm::sea_query::Expr::value(now.clone()),
            )
            .col_expr(
                event_deliveries::Column::ResumePending,
                sea_orm::sea_query::Expr::value(1i64),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(event_deliveries::Column::Id.eq(id))
            .filter(event_deliveries::Column::State.eq(STATE_PENDING))
            .filter(event_deliveries::Column::CheckpointJson.is_not_null())
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Delete terminal deliveries past their retention windows, then empty events.
    ///
    /// Acked/rejected rows use `retention_days`. Dead letters use
    /// `dead_letter_retention_days`. Dispatched parent events with no remaining
    /// deliveries are removed only after the same `retention_days` cutoff.
    /// Expired per-node catalog heartbeats are swept here too.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn prune_event_deliveries(
        &self,
        retention_days: u64,
        dead_letter_retention_days: u64,
    ) -> Result<u64> {
        let terminal_cutoff = (Utc::now()
            - chrono::Duration::days(i64::try_from(retention_days).unwrap_or(7)))
        .to_rfc3339();
        let dead_letter_cutoff = (Utc::now()
            - chrono::Duration::days(i64::try_from(dead_letter_retention_days).unwrap_or(30)))
        .to_rfc3339();
        let heartbeat_cutoff =
            (Utc::now() - Duration::seconds(EVENT_SUBSCRIBER_HEARTBEAT_TTL_SECS)).to_rfc3339();
        let acked = event_deliveries::Entity::delete_many()
            .filter(event_deliveries::Column::State.is_in([STATE_ACKED, STATE_REJECTED]))
            .filter(event_deliveries::Column::UpdatedAt.lte(terminal_cutoff.clone()))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let dead = event_deliveries::Entity::delete_many()
            .filter(event_deliveries::Column::State.eq(STATE_DEAD_LETTER))
            .filter(event_deliveries::Column::UpdatedAt.lte(dead_letter_cutoff))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let stale_nodes = event_subscriber_nodes::Entity::delete_many()
            .filter(event_subscriber_nodes::Column::HeartbeatAt.lt(heartbeat_cutoff))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let backend = self.db.get_database_backend();
        let events = self
            .db
            .execute_raw(statement_for_backend(
                backend,
                "DELETE FROM domain_events WHERE dispatch_state = 'dispatched' \
                 AND created_at <= ? AND NOT EXISTS ( \
                    SELECT 1 FROM event_deliveries d WHERE d.event_id = domain_events.id \
                 )",
                [terminal_cutoff.into()],
            ))
            .await
            .map_err(LibraryError::Orm)?;
        Ok(acked.rows_affected
            + dead.rows_affected
            + stale_nodes.rows_affected
            + events.rows_affected())
    }
}

/// Mint an event id when empty and reject oversized or blank publish fields.
///
/// # Errors
///
/// Returns [`LibraryError::Other`] when the payload exceeds 64 KiB, when
/// `event_type` / `dedup_key` are blank, or when `source` is not empty and
/// fails the plugin-id grammar.
pub fn prepare_publish_domain_event(
    mut spec: PublishDomainEventSpec,
) -> Result<PublishDomainEventSpec> {
    const MAX_PAYLOAD: usize = 65_536;
    if spec.payload.len() > MAX_PAYLOAD {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "domain event payload of {} bytes exceeds {MAX_PAYLOAD}",
            spec.payload.len()
        )));
    }
    if spec.event_type.trim().is_empty() || spec.dedup_key.trim().is_empty() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "domain event type and dedup_key are required"
        )));
    }
    validate_event_source(&spec.source)?;
    if spec.id.trim().is_empty() {
        spec.id = Uuid::new_v4().to_string();
    } else {
        spec.id = spec.id.trim().to_string();
    }
    Ok(spec)
}

/// Build the `book_acquired` outbox spec for a library row.
pub(crate) fn book_acquired_spec(
    book: &books::Model,
    storage_key: Option<&str>,
) -> PublishDomainEventSpec {
    let key = storage_key
        .map(str::to_string)
        .or_else(|| book.storage_key.clone())
        .unwrap_or_default();
    let path_keys = if key.is_empty() {
        Vec::<String>::new()
    } else {
        vec![key]
    };
    let payload = serde_json::json!({
        "titleId": book.uuid,
        "source": book.source,
        "asin": book.asin,
        "isbn": book.isbn,
        "pathKeys": path_keys,
        "accountId": book.account_id,
    });
    PublishDomainEventSpec {
        id: String::new(),
        event_type: "book_acquired".into(),
        schema_version: 1,
        account_id: book.account_id.clone(),
        source: book.source.clone(),
        correlation_id: book.uuid.clone(),
        causation_id: String::new(),
        dedup_key: format!("book_acquired:{}", book.uuid),
        payload: serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into()),
        ordering_key: book.uuid.clone(),
    }
}

/// Apply acquire status and, when becoming acquired, publish the outbox row.
pub(crate) async fn set_acquire_status_on<C: ConnectionTrait>(
    db: &C,
    model: books::Model,
    status: AcquireStatus,
    storage_key: Option<&str>,
    error_message: Option<&str>,
) -> Result<()> {
    let spec = if status == AcquireStatus::Acquired {
        Some(prepare_publish_domain_event(book_acquired_spec(
            &model,
            storage_key,
        ))?)
    } else {
        None
    };
    update_acquire_status_on(db, model, status, storage_key, error_message).await?;
    if let Some(spec) = spec {
        publish_domain_event_on(db, spec).await?;
    }
    Ok(())
}

/// Update acquire columns without publishing.
pub(crate) async fn update_acquire_status_on<C: ConnectionTrait>(
    db: &C,
    model: books::Model,
    status: AcquireStatus,
    storage_key: Option<&str>,
    error_message: Option<&str>,
) -> Result<()> {
    let mut am: books::ActiveModel = model.into();
    am.acquire_status = Set(status.as_str().to_string());
    am.storage_key = Set(storage_key.map(str::to_string));
    am.error_message = Set(error_message.map(str::to_string));
    am.updated_at = Set(now_str());
    am.update(db).await.map_err(LibraryError::Orm)?;
    Ok(())
}

/// Insert an outbox row or return the existing id for the same namespaced key.
pub(crate) async fn publish_domain_event_on<C: ConnectionTrait>(
    db: &C,
    spec: PublishDomainEventSpec,
) -> Result<PublishDomainEventOutcome> {
    if take_publish_fault() {
        return Err(LibraryError::Other(anyhow!("injected event publish fault")));
    }
    let spec = prepare_publish_domain_event(spec)?;
    if let Some(existing) = domain_events::Entity::find()
        .filter(domain_events::Column::AccountId.eq(&spec.account_id))
        .filter(domain_events::Column::Source.eq(&spec.source))
        .filter(domain_events::Column::EventType.eq(&spec.event_type))
        .filter(domain_events::Column::DedupKey.eq(&spec.dedup_key))
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
    {
        return Ok(PublishDomainEventOutcome::Duplicate {
            existing_id: existing.id,
        });
    }
    let now = now_str();
    let id = spec.id;
    let event_type = spec.event_type.clone();
    let dedup_key = spec.dedup_key.clone();
    let account_id = spec.account_id.clone();
    let source = spec.source.clone();
    let model = domain_events::ActiveModel {
        id: Set(id.clone()),
        event_type: Set(spec.event_type),
        schema_version: Set(spec.schema_version.max(1)),
        occurred_at: Set(now.clone()),
        account_id: Set(spec.account_id),
        source: Set(spec.source),
        correlation_id: Set(spec.correlation_id),
        causation_id: Set(spec.causation_id),
        dedup_key: Set(spec.dedup_key),
        payload: Set(spec.payload),
        ordering_key: Set(spec.ordering_key),
        dispatch_state: Set(STATE_PENDING.into()),
        created_at: Set(now),
        wake_pending: Set(1),
        wake_lease_owner: NotSet,
        wake_lease_expires_at: NotSet,
        wake_cursor_at: Set(String::new()),
        wake_cursor_id: Set(String::new()),
        dispatch_snapshot_json: Set(String::new()),
    };
    match model.insert(db).await {
        Ok(_) => Ok(PublishDomainEventOutcome::Created { id }),
        Err(err) if is_unique_violation(&err) => {
            if let Some(existing) = domain_events::Entity::find()
                .filter(domain_events::Column::AccountId.eq(account_id))
                .filter(domain_events::Column::Source.eq(source))
                .filter(domain_events::Column::EventType.eq(event_type))
                .filter(domain_events::Column::DedupKey.eq(dedup_key))
                .one(db)
                .await
                .map_err(LibraryError::Orm)?
            {
                return Ok(PublishDomainEventOutcome::Duplicate {
                    existing_id: existing.id,
                });
            }
            Err(LibraryError::Orm(err))
        }
        Err(err) => Err(LibraryError::Orm(err)),
    }
}

/// Process one delivery page for a claimed `wake_pending` event, then persist cursor.
///
/// Cursor release and finish require `wake_lease_owner = owner` and `wake_pending = 1`.
/// Zero rows affected is fence loss: do not overwrite another owner’s cursor.
async fn wake_one_page_on<C: ConnectionTrait>(
    db: &C,
    row: &domain_events::Model,
    owner: &str,
    page: u64,
    max_binds: usize,
) -> Result<u32> {
    let event = map_event(row.clone())?;
    if event.event_type.trim().is_empty() {
        let _ = finish_wake_on(db, &event.id, owner).await?;
        return Ok(0);
    }
    let now = now_str();
    let mut cursor_at = row.wake_cursor_at.clone();
    let mut cursor_id = row.wake_cursor_id.clone();
    let rows = event_deliveries::Entity::find()
        .filter(event_deliveries::Column::State.eq(STATE_PENDING))
        .filter(event_deliveries::Column::WakeEventType.eq(&event.event_type))
        .filter(event_deliveries::Column::EventId.ne(&event.id))
        .filter(
            Condition::any()
                .add(event_deliveries::Column::CreatedAt.gt(cursor_at.clone()))
                .add(
                    Condition::all()
                        .add(event_deliveries::Column::CreatedAt.eq(cursor_at.clone()))
                        .add(event_deliveries::Column::Id.gt(cursor_id.clone())),
                ),
        )
        .order_by_asc(event_deliveries::Column::CreatedAt)
        .order_by_asc(event_deliveries::Column::Id)
        .limit(page)
        .all(db)
        .await
        .map_err(LibraryError::Orm)?;
    if rows.is_empty() {
        let _ = finish_wake_on(db, &event.id, owner).await?;
        return Ok(0);
    }
    let page_len = rows.len();
    let parent_ids: Vec<String> = rows
        .iter()
        .map(|delivery| delivery.event_id.clone())
        .collect();
    let mut account_by_id = std::collections::HashMap::<String, String>::new();
    let in_chunk = max_binds.max(1);
    for chunk in parent_ids.chunks(in_chunk) {
        let parents = domain_events::Entity::find()
            .filter(domain_events::Column::Id.is_in(chunk.to_vec()))
            .all(db)
            .await
            .map_err(LibraryError::Orm)?;
        for parent in parents {
            account_by_id.insert(parent.id, parent.account_id);
        }
    }
    let mut ids = Vec::new();
    for delivery in rows {
        cursor_at.clone_from(&delivery.created_at);
        cursor_id.clone_from(&delivery.id);
        if account_by_id.get(&delivery.event_id).map(String::as_str)
            != Some(event.account_id.as_str())
        {
            continue;
        }
        if event_matches_wake_grants(
            &delivery.wake_grants_json,
            &delivery.wake_filter_json,
            event.schema_version,
            &event.payload,
        ) {
            ids.push(delivery.id);
        }
    }
    let woken = wake_deliveries_fenced_on(db, &event.id, owner, &ids, &now, max_binds).await?;
    if u64::try_from(page_len).unwrap_or(page) < page {
        let _ = finish_wake_on(db, &event.id, owner).await?;
    } else {
        let _ = release_wake_cursor_on(db, &event.id, owner, &cursor_at, &cursor_id).await?;
    }
    Ok(woken)
}

/// Persist the next-page cursor and drop the wake lease when this token still owns it.
pub(crate) async fn release_wake_cursor_on<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    owner: &str,
    cursor_at: &str,
    cursor_id: &str,
) -> Result<bool> {
    let res = domain_events::Entity::update_many()
        .col_expr(
            domain_events::Column::WakeCursorAt,
            sea_orm::sea_query::Expr::value(cursor_at.to_string()),
        )
        .col_expr(
            domain_events::Column::WakeCursorId,
            sea_orm::sea_query::Expr::value(cursor_id.to_string()),
        )
        .col_expr(
            domain_events::Column::WakeLeaseOwner,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            domain_events::Column::WakeLeaseExpiresAt,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .filter(domain_events::Column::Id.eq(event_id))
        .filter(domain_events::Column::WakePending.eq(1i64))
        .filter(domain_events::Column::WakeLeaseOwner.eq(owner))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(res.rows_affected == 1)
}

/// Clear `wake_pending` when `owner` still holds the claim token.
pub(crate) async fn finish_wake_on<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    owner: &str,
) -> Result<bool> {
    let res = domain_events::Entity::update_many()
        .col_expr(
            domain_events::Column::WakePending,
            sea_orm::sea_query::Expr::value(0i64),
        )
        .col_expr(
            domain_events::Column::WakeLeaseOwner,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            domain_events::Column::WakeLeaseExpiresAt,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            domain_events::Column::WakeCursorAt,
            sea_orm::sea_query::Expr::value(String::new()),
        )
        .col_expr(
            domain_events::Column::WakeCursorId,
            sea_orm::sea_query::Expr::value(String::new()),
        )
        .filter(domain_events::Column::Id.eq(event_id))
        .filter(domain_events::Column::WakePending.eq(1i64))
        .filter(domain_events::Column::WakeLeaseOwner.eq(owner))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(res.rows_affected == 1)
}

/// Create deliveries for `subscribers` and mark the event dispatched.
pub(crate) async fn dispatch_event_deliveries_on<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    subscribers: &[EventSubscriber],
) -> Result<u32> {
    let Some(event) = domain_events::Entity::find_by_id(event_id)
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(0);
    };
    let now = now_str();
    let first_dispatch = event.dispatch_state == STATE_PENDING;
    let created_at = parse_dt(&event.created_at);
    let mut created = 0u32;
    for sub in subscribers {
        if sub.plugin_id.trim().is_empty() {
            continue;
        }
        let resource_class = if sub.resource_class.trim().is_empty() {
            EVENT_RESOURCE_CLASS_NETWORK
        } else {
            sub.resource_class.trim()
        };
        if resource_class != EVENT_RESOURCE_CLASS_NETWORK {
            continue;
        }
        let delivery_id = format!("{event_id}:{}", sub.plugin_id);
        let idempotency_key = delivery_id.clone();
        let model = event_deliveries::ActiveModel {
            id: Set(delivery_id),
            event_id: Set(event_id.to_string()),
            plugin_id: Set(sub.plugin_id.clone()),
            idempotency_key: Set(idempotency_key),
            state: Set(STATE_PENDING.into()),
            attempt_count: Set(0),
            max_attempts: Set(EVENT_DELIVERY_MAX_ATTEMPTS),
            lease_owner: NotSet,
            lease_expires_at: NotSet,
            lease_generation: Set(0),
            run_after: Set(now.clone()),
            invocation_sequence: Set(0),
            resume_pending: Set(0),
            checkpoint_json: NotSet,
            checkpoint_schema_version: Set(0),
            ordering_key: Set(event.ordering_key.clone()),
            outcome: NotSet,
            error_message: NotSet,
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            cancel_requested: Set(0),
            resource_class: Set(EVENT_RESOURCE_CLASS_NETWORK.into()),
            wake_event_type: Set(String::new()),
            wake_filter_json: Set(String::new()),
            wake_grants_json: Set(String::new()),
        };
        match model.insert(db).await {
            Ok(_) => created += 1,
            Err(err) if is_unique_violation(&err) => {}
            Err(err) => return Err(LibraryError::Orm(err)),
        }
    }
    let dispatched = domain_events::Entity::update_many()
        .col_expr(
            domain_events::Column::DispatchState,
            sea_orm::sea_query::Expr::value(STATE_DISPATCHED),
        )
        .filter(domain_events::Column::Id.eq(event_id))
        .filter(domain_events::Column::DispatchState.eq(STATE_PENDING))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    if first_dispatch && dispatched.rows_affected == 1 {
        let ms = (Utc::now() - created_at).num_milliseconds().max(0);
        bump_event_stats(db, 0, 0, 0, Some(ms), None).await?;
    }
    Ok(created)
}

/// Claim the next ready delivery, skipping blocked FIFO keys.
pub(crate) async fn claim_next_event_delivery_on<C: ConnectionTrait>(
    db: &C,
    owner: &str,
    lease_secs: u64,
    plugin_ids: &[String],
    max_in_flight: u32,
    node_id: &str,
) -> Result<Option<EventDeliveryRecord>> {
    if plugin_ids.is_empty() {
        return Ok(None);
    }
    let page = claim_page_size();
    let now = Utc::now();
    let now_s = now.to_rfc3339();
    sanitize_unknown_event_resource_class_on(db, &now_s).await?;
    let lease_expires =
        (now + Duration::seconds(i64::try_from(lease_secs).unwrap_or(60))).to_rfc3339();
    let mut cursor: Option<(String, String)> = None;
    loop {
        let mut query = event_deliveries::Entity::find()
            .filter(event_deliveries::Column::State.eq(STATE_PENDING))
            .filter(event_deliveries::Column::RunAfter.lte(now_s.clone()))
            .filter(event_deliveries::Column::PluginId.is_in(plugin_ids.to_vec()))
            .filter(event_deliveries::Column::ResourceClass.eq(EVENT_RESOURCE_CLASS_NETWORK));
        if let Some((created_at, id)) = &cursor {
            query = query.filter(claim_keyset_after(created_at, id));
        }
        let candidates = query
            .order_by_asc(event_deliveries::Column::CreatedAt)
            .order_by_asc(event_deliveries::Column::Id)
            .limit(page)
            .all(db)
            .await
            .map_err(LibraryError::Orm)?;
        if candidates.is_empty() {
            return Ok(None);
        }
        cursor = candidates
            .last()
            .map(|m| (m.created_at.clone(), m.id.clone()));
        for model in candidates {
            if fifo_blocked(db, &model).await? {
                continue;
            }
            if !node_id.trim().is_empty()
                && !delivery_model_matches_node_catalog(db, node_id, &model).await?
            {
                continue;
            }
            lock_plugin_in_flight(db, &model.plugin_id, &model.resource_class).await?;
            if plugin_in_flight_at_cap(db, &model.plugin_id, &model.resource_class, max_in_flight)
                .await?
            {
                continue;
            }
            let resuming = model.resume_pending != 0;
            let attempt = if resuming {
                model.attempt_count.max(1)
            } else {
                model.attempt_count + 1
            };
            let generation = model.lease_generation + 1;
            let mut update = event_deliveries::Entity::update_many()
                .col_expr(
                    event_deliveries::Column::State,
                    sea_orm::sea_query::Expr::value(STATE_RUNNING),
                )
                .col_expr(
                    event_deliveries::Column::AttemptCount,
                    sea_orm::sea_query::Expr::value(attempt),
                )
                .col_expr(
                    event_deliveries::Column::LeaseOwner,
                    sea_orm::sea_query::Expr::value(Some(owner.to_string())),
                )
                .col_expr(
                    event_deliveries::Column::LeaseExpiresAt,
                    sea_orm::sea_query::Expr::value(Some(lease_expires.clone())),
                )
                .col_expr(
                    event_deliveries::Column::LeaseGeneration,
                    sea_orm::sea_query::Expr::value(generation),
                )
                .col_expr(
                    event_deliveries::Column::UpdatedAt,
                    sea_orm::sea_query::Expr::value(now_s.clone()),
                );
            if resuming {
                update = update.col_expr(
                    event_deliveries::Column::ResumePending,
                    sea_orm::sea_query::Expr::value(0i64),
                );
            }
            let res = update
                .filter(event_deliveries::Column::Id.eq(&model.id))
                .filter(event_deliveries::Column::State.eq(STATE_PENDING))
                .exec(db)
                .await
                .map_err(LibraryError::Orm)?;
            if res.rows_affected != 1 {
                continue;
            }
            let Some(updated) = event_deliveries::Entity::find_by_id(&model.id)
                .one(db)
                .await
                .map_err(LibraryError::Orm)?
            else {
                continue;
            };
            return Ok(Some(map_delivery(updated)?));
        }
    }
}

async fn fifo_blocked<C: ConnectionTrait>(
    db: &C,
    candidate: &event_deliveries::Model,
) -> Result<bool> {
    if candidate.ordering_key.is_empty() {
        return Ok(false);
    }
    let n = event_deliveries::Entity::find()
        .filter(event_deliveries::Column::PluginId.eq(&candidate.plugin_id))
        .filter(event_deliveries::Column::OrderingKey.eq(&candidate.ordering_key))
        .filter(event_deliveries::Column::CreatedAt.lt(&candidate.created_at))
        .filter(event_deliveries::Column::State.is_in([STATE_PENDING, STATE_RUNNING]))
        .count(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(n > 0)
}

async fn finalize_delivery<C: ConnectionTrait>(
    db: &C,
    fence: &EventDeliveryFence,
    state: &str,
    outcome: Option<&str>,
    error: Option<&str>,
) -> Result<bool> {
    let now = now_str();
    let res = event_deliveries::Entity::update_many()
        .col_expr(
            event_deliveries::Column::State,
            sea_orm::sea_query::Expr::value(state),
        )
        .col_expr(
            event_deliveries::Column::Outcome,
            sea_orm::sea_query::Expr::value(outcome.map(str::to_string)),
        )
        .col_expr(
            event_deliveries::Column::ErrorMessage,
            sea_orm::sea_query::Expr::value(error.map(str::to_string)),
        )
        .col_expr(
            event_deliveries::Column::LeaseOwner,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            event_deliveries::Column::LeaseExpiresAt,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            event_deliveries::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(now),
        )
        .filter(event_deliveries::Column::Id.eq(&fence.delivery_id))
        .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
        .filter(event_deliveries::Column::LeaseOwner.eq(&fence.owner))
        .filter(event_deliveries::Column::LeaseGeneration.eq(fence.generation))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(res.rows_affected == 1)
}

fn is_unique_violation(err: &sea_orm::DbErr) -> bool {
    let s = err.to_string().to_lowercase();
    s.contains("unique") || s.contains("constraint")
}

/// Serialize COUNT+claim per plugin under every isolation level.
async fn lock_plugin_in_flight<C: ConnectionTrait>(
    db: &C,
    plugin_id: &str,
    resource_class: &str,
) -> Result<()> {
    let class = if resource_class.trim().is_empty() {
        EVENT_RESOURCE_CLASS_NETWORK
    } else {
        resource_class.trim()
    };
    let key = crate::sql_plan::event_inflight_slot(plugin_id, class);
    crate::sql_plan::lock_serialization_slot(db, &key).await
}

async fn plugin_in_flight_at_cap<C: ConnectionTrait>(
    db: &C,
    plugin_id: &str,
    resource_class: &str,
    max_in_flight: u32,
) -> Result<bool> {
    if max_in_flight == 0 {
        return Ok(true);
    }
    let class = if resource_class.trim().is_empty() {
        EVENT_RESOURCE_CLASS_NETWORK
    } else {
        resource_class.trim()
    };
    let n = event_deliveries::Entity::find()
        .filter(event_deliveries::Column::PluginId.eq(plugin_id))
        .filter(event_deliveries::Column::State.eq(STATE_RUNNING))
        .filter(event_deliveries::Column::ResourceClass.eq(class))
        .count(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(n >= u64::from(max_in_flight))
}

/// Empty is allowed; otherwise the same grammar as a plugin id.
fn validate_event_source(source: &str) -> Result<()> {
    if source.is_empty() {
        return Ok(());
    }
    if source != source.trim() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "event source `{source}` must not have leading or trailing whitespace"
        )));
    }
    if source.len() < 2 || source.len() > 32 {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "event source `{source}` must be 2–32 characters"
        )));
    }
    if !source
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "event source `{source}` must be lowercase ascii letters, digits, or `_`"
        )));
    }
    if source.starts_with('_') || source.ends_with('_') || source.contains("__") {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "event source `{source}` must not start/end with `_` or contain `__`"
        )));
    }
    Ok(())
}

fn validate_wake_event_type(event_type: &str) -> Result<()> {
    if event_type.len() > 64
        || event_type.is_empty()
        || !event_type
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "wake event type `{event_type}` must be 1–64 lowercase ascii letters, digits, or `_`"
        )));
    }
    Ok(())
}

fn map_event(m: domain_events::Model) -> Result<DomainEventRecord> {
    Ok(DomainEventRecord {
        id: m.id,
        event_type: m.event_type,
        schema_version: m.schema_version,
        occurred_at: parse_dt(&m.occurred_at),
        account_id: m.account_id,
        source: m.source,
        correlation_id: m.correlation_id,
        causation_id: m.causation_id,
        dedup_key: m.dedup_key,
        payload: m.payload,
        ordering_key: m.ordering_key,
        dispatch_state: m.dispatch_state,
        created_at: parse_dt(&m.created_at),
        wake_pending: m.wake_pending != 0,
    })
}

fn map_delivery(m: event_deliveries::Model) -> Result<EventDeliveryRecord> {
    Ok(EventDeliveryRecord {
        id: m.id,
        event_id: m.event_id,
        plugin_id: m.plugin_id,
        idempotency_key: m.idempotency_key,
        state: m.state,
        attempt_count: m.attempt_count,
        max_attempts: m.max_attempts,
        lease_owner: m.lease_owner,
        lease_expires_at: parse_dt_opt(m.lease_expires_at.as_deref()),
        lease_generation: m.lease_generation,
        run_after: parse_dt(&m.run_after),
        invocation_sequence: m.invocation_sequence,
        resume_pending: m.resume_pending != 0,
        checkpoint_json: m.checkpoint_json,
        checkpoint_schema_version: m.checkpoint_schema_version,
        ordering_key: m.ordering_key,
        outcome: m.outcome,
        error_message: m.error_message,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
        cancel_requested: m.cancel_requested != 0,
        resource_class: if m.resource_class.trim().is_empty() {
            EVENT_RESOURCE_CLASS_NETWORK.into()
        } else {
            m.resource_class
        },
        wake_event_type: m.wake_event_type,
        wake_filter_json: m.wake_filter_json,
        wake_grants_json: m.wake_grants_json,
    })
}

fn map_node_row(m: event_subscriber_nodes::Model) -> Result<EventSubscriberNodeRecord> {
    let subscriptions: Vec<EventCatalogSubscription> =
        serde_json::from_str(&m.subscriptions_json).unwrap_or_default();
    Ok(EventSubscriberNodeRecord {
        node_id: m.node_id,
        plugin_id: m.plugin_id,
        subscriptions,
        enabled: m.enabled != 0,
        heartbeat_at: parse_dt(&m.heartbeat_at),
    })
}

async fn node_plugin_subscriptions<C: ConnectionTrait>(
    db: &C,
    node_id: &str,
    plugin_id: &str,
) -> Result<Option<Vec<EventCatalogSubscription>>> {
    let Some(row) = event_subscriber_nodes::Entity::find()
        .filter(event_subscriber_nodes::Column::NodeId.eq(node_id))
        .filter(event_subscriber_nodes::Column::PluginId.eq(plugin_id))
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    if row.enabled == 0 {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_str(&row.subscriptions_json).unwrap_or_default(),
    ))
}

async fn delivery_model_matches_node_catalog<C: ConnectionTrait>(
    db: &C,
    node_id: &str,
    model: &event_deliveries::Model,
) -> Result<bool> {
    let Some(event_row) = domain_events::Entity::find_by_id(&model.event_id)
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(false);
    };
    let event = map_event(event_row)?;
    let Some(subs) = node_plugin_subscriptions(db, node_id, &model.plugin_id).await? else {
        return Ok(false);
    };
    Ok(subs.iter().any(|s| subscription_matches_event(s, &event)))
}

async fn ensure_event_outbox_stats<C: ConnectionTrait>(
    db: &C,
) -> Result<event_outbox_stats::Model> {
    if let Some(row) = event_outbox_stats::Entity::find_by_id(EVENT_OUTBOX_STATS_ID)
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
    {
        return Ok(row);
    }
    let model = event_outbox_stats::ActiveModel {
        id: Set(EVENT_OUTBOX_STATS_ID),
        retries_total: Set(0),
        suspensions_total: Set(0),
        dead_letters_total: Set(0),
        dispatch_latency_ms_sum: Set(0),
        dispatch_count: Set(0),
        handler_latency_ms_sum: Set(0),
        handler_count: Set(0),
    };
    match event_outbox_stats::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(event_outbox_stats::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec(db)
        .await
    {
        Ok(_) => {}
        Err(err) if is_unique_violation(&err) => {}
        Err(err) => return Err(LibraryError::Orm(err)),
    }
    event_outbox_stats::Entity::find_by_id(EVENT_OUTBOX_STATS_ID)
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
        .ok_or_else(|| LibraryError::Other(anyhow!("event_outbox_stats singleton missing")))
}

async fn bump_event_stats<C: ConnectionTrait>(
    db: &C,
    retries: i64,
    suspensions: i64,
    dead_letters: i64,
    dispatch_latency_ms: Option<i64>,
    handler_latency_ms: Option<i64>,
) -> Result<()> {
    let _ = ensure_event_outbox_stats(db).await?;
    let backend = db.get_database_backend();
    let (dispatch_sum, dispatch_n) = match dispatch_latency_ms {
        Some(ms) => (ms, 1i64),
        None => (0, 0),
    };
    let (handler_sum, handler_n) = match handler_latency_ms {
        Some(ms) => (ms, 1i64),
        None => (0, 0),
    };
    db.execute_raw(statement_for_backend(
        backend,
        "UPDATE event_outbox_stats SET \
            retries_total = retries_total + ?, \
            suspensions_total = suspensions_total + ?, \
            dead_letters_total = dead_letters_total + ?, \
            dispatch_latency_ms_sum = dispatch_latency_ms_sum + ?, \
            dispatch_count = dispatch_count + ?, \
            handler_latency_ms_sum = handler_latency_ms_sum + ?, \
            handler_count = handler_count + ? \
         WHERE id = ?",
        [
            retries.into(),
            suspensions.into(),
            dead_letters.into(),
            dispatch_sum.into(),
            dispatch_n.into(),
            handler_sum.into(),
            handler_n.into(),
            EVENT_OUTBOX_STATS_ID.into(),
        ],
    ))
    .await
    .map_err(LibraryError::Orm)?;
    Ok(())
}

async fn sanitize_unknown_event_resource_class_on<C: ConnectionTrait>(
    db: &C,
    now_s: &str,
) -> Result<()> {
    let suspects = event_deliveries::Entity::find()
        .filter(event_deliveries::Column::State.eq(STATE_PENDING))
        .filter(event_deliveries::Column::ResourceClass.ne(EVENT_RESOURCE_CLASS_NETWORK))
        .filter(event_deliveries::Column::ResourceClass.ne(""))
        .limit(32)
        .all(db)
        .await
        .map_err(LibraryError::Orm)?;
    for model in suspects {
        let reason = format!("unknown event resource class `{}`", model.resource_class);
        let _ = event_deliveries::Entity::update_many()
            .col_expr(
                event_deliveries::Column::State,
                sea_orm::sea_query::Expr::value(STATE_REJECTED),
            )
            .col_expr(
                event_deliveries::Column::Outcome,
                sea_orm::sea_query::Expr::value(Some("reject".to_string())),
            )
            .col_expr(
                event_deliveries::Column::ErrorMessage,
                sea_orm::sea_query::Expr::value(Some(reason)),
            )
            .col_expr(
                event_deliveries::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now_s.to_string()),
            )
            .filter(event_deliveries::Column::Id.eq(&model.id))
            .filter(event_deliveries::Column::State.eq(STATE_PENDING))
            .exec(db)
            .await
            .map_err(LibraryError::Orm)?;
    }
    Ok(())
}

#[cfg(test)]
mod placeholder_tests {
    use super::*;

    #[test]
    fn postgres_rewrites_question_marks_in_order() {
        assert_eq!(
            rewrite_sql_placeholders(DatabaseBackend::Postgres, "a = ? AND b IN (?, ?) LIMIT ?"),
            "a = $1 AND b IN ($2, $3) LIMIT $4"
        );
        assert_eq!(
            rewrite_sql_placeholders(DatabaseBackend::Sqlite, "a = ? AND b IN (?, ?)"),
            "a = ? AND b IN (?, ?)"
        );
    }

    #[test]
    fn wake_fenced_update_full_page_stays_under_d1_bind_limit() {
        let cap = usize::try_from(bookclerk_plugin_abi::D1_MAX_BINDS).unwrap_or(0);
        let page = usize::try_from(crate::wake_page_for_max_binds(
            bookclerk_plugin_abi::D1_MAX_BINDS,
        ))
        .unwrap_or(0);
        let sql = wake_fenced_update_sql(page);
        let binds = sql.matches('?').count();
        assert_eq!(binds, wake_delivery_update_bind_count(page));
        assert!(
            binds <= cap,
            "full-page wake UPDATE bind count {binds} exceeds D1's {cap}"
        );
        assert!(
            page <= cap,
            "wake page {page} exceeds D1's {cap} for parent SELECT IN"
        );
        assert!(wake_in_chunk_size(cap) + WAKE_UPDATE_FIXED_BINDS <= cap);
    }
}
