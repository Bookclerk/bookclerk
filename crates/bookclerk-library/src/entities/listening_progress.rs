//! SeaORM entity for the `listening_progress` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "listening_progress")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// External identity or integration provider id (for example ABS).
    pub provider: String,
    /// User id at the external provider.
    pub external_user_id: String,
    /// Foreign key to `books.uuid`.
    pub book_uuid: Option<String>,
    /// Canonical work id this edition or request resolves to.
    pub work_id: Option<String>,
    /// Provider-native listening-progress item id.
    pub external_item_id: String,
    /// Display title of the work or edition.
    pub title: Option<String>,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Amazon ASIN when known; otherwise null.
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    pub isbn: Option<String>,
    /// Fractional progress 0.0–1.0 when the provider reports it.
    pub progress: Option<f64>,
    /// Current playback position within the title, in seconds.
    pub current_time_seconds: Option<f64>,
    /// Total duration in seconds when known.
    pub duration_seconds: Option<f64>,
    /// Whether the listener marked the title finished (0/1 or bool).
    pub is_finished: i64,
    /// RFC 3339 time of the last playback update.
    pub last_listened_at: Option<String>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
