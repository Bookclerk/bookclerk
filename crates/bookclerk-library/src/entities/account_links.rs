//! SeaORM entity for the `account_links` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_links")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: i64,
    /// Provenance-qualified PluginKey that owns the account.
    pub plugin_key: String,
    /// Store or operator account id this row belongs to.
    pub account_id: String,
    /// Display alias of the owning plugin (`audible`, `libro`, …).
    pub source: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
