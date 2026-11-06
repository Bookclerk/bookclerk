//! SeaORM entity for the `discover_store_calls` table.

use sea_orm::entity::prelude::*;

/// One cached storefront answer shared by every Discover subject (`DeriveEntityModel`).
///
/// Two people whose seeds ask a store the same question inside the cache
/// window reuse one call.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "discover_store_calls")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// SHA-256 hex of the store, call kind and query; unique per question.
    #[sea_orm(unique)]
    pub call_key: String,
    /// Storefront plugin id that answered.
    pub source: String,
    /// The store's catalog hits as a JSON array.
    pub response_json: String,
    /// RFC 3339 timestamp of the call.
    pub fetched_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
