//! SeaORM entity for the `patron_scope_keys` table: the include and exclude
//! values of a patron scope as rows, so the Library filter joins them
//! instead of binding one parameter per rule.
use sea_orm::entity::prelude::*;

/// One value of one scope. Rows are keyed by the content id of the whole
/// value set (`crate::store::library_index::WorkScope::key_set_id`), so a
/// set's rows never change once written and any scope with the same values
/// shares them.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "patron_scope_keys")]
pub struct Model {
    /// Content id of the value set: lowercase hex SHA-256.
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_hash: String,
    /// `include` or `exclude`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub effect: String,
    /// The page index facet (`library_work_facets.facet`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub facet: String,
    /// The facet key (`library_work_facets.key`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
