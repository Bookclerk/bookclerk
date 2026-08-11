//! `oidc_refresh_tokens` — refresh tokens bound to a User.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "oidc_refresh_tokens")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub token_hash: String,
    pub client_id: String,
    pub user_id: i64,
    pub scope: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
