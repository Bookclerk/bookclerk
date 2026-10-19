//! SeaORM entity for `node_credentials`: the public keys an enrolled host
//! node proves itself with.

use sea_orm::entity::prelude::*;

/// Row shape for one host node's credential.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "node_credentials")]
pub struct Model {
    /// Host node this credential belongs to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub node_id: String,
    /// Ed25519 public signing key (32 bytes).
    pub sign_public_key: Vec<u8>,
    /// X25519 public box key (32 bytes).
    pub box_public_key: Vec<u8>,
    /// Enrollment link that created this credential.
    pub link_id: String,
    /// RFC 3339 enrollment time.
    pub enrolled_at: String,
    /// RFC 3339 revoke time, once revoked.
    pub revoked_at: Option<String>,
    /// RFC 3339 time of the last authenticated connection.
    pub last_connected_at: Option<String>,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
