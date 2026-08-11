//! `portal_identities` table entity.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "portal_identities")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub provider: String,
    pub external_user_id: String,
    pub label: Option<String>,
    /// First-party [`crate::entities::users`] row (Phase 1).
    pub user_id: Option<i64>,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
