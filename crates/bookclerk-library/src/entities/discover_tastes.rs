//! SeaORM entity for the `discover_tastes` table.

use sea_orm::entity::prelude::*;

/// The stored Discover taste of one subject (`DeriveEntityModel`): what the
/// subject's books, listening and wishes say they like, computed by the
/// candidate fetch so a ranking reads it instead of the whole library.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "discover_tastes")]
pub struct Model {
    /// Discover subject (`operator`, `user:<id>`, `portal:<id>`); one taste each.
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_key: String,
    /// The taste as JSON (`bookclerk_discover::StoredTaste`).
    pub taste_json: String,
    /// Which seeds the taste comes from: `own` or `household`.
    pub seed_scope: String,
    /// RFC 3339 timestamp when the taste was computed.
    pub computed_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
