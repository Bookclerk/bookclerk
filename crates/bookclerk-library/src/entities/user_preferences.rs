//! SeaORM entity for the `user_preferences` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "user_preferences")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Preference subject key (`operator`, `user:<id>`, portal key).
    #[sea_orm(unique)]
    pub subject_key: String,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// Preferred library SPA view identifier (for example `grid` or `list`).
    pub default_view: String,
    /// JSON list of shelf ids hidden in the UI.
    pub disabled_shelves_json: String,
    /// Preferred Discover sort column (for example `title` or `added`).
    pub discover_sort: String,
    /// Discover sort direction (`asc` / `desc`).
    pub discover_sort_dir: String,
    /// BCP-47 language code used to filter Discover results, when set.
    pub discover_language: Option<String>,
    /// JSON list of storefronts excluded from Discover.
    pub discover_excluded_sources_json: String,
    /// Appearance preference (`system`, `light`, or `dark`).
    pub theme: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
