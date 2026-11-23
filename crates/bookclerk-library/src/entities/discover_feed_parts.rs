//! SeaORM entity for the `discover_feed_parts` table (composite primary key).

use sea_orm::entity::prelude::*;

/// One piece of a stored Discover feed too wide for one statement or cell
/// (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "discover_feed_parts")]
pub struct Model {
    /// Discover subject the feed belongs to (`discover_feeds.subject_key`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_key: String,
    /// The write this piece belongs to; `discover_feeds.feed_version` names the one to read.
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: String,
    /// The piece's place in the feed, from 0.
    #[sea_orm(primary_key, auto_increment = false)]
    pub seq: i64,
    /// The piece of the feed's JSON text, cut on a character boundary.
    pub body: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
