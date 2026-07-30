//! `books` table entity.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "books")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub uuid: String,
    pub source: String,
    pub account_id: String,
    pub product_id: String,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub marketplace: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub series_asin: Option<String>,
    pub acquire_status: String,
    pub storage_key: Option<String>,
    pub error_message: Option<String>,
    pub purchased_at: Option<String>,
    pub tags: Option<String>,
    pub rating_overall: Option<f64>,
    pub rating_performance: Option<f64>,
    pub rating_story: Option<f64>,
    pub is_finished: i64,
    pub pdf_status: String,
    pub pdf_storage_key: Option<String>,
    pub publisher: Option<String>,
    pub length_minutes: Option<i64>,
    pub is_abridged: i64,
    pub content_kind: String,
    pub categories: Option<String>,
    pub subtitle: Option<String>,
    pub published_at: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub cover_url: Option<String>,
    pub subjects: Option<String>,
    pub enrich_source: Option<String>,
    pub enrich_confidence: Option<f64>,
    pub enrich_updated_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
