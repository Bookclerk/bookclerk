//! `account_links` table entity.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_links")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub identity_id: i64,
    pub account_id: String,
    pub source: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
