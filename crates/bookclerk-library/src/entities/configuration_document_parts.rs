//! SeaORM entity for the `configuration_document_parts` table (composite
//! primary key).

use sea_orm::entity::prelude::*;

/// One piece of a configuration document too wide for one cell or
/// statement of the database plugin (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "configuration_document_parts")]
pub struct Model {
    /// Scope kind of the document (`configuration_documents.scope_type`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_type: String,
    /// Scope instance of the document (`configuration_documents.scope_id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_id: String,
    /// Typed domain name of the document (`configuration_documents.namespace`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub namespace: String,
    /// The set of pieces this one belongs to, unique to the write that
    /// staged it; a pieced document row's `document_json` names the set to
    /// read.
    #[sea_orm(primary_key, auto_increment = false)]
    pub parts_id: String,
    /// The piece's place in the document, from 0.
    #[sea_orm(primary_key, auto_increment = false)]
    pub seq: i64,
    /// The piece of the document's JSON text, cut on a character boundary.
    pub body: String,
}

/// Declared SeaORM relations (documents are addressed by the composite key).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
