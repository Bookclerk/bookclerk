//! SeaORM entity for the `work_identifiers` table (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "work_identifiers")]
pub struct Model {
    /// Identifier scheme (`openlibrary_work`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub scheme: String,
    /// Normalised identifier value.
    #[sea_orm(primary_key, auto_increment = false)]
    pub value: String,
    /// The work it names.
    pub work_id: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
