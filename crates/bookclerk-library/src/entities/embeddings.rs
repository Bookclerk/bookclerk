//! `embeddings` table entity.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "embeddings")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub target_kind: String,
    pub target_id: String,
    pub model: String,
    pub dims: i64,
    pub vector: Vec<u8>,
    pub text_hash: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
