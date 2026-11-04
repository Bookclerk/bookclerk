//! SeaORM entity for the `works` table (text primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "works")]
pub struct Model {
    /// Work id (a random UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Kind of work: `book`, `podcast_show`, `podcast_episode` or `drama`.
    pub kind: String,
    /// Display title.
    pub title: String,
    /// The work matcher's key (cleaned title and authors); empty when the work is never matched by text.
    pub match_key: String,
    /// The show a podcast episode belongs to.
    pub parent_work_id: Option<String>,
    /// Blurb or synopsis (may contain HTML) from enrichment.
    pub description: Option<String>,
    /// Subject tags from enrichment (serialized).
    pub subjects: Option<String>,
    /// Category or genre labels (serialized).
    pub categories: Option<String>,
    /// HTTPS URL of cover art when known.
    pub cover_url: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
