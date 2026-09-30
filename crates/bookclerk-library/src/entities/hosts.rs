//! SeaORM entity for durable host identity and liveness observations.

use sea_orm::entity::prelude::*;

/// Row shape for one Bookclerk host in the cluster.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "hosts")]
pub struct Model {
    /// Stable host identifier. Not derived from hostname.
    #[sea_orm(primary_key, auto_increment = false)]
    pub host_id: String,
    /// Cluster this host enrolled into.
    pub cluster_id: String,
    /// RFC 3339 timestamp of the first successful registration.
    pub created_at: String,
    /// Latest process incarnation observed for this host.
    pub incarnation: String,
    /// RFC 3339 timestamp of the latest heartbeat.
    pub heartbeat_at: String,
    /// Bookclerk version string reported by the latest heartbeat.
    pub software_version: String,
    /// Schema state display reported by the latest heartbeat.
    pub schema_state: String,
    /// `1` when the reporting binary accepted the library schema.
    pub compatible: i64,
}

/// Declared SeaORM relations (hosts are addressed by `host_id`).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
