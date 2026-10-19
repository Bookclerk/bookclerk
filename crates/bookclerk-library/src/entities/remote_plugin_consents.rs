//! SeaORM entity for `remote_plugin_consents`: on a host node, this
//! install's operator consent for each owner plugin an owner cluster wants
//! to run here (written by `control_plane::remote_consents`).

use sea_orm::entity::prelude::*;

/// Row shape for one owner plugin of one owner cluster.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "remote_plugin_consents")]
pub struct Model {
    /// Owner cluster id the plugin belongs to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner_cluster_id: String,
    /// Canonical plugin key.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Version the owner currently deploys here.
    pub requested_version: String,
    /// The owner's approved grant (JSON) for that version.
    pub requested_grant_json: String,
    /// URL of the archive the owner deploys to this host (the artifact for
    /// this host's target).
    pub requested_artifact_url: String,
    /// SHA-256 of that archive, as the owner states it.
    pub requested_artifact_sha256: String,
    /// Version this install's operator approved; `None` until approved.
    pub version: Option<String>,
    /// The owner's grant as it stood when the operator approved; a later
    /// request outside it needs approval again.
    pub approved_request_json: Option<String>,
    /// The archive URL as it stood when the operator approved; another URL
    /// needs approval again.
    pub approved_artifact_url: Option<String>,
    /// The archive SHA-256 as it stood when the operator approved.
    pub approved_artifact_sha256: Option<String>,
    /// The grant this install's operator consented to (JSON), never wider
    /// than `approved_request_json`.
    pub grant_json: Option<String>,
    /// RFC 3339 approval time.
    pub approved_at: Option<String>,
    /// Audit identity of the approver (`platform` for an automatic
    /// approval of a bundled platform plugin).
    pub approved_by: Option<String>,
    /// RFC 3339 time this install's operator revoked the approval; while
    /// set, no automatic approval is given (only an operator approval
    /// clears it).
    pub revoked_at: Option<String>,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
