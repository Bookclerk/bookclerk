//! SeaORM entity for the `credits` table (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "credits")]
pub struct Model {
    /// Level of the target: `work`, `expression` or `edition`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub target_kind: String,
    /// Id of the work, expression or edition.
    #[sea_orm(primary_key, auto_increment = false)]
    pub target_id: String,
    /// The credited contributor.
    #[sea_orm(primary_key, auto_increment = false)]
    pub contributor_id: String,
    /// What they did (`author`, `narrator`, …).
    #[sea_orm(primary_key, auto_increment = false)]
    pub role: String,
    /// Billing position within the target, from 0.
    pub position: i64,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
