//! SeaORM entity for a durable plugin instance.

use sea_orm::entity::prelude::*;

/// Row shape for one plugin instance.
///
/// The id is not a plugin key, alias, capability, or host id.
/// `plugin_key` is an immutable reference to the package this instance installs.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_instances")]
pub struct Model {
    /// Stable instance id (hyphenated UUID). Minted once at create.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_instance_id: String,
    /// Canonical plugin key of the package this instance runs.
    pub plugin_key: String,
    /// RFC 3339 timestamp of the insert.
    pub created_at: String,
    /// Audit identity of the creator.
    pub created_by: String,
}

/// Declared SeaORM relations (instances are addressed by id).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
