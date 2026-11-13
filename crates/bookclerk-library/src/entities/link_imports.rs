//! SeaORM entity for `link_imports`: on a branch, the branch owner's books
//! acquired for a library it joined and imported into this library as well
//! (written by `control_plane::link_imports`). The catalog sync never sends
//! a library a book recorded here for it.

use sea_orm::entity::prelude::*;

/// One imported book, for one library.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "link_imports")]
pub struct Model {
    /// Surrogate primary key; the keyset the catalog sync pages by.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// The library the book was acquired for.
    pub cluster_id: String,
    /// Provenance-qualified plugin key of the store.
    pub plugin_key: String,
    /// Store account that holds the book.
    pub account_id: String,
    /// Store marketplace, as `store_listings` stores it.
    pub marketplace: String,
    /// Store product id, as `store_listings` stores it.
    pub product_id: String,
    /// RFC 3339 time of the first import from that library.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
