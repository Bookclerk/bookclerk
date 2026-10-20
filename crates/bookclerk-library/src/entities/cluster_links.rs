//! SeaORM entity for `cluster_links`: on a host node, the owner clusters it
//! enrolled into (written by `control_plane::record_cluster_link`).

use sea_orm::entity::prelude::*;

/// Row shape for one link from this install to an owner cluster.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "cluster_links")]
pub struct Model {
    /// Owner cluster id returned by enrollment.
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner_cluster_id: String,
    /// Owner origin (`https://owner.example`).
    pub owner_origin: String,
    /// Node id the owner minted for this install.
    pub remote_node_id: String,
    /// SPKI pin from the enrollment link, when it carried one.
    pub spki_pin: Option<String>,
    /// `active`, `revoked`, or `left`.
    pub state: String,
    /// RFC 3339 enrollment time.
    pub enrolled_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
