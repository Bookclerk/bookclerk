//! SeaORM entity for the `holdings` table (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "holdings")]
pub struct Model {
    /// The listing held.
    #[sea_orm(primary_key, auto_increment = false)]
    pub listing_id: String,
    /// `account:<account_id>` or `node:<node_id>`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub holder: String,
    /// `purchase`, `plus`, `loan` or `free`.
    pub entitlement: String,
    /// RFC 3339 time it was bought or added, when known.
    pub purchased_at: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
