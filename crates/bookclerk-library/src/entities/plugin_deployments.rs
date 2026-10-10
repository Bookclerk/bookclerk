//! SeaORM entity for desired plugin deployment state.

use sea_orm::entity::prelude::*;

/// Row shape for one desired deployment of a plugin instance onto a host.
///
/// This is desired state. It is not evidence that the host installed the package.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_deployments")]
pub struct Model {
    /// Deployment id (hyphenated UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub deployment_id: String,
    /// Instance this deployment runs.
    pub plugin_instance_id: String,
    /// Host that should install and run the instance.
    pub host_id: String,
    /// Desired presence. This slice stores `present` only.
    pub desired: String,
    /// Monotonic compare-and-swap revision. Starts at 1.
    pub revision: i64,
    /// RFC 3339 timestamp of the last desired-state write.
    pub updated_at: String,
    /// Audit identity of the last desired-state writer.
    pub updated_by: String,
}

/// Declared SeaORM relations (deployments are addressed by id).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
