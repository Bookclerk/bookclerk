//! `oidc_auth_codes` — short-lived authorization codes (PKCE).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "oidc_auth_codes")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub code_hash: String,
    pub client_id: String,
    pub user_id: i64,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub scope: String,
    pub expires_at: String,
    pub consumed_at: Option<String>,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
