//! SeaORM entity for `node_enrollment_links`: single-use links that enroll a
//! host node into this cluster. Only the hash of each link's secret is stored.

use sea_orm::entity::prelude::*;

/// Row shape for one enrollment link.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "node_enrollment_links")]
pub struct Model {
    /// Public link id (the second field of `bcn1.<link_id>.<secret>`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub link_id: String,
    /// `hash_token(secret)`. The secret itself is never stored.
    pub token_hash: String,
    /// User who will own the enrolled node.
    pub for_user_id: i64,
    /// Free-form label shown in listings.
    pub label: String,
    /// Audit id of the minter (`operator:<id>` or `user:<id>`).
    pub created_by: String,
    /// RFC 3339 mint time.
    pub created_at: String,
    /// RFC 3339 expiry.
    pub expires_at: String,
    /// RFC 3339 redeem time, once redeemed.
    pub redeemed_at: Option<String>,
    /// Node the redeem created.
    pub redeemed_node_id: Option<String>,
    /// RFC 3339 revoke time, once revoked.
    pub revoked_at: Option<String>,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
