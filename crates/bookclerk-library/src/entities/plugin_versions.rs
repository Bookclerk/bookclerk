//! SeaORM entity for `plugin_versions`: one installed version of a plugin.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_versions")]
pub struct Model {
    /// Canonical plugin key.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Semver package version.
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: String,
    /// Manifest id (display alias).
    pub manifest_id: String,
    /// `plugin.toml` text of the version.
    pub manifest_toml: String,
    /// JSON `SettingsSchema` the version declares.
    pub settings_json: String,
    /// JSON list of per-target artifacts with digests.
    pub artifacts_json: String,
    /// 1 when the publisher was verified at install time.
    pub publisher_verified: i64,
    /// JSON consent grant the operator approved; empty until approved.
    pub grant_json: String,
    /// Package coordinate the version was resolved from (`cargo:…@v`);
    /// empty for a local archive.
    pub coordinate: String,
    /// RFC 3339 install time.
    pub installed_at: String,
    /// Audit identity of the publisher.
    pub installed_by: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
