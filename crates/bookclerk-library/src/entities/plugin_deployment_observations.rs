//! SeaORM entity for observed plugin deployment state.

use sea_orm::entity::prelude::*;

/// Row shape for one host's observation of a deployment.
///
/// Written only by the reconciler on that host. A desired row does not create this row.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_deployment_observations")]
pub struct Model {
    /// Deployment this observation describes.
    #[sea_orm(primary_key, auto_increment = false)]
    pub deployment_id: String,
    /// Host that recorded the observation. Matches the deployment target when local.
    #[sea_orm(primary_key, auto_increment = false)]
    pub host_id: String,
    /// Process incarnation that recorded the row.
    pub incarnation: String,
    /// `installed`, `running`, `healthy`, or `error`.
    pub status: String,
    /// Bounded detail. Empty when the status is `healthy`.
    pub detail: String,
    /// Config revision applied by the last spawn. Null until a spawn applies one.
    pub applied_config_revision: Option<i64>,
    /// RFC 3339 timestamp of this observation.
    pub observed_at: String,
}

/// Declared SeaORM relations (observations are addressed by deployment and host).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
