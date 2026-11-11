//! SeaORM entity for the `discover_request_queue` table.

use sea_orm::entity::prelude::*;

/// The stored taste ranking of one entry of the shared request queue
/// (`DeriveEntityModel`).
///
/// The row holds only what the ranking job computed. Titles, covers, wish
/// counts and wishers stay on the open `title_requests` rows and are joined
/// when the queue is read, so a new wish shows at once and nothing here
/// copies catalog metadata.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "discover_request_queue")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// The queue entry the row ranks: `work:<work id>` when the entry's wishes
    /// resolved to a library work, otherwise the entry's `work_key`.
    #[sea_orm(unique)]
    pub entry_key: String,
    /// Library work (`works.id`) the entry's wishes resolved to, when any.
    pub work_id: Option<String>,
    /// 1 when the library already holds the entry's work, so the queue hides it.
    pub owned: i64,
    /// Household taste score of the entry before the wish-count boost
    /// (finite; 0 when nothing in the library matches it).
    pub taste_score: f64,
    /// Taste reasons as a JSON array of strings; wish-count reasons are added at read time.
    pub reasons_json: String,
    /// RFC 3339 timestamp of the ranking pass that wrote the row.
    pub ranked_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
