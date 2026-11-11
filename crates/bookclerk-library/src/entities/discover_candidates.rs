//! SeaORM entity for the `discover_candidates` table.

use sea_orm::entity::prelude::*;

/// One storefront title fetched for one Discover subject: a reference to
/// the shared listing plus what is personal to the subject (`DeriveEntityModel`).
///
/// Rows for a `(subject_key, source)` pair are replaced together when that
/// store's fetch for the subject finishes, so one slow store never erases
/// another store's results.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "discover_candidates")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Discover subject the candidate was fetched for (`operator`, `user:<id>`, `portal:<id>`).
    pub subject_key: String,
    /// Storefront plugin id that returned the title (`audible`, `chirp`, …).
    pub source: String,
    /// The `store_listings` row the candidate is (shared by every subject).
    pub listing_id: String,
    /// Which seeds produced the row: `own` (the subject's taste) or `household`.
    pub seed_scope: String,
    /// How the store found it (`related to …`, `deals`), as it said.
    pub origin: String,
    /// The subject's title that seeded it, when a seed did.
    pub seed_title: Option<String>,
    /// Categories of that seed, used to explain and rank the card.
    pub seed_categories: Option<String>,
    /// Starter genre it was browsed from, for a subject with thin taste.
    pub starter_genre: Option<String>,
    /// RFC 3339 timestamp of the store fetch that produced the row.
    pub fetched_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
