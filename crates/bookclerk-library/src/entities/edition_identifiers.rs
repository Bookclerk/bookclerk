//! SeaORM entity for the `edition_identifiers` table (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "edition_identifiers")]
pub struct Model {
    /// The edition it names.
    #[sea_orm(primary_key, auto_increment = false)]
    pub edition_id: String,
    /// Identifier scheme (`isbn`, `asin`, `openlibrary_edition`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub scheme: String,
    /// Normalised identifier value.
    #[sea_orm(primary_key, auto_increment = false)]
    pub value: String,
    /// Lower-cased marketplace for a scoped scheme (ASIN); empty otherwise.
    #[sea_orm(primary_key, auto_increment = false)]
    pub marketplace: String,
    /// `strong` (names one edition) or `weak` (shared, only proposes a match).
    pub strength: String,
    /// Unique key of a strong identifier; `None` for a weak one.
    pub strong_key: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
