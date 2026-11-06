//! SeaORM entity for the `discover_feeds` table.

use sea_orm::entity::prelude::*;

/// The stored Discover shelves of one subject (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "discover_feeds")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Discover subject (`operator`, `user:<id>`, `portal:<id>`); one feed each.
    #[sea_orm(unique)]
    pub subject_key: String,
    /// The ranked feed as JSON (`bookclerk_discover::DiscoverFeed`), before hidden shelves are removed.
    pub feed_json: String,
    /// Which seeds the feed was ranked from: `own` or `household`.
    pub seed_scope: String,
    /// RFC 3339 timestamp of the newest candidate fetch the feed used, when any.
    pub candidates_fetched_at: Option<String>,
    /// RFC 3339 timestamp when the feed was ranked.
    pub computed_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
