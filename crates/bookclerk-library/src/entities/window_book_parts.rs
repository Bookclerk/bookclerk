//! SeaORM entity for `window_book_parts`: one piece of a window book's
//! stored edition and identifiers (`control_plane::window_books`).

use sea_orm::entity::prelude::*;

/// One piece of a window book's JSON, at most the database's cell limit.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "window_book_parts")]
pub struct Model {
    /// The library (owner cluster id) that holds the book.
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner_cluster_id: String,
    /// The store's plain id (`audible`, `libro`, ...).
    #[sea_orm(primary_key, auto_increment = false)]
    pub store: String,
    /// Lower-cased store marketplace; empty when the store has none.
    #[sea_orm(primary_key, auto_increment = false)]
    pub marketplace: String,
    /// The store's product id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub product_id: String,
    /// The write that added it (random); the book row names the version
    /// to read.
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: String,
    /// The piece's place, from 0; the pieces joined in this order are the
    /// whole JSON.
    #[sea_orm(primary_key, auto_increment = false)]
    pub seq: i64,
    /// This piece of the JSON text.
    pub body: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
