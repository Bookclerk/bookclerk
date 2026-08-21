//! SeaORM entity for the singleton durable event-outbox counters.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "event_outbox_stats")]
pub struct Model {
    /// Singleton id; always `1`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    /// Persisted `retry` outcomes (including reclaim-as-retry).
    pub retries_total: i64,
    /// Persisted `suspended` outcomes.
    pub suspensions_total: i64,
    /// Transitions into `dead_letter`.
    pub dead_letters_total: i64,
    /// Sum of first-dispatch latencies in milliseconds.
    pub dispatch_latency_ms_sum: i64,
    /// Number of first-dispatch samples.
    pub dispatch_count: i64,
    /// Sum of `onEvent` handler durations in milliseconds.
    pub handler_latency_ms_sum: i64,
    /// Number of handler-duration samples.
    pub handler_count: i64,
}

/// Declared SeaORM relations (this table is a singleton).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
