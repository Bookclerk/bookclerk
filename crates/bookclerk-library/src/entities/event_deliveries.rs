//! SeaORM entity for per-subscriber `event_deliveries`.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "event_deliveries")]
pub struct Model {
    /// Stable delivery id (`{event_id}:{plugin_id}`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Parent [`super::domain_events`] id.
    pub event_id: String,
    /// Subscriber plugin id.
    pub plugin_id: String,
    /// Stable idempotency key independent of attempt/generation.
    pub idempotency_key: String,
    /// Lifecycle state (`pending`, `running`, `acked`, `rejected`, `dead_letter`).
    pub state: String,
    /// Number of times a worker has claimed this row (not incremented on resume).
    pub attempt_count: i64,
    /// Maximum claims before a failure becomes `dead_letter`.
    pub max_attempts: i64,
    /// Worker id that currently holds the lease, when `state = running`.
    pub lease_owner: Option<String>,
    /// RFC 3339 lease expiry.
    pub lease_expires_at: Option<String>,
    /// Per-claim generation used to fence heartbeat and finalization.
    pub lease_generation: i64,
    /// RFC 3339 time after which a pending delivery may be claimed.
    pub run_after: String,
    /// Resume ordinal distinct from [`Self::attempt_count`].
    pub invocation_sequence: i64,
    /// `1` when the next claim should resume rather than increment attempt.
    pub resume_pending: i64,
    /// Bounded checkpoint JSON from `EventResult::Suspended`.
    pub checkpoint_json: Option<String>,
    /// Checkpoint schema version.
    pub checkpoint_schema_version: i64,
    /// FIFO key (`book uuid` for `book_acquired`).
    pub ordering_key: String,
    /// Terminal outcome (`ack`, `reject`, `dead_letter`) when finished.
    pub outcome: Option<String>,
    /// Operator-facing error or reject reason.
    pub error_message: Option<String>,
    /// RFC 3339 insert time.
    pub created_at: String,
    /// RFC 3339 last-modified time.
    pub updated_at: String,
    /// Cooperative cancel flag (`0` / `1`); running workers check on heartbeat.
    pub cancel_requested: i64,
    /// Concurrency class copied from the catalog (`network` today).
    pub resource_class: String,
    /// Event type that can wake this suspended delivery; empty = timestamp-only.
    pub wake_event_type: String,
    /// Host-owned payload object filter JSON for event-wake; empty = type only.
    pub wake_filter_json: String,
    /// Host-derived wake grants JSON (`schemaVersions` + intersected filter).
    pub wake_grants_json: String,
}

/// Declared SeaORM relations (parent event is queried by `event_id`).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
