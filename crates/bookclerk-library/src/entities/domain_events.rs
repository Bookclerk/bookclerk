//! SeaORM entity for the durable `domain_events` outbox.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "domain_events")]
pub struct Model {
    /// Stable event id (UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Event type (`book_acquired`, …).
    pub event_type: String,
    /// Payload schema version.
    pub schema_version: i64,
    /// RFC 3339 occurrence time.
    pub occurred_at: String,
    /// Tenant / account id, when known.
    pub account_id: String,
    /// Producer plugin id (`audible`, …); empty when unknown.
    pub source: String,
    /// Trace correlation id.
    pub correlation_id: String,
    /// Causing event or job id.
    pub causation_id: String,
    /// Producer idempotency key unique with `(account_id, source, event_type)`.
    pub dedup_key: String,
    /// Bounded JSON payload (never media bytes).
    pub payload: String,
    /// Producer FIFO key copied onto each delivery.
    pub ordering_key: String,
    /// `pending` until deliveries are created, then `dispatched`.
    pub dispatch_state: String,
    /// RFC 3339 insert time.
    pub created_at: String,
    /// `1` until matching suspended deliveries have been woken (or none exist).
    pub wake_pending: i64,
    /// Worker that currently holds the wake-slice lease.
    pub wake_lease_owner: Option<String>,
    /// RFC 3339 wake-slice lease expiry.
    pub wake_lease_expires_at: Option<String>,
    /// `(created_at, id)` cursor for the next wake delivery page.
    pub wake_cursor_at: String,
    /// Delivery id cursor paired with [`Self::wake_cursor_at`].
    pub wake_cursor_id: String,
    /// Frozen JSON array of plugin ids for paged dispatch receipts.
    pub dispatch_snapshot_json: String,
}

/// Declared SeaORM relations (deliveries are queried by `event_id`).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
