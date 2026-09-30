//! SeaORM entity for paged storage-scan rows.
//!
//! Object and identity hits live here so a scan does not retain the inventory
//! in process memory. `kind` is `object` or `identity`.

use sea_orm::entity::prelude::*;

/// One durable scan row.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "storage_scan_rows")]
pub struct Model {
    /// Scan generation id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub scan_id: String,
    /// `object` or `identity`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub kind: String,
    /// Uppercase ASIN/ISBN for identity rows; empty for object rows.
    #[sea_orm(primary_key, auto_increment = false)]
    pub identity: String,
    /// Storage key.
    #[sea_orm(primary_key, auto_increment = false)]
    pub object_key: String,
    /// Object size in bytes.
    pub size: i64,
    /// Lower is a better packaged format.
    pub media_rank: i64,
    /// 1 when the key is acquired audio.
    pub is_audio: i64,
    /// 1 after a library row claims the object.
    pub claimed: i64,
}

/// No declared relations.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
