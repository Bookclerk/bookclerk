//! `listening_progress` table entity.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "listening_progress")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub identity_id: Option<i64>,
    pub provider: String,
    pub external_user_id: String,
    pub book_uuid: Option<String>,
    pub work_id: Option<String>,
    pub external_item_id: String,
    pub title: Option<String>,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub progress: Option<f64>,
    pub current_time_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    pub is_finished: i64,
    pub last_listened_at: Option<String>,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
