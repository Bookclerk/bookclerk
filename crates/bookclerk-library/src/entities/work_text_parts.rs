//! SeaORM entity for the `work_text_parts` table (composite primary key).

use sea_orm::entity::prelude::*;

/// One piece of a work's text too wide for its row on the database plugin
/// (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "work_text_parts")]
pub struct Model {
    /// The work the text belongs to (`works.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// The write this piece belongs to; `works.text_version` names the one
    /// to read.
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: String,
    /// Which text it is: `description`, `subjects` or `categories`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub field: String,
    /// The piece's place in the text, from 0.
    #[sea_orm(primary_key, auto_increment = false)]
    pub seq: i64,
    /// How many pieces the text has; the same on every piece of it.
    pub parts: i64,
    /// The piece of the text, cut on a character boundary.
    pub body: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
