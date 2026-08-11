//! `operator_sessions` table entity.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "operator_sessions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub token_hash: String,
    pub expires_at: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    /// Set when an Administrator elevates to Operator (Phase 2).
    pub elevated_from_user_id: Option<i64>,
    /// Operator impersonation target user id (Phase 2).
    pub impersonating_user_id: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
