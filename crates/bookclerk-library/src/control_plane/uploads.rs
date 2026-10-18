//! Streamed uploads from a Host node (H-4 design, section 6; decision 4).
//!
//! An acquire a Host runs returns its files to the owner as streams: the
//! Host asks for an upload of one title ([`grant_upload`]), streams each
//! declared file to the owner's upload endpoint, and commits the title
//! ([`commit_upload`]). The owner node streams every file straight on to
//! its output destinations; nothing is staged on the owner, and this
//! module stores only the upload's bookkeeping (`node_upload_sessions`).
//!
//! The upload is bound to the job's fence: it is granted only under the
//! live lease of a node still trusted with the job, every file and the
//! commit are accepted only while that holds ([`upload_fence_live`]), and
//! the commit is one canonical batch whose every statement is gated on the
//! job still running under that fence and the node's credential still
//! being live, so a fence lost at any point before the commit leaves the
//! library untouched. The commit marks the title acquired, publishes its
//! `book_acquired` event, and, for the job's last upload, completes the
//! job in the same transaction.
//!
//! The bearer token is 32 random bytes; only its hash
//! ([`crate::hash_token`]) is stored, and the plaintext travels to the Host
//! sealed to its box key, never as frame text.

use std::collections::BTreeSet;

use base64::Engine as _;
use bookclerk_node_proto::{UploadFile, MAX_UPLOAD_FILES};
use chrono::{DateTime, Duration, Utc};
use rand::RngCore as _;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::batch::{self, exec, request, text};
use super::job_routing::{job_plugin_keys, JobClaimLimits};
use super::remote_work::{fenced_job, held_job_permits, remote_job_fence};
use super::semaphore::release_permits;
use super::trust::{accounts_owned_by, node_may_claim_job, node_trust, Claimant, NodeTrust};
use crate::entities::{books, node_upload_sessions};
use crate::error::{LibraryError, Result};
use crate::models::{JobKind, JobState};
use crate::store::event_outbox::{book_acquired_spec, prepare_publish_domain_event};
use crate::store::LibraryStore;

/// How long a granted upload's token is accepted.
pub const UPLOAD_TOKEN_TTL_SECS: i64 = 60 * 60;

/// Session state: files may still arrive.
const STATE_OPEN: &str = "open";
/// Session state: the title was committed.
const STATE_COMMITTED: &str = "committed";
/// Session state: abandoned (superseded, failed, or its fence lost).
const STATE_ABORTED: &str = "aborted";

/// The declared files of an upload, as stored in `manifest_json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    /// Every file, in upload order.
    files: Vec<UploadFile>,
    /// Storage key of the title's companion PDF, when one is uploaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pdf_key: Option<String>,
}

/// What a Host asks to upload for one title.
#[derive(Debug, Clone, Copy)]
pub struct UploadGrantRequest<'a> {
    /// Library id of the title. Empty for the job's closing upload, which
    /// carries no files and only completes the job.
    pub book_uuid: &'a str,
    /// Storage key of the title's primary audio file (one of `files`).
    pub primary_key: &'a str,
    /// Storage key of the companion PDF (one of `files`), when uploaded.
    pub pdf_key: Option<&'a str>,
    /// Every file, in upload order.
    pub files: &'a [UploadFile],
}

/// Outcome of [`grant_upload`].
pub enum UploadGrant {
    /// The upload is open.
    Granted {
        /// Upload id.
        upload_id: String,
        /// The bearer token (zeroized on drop; seal it before it leaves
        /// the process).
        token: Zeroizing<String>,
        /// When the token stops being accepted.
        expires_at: DateTime<Utc>,
    },
    /// The fence no longer owns the job, or the node is no longer trusted
    /// with it.
    Lost,
    /// Not allowed (the reason is stable).
    Refused(&'static str),
}

impl std::fmt::Debug for UploadGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Granted {
                upload_id,
                expires_at,
                ..
            } => f
                .debug_struct("Granted")
                .field("upload_id", upload_id)
                .field("token", &"<redacted>")
                .field("expires_at", expires_at)
                .finish(),
            Self::Lost => f.write_str("Lost"),
            Self::Refused(reason) => f.debug_tuple("Refused").field(reason).finish(),
        }
    }
}

/// One open upload, as the upload endpoint sees it.
#[derive(Debug, Clone)]
pub struct UploadSession {
    /// Upload id.
    pub upload_id: String,
    /// Host node it was granted to.
    pub node_id: String,
    /// Acquire job.
    pub job_id: String,
    /// Lease generation it is bound to.
    pub generation: i64,
    /// Owner node that receives the files.
    pub receiving_node_id: String,
    /// Library id of the title (empty for the job's closing upload).
    pub book_uuid: String,
    /// Storage key of the primary audio file.
    pub primary_key: String,
    /// Storage key of the companion PDF, when uploaded.
    pub pdf_key: Option<String>,
    /// The declared files.
    pub files: Vec<UploadFile>,
    /// Indexes of the files every destination has.
    pub received: BTreeSet<usize>,
    /// `open`, `committed` or `aborted`.
    pub state: String,
}

impl UploadSession {
    /// True once every declared file was received.
    #[must_use]
    pub fn complete(&self) -> bool {
        (0..self.files.len()).all(|index| self.received.contains(&index))
    }
}

/// Outcome of [`commit_upload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadCommit {
    /// The title is committed (now or by an earlier identical commit).
    Committed {
        /// The job is finished.
        job_completed: bool,
    },
    /// The fence was lost before the commit; nothing was written.
    Lost,
    /// Not allowed (the reason is stable).
    Refused(&'static str),
}

/// RFC 3339 text with nanoseconds (unique enough to mark one commit).
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// A fresh bearer token: 32 random bytes, base64url.
fn mint_token() -> Zeroizing<String> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(bytes.as_mut_slice());
    Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes.as_slice()))
}

/// True when `book` is one of the titles `title` names.
fn names_book(title: &str, book: &crate::models::BookRecord) -> bool {
    title.eq_ignore_ascii_case(&book.uuid)
        || title.eq_ignore_ascii_case(&book.product_id)
        || book
            .isbn
            .as_deref()
            .is_some_and(|isbn| title.eq_ignore_ascii_case(isbn))
        || book
            .asin
            .as_deref()
            .is_some_and(|asin| title.eq_ignore_ascii_case(asin))
}

/// Checks the declared files: well formed, unique keys, the primary (and
/// PDF) among them. A closing upload (no title) declares none.
fn manifest_problem(req: &UploadGrantRequest<'_>) -> Option<&'static str> {
    if req.book_uuid.is_empty() {
        return (!req.files.is_empty() || req.pdf_key.is_some()).then_some("invalid_manifest");
    }
    if req.files.is_empty() || req.files.len() > MAX_UPLOAD_FILES {
        return Some("invalid_manifest");
    }
    let mut keys = BTreeSet::new();
    for file in req.files {
        if !file.is_well_formed() || !keys.insert(file.key.as_str()) {
            return Some("invalid_manifest");
        }
    }
    if !keys.contains(req.primary_key) || req.pdf_key.is_some_and(|key| !keys.contains(key)) {
        return Some("invalid_manifest");
    }
    None
}

/// Opens an upload of one title of the acquire job `node_id` holds at
/// `generation`, to be received by `receiving_node_id`.
///
/// Granted only when all of these hold: the fence is the job's live lease
/// for this node and the node is still trusted with the job; the job is an
/// acquire; the title exists, is one of the job's plugins and accounts (and
/// the job's title, when it names one), and for a Host trusted only with
/// its owner's accounts is one of that user's; and the manifest is well
/// formed. An earlier open upload of the same title by the same job is
/// aborted.
///
/// # Errors
///
/// Returns an error when a read or the insert fails.
pub async fn grant_upload(
    store: &LibraryStore,
    node_id: &str,
    job_id: &str,
    generation: i64,
    receiving_node_id: &str,
    req: &UploadGrantRequest<'_>,
) -> Result<UploadGrant> {
    if let Some(reason) = manifest_problem(req) {
        return Ok(UploadGrant::Refused(reason));
    }
    let fence = remote_job_fence(node_id, job_id, generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(UploadGrant::Lost);
    };
    if !node_may_claim_job(store, node_id, Claimant::Remote, &job).await? {
        return Ok(UploadGrant::Lost);
    }
    if job.kind != JobKind::Acquire {
        return Ok(UploadGrant::Refused("not_an_acquire"));
    }
    if !req.book_uuid.is_empty() {
        let Some(book) = store.get_book_by_uuid(req.book_uuid).await? else {
            return Ok(UploadGrant::Refused("unknown_book"));
        };
        let keys = job_plugin_keys(store, &job).await?.unwrap_or_default();
        let account_ok = job
            .payload
            .account
            .as_deref()
            .filter(|a| !a.is_empty())
            .is_none_or(|account| account == book.account_id);
        let title_ok = job
            .payload
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .is_none_or(|title| names_book(title, &book));
        if !keys.contains(&book.plugin_key) || !account_ok || !title_ok {
            return Ok(UploadGrant::Refused("book_not_in_job"));
        }
        match node_trust(store, node_id, Claimant::Remote).await? {
            NodeTrust::All => {}
            NodeTrust::OwnedBy(user_id) => {
                let owned = accounts_owned_by(store, user_id).await?;
                if !owned.contains(&(book.plugin_key.clone(), book.account_id.clone())) {
                    return Ok(UploadGrant::Refused("not_your_account"));
                }
            }
            NodeTrust::Nothing => return Ok(UploadGrant::Lost),
        }
    }
    let manifest = serde_json::to_string(&Manifest {
        files: req.files.to_vec(),
        pdf_key: req.pdf_key.map(str::to_string),
    })
    .map_err(|err| LibraryError::Other(anyhow::anyhow!("upload manifest encode: {err}")))?;
    node_upload_sessions::Entity::update_many()
        .col_expr(
            node_upload_sessions::Column::State,
            sea_orm::sea_query::Expr::value(STATE_ABORTED),
        )
        .filter(node_upload_sessions::Column::JobId.eq(job_id))
        .filter(node_upload_sessions::Column::BookUuid.eq(req.book_uuid))
        .filter(node_upload_sessions::Column::State.eq(STATE_OPEN))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let upload_id = uuid::Uuid::new_v4().hyphenated().to_string();
    let token = mint_token();
    let now = Utc::now();
    let expires_at = now + Duration::seconds(UPLOAD_TOKEN_TTL_SECS);
    let row = node_upload_sessions::ActiveModel {
        upload_id: Set(upload_id.clone()),
        token_hash: Set(crate::hash_token(&token)),
        node_id: Set(node_id.to_string()),
        job_id: Set(job_id.to_string()),
        lease_generation: Set(generation),
        receiving_node_id: Set(receiving_node_id.to_string()),
        book_uuid: Set(req.book_uuid.to_string()),
        primary_key: Set(req.primary_key.to_string()),
        manifest_json: Set(manifest),
        received_json: Set("[]".into()),
        state: Set(STATE_OPEN.into()),
        created_at: Set(stamp(now)),
        expires_at: Set(stamp(expires_at)),
        committed_at: Set(None),
    };
    node_upload_sessions::Entity::insert(row)
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    tracing::info!(
        node_id,
        job_id,
        fence = generation,
        upload_id = %upload_id,
        book_uuid = req.book_uuid,
        files = req.files.len(),
        "granted a host upload"
    );
    Ok(UploadGrant::Granted {
        upload_id,
        token,
        expires_at,
    })
}

/// Maps a stored row onto the session.
fn session_from_model(row: node_upload_sessions::Model) -> Result<UploadSession> {
    let manifest: Manifest = serde_json::from_str(&row.manifest_json)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("upload manifest decode: {err}")))?;
    let received: BTreeSet<usize> = serde_json::from_str(&row.received_json)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("upload receipts decode: {err}")))?;
    Ok(UploadSession {
        upload_id: row.upload_id,
        node_id: row.node_id,
        job_id: row.job_id,
        generation: row.lease_generation,
        receiving_node_id: row.receiving_node_id,
        book_uuid: row.book_uuid,
        primary_key: row.primary_key,
        pdf_key: manifest.pdf_key,
        files: manifest.files,
        received,
        state: row.state,
    })
}

/// Loads one upload by id.
///
/// # Errors
///
/// Returns an error when the read fails or the row is malformed.
pub async fn load_upload(store: &LibraryStore, upload_id: &str) -> Result<Option<UploadSession>> {
    node_upload_sessions::Entity::find_by_id(upload_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(session_from_model)
        .transpose()
}

/// The upload `token` opens: `upload_id`'s row when the token's hash
/// matches, the token has not expired, and the upload was neither aborted
/// nor (unless `allow_committed`) committed.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn authorize_upload(
    store: &LibraryStore,
    upload_id: &str,
    token: &str,
    allow_committed: bool,
) -> Result<Option<UploadSession>> {
    if token.is_empty() || token.len() > 128 {
        return Ok(None);
    }
    let Some(row) = node_upload_sessions::Entity::find_by_id(upload_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    if row.token_hash != crate::hash_token(token) {
        return Ok(None);
    }
    let unexpired = DateTime::parse_from_rfc3339(&row.expires_at)
        .is_ok_and(|expires| expires.with_timezone(&Utc) > Utc::now());
    let usable = row.state == STATE_OPEN || (allow_committed && row.state == STATE_COMMITTED);
    if !unexpired || !usable {
        return Ok(None);
    }
    session_from_model(row).map(Some)
}

/// True while the upload's fence is the job's live lease for its node and
/// the node is still trusted with the job.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn upload_fence_live(store: &LibraryStore, session: &UploadSession) -> Result<bool> {
    let fence = remote_job_fence(&session.node_id, &session.job_id, session.generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(false);
    };
    node_may_claim_job(store, &session.node_id, Claimant::Remote, &job).await
}

/// Records that file `index` reached every destination. Answers false when
/// the upload is no longer open.
///
/// # Errors
///
/// Returns an error when a read or write fails.
pub async fn record_upload_file(
    store: &LibraryStore,
    upload_id: &str,
    index: usize,
) -> Result<bool> {
    for _ in 0..8 {
        let Some(row) = node_upload_sessions::Entity::find_by_id(upload_id.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        if row.state != STATE_OPEN {
            return Ok(false);
        }
        let mut received: BTreeSet<usize> = serde_json::from_str(&row.received_json)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("upload receipts decode: {err}")))?;
        if !received.insert(index) {
            return Ok(true);
        }
        let next = serde_json::to_string(&received)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("upload receipts encode: {err}")))?;
        let updated = node_upload_sessions::Entity::update_many()
            .col_expr(
                node_upload_sessions::Column::ReceivedJson,
                sea_orm::sea_query::Expr::value(next),
            )
            .filter(node_upload_sessions::Column::UploadId.eq(upload_id))
            .filter(node_upload_sessions::Column::State.eq(STATE_OPEN))
            .filter(node_upload_sessions::Column::ReceivedJson.eq(row.received_json))
            .exec(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if updated.rows_affected == 1 {
            return Ok(true);
        }
    }
    Err(LibraryError::Unavailable(
        "upload receipts kept changing".into(),
    ))
}

/// Aborts an open upload (its fence was lost, or a file failed).
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn abort_upload(store: &LibraryStore, upload_id: &str) -> Result<()> {
    node_upload_sessions::Entity::update_many()
        .col_expr(
            node_upload_sessions::Column::State,
            sea_orm::sea_query::Expr::value(STATE_ABORTED),
        )
        .filter(node_upload_sessions::Column::UploadId.eq(upload_id))
        .filter(node_upload_sessions::Column::State.eq(STATE_OPEN))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// True when the job finished successfully.
async fn job_done(store: &LibraryStore, job_id: &str) -> Result<bool> {
    Ok(store
        .get_job(job_id)
        .await?
        .is_some_and(|job| job.state == JobState::Succeeded))
}

/// Commits an upload whose every file was received: marks the title
/// acquired under its primary key (and its PDF, when uploaded), publishes
/// `book_acquired`, and, when `finish` is set, completes the job with that
/// text as its progress and gives its semaphore permits back.
///
/// One canonical batch, every statement gated on the upload still being
/// open, the job still running under this upload's fence, and the node's
/// credential still being live; trust is re-checked just before. A repeat
/// of a commit that already landed answers [`UploadCommit::Committed`]
/// again without writing.
///
/// # Errors
///
/// Returns an error when a read or the batch fails.
pub async fn commit_upload(
    store: &LibraryStore,
    upload_id: &str,
    finish: Option<&str>,
    limits: JobClaimLimits,
) -> Result<UploadCommit> {
    let Some(session) = load_upload(store, upload_id).await? else {
        return Ok(UploadCommit::Refused("unknown_upload"));
    };
    if session.state == STATE_COMMITTED {
        return Ok(UploadCommit::Committed {
            job_completed: job_done(store, &session.job_id).await?,
        });
    }
    if session.state != STATE_OPEN {
        return Ok(UploadCommit::Refused("upload_aborted"));
    }
    if !session.complete() {
        return Ok(UploadCommit::Refused("files_missing"));
    }
    let fence = remote_job_fence(&session.node_id, &session.job_id, session.generation);
    let Some(job) = fenced_job(store, &fence).await? else {
        return Ok(UploadCommit::Lost);
    };
    if !node_may_claim_job(store, &session.node_id, Claimant::Remote, &job).await? {
        return Ok(UploadCommit::Lost);
    }
    let now = stamp(Utc::now());
    let gate_sql = "EXISTS (SELECT 1 FROM node_upload_sessions \
                     WHERE upload_id = ? AND state = 'committed' AND committed_at = ?)";
    let gate = || vec![text(upload_id), text(&now)];
    let mut statements = vec![exec(
        "UPDATE node_upload_sessions SET state = 'committed', committed_at = ? \
          WHERE upload_id = ? AND state = 'open' \
            AND EXISTS (SELECT 1 FROM jobs WHERE id = ? AND state = 'running' \
                         AND lease_owner = ? AND lease_generation = ?) \
            AND EXISTS (SELECT 1 FROM node_credentials \
                         WHERE node_id = ? AND revoked_at IS NULL)",
        vec![
            text(&now),
            text(upload_id),
            text(&fence.job_id),
            text(&fence.owner),
            batch::int(fence.generation),
            text(&session.node_id),
        ],
    )];
    if !session.book_uuid.is_empty() {
        let Some(model) = books::Entity::find()
            .filter(books::Column::Uuid.eq(&session.book_uuid))
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(UploadCommit::Refused("unknown_book"));
        };
        let mut binds = vec![
            text(&session.primary_key),
            text(&now),
            text(&session.book_uuid),
        ];
        binds.extend(gate());
        statements.push(exec(
            &format!(
                "UPDATE books SET acquire_status = 'acquired', storage_key = ?, \
                   error_message = NULL, updated_at = ? \
                  WHERE uuid = ? AND {gate_sql}"
            ),
            binds,
        ));
        if let Some(pdf_key) = session.pdf_key.as_deref() {
            let mut binds = vec![text(pdf_key), text(&now), text(&session.book_uuid)];
            binds.extend(gate());
            statements.push(exec(
                &format!(
                    "UPDATE books SET pdf_status = 'acquired', pdf_storage_key = ?, \
                       updated_at = ? \
                      WHERE uuid = ? AND {gate_sql}"
                ),
                binds,
            ));
        }
        let event =
            prepare_publish_domain_event(book_acquired_spec(&model, Some(&session.primary_key)))?;
        let mut binds = vec![
            text(&event.id),
            text(&event.event_type),
            batch::int(event.schema_version.max(1)),
            text(&now),
            text(&event.account_id),
            text(&event.source),
            text(&event.correlation_id),
            text(&event.causation_id),
            text(&event.dedup_key),
            text(&event.payload),
            text(&event.ordering_key),
            text(&now),
            text(&event.account_id),
            text(&event.source),
            text(&event.event_type),
            text(&event.dedup_key),
        ];
        binds.extend(gate());
        statements.push(exec(
            &format!(
                "INSERT INTO domain_events (\
                    id, event_type, schema_version, occurred_at, account_id, source, \
                    correlation_id, causation_id, dedup_key, payload, ordering_key, \
                    dispatch_state, created_at, wake_pending\
                 ) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 1 \
                 WHERE NOT EXISTS (\
                    SELECT 1 FROM domain_events \
                    WHERE queue_name = 'core.events' AND account_id = ? AND source = ? \
                      AND event_type = ? AND dedup_key = ?\
                 ) AND {gate_sql}"
            ),
            binds,
        ));
    }
    let job_index = finish.map(|detail| {
        let mut binds = vec![
            text(&now),
            text(&now),
            text(detail),
            text(&fence.job_id),
            text(&fence.owner),
            batch::int(fence.generation),
        ];
        binds.extend(gate());
        statements.push(exec(
            &format!(
                "UPDATE jobs SET state = 'succeeded', lease_owner = NULL, \
                   lease_expires_at = NULL, finished_at = ?, updated_at = ?, progress = ? \
                  WHERE id = ? AND state = 'running' AND lease_owner = ? \
                    AND lease_generation = ? AND {gate_sql}"
            ),
            binds,
        ));
        statements.len() - 1
    });
    let reply = batch::execute_host_batch(
        store,
        request(&format!("node-upload-commit:{upload_id}:{now}"), statements),
    )
    .await?;
    if batch::rows_affected(&reply, 0) != 1 {
        // A concurrent identical commit may have landed in between.
        let again = load_upload(store, upload_id).await?;
        return Ok(match again {
            Some(row) if row.state == STATE_COMMITTED => UploadCommit::Committed {
                job_completed: job_done(store, &row.job_id).await?,
            },
            _ => UploadCommit::Lost,
        });
    }
    let job_completed = job_index.is_some_and(|index| batch::rows_affected(&reply, index) == 1);
    if job_completed {
        let (permits, _) = held_job_permits(store, &session.node_id, &job, limits).await?;
        release_permits(store, &permits).await?;
    }
    tracing::info!(
        node_id = %session.node_id,
        job_id = %session.job_id,
        fence = session.generation,
        upload_id,
        book_uuid = %session.book_uuid,
        job_completed,
        "committed a host upload"
    );
    Ok(UploadCommit::Committed { job_completed })
}
