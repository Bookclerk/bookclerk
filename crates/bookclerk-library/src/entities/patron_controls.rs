//! SeaORM entity for the `patron_controls` table: one Member's overrides
//! of the library's default member controls, set by an Administrator.
use sea_orm::entity::prelude::*;

/// Row shape for one Member's overrides. A Member with no row follows the
/// library's default for every control.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "patron_controls")]
pub struct Model {
    /// The Member (`users.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The overrides as JSON (`crate::control_plane::ControlOverrides`,
    /// schema version 1): only the controls that differ from the default.
    pub controls_json: String,
    /// RFC 3339 time the overrides were last set.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
