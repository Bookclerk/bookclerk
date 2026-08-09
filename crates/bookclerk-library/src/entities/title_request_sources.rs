//! `title_request_sources` — per-storefront catalog/pricing snapshot for a wish.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "title_request_sources")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub title_request_id: i64,
    pub source: String,
    pub product_id: String,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub length_minutes: Option<i64>,
    pub published_at: Option<String>,
    pub categories: Option<String>,
    pub language: Option<String>,
    pub cover_url: Option<String>,
    pub url: Option<String>,
    pub price_cents: Option<i64>,
    pub currency: Option<String>,
    pub price_label: Option<String>,
    pub list_price_cents: Option<i64>,
    pub list_price_label: Option<String>,
    pub member_price_cents: Option<i64>,
    pub member_price_label: Option<String>,
    pub observed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::title_requests::Entity",
        from = "Column::TitleRequestId",
        to = "super::title_requests::Column::Id",
        on_delete = "Cascade"
    )]
    TitleRequest,
}

impl Related<super::title_requests::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::TitleRequest.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
