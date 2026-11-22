//! SeaORM entity for `notifications`: one notification the router
//! accepted, rendered once for every destination and the inbox.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notifications")]
pub struct Model {
    /// Random UUID; the `notificationId` every copy carries.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Qualified notification type.
    pub notification_type: String,
    /// `operational`, `patron` or `account`.
    pub audience: String,
    /// `info`, `warning` or `critical`.
    pub severity: String,
    /// Short title.
    pub title: String,
    /// Plain-text body.
    pub body_text: String,
    /// Path inside the library the notification links to; the send job
    /// prefixes the public origin.
    pub link_path: Option<String>,
    /// JSON array of `{name, value}` fields.
    pub fields_json: String,
    /// Grouping key, when the raiser gave one.
    pub group_key: Option<String>,
    /// `none`, `trigger` or `resolve`.
    pub alert_action: String,
    /// Alert key; set exactly when `alert_action` is not `none`.
    pub alert_key: Option<String>,
    /// Empty for a host type, else the raising plugin's id.
    pub source: String,
    /// Patron a patron-audience notification is for.
    pub user_id: Option<i64>,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
