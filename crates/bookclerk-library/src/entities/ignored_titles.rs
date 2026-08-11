//! SeaORM entity for the `ignored_titles` table entity (composite primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "ignored_titles")]
pub struct Model {
    /// Content-source plugin id (`audible`, `libro`, …).
    #[sea_orm(primary_key, auto_increment = false)]
    pub source: String,
    /// Store or operator account id this row belongs to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub account_id: String,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    #[sea_orm(primary_key, auto_increment = false)]
    pub product_id: String,
    /// Why the title was ignored (operator note).
    pub reason: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
