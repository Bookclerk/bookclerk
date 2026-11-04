//! SeaORM entity for the `series` table (text primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "series")]
pub struct Model {
    /// Series id (a random UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Display name as first reported.
    pub name: String,
    /// Cleaned name series are matched by; unique.
    pub name_key: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
