//! SeaORM entity for `catalog_listing_optouts`: on a branch, the members
//! whose books it does not list to the libraries it joined.

use sea_orm::entity::prelude::*;

/// One member who opted out.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "catalog_listing_optouts")]
pub struct Model {
    /// The member (`users.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// RFC 3339 time they opted out.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
