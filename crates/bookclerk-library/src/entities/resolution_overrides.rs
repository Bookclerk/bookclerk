//! SeaORM entity for the `resolution_overrides` table (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "resolution_overrides")]
pub struct Model {
    /// `listing` or `work`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_kind: String,
    /// Id of the listing or work.
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_id: String,
    /// `pin_edition` (with `target_id`) or `isolate`.
    pub action: String,
    /// The edition a pinned listing sells.
    pub target_id: Option<String>,
    /// Administrator who made the decision.
    pub created_by_user_id: Option<i64>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
