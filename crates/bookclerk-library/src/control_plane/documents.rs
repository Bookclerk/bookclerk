//! Typed configuration documents: atomic replace, audit, and change notices.
//!
//! A JSON string is the physical body. Callers pass an already-validated
//! document for one namespace. Lost updates fail the `revision` predicate and
//! do not insert an audit row or a change notice.

use chrono::{Duration, Utc};
use sea_orm::EntityTrait;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::batch::{self, exec, int, query, request, text};
use crate::entities::{bookclerk_receipts, configuration_documents};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Cluster scope instance. There is one cluster document per namespace.
pub const CLUSTER_SCOPE_ID: &str = "singleton";

/// Scope type stored for cluster documents.
pub const CLUSTER_SCOPE_TYPE: &str = "cluster";

/// Scope type stored for host documents.
pub const HOST_SCOPE_TYPE: &str = "host";

/// Scope type stored for one plugin instance's configuration.
pub const PLUGIN_INSTANCE_SCOPE_TYPE: &str = "plugin_instance";

/// Namespace of a plugin instance's typed configuration document.
pub const PLUGIN_INSTANCE_CONFIG_NAMESPACE: &str = "config";

/// Maximum UTF-8 size of `document_json`.
pub const MAX_CONFIGURATION_DOCUMENT_BYTES: usize = 16 * 1024;

/// Maximum length of an audit actor or operation id.
const MAX_ACTOR_LEN: usize = 128;

/// Receipt status for a committed replacement.
const STATUS_OK: &str = "ok";

/// Receipt status when the expected revision did not match.
const STATUS_CONFLICT: &str = "conflict";

/// `bookclerk_receipts.operation_kind` for a configuration replacement.
const REPLACE_KIND: &str = "replaceConfiguration";

/// Who is allowed to write a configuration document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigActor {
    /// Control-plane operator (CLI or operator token).
    Operator {
        /// Stable audit id (`cli`, `operator`, …).
        id: String,
    },
    /// Portal administrator. Cannot write cluster or host configuration.
    Administrator {
        /// User id recorded if a write is attempted.
        id: String,
    },
    /// Portal member. Cannot write configuration.
    Member {
        /// User id recorded if a write is attempted.
        id: String,
    },
    /// Process startup. May import a missing document and may not replace one.
    Bootstrap,
}

impl ConfigActor {
    /// Audit string stored on a committed document.
    pub(super) fn audit_id(&self) -> &str {
        match self {
            Self::Operator { id } | Self::Administrator { id } | Self::Member { id } => id,
            Self::Bootstrap => "bootstrap",
        }
    }

    /// True when this actor may insert a missing document.
    fn can_import(&self) -> bool {
        matches!(self, Self::Operator { .. } | Self::Bootstrap)
    }

    /// True when this actor may replace an existing document.
    fn can_replace(&self) -> bool {
        matches!(self, Self::Operator { .. })
    }
}

/// Address of one configuration document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentKey {
    /// `cluster`, `host`, or `plugin_instance`.
    pub scope_type: String,
    /// `singleton`, a host id, or a plugin instance id.
    pub scope_id: String,
    /// Domain namespace.
    pub namespace: String,
}

impl DocumentKey {
    /// Cluster-scoped domain.
    #[must_use]
    pub fn cluster(namespace: &str) -> Self {
        Self {
            scope_type: CLUSTER_SCOPE_TYPE.to_string(),
            scope_id: CLUSTER_SCOPE_ID.to_string(),
            namespace: namespace.to_string(),
        }
    }

    /// Host-scoped domain.
    #[must_use]
    pub fn host(host_id: &str, namespace: &str) -> Self {
        Self {
            scope_type: HOST_SCOPE_TYPE.to_string(),
            scope_id: host_id.to_string(),
            namespace: namespace.to_string(),
        }
    }

    /// Plugin-instance configuration document.
    ///
    /// The address is `(plugin_instance, instance_id, config)`. It does not
    /// include a plugin key, capability, alias, or host id.
    #[must_use]
    pub fn plugin_instance(instance_id: &str) -> Self {
        Self {
            scope_type: PLUGIN_INSTANCE_SCOPE_TYPE.to_string(),
            scope_id: instance_id.to_string(),
            namespace: PLUGIN_INSTANCE_CONFIG_NAMESPACE.to_string(),
        }
    }
}

/// One stored document before domain parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDocument {
    /// Document address.
    pub key: DocumentKey,
    /// Domain schema version column.
    pub schema_version: i64,
    /// Compare-and-swap revision.
    pub revision: i64,
    /// JSON body.
    pub document_json: String,
    /// RFC 3339 commit time.
    pub updated_at: String,
    /// Audit identity of the last writer.
    pub updated_by: String,
    /// Operation id of the write that produced this revision.
    pub write_operation_id: String,
}

/// Result of a compare-and-swap replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceOutcome {
    /// The document is the committed body.
    Applied(StoredDocument),
    /// The expected revision lost. No document, audit, or change row was written.
    Conflict {
        /// Revision currently stored.
        current_revision: i64,
    },
    /// This `operation_id` already committed. The document was not modified again.
    Replayed {
        /// Revision stored on the original receipt.
        revision: i64,
    },
}

/// Reads one document.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_document(
    store: &LibraryStore,
    key: &DocumentKey,
) -> Result<Option<StoredDocument>> {
    let row = configuration_documents::Entity::find_by_id((
        key.scope_type.clone(),
        key.scope_id.clone(),
        key.namespace.clone(),
    ))
    .one(store.db())
    .await
    .map_err(LibraryError::Orm)?;
    Ok(row.map(stored_from_model))
}

/// Inserts `document_json` when the key is absent.
///
/// The document, its revision-1 audit row, and its change notice commit in one
/// batch. A concurrent insert keeps the winner and does not add a second
/// notice. This does not update an existing row.
///
/// # Errors
///
/// Returns an error when the actor may not import, the body is invalid, or the
/// write fails. A failed batch leaves no document, audit, or change row.
pub async fn import_if_absent(
    store: &LibraryStore,
    actor: &ConfigActor,
    key: &DocumentKey,
    schema_version: i64,
    document_json: &str,
    operation_id: &str,
) -> Result<StoredDocument> {
    authorize_import(actor)?;
    validate_payload(document_json, operation_id, actor)?;
    if let Some(existing) = load_document(store, key).await? {
        return Ok(existing);
    }
    let now = Utc::now().to_rfc3339();
    batch::execute_host_batch(
        store,
        request(
            operation_id,
            vec![
                exec(
                    "INSERT OR IGNORE INTO configuration_documents (
                    scope_type, scope_id, namespace, schema_version, revision,
                    document_json, updated_at, updated_by, write_operation_id
                ) VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?)",
                    vec![
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        int(schema_version),
                        text(document_json),
                        text(&now),
                        text(actor.audit_id()),
                        text(operation_id),
                    ],
                ),
                exec(
                    "INSERT INTO configuration_audit (
                        scope_type, scope_id, namespace, schema_version, revision, actor, recorded_at
                     ) SELECT scope_type, scope_id, namespace, schema_version, revision,
                              updated_by, updated_at
                       FROM configuration_documents
                      WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                        AND write_operation_id = ? AND revision = 1
                        AND NOT EXISTS (
                            SELECT 1 FROM configuration_audit
                             WHERE scope_type = ? AND scope_id = ? AND namespace = ? AND revision = 1
                        )",
                    vec![
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        text(operation_id),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                    ],
                ),
                exec(
                    "INSERT INTO configuration_changes (
                        scope_type, scope_id, namespace, revision, committed_at
                     ) SELECT scope_type, scope_id, namespace, revision, updated_at
                       FROM configuration_documents
                      WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                        AND write_operation_id = ? AND revision = 1
                        AND NOT EXISTS (
                            SELECT 1 FROM configuration_changes
                             WHERE scope_type = ? AND scope_id = ? AND namespace = ? AND revision = 1
                        )",
                    vec![
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        text(operation_id),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                    ],
                ),
            ],
        ),
    )
    .await?;
    load_document(store, key)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("configuration {}", key.namespace)))
}

/// Replaces one document when `expected_revision` is current.
///
/// Authorization and payload validation run first. An existing receipt is then
/// matched on operation kind and request hash before the revision predicate, so
/// a later edit does not turn a retry of an earlier success into a conflict.
/// A receipt for this operation id still replays after `expires_at`: durable
/// cleanup deletes other expired receipts and keeps the current id so a retry
/// matches. A different kind or hash is an idempotency conflict and writes
/// nothing. A revision mismatch or a failed batch writes no audit row and no
/// change notice.
///
/// # Errors
///
/// Returns an error when the actor is not allowed, the body or version is
/// rejected, the document is missing, or the engine rejects the batch.
pub async fn replace_document(
    store: &LibraryStore,
    actor: &ConfigActor,
    key: &DocumentKey,
    schema_version: i64,
    expected_revision: i64,
    document_json: &str,
    operation_id: &str,
) -> Result<ReplaceOutcome> {
    authorize_replace(actor)?;
    validate_payload(document_json, operation_id, actor)?;
    if expected_revision < 1 {
        return Err(invalid("expected revision must be >= 1"));
    }
    let request_hash = replacement_hash(
        key,
        schema_version,
        expected_revision,
        document_json,
        actor.audit_id(),
    );
    if let Some(outcome) = existing_receipt_outcome(store, operation_id, &request_hash).await? {
        return Ok(outcome);
    }
    // A commit can land after the empty lookup and before the revision read.
    pause_after_empty_receipt_lookup(operation_id).await;
    let current = load_document(store, key).await?.ok_or_else(|| {
        LibraryError::NotFound(format!(
            "configuration {} is not initialized",
            key.namespace
        ))
    })?;
    if current.schema_version != schema_version {
        return Err(unsupported(&key.namespace, current.schema_version));
    }
    if current.revision != expected_revision {
        if let Some(outcome) = existing_receipt_outcome(store, operation_id, &request_hash).await? {
            return Ok(outcome);
        }
        return Ok(ReplaceOutcome::Conflict {
            current_revision: current.revision,
        });
    }
    let now = Utc::now().to_rfc3339();
    let expires = (Utc::now() + Duration::hours(24)).to_rfc3339();
    let slot = format!("config-cas:{operation_id}");
    let reply = batch::execute_host_batch(
        store,
        request(
            operation_id,
            vec![
                exec(
                    "INSERT OR IGNORE INTO bookclerk_slots (slot_key, bump) \
                     SELECT ?, 0 WHERE NOT EXISTS ( \
                        SELECT 1 FROM bookclerk_receipts WHERE operation_id = ? \
                     )",
                    vec![text(&slot), text(operation_id)],
                ),
                exec(
                    "UPDATE configuration_documents SET
                        schema_version = ?, revision = revision + 1, document_json = ?,
                        updated_at = ?, updated_by = ?, write_operation_id = ?
                     WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                       AND revision = ? AND schema_version = ?
                       AND NOT EXISTS (
                            SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?
                       )",
                    vec![
                        int(schema_version),
                        text(document_json),
                        text(&now),
                        text(actor.audit_id()),
                        text(operation_id),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        int(expected_revision),
                        int(schema_version),
                        text(operation_id),
                    ],
                ),
                exec(
                    "UPDATE bookclerk_slots SET bump = 1
                     WHERE slot_key = ? AND bump = 0
                       AND EXISTS (
                            SELECT 1 FROM configuration_documents
                             WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                               AND write_operation_id = ?
                       )",
                    vec![
                        text(&slot),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        text(operation_id),
                    ],
                ),
                exec(
                    "INSERT INTO configuration_audit (
                        scope_type, scope_id, namespace, schema_version, revision, actor, recorded_at
                     ) SELECT d.scope_type, d.scope_id, d.namespace, d.schema_version, d.revision,
                              d.updated_by, d.updated_at
                       FROM configuration_documents AS d
                       JOIN bookclerk_slots AS s ON s.slot_key = ?
                      WHERE s.bump = 1
                        AND d.scope_type = ? AND d.scope_id = ? AND d.namespace = ?
                        AND d.write_operation_id = ?
                        AND NOT EXISTS (
                            SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?
                        )",
                    vec![
                        text(&slot),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        text(operation_id),
                        text(operation_id),
                    ],
                ),
                exec(
                    "INSERT INTO configuration_changes (
                        scope_type, scope_id, namespace, revision, committed_at
                     ) SELECT d.scope_type, d.scope_id, d.namespace, d.revision, d.updated_at
                       FROM configuration_documents AS d
                       JOIN bookclerk_slots AS s ON s.slot_key = ?
                      WHERE s.bump = 1
                        AND d.scope_type = ? AND d.scope_id = ? AND d.namespace = ?
                        AND d.write_operation_id = ?
                        AND NOT EXISTS (
                            SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?
                        )",
                    vec![
                        text(&slot),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        text(operation_id),
                        text(operation_id),
                    ],
                ),
                exec(
                    "INSERT INTO bookclerk_receipts (
                        operation_id, operation_kind, request_hash, status, payload,
                        created_at, expires_at, consume_key
                     ) SELECT ?, 'replaceConfiguration', ?,
                        CASE WHEN s.bump = 1 THEN 'ok' ELSE 'conflict' END,
                        json_object('revision', d.revision, 'schema_version', d.schema_version),
                        ?, ?, NULL
                       FROM bookclerk_slots AS s
                       JOIN configuration_documents AS d
                         ON d.scope_type = ? AND d.scope_id = ? AND d.namespace = ?
                      WHERE s.slot_key = ?
                        AND NOT EXISTS (
                            SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?
                        )",
                    vec![
                        text(operation_id),
                        text(&request_hash),
                        text(&now),
                        text(&expires),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        text(&slot),
                        text(operation_id),
                    ],
                ),
                exec(
                    "DELETE FROM bookclerk_slots WHERE slot_key = ?",
                    vec![text(&slot)],
                ),
                query(
                    "SELECT status, request_hash, payload FROM bookclerk_receipts \
                     WHERE operation_id = ?",
                    vec![text(operation_id)],
                    1,
                ),
            ],
        ),
    )
    .await?;

    let rows = batch::rows(&reply, 7);
    let Some(row) = rows.first() else {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "configuration replace for {} did not record a receipt",
            key.namespace
        )));
    };
    let status = batch::text_cell(row, 0)?;
    let stored_hash = batch::text_cell(row, 1)?;
    let payload = batch::text_cell(row, 2)?;
    if stored_hash != request_hash {
        return Err(LibraryError::Conflict(format!(
            "idempotency conflict: operation {operation_id} was already used for a different configuration write"
        )));
    }
    let receipt: ReceiptPayload = serde_json::from_str(&payload).map_err(|err| {
        LibraryError::Other(anyhow::anyhow!(
            "invalid configuration receipt payload: {err}"
        ))
    })?;
    if status == STATUS_CONFLICT {
        return Ok(ReplaceOutcome::Conflict {
            current_revision: receipt.revision,
        });
    }
    if status != STATUS_OK {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "invalid configuration receipt status {status}"
        )));
    }
    let stored = load_document(store, key)
        .await?
        .ok_or_else(|| LibraryError::NotFound(key.namespace.clone()))?;
    if stored.write_operation_id == operation_id || stored.revision == receipt.revision {
        Ok(ReplaceOutcome::Applied(stored))
    } else {
        Ok(ReplaceOutcome::Replayed {
            revision: receipt.revision,
        })
    }
}

/// Counts committed change notices for one namespace.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn change_count(store: &LibraryStore, namespace: &str) -> Result<u64> {
    use sea_orm::{ColumnTrait, EntityTrait as _, PaginatorTrait, QueryFilter};
    crate::entities::configuration_changes::Entity::find()
        .filter(crate::entities::configuration_changes::Column::Namespace.eq(namespace))
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)
}

/// Counts audit rows for one namespace.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn audit_count(store: &LibraryStore, namespace: &str) -> Result<u64> {
    use sea_orm::{ColumnTrait, EntityTrait as _, PaginatorTrait, QueryFilter};
    crate::entities::configuration_audit::Entity::find()
        .filter(crate::entities::configuration_audit::Column::Namespace.eq(namespace))
        .count(store.db())
        .await
        .map_err(LibraryError::Orm)
}

/// Test barrier between an empty receipt lookup and the revision read.
///
/// Production builds do nothing. Tests arm it so a peer can commit the same
/// operation id before this caller treats the newer revision as a conflict.
#[cfg(test)]
async fn pause_after_empty_receipt_lookup(operation_id: &str) {
    let matches = EMPTY_RECEIPT_PAUSE
        .lock()
        .await
        .as_ref()
        .is_some_and(|pause| pause.operation_id == operation_id);
    if !matches {
        return;
    }
    let Some(pause) = EMPTY_RECEIPT_PAUSE.lock().await.take() else {
        return;
    };
    let _ = pause.arrived.send(());
    let _ = pause.release.await;
}

/// No pause outside tests.
#[cfg(not(test))]
async fn pause_after_empty_receipt_lookup(_operation_id: &str) {}

/// One paused empty-receipt lookup.
#[cfg(test)]
pub(crate) struct EmptyReceiptPause {
    /// Operation id that should pause. Other writers are not blocked.
    pub operation_id: String,
    /// Fired after the lookup observed no receipt.
    pub arrived: tokio::sync::oneshot::Sender<()>,
    /// Completes when the test has committed the original operation.
    pub release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
static EMPTY_RECEIPT_PAUSE: tokio::sync::Mutex<Option<EmptyReceiptPause>> =
    tokio::sync::Mutex::const_new(None);

/// Arms the next empty receipt lookup to pause until `release` completes.
#[cfg(test)]
pub(crate) async fn arm_empty_receipt_pause(pause: EmptyReceiptPause) {
    *EMPTY_RECEIPT_PAUSE.lock().await = Some(pause);
}

/// Resolves a prior receipt before a new compare-and-swap.
///
/// Kind and request hash must match. Expiry does not turn this operation id
/// into a new attempt: cleanup keeps the current id so a retry still matches.
async fn existing_receipt_outcome(
    store: &LibraryStore,
    operation_id: &str,
    request_hash: &str,
) -> Result<Option<ReplaceOutcome>> {
    let Some(receipt) = bookclerk_receipts::Entity::find_by_id(operation_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    if receipt.operation_kind != REPLACE_KIND || receipt.request_hash != request_hash {
        return Err(LibraryError::Conflict(format!(
            "idempotency conflict: operation {operation_id} was already used for a different configuration write"
        )));
    }
    let payload = receipt.payload.unwrap_or_default();
    let parsed: ReceiptPayload = serde_json::from_str(&payload).map_err(|err| {
        LibraryError::Other(anyhow::anyhow!(
            "invalid configuration receipt payload: {err}"
        ))
    })?;
    if receipt.status == STATUS_CONFLICT {
        return Ok(Some(ReplaceOutcome::Conflict {
            current_revision: parsed.revision,
        }));
    }
    if receipt.status != STATUS_OK {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "invalid configuration receipt status {}",
            receipt.status
        )));
    }
    Ok(Some(ReplaceOutcome::Replayed {
        revision: parsed.revision,
    }))
}

/// Maps a SeaORM row onto the domain document.
fn stored_from_model(row: configuration_documents::Model) -> StoredDocument {
    StoredDocument {
        key: DocumentKey {
            scope_type: row.scope_type,
            scope_id: row.scope_id,
            namespace: row.namespace,
        },
        schema_version: row.schema_version,
        revision: row.revision,
        document_json: row.document_json,
        updated_at: row.updated_at,
        updated_by: row.updated_by,
        write_operation_id: row.write_operation_id,
    }
}

/// Rejects actors that may not import.
fn authorize_import(actor: &ConfigActor) -> Result<()> {
    if actor.can_import() {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

/// Rejects actors that may not replace a document.
fn authorize_replace(actor: &ConfigActor) -> Result<()> {
    if actor.can_replace() {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

/// Rejects oversized, non-JSON, or NUL-bearing write inputs.
fn validate_payload(document_json: &str, operation_id: &str, actor: &ConfigActor) -> Result<()> {
    if document_json.len() > MAX_CONFIGURATION_DOCUMENT_BYTES {
        return Err(invalid(&format!(
            "document exceeds {MAX_CONFIGURATION_DOCUMENT_BYTES} bytes"
        )));
    }
    if document_json.contains('\u{0000}') || operation_id.contains('\u{0000}') {
        return Err(invalid("document and operation id must not contain NUL"));
    }
    if operation_id.is_empty() || operation_id.len() > MAX_ACTOR_LEN {
        return Err(invalid("operation id must be 1..=128 characters"));
    }
    let actor_id = actor.audit_id();
    if actor_id.is_empty() || actor_id.len() > MAX_ACTOR_LEN || actor_id.contains('\u{0000}') {
        return Err(invalid("audit actor must be 1..=128 characters"));
    }
    if serde_json::from_str::<serde_json::Value>(document_json).is_err() {
        return Err(invalid("document is not JSON"));
    }
    Ok(())
}

/// Stable request hash for one replacement attempt.
fn replacement_hash(
    key: &DocumentKey,
    schema_version: i64,
    expected_revision: i64,
    document_json: &str,
    actor: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        key.scope_type.as_str(),
        key.scope_id.as_str(),
        key.namespace.as_str(),
        &schema_version.to_string(),
        &expected_revision.to_string(),
        document_json,
        actor,
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0xff]);
    }
    hex::encode(hasher.finalize())
}

/// Error for a write the actor is not allowed to perform.
fn unauthorized() -> LibraryError {
    LibraryError::Other(anyhow::anyhow!(
        "unauthorized configuration write: operator authority is required"
    ))
}

/// Error for a document or operation id that fails validation.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("invalid configuration: {detail}"))
}

/// Error for a domain schema version this binary does not apply.
pub(super) fn unsupported(namespace: &str, version: i64) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!(
        "unsupported configuration schema version {version} for {namespace}"
    ))
}

#[derive(Debug, Deserialize)]
/// JSON stored on the replacement receipt.
struct ReceiptPayload {
    /// Revision observed when the receipt was written.
    revision: i64,
    /// Schema version observed when the receipt was written.
    #[allow(dead_code)]
    schema_version: i64,
}
