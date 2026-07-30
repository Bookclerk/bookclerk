//! `work_editions` table entity (composite primary key).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "work_editions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub book_uuid: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
