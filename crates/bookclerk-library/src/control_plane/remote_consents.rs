//! On a host node: this install's operator consent for owner plugins (H-4
//! design, section 5; step 8).
//!
//! An owner cluster that deploys a plugin to this host says which version
//! it runs and the grant the cluster's operator approved for it. The host
//! records that request ([`record_remote_plugin_request`]) but runs nothing
//! until its own operator approves ([`approve_remote_plugin`]). An approval
//! stores two grants:
//!
//! - `approved_request_json`: the owner's grant as the operator saw it.
//!   A later request that asks for anything outside it (a new version with
//!   a wider grant) needs approval again.
//! - `grant_json`: what the operator consented to, never wider than that
//!   request. The grant a guest runs with is the owner's current request
//!   intersected with it, so the host can narrow but never widen.
//!
//! Grants are stored as JSON text; the grant algebra (coverage and
//! intersection) lives with the plugin host, which owns the grant type.
//! An operator approves a request by its digest
//! ([`RemotePluginConsent::request_digest`], shown with the request), so
//! what is approved is exactly what was reviewed.
//!
//! [`revoke_remote_plugin`] clears the approval and keeps the request, so
//! the plugin shows as pending again. A revoke is recorded (`revoked_at`):
//! an automatic approval ([`RemoteApprover::Platform`]) never overrides it,
//! only the operator approving again does.

use chrono::{SecondsFormat, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::Serialize;

use crate::entities::remote_plugin_consents;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// `approved_by` of an automatic approval: a bundled platform plugin whose
/// request stays inside the platform envelope.
pub const REMOTE_CONSENT_PLATFORM: &str = "platform";

/// Longest plugin key, version, or approver stored.
const MAX_TEXT_BYTES: usize = 512;

/// Largest grant document stored.
const MAX_GRANT_BYTES: usize = 256 * 1024;

/// Domain separator of [`remote_request_digest`].
const REQUEST_DIGEST_DOMAIN: &[u8] = b"bookclerk.remote-plugin-request.v1\0";

/// The digest an operator approves a request by: SHA-256 (lowercase hex)
/// over the requested version and the owner's grant exactly as recorded,
/// each length-prefixed, so any change to either gives another digest.
///
/// # Arguments
///
/// * `version` - The requested version.
/// * `requested_grant_json` - The owner's grant as recorded.
///
/// # Returns
///
/// 64 lowercase hex characters.
#[must_use]
pub fn remote_request_digest(version: &str, requested_grant_json: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(REQUEST_DIGEST_DOMAIN);
    for part in [version, requested_grant_json] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// One owner plugin of one owner cluster, as this install sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemotePluginConsent {
    /// Owner cluster id.
    pub owner_cluster_id: String,
    /// Canonical plugin key.
    pub plugin_key: String,
    /// Version the owner deploys here now.
    pub requested_version: String,
    /// The owner's approved grant for that version (JSON).
    pub requested_grant_json: String,
    /// Version this install's operator approved.
    pub version: Option<String>,
    /// The owner's grant when the operator approved (JSON).
    pub approved_request_json: Option<String>,
    /// The grant the operator consented to (JSON).
    pub grant_json: Option<String>,
    /// RFC 3339 approval time.
    pub approved_at: Option<String>,
    /// Who approved ([`REMOTE_CONSENT_PLATFORM`] for an automatic approval).
    pub approved_by: Option<String>,
    /// RFC 3339 time the operator revoked the approval; cleared only by an
    /// operator approval.
    pub revoked_at: Option<String>,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

impl RemotePluginConsent {
    /// True when an approval is stored. Whether it still covers the
    /// current request is the grant algebra's call.
    #[must_use]
    pub fn is_approved(&self) -> bool {
        self.approved_at.is_some()
            && self.approved_request_json.is_some()
            && self.grant_json.is_some()
    }

    /// True when the operator revoked the approval and has not approved
    /// again: no automatic approval is given.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// The digest of the current request ([`remote_request_digest`]); an
    /// approval must name it.
    #[must_use]
    pub fn request_digest(&self) -> String {
        remote_request_digest(&self.requested_version, &self.requested_grant_json)
    }
}

impl From<remote_plugin_consents::Model> for RemotePluginConsent {
    fn from(row: remote_plugin_consents::Model) -> Self {
        Self {
            owner_cluster_id: row.owner_cluster_id,
            plugin_key: row.plugin_key,
            requested_version: row.requested_version,
            requested_grant_json: row.requested_grant_json,
            version: row.version,
            approved_request_json: row.approved_request_json,
            grant_json: row.grant_json,
            approved_at: row.approved_at,
            approved_by: row.approved_by,
            revoked_at: row.revoked_at,
            updated_at: row.updated_at,
        }
    }
}

/// RFC 3339 now.
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Checks the size of one text field.
fn bounded(what: &str, value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max {
        return Err(LibraryError::Invalid(format!(
            "{what} must be 1 to {max} bytes"
        )));
    }
    Ok(())
}

/// Checks that `json` is a JSON object of bounded size.
fn grant_document(what: &str, json: &str) -> Result<()> {
    bounded(what, json, MAX_GRANT_BYTES)?;
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Object(_)) => Ok(()),
        _ => Err(LibraryError::Invalid(format!(
            "{what} must be a JSON object"
        ))),
    }
}

/// The consent row for `plugin_key` of `cluster_id`.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_remote_plugin_consent(
    store: &LibraryStore,
    cluster_id: &str,
    plugin_key: &str,
) -> Result<Option<RemotePluginConsent>> {
    Ok(
        remote_plugin_consents::Entity::find_by_id((
            cluster_id.to_string(),
            plugin_key.to_string(),
        ))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(RemotePluginConsent::from),
    )
}

/// Every consent row, of one cluster when `cluster_id` is given, by
/// cluster then plugin key.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_remote_plugin_consents(
    store: &LibraryStore,
    cluster_id: Option<&str>,
) -> Result<Vec<RemotePluginConsent>> {
    let mut query = remote_plugin_consents::Entity::find();
    if let Some(cluster_id) = cluster_id {
        query = query.filter(remote_plugin_consents::Column::OwnerClusterId.eq(cluster_id));
    }
    Ok(query
        .order_by_asc(remote_plugin_consents::Column::OwnerClusterId)
        .order_by_asc(remote_plugin_consents::Column::PluginKey)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(RemotePluginConsent::from)
        .collect())
}

/// Records what the owner asks to run here: `version` of `plugin_key` with
/// the owner's approved grant. A stored approval is kept; whether it still
/// covers the new request is decided when the grant is evaluated. Writes
/// nothing when the request is unchanged.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for an empty or oversized field or a
/// grant that is not a JSON object, and the write error.
pub async fn record_remote_plugin_request(
    store: &LibraryStore,
    cluster_id: &str,
    plugin_key: &str,
    version: &str,
    requested_grant_json: &str,
) -> Result<RemotePluginConsent> {
    bounded("cluster id", cluster_id, MAX_TEXT_BYTES)?;
    bounded("plugin key", plugin_key, MAX_TEXT_BYTES)?;
    bounded("version", version, MAX_TEXT_BYTES)?;
    grant_document("requested grant", requested_grant_json)?;
    if let Some(existing) = load_remote_plugin_consent(store, cluster_id, plugin_key).await? {
        if existing.requested_version == version
            && existing.requested_grant_json == requested_grant_json
        {
            return Ok(existing);
        }
    }
    let row = remote_plugin_consents::ActiveModel {
        owner_cluster_id: Set(cluster_id.to_string()),
        plugin_key: Set(plugin_key.to_string()),
        requested_version: Set(version.to_string()),
        requested_grant_json: Set(requested_grant_json.to_string()),
        version: Set(None),
        approved_request_json: Set(None),
        grant_json: Set(None),
        approved_at: Set(None),
        approved_by: Set(None),
        revoked_at: Set(None),
        updated_at: Set(now()),
    };
    remote_plugin_consents::Entity::insert(row)
        .on_conflict(
            OnConflict::columns([
                remote_plugin_consents::Column::OwnerClusterId,
                remote_plugin_consents::Column::PluginKey,
            ])
            .update_columns([
                remote_plugin_consents::Column::RequestedVersion,
                remote_plugin_consents::Column::RequestedGrantJson,
                remote_plugin_consents::Column::UpdatedAt,
            ])
            .to_owned(),
        )
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    load_remote_plugin_consent(store, cluster_id, plugin_key)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("remote plugin consent {plugin_key}")))
}

/// Who gives an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteApprover<'a> {
    /// This install's operator, by audit identity (never
    /// [`REMOTE_CONSENT_PLATFORM`]). Clears a recorded revoke.
    Operator(&'a str),
    /// The automatic approval of a bundled platform plugin. Refused while
    /// a revoke is recorded.
    Platform,
}

impl RemoteApprover<'_> {
    /// The audit identity stored as `approved_by`.
    #[must_use]
    pub fn audit_identity(&self) -> &str {
        match self {
            Self::Operator(id) => id,
            Self::Platform => REMOTE_CONSENT_PLATFORM,
        }
    }
}

/// What an operator approves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteApproval<'a> {
    /// Owner cluster id.
    pub cluster_id: &'a str,
    /// Canonical plugin key.
    pub plugin_key: &'a str,
    /// The requested version the operator reviewed.
    pub version: &'a str,
    /// The owner's grant the operator reviewed, exactly as recorded.
    pub reviewed_request_json: &'a str,
    /// The grant consented to (already checked to be no wider than the
    /// reviewed request).
    pub grant_json: &'a str,
    /// Who approves.
    pub approver: RemoteApprover<'a>,
}

/// Stores an approval, but only when the recorded request is still the
/// one the operator reviewed: a request that changed in between (a new
/// version, a wider grant) answers [`LibraryError::Conflict`] and nothing
/// is written. An automatic approval ([`RemoteApprover::Platform`]) is
/// likewise refused while a revoke is recorded; an operator approval
/// clears it.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] when no request is recorded,
/// [`LibraryError::Conflict`] when it changed or an automatic approval
/// meets a revoke, [`LibraryError::Invalid`] for a malformed field (an
/// operator identity of [`REMOTE_CONSENT_PLATFORM`] among them), and the
/// write error.
pub async fn approve_remote_plugin(
    store: &LibraryStore,
    approval: &RemoteApproval<'_>,
) -> Result<RemotePluginConsent> {
    let approved_by = approval.approver.audit_identity();
    bounded("approver", approved_by, MAX_TEXT_BYTES)?;
    if matches!(approval.approver, RemoteApprover::Operator(id) if id == REMOTE_CONSENT_PLATFORM) {
        return Err(LibraryError::Invalid(format!(
            "{REMOTE_CONSENT_PLATFORM} is reserved for automatic approvals"
        )));
    }
    grant_document("consented grant", approval.grant_json)?;
    let Some(existing) =
        load_remote_plugin_consent(store, approval.cluster_id, approval.plugin_key).await?
    else {
        return Err(LibraryError::NotFound(format!(
            "no request from cluster {} for plugin {}",
            approval.cluster_id, approval.plugin_key
        )));
    };
    let automatic = matches!(approval.approver, RemoteApprover::Platform);
    if automatic && existing.is_revoked() {
        return Err(LibraryError::Conflict(format!(
            "this host's operator revoked {}; only the operator can approve it again",
            approval.plugin_key
        )));
    }
    let stamp = now();
    let mut update = remote_plugin_consents::Entity::update_many()
        .col_expr(
            remote_plugin_consents::Column::Version,
            sea_orm::sea_query::Expr::value(Some(approval.version.to_string())),
        )
        .col_expr(
            remote_plugin_consents::Column::ApprovedRequestJson,
            sea_orm::sea_query::Expr::value(Some(approval.reviewed_request_json.to_string())),
        )
        .col_expr(
            remote_plugin_consents::Column::GrantJson,
            sea_orm::sea_query::Expr::value(Some(approval.grant_json.to_string())),
        )
        .col_expr(
            remote_plugin_consents::Column::ApprovedAt,
            sea_orm::sea_query::Expr::value(Some(stamp.clone())),
        )
        .col_expr(
            remote_plugin_consents::Column::ApprovedBy,
            sea_orm::sea_query::Expr::value(Some(approved_by.to_string())),
        )
        .col_expr(
            remote_plugin_consents::Column::RevokedAt,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            remote_plugin_consents::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(stamp),
        )
        .filter(remote_plugin_consents::Column::OwnerClusterId.eq(approval.cluster_id))
        .filter(remote_plugin_consents::Column::PluginKey.eq(approval.plugin_key))
        .filter(remote_plugin_consents::Column::RequestedVersion.eq(approval.version))
        .filter(
            remote_plugin_consents::Column::RequestedGrantJson.eq(approval.reviewed_request_json),
        );
    if automatic {
        // A revoke that lands after the read above still wins.
        update = update.filter(remote_plugin_consents::Column::RevokedAt.is_null());
    }
    let updated = update.exec(store.db()).await.map_err(LibraryError::Orm)?;
    if updated.rows_affected != 1 {
        return Err(LibraryError::Conflict(format!(
            "the request for {} changed since it was reviewed (or was revoked); review it again",
            approval.plugin_key
        )));
    }
    load_remote_plugin_consent(store, approval.cluster_id, approval.plugin_key)
        .await?
        .ok_or_else(|| {
            LibraryError::NotFound(format!("remote plugin consent {}", approval.plugin_key))
        })
}

/// Clears the approval of `plugin_key` of `cluster_id`, keeping its
/// request, and records the revoke so no automatic approval brings it
/// back. Answers the row, or `None` when nothing is recorded.
///
/// # Errors
///
/// Returns an error when a read or the write fails.
pub async fn revoke_remote_plugin(
    store: &LibraryStore,
    cluster_id: &str,
    plugin_key: &str,
) -> Result<Option<RemotePluginConsent>> {
    let stamp = now();
    remote_plugin_consents::Entity::update_many()
        .col_expr(
            remote_plugin_consents::Column::Version,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            remote_plugin_consents::Column::ApprovedRequestJson,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            remote_plugin_consents::Column::GrantJson,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            remote_plugin_consents::Column::ApprovedAt,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            remote_plugin_consents::Column::ApprovedBy,
            sea_orm::sea_query::Expr::value(Option::<String>::None),
        )
        .col_expr(
            remote_plugin_consents::Column::RevokedAt,
            sea_orm::sea_query::Expr::value(Some(stamp.clone())),
        )
        .col_expr(
            remote_plugin_consents::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(stamp),
        )
        .filter(remote_plugin_consents::Column::OwnerClusterId.eq(cluster_id))
        .filter(remote_plugin_consents::Column::PluginKey.eq(plugin_key))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    load_remote_plugin_consent(store, cluster_id, plugin_key).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> LibraryStore {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        crate::apply_host_schema(&db).await.unwrap();
        LibraryStore::from_connection(db)
    }

    fn approval<'a>(version: &'a str, reviewed: &'a str, grant: &'a str) -> RemoteApproval<'a> {
        RemoteApproval {
            cluster_id: "cluster-a",
            plugin_key: "store",
            version,
            reviewed_request_json: reviewed,
            grant_json: grant,
            approver: RemoteApprover::Operator("operator"),
        }
    }

    #[tokio::test]
    async fn requests_approvals_and_revocation() {
        let store = store().await;
        let requested = r#"{"domains":["a.example"]}"#;
        let first = record_remote_plugin_request(&store, "cluster-a", "store", "1.0.0", requested)
            .await
            .unwrap();
        assert!(!first.is_approved());
        // An unchanged request writes nothing.
        let again = record_remote_plugin_request(&store, "cluster-a", "store", "1.0.0", requested)
            .await
            .unwrap();
        assert_eq!(again.updated_at, first.updated_at);

        // Approval must name the request as reviewed.
        assert!(matches!(
            approve_remote_plugin(&store, &approval("0.9.0", requested, "{}")).await,
            Err(LibraryError::Conflict(_))
        ));
        assert!(matches!(
            approve_remote_plugin(&store, &approval("1.0.0", "{}", "{}")).await,
            Err(LibraryError::Conflict(_))
        ));
        assert!(matches!(
            approve_remote_plugin(&store, &approval("1.0.0", requested, "[]")).await,
            Err(LibraryError::Invalid(_))
        ));
        let approved = approve_remote_plugin(&store, &approval("1.0.0", requested, "{}"))
            .await
            .unwrap();
        assert!(approved.is_approved());
        assert_eq!(approved.version.as_deref(), Some("1.0.0"));
        assert_eq!(approved.approved_request_json.as_deref(), Some(requested));

        // A new request keeps the approval for the grant algebra to judge.
        let wider = r#"{"domains":["a.example","b.example"]}"#;
        let moved = record_remote_plugin_request(&store, "cluster-a", "store", "1.1.0", wider)
            .await
            .unwrap();
        assert!(moved.is_approved());
        assert_eq!(moved.requested_version, "1.1.0");
        assert_eq!(moved.approved_request_json.as_deref(), Some(requested));

        let revoked = revoke_remote_plugin(&store, "cluster-a", "store")
            .await
            .unwrap()
            .unwrap();
        assert!(!revoked.is_approved());
        assert_eq!(revoked.requested_version, "1.1.0");
        assert!(matches!(
            approve_remote_plugin(
                &store,
                &RemoteApproval {
                    plugin_key: "other",
                    ..approval("1.0.0", requested, "{}")
                }
            )
            .await,
            Err(LibraryError::NotFound(_))
        ));
        assert_eq!(
            list_remote_plugin_consents(&store, Some("cluster-a"))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(list_remote_plugin_consents(&store, Some("cluster-b"))
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_request_digest_covers_version_and_grant() {
        let base = remote_request_digest("1.0.0", r#"{"domains":["a.example"]}"#);
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            remote_request_digest("1.0.0", r#"{"domains":["a.example"]}"#)
        );
        assert_ne!(
            base,
            remote_request_digest("1.0.0", r#"{"domains":["b.example"]}"#)
        );
        assert_ne!(
            base,
            remote_request_digest("1.0.1", r#"{"domains":["a.example"]}"#)
        );
        // Length prefixes keep the split between the two unambiguous.
        assert_ne!(
            remote_request_digest("1.0.0{", "}"),
            remote_request_digest("1.0.0", "{}")
        );
    }

    #[tokio::test]
    async fn an_operator_revoke_holds_against_automatic_approval() {
        let store = store().await;
        let requested = r"{}";
        record_remote_plugin_request(&store, "cluster-a", "store", "1.0.0", requested)
            .await
            .unwrap();
        let platform = RemoteApproval {
            approver: RemoteApprover::Platform,
            ..approval("1.0.0", requested, "{}")
        };
        let approved = approve_remote_plugin(&store, &platform).await.unwrap();
        assert_eq!(
            approved.approved_by.as_deref(),
            Some(REMOTE_CONSENT_PLATFORM)
        );
        assert!(!approved.is_revoked());

        let revoked = revoke_remote_plugin(&store, "cluster-a", "store")
            .await
            .unwrap()
            .unwrap();
        assert!(revoked.is_revoked() && !revoked.is_approved());
        assert!(matches!(
            approve_remote_plugin(&store, &platform).await,
            Err(LibraryError::Conflict(_))
        ));
        // The revoke survives a new request from the owner.
        let moved = record_remote_plugin_request(&store, "cluster-a", "store", "1.1.0", requested)
            .await
            .unwrap();
        assert!(moved.is_revoked());
        assert!(matches!(
            approve_remote_plugin(
                &store,
                &RemoteApproval {
                    approver: RemoteApprover::Platform,
                    ..approval("1.1.0", requested, "{}")
                }
            )
            .await,
            Err(LibraryError::Conflict(_))
        ));
        // An operator cannot pose as the automatic approver.
        assert!(matches!(
            approve_remote_plugin(
                &store,
                &RemoteApproval {
                    approver: RemoteApprover::Operator(REMOTE_CONSENT_PLATFORM),
                    ..approval("1.1.0", requested, "{}")
                }
            )
            .await,
            Err(LibraryError::Invalid(_))
        ));
        // Only the operator approving again clears it.
        let again = approve_remote_plugin(&store, &approval("1.1.0", requested, "{}"))
            .await
            .unwrap();
        assert!(again.is_approved() && !again.is_revoked());
    }
}
