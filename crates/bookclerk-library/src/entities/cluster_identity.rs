//! SeaORM entity for the singleton cluster identity and secret-root fingerprint.

use sea_orm::entity::prelude::*;

/// Row shape for the singleton `cluster_identity` table.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "cluster_identity")]
pub struct Model {
    /// Singleton primary key. Application code inserts `1`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    /// Stable cluster identifier shared by every host of this database.
    pub cluster_id: String,
    /// SHA-256 hex of the cluster data-encryption key (not the wrapped file).
    pub secret_fingerprint: String,
    /// Schema state display recorded when the row was inserted.
    pub schema_state: String,
    /// RFC 3339 timestamp when the cluster row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (this table is a singleton).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
