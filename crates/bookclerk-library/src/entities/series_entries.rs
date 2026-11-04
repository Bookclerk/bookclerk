//! SeaORM entity for the `series_entries` table (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "series_entries")]
pub struct Model {
    /// The series.
    #[sea_orm(primary_key, auto_increment = false)]
    pub series_id: String,
    /// The work in it.
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// Position label as reported (`1`, `2.5`, `1-3`).
    pub position: Option<String>,
    /// The label's leading number, for ordering.
    pub sort_position: Option<f64>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
