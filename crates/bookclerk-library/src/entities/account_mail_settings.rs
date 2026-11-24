//! SeaORM entity for `account_mail_settings`: the notifier instance
//! administrators chose for account mail (one row, `id = 1`).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_mail_settings")]
pub struct Model {
    /// Always 1: the table holds one row.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    /// The notifier instance that sends account mail.
    pub plugin_instance_id: String,
    /// RFC 3339 time a test message through that instance was last
    /// delivered; `None` until one is.
    pub tested_at: Option<String>,
    /// RFC 3339 time the row last changed.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
