//! SeaORM entity for the `discover_store_call_hits` table (composite primary key).

use sea_orm::entity::prelude::*;

/// One listing a cached storefront call answered (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "discover_store_call_hits")]
pub struct Model {
    /// The cached call (`discover_store_calls.call_key`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub call_key: String,
    /// 0-based position in the store's answer.
    #[sea_orm(primary_key, auto_increment = false)]
    pub position: i64,
    /// The `store_listings` row the hit resolved to.
    pub listing_id: String,
    /// How the store found it for this call (`related to …`), as it said.
    pub origin: String,
    /// The store's own 1-based rank for the hit, when it gave one.
    pub rank_hint: Option<i64>,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
