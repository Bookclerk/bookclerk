//! SeaORM entity for the `admin_controls` table: one Administrator's
//! overrides of the library's default Administrator switches, set by an
//! Owner or the operator.
use sea_orm::entity::prelude::*;

/// Row shape for one Administrator's overrides. An Administrator with no
/// row follows the library's default for every switch.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "admin_controls")]
pub struct Model {
    /// The Administrator (`users.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The overrides as JSON (`crate::control_plane::AdminControlOverrides`,
    /// schema version 1): only the switches that differ from the default.
    pub controls_json: String,
    /// RFC 3339 time the overrides were last set.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
