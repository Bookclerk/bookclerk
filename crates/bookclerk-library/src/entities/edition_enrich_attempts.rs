//! SeaORM entity for the `edition_enrich_attempts` table (composite primary key).

use sea_orm::entity::prelude::*;

/// One metadata gap-fill attempt that left an edition's gaps open.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "edition_enrich_attempts")]
pub struct Model {
    /// The edition asked about (`editions.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub edition_id: String,
    /// Which enricher asked (such as `metadata`, the metadata providers).
    #[sea_orm(primary_key, auto_increment = false)]
    pub enricher: String,
    /// How the attempt ended: `partial` or `unmatched`
    /// (`crate::EnrichAttemptOutcome`).
    pub outcome: String,
    /// RFC 3339 time of the attempt.
    pub attempted_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
