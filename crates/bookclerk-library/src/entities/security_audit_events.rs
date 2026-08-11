//! `security_audit_events` table entity — elevate / impersonate / login audit.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "security_audit_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub at: String,
    pub actor: String,
    pub action: String,
    pub detail_json: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
