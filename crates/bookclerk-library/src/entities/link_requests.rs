//! SeaORM entity for `link_requests`: on a branch, the books a library it
//! joined asked for through an interlibrary request (written by
//! `control_plane::branch_requests`). The catalog sync sends that library
//! a book recorded here for it, although the book is a member's.

use sea_orm::entity::prelude::*;

/// One requested book, for one library.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "link_requests")]
pub struct Model {
    /// Surrogate primary key; the keyset the catalog sync pages by.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// The library that asked for the book.
    pub cluster_id: String,
    /// Provenance-qualified plugin key of the store.
    pub plugin_key: String,
    /// Store account here that holds the book and fulfils the request.
    pub account_id: String,
    /// Store marketplace, as `store_listings` stores it.
    pub marketplace: String,
    /// Store product id, as `store_listings` stores it.
    pub product_id: String,
    /// RFC 3339 time of the first request from that library.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
