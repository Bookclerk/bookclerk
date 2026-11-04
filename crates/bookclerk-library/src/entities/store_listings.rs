//! SeaORM entity for the `store_listings` table (text primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "store_listings")]
pub struct Model {
    /// Listing id (a random UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Provenance-qualified PluginKey of the storefront.
    pub plugin_key: String,
    /// Lower-cased marketplace.
    pub marketplace: String,
    /// The store's own product id.
    pub product_id: String,
    /// The edition it sells.
    pub edition_id: String,
    /// Title as the store lists it.
    pub title: String,
    /// Store category labels when reported.
    pub categories: Option<String>,
    /// RFC 3339 time of the latest scan that reported it.
    pub observed_at: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
