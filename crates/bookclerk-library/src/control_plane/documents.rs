//! Typed configuration documents: atomic replace, audit, and change notices.
//!
//! A JSON string is the physical body. Callers pass an already-validated
//! document for one namespace. Lost updates fail the `revision` predicate and
//! do not insert an audit row or a change notice.
//!
//! A body wider than one piece the database plugin carries (the smallest of
//! its cell, statement and result limits) is stored in
//! `configuration_document_parts` pieces under a set id unique to the write,
//! written before the batch that commits the document. That batch commits
//! only while every staged piece is still there; the document row then
//! names the set and its piece count, and the batch drops the set the row
//! replaced and, when it lost, the set it staged, each only while no row
//! names it. A write whose batch fails drops its staged set the same
//! guarded way.

use crate::EntityKind;
use bookclerk_plugin_abi::TypedDbStatement;
use chrono::{Duration, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::batch::{self, exec, int, query, request, text};
use crate::entities::{bookclerk_receipts, configuration_document_parts, configuration_documents};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Library scope instance. There is one library document per namespace.
pub const LIBRARY_SCOPE_ID: &str = "singleton";

/// Scope type stored for library documents.
pub const LIBRARY_SCOPE_TYPE: &str = "library";

/// Scope type stored for node documents.
pub const NODE_SCOPE_TYPE: &str = "node";

/// Scope type stored for one plugin instance's configuration.
pub const PLUGIN_INSTANCE_SCOPE_TYPE: &str = "plugin_instance";

/// Namespace of a plugin instance's typed configuration document.
pub const PLUGIN_INSTANCE_CONFIG_NAMESPACE: &str = "config";

/// Maximum UTF-8 size of `document_json`.
pub const MAX_CONFIGURATION_DOCUMENT_BYTES: usize = 16 * 1024;

/// Room a piece's row takes in a reply beside its body: its key columns and
/// the reply's framing.
const PART_ROW_OVERHEAD: usize = 512;

/// Reads of a document stored in pieces before a reader gives up because
/// concurrent replacements keep dropping the pieces it is about to read.
const PART_READ_ATTEMPTS: usize = 3;

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
    /// Portal administrator. Cannot write library or node configuration.
    Administrator {
        /// User id recorded if a write is attempted.
        id: String,
    },
    /// Portal member. Cannot write configuration.
    Member {
        /// User id recorded if a write is attempted.
        id: String,
    },
    /// Process startup. May import a missing document and may rewrite an
    /// instance document to a newer pinned schema; it is never constructed
    /// for an API caller.
    Bootstrap,
    /// The placement scheduler (one daemon, under the `scheduler` lease). It
    /// writes only `placed` / `unplaced` deployments; it is never
    /// constructed for an API caller.
    Scheduler,
    /// The user who owns one node. Writes that node's `node.runtime`
    /// document only. Built by [`super::verify_node_owner`], which checks
    /// `nodes.owner_user_id`, so a caller cannot claim a node it does not own.
    NodeOwner(VerifiedNodeOwner),
    /// An active portal Owner. Writes the library's branch-enrollment switch
    /// (`core.branch_enrollment`) and the default Administrator switches
    /// (`core.admin_controls`) only; every other document stays an
    /// operator write. Built by [`super::verify_library_owner`], which reads
    /// the user's stored role and status.
    LibraryOwner(VerifiedLibraryOwner),
    /// An active portal Owner or Administrator. Writes the library's
    /// patron defaults only: the default scope (`core.patron_scope`), the
    /// default member controls (`core.member_controls`) and the
    /// collections scopes match (`core.collections`). Built by
    /// [`super::verify_patron_administrator`], which reads the user's
    /// stored role and status.
    PatronAdministrator(VerifiedPatronAdministrator),
}

/// Proof that a user was an active Owner when the write was authorized.
///
/// Only [`super::verify_library_owner`] constructs one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedLibraryOwner {
    /// Owner's user id.
    pub(super) user_id: i64,
    /// Audit id (`owner:<user_id>`).
    pub(super) audit: String,
}

impl VerifiedLibraryOwner {
    /// Owner's user id.
    #[must_use]
    pub fn user_id(&self) -> i64 {
        self.user_id
    }
}

/// Proof that a user was an active Owner or Administrator when a write of
/// the library's patron defaults was authorized.
///
/// Only [`super::verify_patron_administrator`] constructs one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPatronAdministrator {
    /// The user's id.
    pub(super) user_id: i64,
    /// Audit id (`user:<user_id>`).
    pub(super) audit: String,
}

impl VerifiedPatronAdministrator {
    /// The user's id.
    #[must_use]
    pub fn user_id(&self) -> i64 {
        self.user_id
    }
}

/// Proof that a user owned a node when the write was authorized.
///
/// Only [`super::verify_node_owner`] constructs one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedNodeOwner {
    /// Owning user.
    pub(super) user_id: i64,
    /// Node the user owns.
    pub(super) node_id: String,
    /// Audit id (`node-owner:<user_id>`).
    pub(super) audit: String,
}

impl VerifiedNodeOwner {
    /// Owning user id.
    #[must_use]
    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// Node this owner may administer.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.node_id
    }
}

impl ConfigActor {
    /// Audit string stored on a committed document.
    pub(super) fn audit_id(&self) -> &str {
        match self {
            Self::Operator { id } | Self::Administrator { id } | Self::Member { id } => id,
            Self::Bootstrap => "bootstrap",
            Self::Scheduler => "scheduler",
            Self::NodeOwner(owner) => &owner.audit,
            Self::LibraryOwner(owner) => &owner.audit,
            Self::PatronAdministrator(admin) => &admin.audit,
        }
    }

    /// True when this actor may insert the missing document at `key`.
    fn can_import(&self, key: &DocumentKey) -> bool {
        match self {
            Self::Operator { .. } | Self::Bootstrap => true,
            Self::NodeOwner(owner) => owner.covers(key),
            Self::LibraryOwner(_) => {
                key.scope_type == LIBRARY_SCOPE_TYPE
                    && key.scope_id == LIBRARY_SCOPE_ID
                    && (key.namespace == super::BRANCH_ENROLLMENT_NAMESPACE
                        || key.namespace == super::ADMIN_CONTROLS_NAMESPACE
                        || key.namespace == super::INGRESS_NAMESPACE)
            }
            Self::PatronAdministrator(_) => {
                key.scope_type == LIBRARY_SCOPE_TYPE
                    && key.scope_id == LIBRARY_SCOPE_ID
                    && (key.namespace == super::PATRON_SCOPE_NAMESPACE
                        || key.namespace == super::MEMBER_CONTROLS_NAMESPACE
                        || key.namespace == super::COLLECTIONS_NAMESPACE)
            }
            Self::Administrator { .. } | Self::Member { .. } | Self::Scheduler => false,
        }
    }

    /// True when this actor may replace the document at `key`.
    fn can_replace(&self, key: &DocumentKey) -> bool {
        self.can_import(key)
    }
}

impl VerifiedNodeOwner {
    /// A node owner writes only its own node's runtime document.
    fn covers(&self, key: &DocumentKey) -> bool {
        key.scope_type == NODE_SCOPE_TYPE
            && key.scope_id == self.node_id
            && key.namespace == super::NODE_RUNTIME_NAMESPACE
    }
}

/// Address of one configuration document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentKey {
    /// `library`, `node`, or `plugin_instance`.
    pub scope_type: String,
    /// `singleton`, a node id, or a plugin instance id.
    pub scope_id: String,
    /// Domain namespace.
    pub namespace: String,
}

impl DocumentKey {
    /// Library-scoped domain.
    #[must_use]
    pub fn library(namespace: &str) -> Self {
        Self {
            scope_type: LIBRARY_SCOPE_TYPE.to_string(),
            scope_id: LIBRARY_SCOPE_ID.to_string(),
            namespace: namespace.to_string(),
        }
    }

    /// Node-scoped domain.
    #[must_use]
    pub fn node(node_id: &str, namespace: &str) -> Self {
        Self {
            scope_type: NODE_SCOPE_TYPE.to_string(),
            scope_id: node_id.to_string(),
            namespace: namespace.to_string(),
        }
    }

    /// Plugin-instance configuration document.
    ///
    /// The address is `(plugin_instance, instance_id, config)`. It does not
    /// include a plugin key, capability, alias, or node id.
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
/// A body stored in pieces is read in keyset pages sized to the database
/// plugin's result limits and joined again. When a concurrent replacement
/// dropped the pieces between the row read and the piece read, the row is
/// read again, up to [`PART_READ_ATTEMPTS`] times.
///
/// # Errors
///
/// Returns an error when the read fails, or [`LibraryError::Unavailable`]
/// when replacements kept dropping the pieces of every row read.
pub async fn load_document(
    store: &LibraryStore,
    key: &DocumentKey,
) -> Result<Option<StoredDocument>> {
    Ok(load_row(store, key).await?.map(|(_, document)| document))
}

/// Reads one document like [`load_document`], with the set of pieces its row
/// names (`None` when the body is inline).
///
/// # Errors
///
/// As [`load_document`].
async fn load_row(
    store: &LibraryStore,
    key: &DocumentKey,
) -> Result<Option<(Option<String>, StoredDocument)>> {
    for _ in 0..PART_READ_ATTEMPTS {
        let Some(row) = configuration_documents::Entity::find_by_id((
            key.scope_type.clone(),
            key.scope_id.clone(),
            key.namespace.clone(),
        ))
        .one(store.db())
        .await
        .map_err(LibraryError::from_db_err)?
        else {
            return Ok(None);
        };
        if row.document_parts == 0 {
            return Ok(Some((None, stored_from_model(row))));
        }
        if let Some(body) = read_parts(store, &row).await? {
            let parts_id = row.document_json.clone();
            return Ok(Some((
                Some(parts_id),
                StoredDocument {
                    document_json: body,
                    ..stored_from_model(row)
                },
            )));
        }
    }
    Err(LibraryError::Unavailable(format!(
        "configuration document {} kept changing while it was read",
        key.namespace
    )))
}

/// The body `row`'s pieces join into, or `None` when they are not a
/// gapless run of `row.document_parts` pieces from 0 (a later replacement
/// dropped them). A pieced row's `document_json` holds the id of its set of
/// pieces.
///
/// # Errors
///
/// Returns the database error.
async fn read_parts(
    store: &LibraryStore,
    row: &configuration_documents::Model,
) -> Result<Option<String>> {
    let caps = store.db_capabilities();
    let width = super::chunk_bytes(caps).saturating_add(PART_ROW_OVERHEAD);
    // The host refuses a plugin whose result limit is below its minimum, so
    // the limit is never zero.
    let by_bytes = u64::from(caps.max_result_bytes) / u64::try_from(width).unwrap_or(u64::MAX);
    let mut page = crate::store::keyset::result_page(store)
        .min(by_bytes)
        .max(1);
    let pieces = crate::store::keyset::read_keyset_pages(
        store,
        configuration_document_parts::Entity::find()
            .filter(configuration_document_parts::Column::ScopeType.eq(row.scope_type.clone()))
            .filter(configuration_document_parts::Column::ScopeId.eq(row.scope_id.clone()))
            .filter(configuration_document_parts::Column::Namespace.eq(row.namespace.clone()))
            .filter(configuration_document_parts::Column::PartsId.eq(row.document_json.clone())),
        &[configuration_document_parts::Column::Seq],
        &mut page,
    )
    .await?;
    let gapless = i64::try_from(pieces.len()).ok() == Some(row.document_parts)
        && (0_i64..).zip(&pieces).all(|(seq, piece)| piece.seq == seq);
    Ok(gapless.then(|| pieces.into_iter().map(|piece| piece.body).collect()))
}

/// How a body is stored: the text the document row holds, its piece count,
/// and the piece inserts to run before the row names them.
struct StoredBody {
    /// `document_json`: the whole body, or the id of its set of pieces.
    inline: String,
    /// The id of the set of pieces, unique to this write; `None` when the
    /// body is inline.
    parts_id: Option<String>,
    /// `document_parts`: `0` when the body is inline.
    parts: i64,
    /// One insert per piece, empty when the body is inline.
    inserts: Vec<TypedDbStatement>,
}

/// Cuts `document_json` into pieces when it is wider than one piece the
/// database plugin carries.
///
/// The pieces get a set id of their own, `operation_id` plus a random
/// suffix, so two calls that reuse an operation id never share or overwrite
/// each other's pieces.
fn stored_body(
    store: &LibraryStore,
    key: &DocumentKey,
    document_json: &str,
    operation_id: &str,
) -> StoredBody {
    let width = super::chunk_bytes(store.db_capabilities());
    if document_json.len() <= width {
        return StoredBody {
            inline: document_json.to_string(),
            parts_id: None,
            parts: 0,
            inserts: Vec::new(),
        };
    }
    let parts_id = format!("{operation_id}:{}", uuid::Uuid::new_v4());
    let inserts: Vec<TypedDbStatement> = (0_i64..)
        .zip(super::cut_text(document_json, width))
        .map(|(seq, body)| {
            exec(
                "INSERT OR IGNORE INTO configuration_document_parts (
                    scope_type, scope_id, namespace, parts_id, seq, body
                 ) VALUES (?, ?, ?, ?, ?, ?)",
                vec![
                    text(&key.scope_type),
                    text(&key.scope_id),
                    text(&key.namespace),
                    text(&parts_id),
                    int(seq),
                    text(body),
                ],
            )
        })
        .collect();
    StoredBody {
        inline: parts_id.clone(),
        parts_id: Some(parts_id),
        parts: i64::try_from(inserts.len()).unwrap_or(i64::MAX),
        inserts,
    }
}

/// Writes `body`'s pieces in as few batches as the database plugin's
/// limits allow. They are invisible until a document row names their set.
/// When a batch fails, the pieces already written are dropped again; a
/// write that stops for good before its row names them (a crash) leaves
/// them unnamed, taking space but never read.
///
/// # Errors
///
/// Returns the database error, or a limit error when one piece does not
/// fit a batch.
async fn write_parts(store: &LibraryStore, key: &DocumentKey, body: StoredBody) -> Result<()> {
    let Some(parts_id) = body.parts_id else {
        return Ok(());
    };
    let mut batches = crate::store::write_batches::WriteBatches::new(
        store.db_capabilities(),
        "configuration piece",
    );
    let mut full = Vec::new();
    for insert in body.inserts {
        full.extend(batches.push(insert)?);
    }
    full.extend(batches.finish());
    for (part, statements) in full.into_iter().enumerate() {
        if let Err(err) =
            batch::execute_host_batch(store, request(&format!("{parts_id}:{part}"), statements))
                .await
        {
            drop_staged_parts(store, key, Some(&parts_id)).await;
            return Err(err);
        }
    }
    Ok(())
}

/// Drops the pieces a write staged under `parts_id` unless the row now
/// names them, on a best-effort basis.
///
/// Runs when a staging or publication batch failed: a publication that
/// never applied leaves its set unnamed and it goes, one that applied with
/// its reply lost names its set and it stays. A failed clean-up leaves only
/// unnamed pieces, which are never read.
///
/// # Arguments
///
/// * `store` - The store the write went to.
/// * `key` - The document the write targeted.
/// * `parts_id` - The set the write staged, `None` for an inline body.
async fn drop_staged_parts(store: &LibraryStore, key: &DocumentKey, parts_id: Option<&str>) {
    let Some(parts_id) = parts_id else {
        return;
    };
    if let Some(drop) = drop_unnamed_parts(key, &[parts_id]) {
        let _ = batch::execute_host_batch(store, request(&format!("{parts_id}:drop"), vec![drop]))
            .await;
    }
}

/// Drops the pieces of `key`'s document in the sets `parts_ids` that its
/// row does not name: the set a replacement replaced, and the set a write
/// staged when it did not commit. A set the row names is kept, and so is
/// every other set, which another write may be staging.
///
/// # Returns
///
/// `None` when there is no set to drop.
fn drop_unnamed_parts(key: &DocumentKey, parts_ids: &[&str]) -> Option<TypedDbStatement> {
    if parts_ids.is_empty() {
        return None;
    }
    let mut binds = vec![
        text(&key.scope_type),
        text(&key.scope_id),
        text(&key.namespace),
    ];
    binds.extend(parts_ids.iter().map(|id| text(id)));
    binds.extend([
        text(&key.scope_type),
        text(&key.scope_id),
        text(&key.namespace),
    ]);
    let marks = vec!["?"; parts_ids.len()].join(", ");
    Some(exec(
        &format!(
            "DELETE FROM configuration_document_parts
              WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                AND parts_id IN ({marks})
                AND parts_id NOT IN (
                    SELECT document_json FROM configuration_documents
                     WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                       AND document_parts > 0
                )"
        ),
        binds,
    ))
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
    authorize_import(actor, key)?;
    validate_payload(document_json, operation_id, actor)?;
    if let Some(existing) = load_document(store, key).await? {
        return Ok(existing);
    }
    let body = stored_body(store, key, document_json, operation_id);
    let (inline, parts, parts_id) = (body.inline.clone(), body.parts, body.parts_id.clone());
    write_parts(store, key, body).await?;
    pause_after_staging(operation_id).await;
    let now = Utc::now().to_rfc3339();
    let mut statements = vec![
                exec(
                    "INSERT OR IGNORE INTO configuration_documents (
                    scope_type, scope_id, namespace, schema_version, revision,
                    document_json, document_parts, updated_at, updated_by, write_operation_id
                ) SELECT ?, ?, ?, ?, 1, ?, ?, ?, ?, ?
                   WHERE (SELECT COUNT(*) FROM configuration_document_parts
                           WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                             AND parts_id = ?) = ?",
                    vec![
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        int(schema_version),
                        text(&inline),
                        int(parts),
                        text(&now),
                        text(actor.audit_id()),
                        text(operation_id),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        text(parts_id.as_deref().unwrap_or_default()),
                        int(parts),
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
                // The notice above went in exactly when this write's document
                // did (or a retry of it, which costs a re-read at most).
                super::bump_if_any(
                    super::LiveTopic::Config,
                    query(
                        "SELECT 1 FROM configuration_documents
                          WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                            AND write_operation_id = ? AND revision = 1",
                        vec![
                            text(&key.scope_type),
                            text(&key.scope_id),
                            text(&key.namespace),
                            text(operation_id),
                        ],
                        1,
                    ),
                ),
    ];
    statements.extend(drop_unnamed_parts(
        key,
        &parts_id.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    if let Err(err) = batch::execute_host_batch(store, request(operation_id, statements)).await {
        drop_staged_parts(store, key, parts_id.as_deref()).await;
        return Err(err);
    }
    load_document(store, key)
        .await?
        .ok_or_else(|| LibraryError::not_found(EntityKind::Configuration, &key.namespace))
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
    authorize_replace(actor, key)?;
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
    let (replaced_parts, current) = load_row(store, key)
        .await?
        .ok_or_else(|| LibraryError::not_found(EntityKind::Configuration, &key.namespace))?;
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
    let body = stored_body(store, key, document_json, operation_id);
    let (inline, parts, parts_id) = (body.inline.clone(), body.parts, body.parts_id.clone());
    write_parts(store, key, body).await?;
    // Another write can commit after these pieces are staged and before the
    // swap below names them.
    pause_after_staging(operation_id).await;
    let now = Utc::now().to_rfc3339();
    let expires = (Utc::now() + Duration::hours(24)).to_rfc3339();
    let slot = format!("config-cas:{operation_id}");
    let mut statements = vec![
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
                        document_parts = ?, updated_at = ?, updated_by = ?,
                        write_operation_id = ?
                     WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                       AND revision = ? AND schema_version = ?
                       AND NOT EXISTS (
                            SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?
                       )
                       AND (SELECT COUNT(*) FROM configuration_document_parts
                             WHERE scope_type = ? AND scope_id = ? AND namespace = ?
                               AND parts_id = ?) = ?",
                    vec![
                        int(schema_version),
                        text(&inline),
                        int(parts),
                        text(&now),
                        text(actor.audit_id()),
                        text(operation_id),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        int(expected_revision),
                        int(schema_version),
                        text(operation_id),
                        text(&key.scope_type),
                        text(&key.scope_id),
                        text(&key.namespace),
                        text(parts_id.as_deref().unwrap_or_default()),
                        int(parts),
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
                // The slot holds 1 exactly when the replacement went in.
                super::bump_if_any(
                    super::LiveTopic::Config,
                    query(
                        "SELECT 1 FROM bookclerk_slots WHERE slot_key = ? AND bump = 1",
                        vec![text(&slot)],
                        1,
                    ),
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
    ];
    // The receipt read stays statement 8; the clean-up goes after it.
    let stale: Vec<&str> = [replaced_parts.as_deref(), parts_id.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    statements.extend(drop_unnamed_parts(key, &stale));
    let reply = match batch::execute_host_batch(store, request(operation_id, statements)).await {
        Ok(reply) => reply,
        Err(err) => {
            drop_staged_parts(store, key, parts_id.as_deref()).await;
            return Err(err);
        }
    };

    let rows = batch::rows(&reply, 8);
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
        .ok_or_else(|| LibraryError::not_found(EntityKind::Configuration, &key.namespace))?;
    if stored.write_operation_id == operation_id || stored.revision == receipt.revision {
        // Library settings and instance documents both land here; every node
        // re-reads what it runs on the next pass.
        super::node_queue::notify_all_nodes(
            store,
            super::node_queue::NodeMessage::ConfigurationChanged {
                scope_type: stored.key.scope_type.clone(),
                scope_id: stored.key.scope_id.clone(),
                namespace: stored.key.namespace.clone(),
                revision: stored.revision,
            },
        )
        .await;
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

/// Test barrier between staging a write's pieces and the swap that
/// names them.
///
/// Production builds do nothing. Tests arm it so another write can commit
/// while this caller's pieces are staged but not yet named.
#[cfg(test)]
async fn pause_after_staging(operation_id: &str) {
    let pause = {
        let mut armed = STAGED_PAUSES.lock().await;
        armed
            .iter()
            .position(|pause| pause.operation_id == operation_id)
            .map(|at| armed.swap_remove(at))
    };
    if let Some(pause) = pause {
        let _ = pause.arrived.send(());
        let _ = pause.release.await;
    }
}

/// No pause outside tests.
#[cfg(not(test))]
async fn pause_after_staging(_operation_id: &str) {}

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

/// The armed pauses between staging a write's pieces and its swap, one per
/// operation id, so tests running at once do not take each other's.
#[cfg(test)]
static STAGED_PAUSES: tokio::sync::Mutex<Vec<EmptyReceiptPause>> =
    tokio::sync::Mutex::const_new(Vec::new());

/// Arms the import or replacement named by `pause.operation_id` to pause after it
/// stages its pieces, until `release` completes.
#[cfg(test)]
pub(crate) async fn arm_staged_pause(pause: EmptyReceiptPause) {
    STAGED_PAUSES.lock().await.push(pause);
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
fn authorize_import(actor: &ConfigActor, key: &DocumentKey) -> Result<()> {
    if actor.can_import(key) {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

/// Rejects actors that may not replace a document.
fn authorize_replace(actor: &ConfigActor, key: &DocumentKey) -> Result<()> {
    if actor.can_replace(key) {
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
    LibraryError::Forbidden(
        "unauthorized configuration write: operator authority is required".into(),
    )
}

/// Error for a document or operation id that fails validation.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid configuration: {detail}"))
}

/// Error for a domain schema version this binary does not apply.
pub(super) fn unsupported(namespace: &str, version: i64) -> LibraryError {
    LibraryError::Invalid(format!(
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

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    use super::*;
    use crate::test_support::{sqlite, uncapped, Profile};

    /// A JSON document of about `bytes` bytes, multibyte so a piece's byte
    /// and character counts differ.
    fn wide_document(bytes: usize) -> String {
        let mut note = String::new();
        while note.len() < bytes {
            note.push_str("é—x");
        }
        serde_json::json!({ "note": note }).to_string()
    }

    /// Every stored piece of `key`'s document, in key order.
    async fn pieces(
        store: &LibraryStore,
        key: &DocumentKey,
    ) -> Vec<configuration_document_parts::Model> {
        let mut rows = uncapped(configuration_document_parts::Entity::find().all(store.db()))
            .await
            .unwrap();
        rows.retain(|row| row.namespace == key.namespace);
        rows.sort_by(|a, b| (&a.parts_id, a.seq).cmp(&(&b.parts_id, b.seq)));
        rows
    }

    /// The operator that writes the test documents.
    fn operator() -> ConfigActor {
        ConfigActor::Operator { id: "cli".into() }
    }

    #[tokio::test]
    async fn a_document_wider_than_a_piece_is_stored_in_pieces_under_every_profile() {
        for profile in Profile::ALL {
            let name = format!("{profile:?}");
            let kit = sqlite::memory(profile).await;
            let store = kit.into_store();
            let key = DocumentKey::library("test.wide");
            let width = super::super::chunk_bytes(store.db_capabilities());
            let wide = wide_document(
                width
                    .saturating_mul(3)
                    .min(MAX_CONFIGURATION_DOCUMENT_BYTES - 64),
            );
            let stored = import_if_absent(&store, &operator(), &key, 1, &wide, "op-import")
                .await
                .unwrap();
            assert_eq!(stored.document_json, wide, "{name}");
            let row = uncapped(
                configuration_documents::Entity::find_by_id((
                    key.scope_type.clone(),
                    key.scope_id.clone(),
                    key.namespace.clone(),
                ))
                .one(store.db()),
            )
            .await
            .unwrap()
            .unwrap();
            let first = pieces(&store, &key).await;
            if wide.len() > width {
                assert!(row.document_json.starts_with("op-import:"), "{name}");
                assert_eq!(
                    i64::try_from(first.len()).unwrap(),
                    row.document_parts,
                    "{name}"
                );
                assert!(
                    first.iter().all(|piece| piece.body.len() <= width),
                    "{name}"
                );
            } else {
                assert_eq!(row.document_parts, 0, "{name}");
                assert!(first.is_empty(), "{name}");
            }

            // A wider replacement goes in under its own operation; the
            // earlier pieces are dropped once the row names the new ones.
            let wider = wide_document(
                width
                    .saturating_mul(4)
                    .min(MAX_CONFIGURATION_DOCUMENT_BYTES - 64),
            );
            let outcome = replace_document(&store, &operator(), &key, 1, 1, &wider, "op-wider")
                .await
                .unwrap();
            let ReplaceOutcome::Applied(applied) = outcome else {
                panic!("{name}: {outcome:?}");
            };
            assert_eq!(applied.document_json, wider, "{name}");
            assert!(
                pieces(&store, &key)
                    .await
                    .iter()
                    .all(|piece| piece.parts_id.starts_with("op-wider:")),
                "{name}"
            );

            // A replacement that loses its revision check leaves the
            // stored document and drops the pieces it staged.
            let lost = replace_document(&store, &operator(), &key, 1, 1, &wide, "op-lost")
                .await
                .unwrap();
            assert_eq!(
                lost,
                ReplaceOutcome::Conflict {
                    current_revision: 2
                },
                "{name}"
            );
            assert!(
                pieces(&store, &key)
                    .await
                    .iter()
                    .all(|piece| piece.parts_id.starts_with("op-wider:")),
                "{name}"
            );
            assert_eq!(
                load_document(&store, &key)
                    .await
                    .unwrap()
                    .unwrap()
                    .document_json,
                wider,
                "{name}"
            );

            // An inline replacement drops every piece.
            let small = r#"{"note":"small"}"#;
            let outcome = replace_document(&store, &operator(), &key, 1, 2, small, "op-small")
                .await
                .unwrap();
            assert!(matches!(outcome, ReplaceOutcome::Applied(_)), "{name}");
            assert!(pieces(&store, &key).await.is_empty(), "{name}");
            assert_eq!(
                load_document(&store, &key)
                    .await
                    .unwrap()
                    .unwrap()
                    .document_json,
                small,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn a_replacement_that_loses_after_staging_keeps_other_writers_pieces() {
        for profile in Profile::ALL {
            let name = format!("{profile:?}");
            let store = sqlite::memory(profile).await.into_store();
            let key = DocumentKey::library("test.wide");
            let width = super::super::chunk_bytes(store.db_capabilities());
            let first = wide_document(width.saturating_mul(3).min(8_000));
            let losing = wide_document(width.saturating_mul(3).min(8_000) + 7);
            let winning = wide_document(width.saturating_mul(3).min(8_000) + 13);
            import_if_absent(&store, &operator(), &key, 1, &first, "op-import")
                .await
                .unwrap();
            // The losing write reads revision 1, stages its pieces and
            // pauses before its swap.
            let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            arm_staged_pause(EmptyReceiptPause {
                operation_id: "op-race-lost".into(),
                arrived: arrived_tx,
                release: release_rx,
            })
            .await;
            let loser_store = store.clone();
            let loser_key = key.clone();
            let loser = tokio::spawn(async move {
                replace_document(
                    &loser_store,
                    &operator(),
                    &loser_key,
                    1,
                    1,
                    &losing,
                    "op-race-lost",
                )
                .await
            });
            arrived_rx
                .await
                .expect("the losing write stages its pieces");
            // Another write commits revision 2 meanwhile.
            let won = replace_document(&store, &operator(), &key, 1, 1, &winning, "op-race-won")
                .await
                .unwrap();
            assert!(matches!(won, ReplaceOutcome::Applied(_)), "{name}: {won:?}");
            // A third write reads revision 2 and stages its pieces before the
            // losing write cleans up.
            let third_body = wide_document(width.saturating_mul(3).min(8_000) + 19);
            let (third_arrived_tx, third_arrived_rx) = tokio::sync::oneshot::channel();
            let (third_release_tx, third_release_rx) = tokio::sync::oneshot::channel();
            arm_staged_pause(EmptyReceiptPause {
                operation_id: "op-race-third".into(),
                arrived: third_arrived_tx,
                release: third_release_rx,
            })
            .await;
            let third = {
                let (store, key, body) = (store.clone(), key.clone(), third_body.clone());
                tokio::spawn(async move {
                    replace_document(&store, &operator(), &key, 1, 2, &body, "op-race-third").await
                })
            };
            third_arrived_rx
                .await
                .expect("the third write stages its pieces");
            release_tx.send(()).expect("release the losing write");
            let lost = loser.await.unwrap().unwrap();
            assert!(
                matches!(
                    lost,
                    ReplaceOutcome::Conflict {
                        current_revision: 2
                    }
                ),
                "{name}: {lost:?}"
            );
            // The winner reads back whole.
            let loaded = load_document(&store, &key).await.unwrap().unwrap();
            assert_eq!(loaded.revision, 2, "{name}");
            assert_eq!(loaded.document_json, winning, "{name}");
            // The third write's staged pieces outlived the loser's clean-up:
            // it commits and reads back whole, and only its pieces remain.
            third_release_tx.send(()).expect("release the third write");
            let applied = third.await.unwrap().unwrap();
            assert!(
                matches!(applied, ReplaceOutcome::Applied(_)),
                "{name}: {applied:?}"
            );
            let loaded = load_document(&store, &key).await.unwrap().unwrap();
            assert_eq!(loaded.revision, 3, "{name}");
            assert_eq!(loaded.document_json, third_body, "{name}");
            let sets: std::collections::BTreeSet<String> = pieces(&store, &key)
                .await
                .into_iter()
                .map(|piece| piece.parts_id)
                .collect();
            if third_body.len() > width {
                assert_eq!(sets.len(), 1, "{name}: {sets:?}");
                assert!(
                    sets.iter().all(|id| id.starts_with("op-race-third:")),
                    "{name}: {sets:?}"
                );
            } else {
                assert!(sets.is_empty(), "{name}: {sets:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_failed_piece_batch_drops_the_pieces_it_staged() {
        let kit = sqlite::memory(Profile::Minimum).await;
        let store = kit.store();
        let key = DocumentKey::library("test.wide");
        let width = super::super::chunk_bytes(store.db_capabilities());
        let wide = wide_document(
            width
                .saturating_mul(40)
                .min(MAX_CONFIGURATION_DOCUMENT_BYTES - 64),
        );
        kit.lose_next_replies(1);
        let err = import_if_absent(store, &operator(), &key, 1, &wide, "op-import")
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err:?}");
        assert!(pieces(store, &key).await.is_empty());
        assert!(load_document(store, &key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_failed_publication_keeps_only_the_named_pieces_under_every_profile() {
        for profile in Profile::ALL {
            for lost_reply in [false, true] {
                let name = format!("{profile:?} lost_reply={lost_reply}");
                let kit = sqlite::memory(profile).await;
                let store = kit.store();
                let key = DocumentKey::library("test.wide");
                let width = super::super::chunk_bytes(store.db_capabilities());
                let first = wide_document(width.saturating_mul(3).min(8_000));
                let second = wide_document(width.saturating_mul(3).min(8_000) + 11);
                let fail_next = |prefix: &str| {
                    kit.aim_faults_at(Some(prefix));
                    if lost_reply {
                        kit.lose_next_replies(1);
                    } else {
                        kit.lock_next_requests(1);
                    }
                };
                // Only the import's own batch fails; its pieces were staged.
                fail_next("INSERT OR IGNORE INTO configuration_documents");
                let err = import_if_absent(store, &operator(), &key, 1, &first, "op-import")
                    .await
                    .unwrap_err();
                kit.aim_faults_at(None);
                assert!(
                    matches!(err, LibraryError::Unavailable(_)),
                    "{name}: {err:?}"
                );
                let loaded = load_document(store, &key).await.unwrap();
                if lost_reply {
                    assert_eq!(loaded.unwrap().document_json, first, "{name}");
                } else {
                    assert!(loaded.is_none(), "{name}");
                    assert!(pieces(store, &key).await.is_empty(), "{name}");
                    import_if_absent(store, &operator(), &key, 1, &first, "op-import")
                        .await
                        .unwrap();
                }
                // Only the replacement's swap fails.
                fail_next("INSERT OR IGNORE INTO bookclerk_slots");
                let err = replace_document(store, &operator(), &key, 1, 1, &second, "op-second")
                    .await
                    .unwrap_err();
                kit.aim_faults_at(None);
                assert!(
                    matches!(err, LibraryError::Unavailable(_)),
                    "{name}: {err:?}"
                );
                let loaded = load_document(store, &key).await.unwrap().unwrap();
                let (revision, body, set) = if lost_reply {
                    (2, &second, "op-second:")
                } else {
                    (1, &first, "op-import:")
                };
                assert_eq!(loaded.revision, revision, "{name}");
                assert_eq!(&loaded.document_json, body, "{name}");
                let sets: std::collections::BTreeSet<String> = pieces(store, &key)
                    .await
                    .into_iter()
                    .map(|piece| piece.parts_id)
                    .collect();
                if body.len() > width {
                    assert_eq!(sets.len(), 1, "{name}: {sets:?}");
                    assert!(
                        sets.iter().all(|id| id.starts_with(set)),
                        "{name}: {sets:?}"
                    );
                } else {
                    assert!(sets.is_empty(), "{name}: {sets:?}");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_write_whose_pieces_vanished_before_its_swap_names_nothing() {
        for profile in Profile::ALL {
            let name = format!("{profile:?}");
            let store = sqlite::memory(profile).await.into_store();
            let key = DocumentKey::library("test.wide");
            let width = super::super::chunk_bytes(store.db_capabilities());
            let wide = wide_document(width.saturating_mul(3).min(8_000));
            if wide.len() <= width {
                continue;
            }
            // Each write's staged pieces are dropped while it is paused before
            // its swap, as a clean-up that ran ahead of a late swap would.
            let paused = |operation_id: &str| {
                let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                let pause = EmptyReceiptPause {
                    operation_id: operation_id.into(),
                    arrived: arrived_tx,
                    release: release_rx,
                };
                (pause, arrived_rx, release_tx)
            };
            let drop_staged = |prefix: &'static str| {
                let store = store.clone();
                async move {
                    uncapped(
                        configuration_document_parts::Entity::delete_many()
                            .filter(
                                configuration_document_parts::Column::PartsId.starts_with(prefix),
                            )
                            .exec(store.db()),
                    )
                    .await
                    .unwrap();
                }
            };

            let (pause, arrived, release) = paused("op-vanish-import");
            arm_staged_pause(pause).await;
            let writer = {
                let (store, key, wide) = (store.clone(), key.clone(), wide.clone());
                tokio::spawn(async move {
                    import_if_absent(&store, &operator(), &key, 1, &wide, "op-vanish-import").await
                })
            };
            arrived.await.expect("the import stages its pieces");
            drop_staged("op-vanish-import:").await;
            release.send(()).expect("release the import");
            assert!(writer.await.unwrap().is_err(), "{name}");
            assert!(
                load_document(&store, &key).await.unwrap().is_none(),
                "{name}"
            );

            import_if_absent(&store, &operator(), &key, 1, &wide, "op-import-2")
                .await
                .unwrap();
            let (pause, arrived, release) = paused("op-vanish-replace");
            arm_staged_pause(pause).await;
            let writer = {
                let (store, key) = (store.clone(), key.clone());
                let wider = wide_document(width.saturating_mul(3).min(8_000) + 5);
                tokio::spawn(async move {
                    replace_document(&store, &operator(), &key, 1, 1, &wider, "op-vanish-replace")
                        .await
                })
            };
            arrived.await.expect("the replacement stages its pieces");
            drop_staged("op-vanish-replace:").await;
            release.send(()).expect("release the replacement");
            let outcome = writer.await.unwrap().unwrap();
            assert!(
                matches!(
                    outcome,
                    ReplaceOutcome::Conflict {
                        current_revision: 1
                    }
                ),
                "{name}: {outcome:?}"
            );
            let loaded = load_document(&store, &key).await.unwrap().unwrap();
            assert_eq!(loaded.revision, 1, "{name}");
            assert_eq!(loaded.document_json, wide, "{name}");
        }
    }

    #[tokio::test]
    async fn a_document_missing_a_piece_is_read_again_then_refused() {
        let store = sqlite::memory(Profile::Minimum).await.into_store();
        let key = DocumentKey::library("test.wide");
        let width = super::super::chunk_bytes(store.db_capabilities());
        let wide = wide_document(width * 3);
        import_if_absent(&store, &operator(), &key, 1, &wide, "op-import")
            .await
            .unwrap();
        uncapped(
            configuration_document_parts::Entity::delete_many()
                .filter(configuration_document_parts::Column::Seq.eq(1))
                .exec(store.db()),
        )
        .await
        .unwrap();
        let err = load_document(&store, &key).await.unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err:?}");
    }
}
