//! SeaORM entity for the `accounts` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "accounts")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Store or operator account id this row belongs to.
    #[sea_orm(unique)]
    pub account_id: String,
    /// Store marketplace / locale code (for example `us`, `uk`).
    pub marketplace: String,
    /// Optional operator-facing display label.
    pub label: Option<String>,
    /// When nonzero/true, scheduled scans include this account.
    pub scan_enabled: i64,
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Credential health: `active` or `revoked` (books retained).
    pub connection_status: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
