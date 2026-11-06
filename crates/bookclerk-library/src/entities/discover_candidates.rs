//! SeaORM entity for the `discover_candidates` table.

use sea_orm::entity::prelude::*;

/// One storefront title fetched for one Discover subject (`DeriveEntityModel`).
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
    /// Identity key of the candidate within its subject and store (ASIN / ISBN / product key).
    pub candidate_key: String,
    /// Which seeds produced the row: `own` (the subject's taste) or `household`.
    pub seed_scope: String,
    /// The candidate itself as JSON (`bookclerk_discover::StorefrontCandidate`).
    pub candidate_json: String,
    /// RFC 3339 timestamp of the store fetch that produced the row.
    pub fetched_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
