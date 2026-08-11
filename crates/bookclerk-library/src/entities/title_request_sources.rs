//! SeaORM entity for the `title_request_sources` — per-storefront catalog/pricing snapshot for a wish.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "title_request_sources")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Foreign key to `title_requests.id`.
    pub title_request_id: i64,
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Display title of the work or edition.
    pub title: Option<String>,
    /// Optional subtitle from bibliographic metadata.
    pub subtitle: Option<String>,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    pub series_index: Option<String>,
    /// Amazon ASIN when known; otherwise null.
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    pub isbn: Option<String>,
    /// Blurb / synopsis text (may contain HTML).
    pub description: Option<String>,
    /// Publisher name from metadata enrichment or the storefront.
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports it.
    pub length_minutes: Option<i64>,
    /// Publication date string from the storefront or enrichment.
    pub published_at: Option<String>,
    /// Storefront category / genre path list (serialized).
    pub categories: Option<String>,
    /// BCP-47 or storefront language code when known.
    pub language: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// Storefront product or purchase URL when known.
    pub url: Option<String>,
    /// Observed price in minor currency units, when known.
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for price fields.
    pub currency: Option<String>,
    /// Storefront-formatted price string for display.
    pub price_label: Option<String>,
    /// List/MSRP price in minor units, when known.
    pub list_price_cents: Option<i64>,
    /// Storefront-formatted list price for display.
    pub list_price_label: Option<String>,
    /// Member/subscriber price in minor units, when known.
    pub member_price_cents: Option<i64>,
    /// Storefront-formatted member price for display.
    pub member_price_label: Option<String>,
    /// RFC 3339 time when this storefront snapshot was observed.
    pub observed_at: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::title_requests::Entity",
        from = "Column::TitleRequestId",
        to = "super::title_requests::Column::Id",
        on_delete = "Cascade"
    )]
    /// Parent [`title_requests::Model`](super::title_requests::Model) row for this snapshot.
    TitleRequest,
}

impl Related<super::title_requests::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::TitleRequest.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
