//! SeaORM entity for the `discover_fetches` table.

use sea_orm::entity::prelude::*;

/// When one subject's last storefront candidate fetch finished
/// (`DeriveEntityModel`), recorded even when it stored no candidates.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "discover_fetches")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Discover subject (`operator`, `user:<id>`, `portal:<id>`); one row each.
    #[sea_orm(unique)]
    pub subject_key: String,
    /// RFC 3339 timestamp when the fetch finished.
    pub fetched_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
