//! SeaORM entity for `window_books`: on a window (a link with "Use this
//! computer to download" off), the books each library holds for this
//! install's person (`control_plane::window_books`).

use sea_orm::entity::prelude::*;

/// One edition a library holds for this install's person.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "window_books")]
pub struct Model {
    /// The library (owner cluster id) that holds it.
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
    /// The library's own key grouping its editions of one work; only
    /// compared within that library.
    pub work_ref: String,
    /// The work of this install's library the identifiers reach, when
    /// exactly one does.
    pub local_work_id: Option<String>,
    /// The `window_book_parts` version holding this edition's details.
    pub parts_version: String,
    /// RFC 3339 stamp of the read that last named this edition.
    pub seen_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
