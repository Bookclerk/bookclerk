//! SeaORM entity for the `plugin_schemas` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_schemas")]
pub struct Model {
    /// Canonical plugin key the schema belongs to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Manifest id (display alias) at pin time.
    pub manifest_id: String,
    /// SHA-256 of the manifest the schema was read from.
    pub manifest_sha256: String,
    /// Package version string at pin time; empty when the manifest has none.
    pub package_version: String,
    /// `SettingsSchema` revision stored in `settings_json`.
    pub schema_version: i64,
    /// JSON `SettingsSchema` body.
    pub settings_json: String,
    /// RFC 3339 timestamp of the last pin.
    pub recorded_at: String,
    /// Host that recorded the last pin.
    pub recorded_by_host: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
