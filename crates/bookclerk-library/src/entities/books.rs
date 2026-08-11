//! SeaORM entity for the `books` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "books")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Stable UUID for this row (API / foreign-key identity).
    #[sea_orm(unique)]
    pub uuid: String,
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Store or operator account id this row belongs to.
    pub account_id: String,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Amazon ASIN when known; otherwise null.
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    pub isbn: Option<String>,
    /// Store marketplace / locale code (for example `us`, `uk`).
    pub marketplace: String,
    /// Display title of the work or edition.
    pub title: String,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    pub series_index: Option<String>,
    /// Amazon series ASIN when the storefront exposes one.
    pub series_asin: Option<String>,
    /// Download/acquire pipeline state (`not_acquired`, `queued`, …).
    pub acquire_status: String,
    /// Object-storage key for the primary audio artifact, if acquired.
    pub storage_key: Option<String>,
    /// Last acquire/convert failure message for operators.
    pub error_message: Option<String>,
    /// RFC 3339 purchase time from the storefront, when known.
    pub purchased_at: Option<String>,
    /// Operator or storefront tags (serialized string).
    pub tags: Option<String>,
    /// Overall user rating from the storefront, if any.
    pub rating_overall: Option<f64>,
    /// Narration/performance rating from the storefront, if any.
    pub rating_performance: Option<f64>,
    /// Story rating from the storefront, if any.
    pub rating_story: Option<f64>,
    /// Whether the listener marked the title finished (0/1 or bool).
    pub is_finished: i64,
    /// Companion PDF acquire state (`not_acquired`, `acquired`, …).
    pub pdf_status: String,
    /// Object-storage key for the companion PDF, if present.
    pub pdf_storage_key: Option<String>,
    /// Publisher name from metadata enrichment or the storefront.
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports it.
    pub length_minutes: Option<i64>,
    /// Whether the edition is abridged (0/1 or bool).
    pub is_abridged: i64,
    /// Title kind: `book`, `episode`, `podcast`, ….
    pub content_kind: String,
    /// Storefront category / genre path list (serialized).
    pub categories: Option<String>,
    /// Optional subtitle from bibliographic metadata.
    pub subtitle: Option<String>,
    /// Publication date string from the storefront or enrichment.
    pub published_at: Option<String>,
    /// Blurb / synopsis text (may contain HTML).
    pub description: Option<String>,
    /// BCP-47 or storefront language code when known.
    pub language: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// Subject / topic tags from enrichment (serialized).
    pub subjects: Option<String>,
    /// Plugin or catalog that last enriched bibliographic fields.
    pub enrich_source: Option<String>,
    /// 0–1 confidence score for the last enrichment pass.
    pub enrich_confidence: Option<f64>,
    /// RFC 3339 time of the last enrichment write.
    pub enrich_updated_at: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
