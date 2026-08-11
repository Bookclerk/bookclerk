//! `user_invites` table entity — admin/operator invite tickets with role.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "user_invites")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub token_hash: String,
    pub role: String,
    pub login_name: Option<String>,
    pub display_name: Option<String>,
    pub expires_at: String,
    pub redeemed_at: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
