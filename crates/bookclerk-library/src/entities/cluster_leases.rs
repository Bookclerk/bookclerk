//! SeaORM entity for `cluster_leases`: singleton maintenance leases.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "cluster_leases")]
pub struct Model {
    /// Lease name (`scheduler`, …).
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
    /// Current holder (a host id).
    pub holder: String,
    /// Fencing token; bumps whenever the holder changes.
    pub token: i64,
    /// Expiry in Unix milliseconds.
    pub expires_at_ms: i64,
    /// RFC 3339 time of the last acquire or renew.
    pub renewed_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
