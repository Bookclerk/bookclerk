//! `user_preferences` table entity.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "user_preferences")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub subject_key: String,
    pub identity_id: Option<i64>,
    pub default_view: String,
    pub disabled_shelves_json: String,
    pub discover_sort: String,
    pub discover_sort_dir: String,
    pub discover_language: Option<String>,
    pub discover_excluded_sources_json: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
