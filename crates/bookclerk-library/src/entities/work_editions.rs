//! SeaORM entity for the `work_editions` table entity (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "work_editions")]
pub struct Model {
    /// Canonical work id this edition or request resolves to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// Foreign key to `books.uuid`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub book_uuid: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
