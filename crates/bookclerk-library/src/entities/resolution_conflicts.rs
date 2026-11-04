//! SeaORM entity for the `resolution_conflicts` table (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "resolution_conflicts")]
pub struct Model {
    /// The listing whose resolution conflicted.
    #[sea_orm(primary_key, auto_increment = false)]
    pub listing_id: String,
    /// `strong_identifiers`, `work_identifiers`, `ambiguous_work` or `work_kind_mismatch`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub kind: String,
    /// JSON array of the edition or work ids involved.
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_ids: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
