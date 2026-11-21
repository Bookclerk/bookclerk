//! SeaORM entity for `notify_deliveries`: the delivery log, one row per
//! notification and destination.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_deliveries")]
pub struct Model {
    /// Random UUID; the `notify_send` job's payload names it.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The notification sent.
    pub notification_id: String,
    /// Where it goes.
    pub destination_id: String,
    /// Notifier instance that sends it (the destination's, when routed).
    pub plugin_instance_id: String,
    /// `pending`, `delivered`, `failed` or `dropped`.
    pub state: String,
    /// Send attempts made.
    pub attempts: i64,
    /// The service's id for the delivered message.
    pub provider_id: Option<String>,
    /// Last failure or retry reason, one sanitised line.
    pub reason: Option<String>,
    /// The `notify_send` job enqueued for it; `None` until enqueued.
    pub job_id: Option<String>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
