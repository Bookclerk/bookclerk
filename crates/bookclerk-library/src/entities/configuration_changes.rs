//! SeaORM entity for the configuration change notice written with each commit.

use sea_orm::entity::prelude::*;

/// Row shape for one committed configuration notice.
///
/// Hosts reconcile by reading document revisions. This table is the
/// transactional notice of that commit; plugin event delivery is not a
/// broadcast to every host.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "configuration_changes")]
pub struct Model {
    /// Surrogate primary key.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Scope kind of the committed document.
    pub scope_type: String,
    /// Scope instance of the committed document.
    pub scope_id: String,
    /// Domain namespace of the committed document.
    pub namespace: String,
    /// Document revision after the commit.
    pub revision: i64,
    /// RFC 3339 timestamp of the commit.
    pub committed_at: String,
}

/// Declared SeaORM relations (notices are append-only).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
