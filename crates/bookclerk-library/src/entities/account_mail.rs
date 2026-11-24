//! SeaORM entity for `account_mail`: one account-mail message (an invite,
//! a verification link or a test) and its delivery.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_mail")]
pub struct Model {
    /// Message id (UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// What it says: `test`, `invite` or `verify_email`.
    pub kind: String,
    /// The user it is for (the caller, for a test).
    pub user_id: i64,
    /// The address it goes to.
    pub address: String,
    /// The notifier instance that sends it.
    pub plugin_instance_id: String,
    /// `pending`, `delivered`, `failed` or `dropped`.
    pub state: String,
    /// Send attempts made.
    pub attempts: i64,
    /// Why the last attempt failed or why it was dropped.
    pub reason: Option<String>,
    /// The `notify_send` job that sends it, once enqueued.
    pub job_id: Option<String>,
    /// The Administrator who asked for an invite; `None` when the library
    /// sent it on its own, for other kinds, or once that user is deleted.
    pub requested_by: Option<i64>,
    /// RFC 3339 time it was queued.
    pub created_at: String,
    /// RFC 3339 time it last changed.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
