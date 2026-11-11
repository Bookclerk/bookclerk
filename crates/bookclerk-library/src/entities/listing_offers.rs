//! SeaORM entity for the `listing_offers` table (composite primary key).

use sea_orm::entity::prelude::*;

/// One price a store asks for one listing, for one audience (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "listing_offers")]
pub struct Model {
    /// The `store_listings` row priced.
    #[sea_orm(primary_key, auto_increment = false)]
    pub listing_id: String,
    /// `non_member` (no account at the store) or `member`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub audience: String,
    /// Price in minor units of [`Self::currency`], when the store gave one.
    pub price_cents: Option<i64>,
    /// The store's formatted price (`$14.95`), when it gave one.
    pub price_label: Option<String>,
    /// ISO 4217 currency code.
    pub currency: Option<String>,
    /// RFC 3339 time the store quoted it.
    pub observed_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
