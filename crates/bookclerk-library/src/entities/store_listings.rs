//! SeaORM entity for the `store_listings` table (text primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "store_listings")]
pub struct Model {
    /// Listing id (a random UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Provenance-qualified PluginKey of the storefront.
    pub plugin_key: String,
    /// Lower-cased marketplace.
    pub marketplace: String,
    /// The store's own product id.
    pub product_id: String,
    /// The edition it sells.
    pub edition_id: String,
    /// Title as the store lists it.
    pub title: String,
    /// Store category labels when reported.
    pub categories: Option<String>,
    /// Subtitle as the store lists it.
    pub subtitle: Option<String>,
    /// Authors as the store credits them, comma-separated, for display.
    /// Work identity comes from the resolver's credits, never from this.
    pub authors: Option<String>,
    /// Narrators as the store credits them, comma-separated, for display.
    pub narrators: Option<String>,
    /// Series name as the store lists it.
    pub series: Option<String>,
    /// Position in that series as the store writes it (`1`, `2.5`).
    pub series_index: Option<String>,
    /// The store's synopsis, kept whole (may contain HTML).
    pub description: Option<String>,
    /// Publisher or imprint as the store lists it.
    pub publisher: Option<String>,
    /// Publication date as the store reports it.
    pub published_at: Option<String>,
    /// Runtime in whole minutes.
    pub length_minutes: Option<i64>,
    /// Language code the store reports (`en`, `de`).
    pub language: Option<String>,
    /// HTTPS cover image URL from the store.
    pub cover_url: Option<String>,
    /// The store's product page URL.
    pub url: Option<String>,
    /// The store's community rating (0 to 5).
    pub rating_overall: Option<f64>,
    /// How many ratings back [`Self::rating_overall`].
    pub rating_count: Option<i64>,
    /// `1` when the store marks it abridged, `0` unabridged, null unknown.
    pub is_abridged: Option<i64>,
    /// RFC 3339 time of the latest scan or catalog answer that reported it.
    pub observed_at: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
