//! SeaORM entity for the `editions` table (text primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "editions")]
pub struct Model {
    /// Edition id (a random UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The expression it releases.
    pub expression_id: String,
    /// Form: `audiobook`, `ebook`, `print` or `podcast_audio`.
    pub media_kind: String,
    /// Display title.
    pub title: String,
    /// Subtitle when distinct from the title.
    pub subtitle: Option<String>,
    /// Publication date (RFC 3339) when known.
    pub published_at: Option<String>,
    /// Runtime in whole minutes when known.
    pub duration_minutes: Option<i64>,
    /// HTTPS URL of cover art when known.
    pub cover_url: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
    /// Who filled the edition's (and its work's) missing metadata:
    /// `metadata:<plugin key>` for a metadata provider, `audible` for the
    /// Audible catalog match; `None` until an enricher filled something.
    pub enrich_source: Option<String>,
    /// How sure that enricher was of its match, from 0.0 to 1.0.
    pub enrich_confidence: Option<f64>,
    /// When the enricher last filled it (RFC 3339).
    pub enrich_updated_at: Option<String>,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
