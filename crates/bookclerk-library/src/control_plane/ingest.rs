//! Ingest batches: files a patron uploads, or references into an intake
//! storage instance, that one `ingest_batch` job identifies and stores (see
//! `docs/adr/uploads-and-ingest.md`).
//!
//! A batch is created with its whole manifest ([`create_upload_batch`],
//! [`create_intake_batch`]); every book it stores is held by the batch's
//! patron. An upload's bytes are staged on the node that received them,
//! and its members count them ([`record_received`]). Committing
//! ([`mark_committed`]) hands the batch to its job, which expands archives
//! ([`add_expanded_members`]), records each member's outcome
//! ([`set_member_outcomes`]) and finishes the batch ([`finish_batch`]).
//!
//! Every write is one atomic host batch, guarded on the state it read.

use crate::column_read::SelectColumnExt;
use std::fmt;

use bookclerk_archive::{EntryPath, FoldedNames};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter as _, QueryOrder as _, QuerySelect};
use serde::{Deserialize, Serialize};

use super::batch::{self, int, text, text_opt};
use super::member_controls::{control_guard, ControlGuard, MemberControl};
use crate::entities::{ingest_batches, ingest_members};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Most files one batch's manifest may name.
pub const MAX_BATCH_FILES: usize = 10_000;

/// Longest intake storage key or signature a member may carry, in bytes;
/// both are stored in one cell.
pub const MAX_INTAKE_REF_BYTES: usize = 1_024;

/// Longest outcome reason, in bytes.
pub const MAX_REASON_BYTES: usize = 128;

/// Member rows written per host batch: one statement each, within the
/// 40-statement minimum every database guest accepts.
const MEMBERS_PER_WRITE: usize = 32;

/// Rough encoded size of one member row, in bytes, used to keep a read
/// page within the database's reply byte cap.
const MEMBER_ROW_BYTES: u64 = 1_600;

/// A batch id: 32 lower-case hex digits, safe as one path segment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct IngestBatchId(String);

impl IngestBatchId {
    /// A new random id.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    /// Parses an id, refusing anything but 32 lower-case hex digits.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        (raw.len() == 32 && raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(raw.to_string()))
    }

    /// The id as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for IngestBatchId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for IngestBatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for IngestBatchId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        Self::parse(&raw).ok_or_else(|| serde::de::Error::custom("not an ingest batch id"))
    }
}

/// A lower-case hex SHA-256 digest a manifest declared for a file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// Parses 64 hex digits, folding them to lower case.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        (raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Self(raw.to_ascii_lowercase()))
    }

    /// The digest as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Sha256Hex {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        Self::parse(&raw).ok_or_else(|| serde::de::Error::custom("not a SHA-256 hex digest"))
    }
}

/// Declares a stored snake_case enum with `ALL`, `as_str` and `parse`.
macro_rules! stored_kind {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The stored text.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            /// Parses stored text; `None` for anything else.
            #[must_use]
            pub fn parse(raw: &str) -> Option<Self> {
                match raw {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

stored_kind! {
    /// Where a batch's bytes come from.
    IngestSource {
        /// Staged on the receiving node by the upload API.
        Upload => "upload",
        /// Read in place from an intake storage instance.
        Intake => "intake",
    }
}

stored_kind! {
    /// Where a batch is in its life.
    IngestState {
        /// Taking bytes; nothing identified yet.
        Open => "open",
        /// Every file arrived and the job is queued.
        Committed => "committed",
        /// The job is identifying and storing it.
        Running => "running",
        /// Every member has its outcome.
        Done => "done",
        /// The job gave up; members keep the outcomes they reached.
        Failed => "failed",
        /// Cancelled before the job started.
        Cancelled => "cancelled",
    }
}

impl IngestState {
    /// True for a state the batch never leaves.
    #[must_use]
    pub fn is_final(self) -> bool {
        match self {
            Self::Done | Self::Failed | Self::Cancelled => true,
            Self::Open | Self::Committed | Self::Running => false,
        }
    }
}

stored_kind! {
    /// What became of one member.
    MemberOutcome {
        /// Not decided yet.
        Pending => "pending",
        /// An archive whose entries joined the batch as their own members.
        Expanded => "expanded",
        /// Stored as part of an edition a strong identifier or ISBN named.
        Matched => "matched",
        /// Stored as part of a new local edition an Administrator should
        /// review.
        NeedsReview => "needs_review",
        /// Its edition already had files here; the existing copy was kept.
        Duplicate => "duplicate",
        /// Not stored, for the given reason.
        Refused => "refused",
        /// Storing it failed.
        Failed => "failed",
    }
}

/// An intake member's place in its storage instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeRef {
    /// The key in the intake instance.
    pub key: String,
    /// The signature the key was listed with, when the backend gives one.
    pub signature: Option<String>,
}

/// One file a manifest names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSpec {
    /// Its relative path in the batch.
    pub path: EntryPath,
    /// Its size in bytes.
    pub size: u64,
    /// Its digest, when the client declared one.
    pub sha256: Option<Sha256Hex>,
    /// For an intake batch, where the file is; `None` for an upload.
    pub intake: Option<IntakeRef>,
}

/// The upload limits a batch is admitted under (`uploads.*` settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadQuota {
    /// Largest file, in bytes.
    pub max_file_bytes: u64,
    /// Most bytes one person may have staged in live upload batches.
    pub max_staged_bytes: u64,
}

/// Why a batch was not created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateRefusal {
    /// The person may not upload (the `upload` member control), is not
    /// active, or is unknown.
    NotAllowed,
    /// The manifest names no file.
    Empty,
    /// The manifest names more than [`MAX_BATCH_FILES`] files.
    TooManyFiles,
    /// A file is larger than the per-file limit.
    FileTooLarge {
        /// The file's position in the manifest.
        member_no: usize,
    },
    /// Two files share a path, or a path folds onto another (case,
    /// Unicode form, or a file where a folder is).
    PathClash {
        /// The later file's position in the manifest.
        member_no: usize,
    },
    /// An intake member lacks its storage reference, an upload member has
    /// one, or a reference is too long.
    BadReference {
        /// The file's position in the manifest.
        member_no: usize,
    },
    /// Admitting the batch would pass the person's staging quota.
    OverQuota {
        /// Bytes the person already has staged.
        staged: u64,
        /// Bytes this batch declares.
        requested: u64,
        /// The quota.
        limit: u64,
    },
}

impl fmt::Display for CreateRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAllowed => f.write_str("uploads are not allowed for this person"),
            Self::Empty => f.write_str("the manifest names no file"),
            Self::TooManyFiles => write!(f, "the manifest names more than {MAX_BATCH_FILES} files"),
            Self::FileTooLarge { member_no } => {
                write!(f, "file {member_no} is larger than the per-file limit")
            }
            Self::PathClash { member_no } => {
                write!(f, "file {member_no} has the same path as an earlier file")
            }
            Self::BadReference { member_no } => {
                write!(
                    f,
                    "file {member_no} has a missing or unusable storage reference"
                )
            }
            Self::OverQuota {
                staged,
                requested,
                limit,
            } => write!(
                f,
                "{requested} more bytes would pass the staging quota ({staged} of {limit} in use)"
            ),
        }
    }
}

/// One stored batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestBatch {
    /// The batch id.
    pub id: IngestBatchId,
    /// The patron who holds its books.
    pub user_id: i64,
    /// The node that holds its bytes and runs it.
    pub node_id: String,
    /// Upload or intake.
    pub source: IngestSource,
    /// For an intake batch, the storage instance it reads.
    pub intake_instance: Option<String>,
    /// Its state.
    pub state: IngestState,
    /// Files the manifest named.
    pub file_count: u64,
    /// Bytes the manifest declared.
    pub declared_bytes: u64,
    /// Its job, once committed.
    pub job_id: Option<String>,
    /// Why it failed.
    pub detail: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last activity.
    pub updated_at: DateTime<Utc>,
    /// When it reached a final state.
    pub finished_at: Option<DateTime<Utc>>,
}

/// One stored member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestMember {
    /// Its position in the batch.
    pub member_no: u32,
    /// The archive member it was expanded from.
    pub parent_no: Option<u32>,
    /// Its relative path (or a refused entry's sanitized name).
    pub path: String,
    /// Its size in bytes.
    pub size: u64,
    /// The digest the manifest declared.
    pub sha256: Option<String>,
    /// Bytes staged so far.
    pub received: u64,
    /// For an intake member, its key.
    pub storage_key: Option<String>,
    /// For an intake member, its listed signature.
    pub signature: Option<String>,
    /// What became of it.
    pub outcome: MemberOutcome,
    /// Why it was refused or failed.
    pub reason: Option<String>,
    /// The edition it was stored under.
    pub edition_id: Option<String>,
}

impl IngestMember {
    /// True once every declared byte is staged.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.received == self.size
    }
}

/// One member's outcome, as the job decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeUpdate {
    /// The member.
    pub member_no: u32,
    /// Its outcome.
    pub outcome: MemberOutcome,
    /// The reason for a refusal or failure, cut to [`MAX_REASON_BYTES`].
    pub reason: Option<String>,
    /// The edition it was stored under.
    pub edition_id: Option<String>,
}

/// One entry an archive member expanded into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedEntry {
    /// Its path in the batch, or the sanitized name of a refused entry.
    pub path: String,
    /// Bytes written (0 for a refused entry).
    pub size: u64,
    /// `Pending` for a written entry, `Refused` with a reason otherwise.
    pub outcome: MemberOutcome,
    /// Why it was refused.
    pub reason: Option<String>,
}

/// RFC 3339 text of `at`.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339()
}

/// Parses a stored timestamp.
fn parse_time(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("bad ingest timestamp: {err}")))
}

/// A stored count or size as `u64`; a negative value is corrupt.
fn unsigned(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("negative ingest {what}: {value}")))
}

/// A size or count as a bind value; one past `i64::MAX` is refused.
fn signed(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("ingest {what} {value} is too large")))
}

/// A member number as a bind value.
fn member_int(no: u32) -> bookclerk_plugin_abi::DbValue {
    int(i64::from(no))
}

/// Cuts `reason` to [`MAX_REASON_BYTES`] on a character boundary.
fn short_reason(reason: &str) -> String {
    let mut end = reason.len().min(MAX_REASON_BYTES);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

/// Reads a batch row into its typed form.
fn batch_from_model(row: ingest_batches::Model) -> Result<IngestBatch> {
    let corrupt = |what: &str| LibraryError::Other(anyhow::anyhow!("bad ingest batch {what}"));
    Ok(IngestBatch {
        id: IngestBatchId::parse(&row.id).ok_or_else(|| corrupt("id"))?,
        user_id: row.user_id,
        node_id: row.node_id,
        source: IngestSource::parse(&row.source).ok_or_else(|| corrupt("source"))?,
        intake_instance: row.intake_instance,
        state: IngestState::parse(&row.state).ok_or_else(|| corrupt("state"))?,
        file_count: unsigned(row.file_count, "file count")?,
        declared_bytes: unsigned(row.declared_bytes, "size")?,
        job_id: row.job_id,
        detail: row.detail,
        created_at: parse_time(&row.created_at)?,
        updated_at: parse_time(&row.updated_at)?,
        finished_at: row.finished_at.as_deref().map(parse_time).transpose()?,
    })
}

/// Reads a member row into its typed form.
fn member_from_model(row: ingest_members::Model) -> Result<IngestMember> {
    let corrupt = |what: &str| LibraryError::Other(anyhow::anyhow!("bad ingest member {what}"));
    let number = |value: i64| u32::try_from(value).map_err(|_| corrupt("number"));
    Ok(IngestMember {
        member_no: number(row.member_no)?,
        parent_no: row.parent_no.map(number).transpose()?,
        path: row.path,
        size: unsigned(row.size, "size")?,
        sha256: row.sha256,
        received: unsigned(row.received, "received")?,
        storage_key: row.storage_key,
        signature: row.signature,
        outcome: MemberOutcome::parse(&row.outcome).ok_or_else(|| corrupt("outcome"))?,
        reason: row.reason,
        edition_id: row.edition_id,
    })
}

/// Checks a manifest: count, sizes, path clashes and references.
///
/// # Returns
///
/// The declared total, or why the manifest is refused.
fn check_manifest(
    specs: &[MemberSpec],
    source: IngestSource,
    max_file_bytes: u64,
) -> std::result::Result<u64, CreateRefusal> {
    if specs.is_empty() {
        return Err(CreateRefusal::Empty);
    }
    if specs.len() > MAX_BATCH_FILES {
        return Err(CreateRefusal::TooManyFiles);
    }
    let mut names = FoldedNames::default();
    let mut total: u64 = 0;
    for (member_no, spec) in specs.iter().enumerate() {
        if spec.size > max_file_bytes || i64::try_from(spec.size).is_err() {
            return Err(CreateRefusal::FileTooLarge { member_no });
        }
        if names.insert(&spec.path).is_err() {
            return Err(CreateRefusal::PathClash { member_no });
        }
        let reference_fits = match (&spec.intake, source) {
            (None, IngestSource::Upload) => true,
            (Some(intake), IngestSource::Intake) => {
                !intake.key.is_empty()
                    && intake.key.len() <= MAX_INTAKE_REF_BYTES
                    && intake
                        .signature
                        .as_ref()
                        .is_none_or(|sig| sig.len() <= MAX_INTAKE_REF_BYTES)
            }
            (Some(_), IngestSource::Upload) | (None, IngestSource::Intake) => false,
        };
        if !reference_fits {
            return Err(CreateRefusal::BadReference { member_no });
        }
        total = total.saturating_add(spec.size);
    }
    if i64::try_from(total).is_err() {
        return Err(CreateRefusal::FileTooLarge { member_no: 0 });
    }
    Ok(total)
}

/// SQL true while a person's live upload batches leave room for `?` more
/// bytes under a quota of `?`; binds the user id, the bytes and the quota.
const QUOTA_ROOM_SQL: &str = "COALESCE((SELECT SUM(declared_bytes) FROM ingest_batches \
     WHERE user_id = ? AND source = 'upload' AND state IN ('open', 'committed', 'running')), 0) \
     + ? <= ?";

/// Bytes `user_id` has in live upload batches (open, committed or
/// running), which the staging quota counts.
///
/// # Errors
///
/// Returns the read error.
pub async fn staged_bytes(store: &LibraryStore, user_id: i64) -> Result<u64> {
    let rows: Vec<i64> = ingest_batches::Entity::find()
        .filter(ingest_batches::Column::UserId.eq(user_id))
        .filter(ingest_batches::Column::Source.eq(IngestSource::Upload.as_str()))
        .filter(ingest_batches::Column::State.is_in([
            IngestState::Open.as_str(),
            IngestState::Committed.as_str(),
            IngestState::Running.as_str(),
        ]))
        .into_column(ingest_batches::Column::DeclaredBytes)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut total: u64 = 0;
    for bytes in rows {
        total = total.saturating_add(unsigned(bytes, "size")?);
    }
    Ok(total)
}

/// Inserts a batch's member rows, [`MEMBERS_PER_WRITE`] per host batch,
/// each guarded on the batch still being open.
///
/// # Errors
///
/// Returns the write error.
async fn insert_members(
    store: &LibraryStore,
    id: &IngestBatchId,
    specs: &[MemberSpec],
    now: &str,
) -> Result<()> {
    for (chunk_no, chunk) in specs.chunks(MEMBERS_PER_WRITE).enumerate() {
        let mut statements = Vec::with_capacity(chunk.len());
        for (offset, spec) in chunk.iter().enumerate() {
            let member_no = chunk_no * MEMBERS_PER_WRITE + offset;
            statements.push(batch::exec(
                "INSERT INTO ingest_members (batch_id, member_no, parent_no, path, size, sha256, \
                 received, storage_key, signature, outcome, updated_at) \
                 SELECT ?, ?, NULL, ?, ?, ?, 0, ?, ?, 'pending', ? \
                 WHERE EXISTS (SELECT 1 FROM ingest_batches WHERE id = ? AND state = 'open')",
                vec![
                    text(id.as_str()),
                    int(signed(
                        u64::try_from(member_no).unwrap_or(u64::MAX),
                        "member",
                    )?),
                    text(spec.path.as_str()),
                    int(signed(spec.size, "size")?),
                    text_opt(spec.sha256.as_ref().map(Sha256Hex::as_str)),
                    text_opt(spec.intake.as_ref().map(|r| r.key.as_str())),
                    text_opt(spec.intake.as_ref().and_then(|r| r.signature.as_deref())),
                    text(now),
                    text(id.as_str()),
                ],
            ));
        }
        store
            .execute_host_batch(batch::request(
                &format!("ingest-members-{id}-{chunk_no}"),
                statements,
            ))
            .await?;
    }
    Ok(())
}

/// Creates an upload batch for `user_id`, staged on `node_id`.
///
/// The person must be allowed to upload (the `upload` member control) and
/// the batch must fit their staging quota; both are checked again inside
/// the write. The node's free-disk floor is the caller's to check.
///
/// # Returns
///
/// The new batch, open and taking bytes, or why it was refused.
///
/// # Errors
///
/// Returns a read or write error.
pub async fn create_upload_batch(
    store: &LibraryStore,
    user_id: i64,
    node_id: &str,
    specs: &[MemberSpec],
    quota: UploadQuota,
) -> Result<std::result::Result<IngestBatch, CreateRefusal>> {
    let total = match check_manifest(specs, IngestSource::Upload, quota.max_file_bytes) {
        Ok(total) => total,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let Some(guard) = control_guard(store, user_id, MemberControl::Upload).await? else {
        return Ok(Err(CreateRefusal::NotAllowed));
    };
    let staged = staged_bytes(store, user_id).await?;
    let over = |staged: u64| CreateRefusal::OverQuota {
        staged,
        requested: total,
        limit: quota.max_staged_bytes,
    };
    if staged.saturating_add(total) > quota.max_staged_bytes {
        return Ok(Err(over(staged)));
    }
    let id = IngestBatchId::new();
    let now = stamp(Utc::now());
    let mut binds = vec![
        text(id.as_str()),
        int(user_id),
        text(node_id),
        int(signed(
            u64::try_from(specs.len()).unwrap_or(u64::MAX),
            "count",
        )?),
        int(signed(total, "size")?),
        text(&now),
        text(&now),
    ];
    binds.extend(guard.values());
    binds.extend([
        int(user_id),
        int(signed(total, "size")?),
        int(signed(
            quota.max_staged_bytes.min(i64::MAX.unsigned_abs()),
            "quota",
        )?),
    ]);
    let reply = store
        .execute_host_batch(batch::request(
            &format!("ingest-create-{id}"),
            vec![batch::exec(
                &format!(
                    "INSERT INTO ingest_batches (id, user_id, node_id, source, intake_instance, \
                     state, file_count, declared_bytes, created_at, updated_at) \
                     SELECT ?, ?, ?, 'upload', NULL, 'open', ?, ?, ?, ? \
                     WHERE {} AND {QUOTA_ROOM_SQL}",
                    ControlGuard::SQL
                ),
                binds,
            )],
        ))
        .await?;
    if batch::rows_affected(&reply, 0) == 0 {
        // Something changed since the reads: say which.
        if control_guard(store, user_id, MemberControl::Upload)
            .await?
            .is_none()
        {
            return Ok(Err(CreateRefusal::NotAllowed));
        }
        let staged = staged_bytes(store, user_id).await?;
        if staged.saturating_add(total) > quota.max_staged_bytes {
            return Ok(Err(over(staged)));
        }
        return Err(LibraryError::Other(anyhow::anyhow!(
            "the upload batch changed while it was created; try again"
        )));
    }
    insert_members(store, &id, specs, &now).await?;
    load_batch(store, &id)
        .await?
        .map(Ok)
        .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("the new upload batch is gone")))
}

/// Creates an intake batch: files read in place from `instance`, held by
/// `user_id`, run on `node_id`. The host creates these itself (the drop
/// folder); the person must be active.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `id` - The batch id, chosen by the caller (a fresh
///   [`IngestBatchId::new`]) and recorded before the call, so that after a
///   crash the caller finds the batch, or knows it was never written.
/// * `user_id` - The patron who holds the books.
/// * `node_id` - The node that runs the batch.
/// * `instance` - The storage instance the members are read from.
/// * `specs` - One member per object.
///
/// # Returns
///
/// The new batch, open, or why it was refused. Its members are complete
/// from the start, so it can be committed at once.
///
/// # Errors
///
/// Returns a read or write error, including for an `id` already in use.
pub async fn create_intake_batch(
    store: &LibraryStore,
    id: &IngestBatchId,
    user_id: i64,
    node_id: &str,
    instance: &str,
    specs: &[MemberSpec],
) -> Result<std::result::Result<IngestBatch, CreateRefusal>> {
    if instance.is_empty() || instance.len() > MAX_INTAKE_REF_BYTES {
        return Ok(Err(CreateRefusal::BadReference { member_no: 0 }));
    }
    let total = match check_manifest(specs, IngestSource::Intake, u64::MAX) {
        Ok(total) => total,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let now = stamp(Utc::now());
    let reply = store
        .execute_host_batch(batch::request(
            &format!("ingest-create-{id}"),
            vec![batch::exec(
                "INSERT INTO ingest_batches (id, user_id, node_id, source, intake_instance, \
                 state, file_count, declared_bytes, created_at, updated_at) \
                 SELECT ?, ?, ?, 'intake', ?, 'open', ?, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM users WHERE id = ? AND status = 'active')",
                vec![
                    text(id.as_str()),
                    int(user_id),
                    text(node_id),
                    text(instance),
                    int(signed(
                        u64::try_from(specs.len()).unwrap_or(u64::MAX),
                        "count",
                    )?),
                    int(signed(total, "size")?),
                    text(&now),
                    text(&now),
                    int(user_id),
                ],
            )],
        ))
        .await?;
    if batch::rows_affected(&reply, 0) == 0 {
        return Ok(Err(CreateRefusal::NotAllowed));
    }
    insert_members(store, id, specs, &now).await?;
    // An intake member's bytes are already in place.
    store
        .execute_host_batch(batch::request(
            &format!("ingest-intake-complete-{id}"),
            vec![batch::exec(
                "UPDATE ingest_members SET received = size WHERE batch_id = ?",
                vec![text(id.as_str())],
            )],
        ))
        .await?;
    load_batch(store, id)
        .await?
        .map(Ok)
        .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("the new intake batch is gone")))
}

/// Reads one batch.
///
/// # Errors
///
/// Returns a read error, or an error for a corrupt row.
pub async fn load_batch(store: &LibraryStore, id: &IngestBatchId) -> Result<Option<IngestBatch>> {
    ingest_batches::Entity::find_by_id(id.as_str().to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(batch_from_model)
        .transpose()
}

/// Rows per member read: the database's result page, kept within its
/// reply byte cap.
fn member_page(store: &LibraryStore) -> u64 {
    let rows = super::trust::result_page(store).max(1);
    match store.db_capabilities().max_result_bytes {
        0 => rows,
        bytes => rows.min((u64::from(bytes) / MEMBER_ROW_BYTES).max(1)),
    }
}

/// Reads every member of a batch, in member order.
///
/// # Errors
///
/// Returns a read error, or an error for a corrupt row.
pub async fn batch_members(store: &LibraryStore, id: &IngestBatchId) -> Result<Vec<IngestMember>> {
    let page = member_page(store);
    let mut out: Vec<IngestMember> = Vec::new();
    loop {
        let mut select = ingest_members::Entity::find()
            .filter(ingest_members::Column::BatchId.eq(id.as_str()))
            .order_by_asc(ingest_members::Column::MemberNo)
            .limit(page);
        if let Some(last) = out.last() {
            select = select.filter(ingest_members::Column::MemberNo.gt(i64::from(last.member_no)));
        }
        let rows = select.all(store.db()).await.map_err(LibraryError::Orm)?;
        let full = u64::try_from(rows.len()).unwrap_or(u64::MAX) >= page;
        for row in rows {
            out.push(member_from_model(row)?);
        }
        if !full {
            return Ok(out);
        }
    }
}

/// Reads one member.
///
/// # Errors
///
/// Returns a read error, or an error for a corrupt row.
pub async fn load_member(
    store: &LibraryStore,
    id: &IngestBatchId,
    member_no: u32,
) -> Result<Option<IngestMember>> {
    ingest_members::Entity::find_by_id((id.as_str().to_string(), i64::from(member_no)))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map(member_from_model)
        .transpose()
}

/// Records that `received` bytes of a member are staged, and that the
/// batch was active now.
///
/// # Returns
///
/// `false` when the batch is no longer open or `received` passes the
/// member's size; nothing is written then.
///
/// # Errors
///
/// Returns the write error.
pub async fn record_received(
    store: &LibraryStore,
    id: &IngestBatchId,
    member_no: u32,
    received: u64,
) -> Result<bool> {
    let now = stamp(Utc::now());
    let received = signed(received, "received")?;
    let open = "EXISTS (SELECT 1 FROM ingest_batches WHERE id = ? AND state = 'open')";
    let reply = store
        .execute_host_batch(batch::request(
            &format!("ingest-received-{id}-{member_no}-{received}"),
            vec![
                batch::exec(
                    &format!(
                        "UPDATE ingest_members SET received = ?, updated_at = ? \
                         WHERE batch_id = ? AND member_no = ? AND ? <= size AND {open}"
                    ),
                    vec![
                        int(received),
                        text(&now),
                        text(id.as_str()),
                        member_int(member_no),
                        int(received),
                        text(id.as_str()),
                    ],
                ),
                batch::exec(
                    "UPDATE ingest_batches SET updated_at = ? WHERE id = ? AND state = 'open'",
                    vec![text(&now), text(id.as_str())],
                ),
            ],
        ))
        .await?;
    Ok(batch::rows_affected(&reply, 0) == 1)
}

/// Hands an open batch to its job: every member must be complete and the
/// manifest's every file present.
///
/// # Returns
///
/// `false` when the batch is not open or a file is incomplete.
///
/// # Errors
///
/// Returns the write error.
pub async fn mark_committed(
    store: &LibraryStore,
    id: &IngestBatchId,
    job_id: &str,
) -> Result<bool> {
    let now = stamp(Utc::now());
    let reply = store
        .execute_host_batch(batch::request(
            &format!("ingest-commit-{id}"),
            vec![batch::exec(
                "UPDATE ingest_batches SET state = 'committed', job_id = ?, updated_at = ? \
                 WHERE id = ? AND state = 'open' \
                 AND NOT EXISTS (SELECT 1 FROM ingest_members WHERE batch_id = ? \
                     AND received <> size) \
                 AND file_count = (SELECT COUNT(*) FROM ingest_members WHERE batch_id = ?)",
                vec![
                    text(job_id),
                    text(&now),
                    text(id.as_str()),
                    text(id.as_str()),
                    text(id.as_str()),
                ],
            )],
        ))
        .await?;
    Ok(batch::rows_affected(&reply, 0) == 1)
}

/// Cancels a batch the job has not started.
///
/// # Returns
///
/// `false` when the batch is running or already final.
///
/// # Errors
///
/// Returns the write error.
pub async fn cancel_batch(store: &LibraryStore, id: &IngestBatchId) -> Result<bool> {
    let now = stamp(Utc::now());
    let reply = store
        .execute_host_batch(batch::request(
            &format!("ingest-cancel-{id}"),
            vec![batch::exec(
                "UPDATE ingest_batches SET state = 'cancelled', updated_at = ?, finished_at = ? \
                 WHERE id = ? AND state IN ('open', 'committed')",
                vec![text(&now), text(&now), text(id.as_str())],
            )],
        ))
        .await?;
    Ok(batch::rows_affected(&reply, 0) == 1)
}

/// Starts (or, after a crash, resumes) the job's run of a batch.
///
/// # Returns
///
/// `false` when the batch is neither committed nor running under
/// `job_id`: it was cancelled, or belongs to another job.
///
/// # Errors
///
/// Returns the write error.
pub async fn start_batch(store: &LibraryStore, id: &IngestBatchId, job_id: &str) -> Result<bool> {
    let now = stamp(Utc::now());
    let reply = store
        .execute_host_batch(batch::request(
            &format!("ingest-start-{id}-{now}"),
            vec![batch::exec(
                "UPDATE ingest_batches SET state = 'running', updated_at = ? \
                 WHERE id = ? AND state IN ('committed', 'running') AND job_id = ?",
                vec![text(&now), text(id.as_str()), text(job_id)],
            )],
        ))
        .await?;
    Ok(batch::rows_affected(&reply, 0) == 1)
}

/// Ends a running batch as `state` (done or failed), with `detail` for a
/// failure.
///
/// # Errors
///
/// Returns the write error, or an error when `state` is not final.
pub async fn finish_batch(
    store: &LibraryStore,
    id: &IngestBatchId,
    state: IngestState,
    detail: Option<&str>,
) -> Result<()> {
    use crate::models::PublishDomainEventSpec;
    use crate::store::event_outbox::prepare_publish_domain_event;
    if !state.is_final() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "an ingest batch cannot finish as {}",
            state.as_str()
        )));
    }
    let Some(current) = load_batch(store, id).await? else {
        return Ok(());
    };
    let now = stamp(Utc::now());
    let detail = detail.map(short_reason);
    let event = prepare_publish_domain_event(PublishDomainEventSpec {
        id: String::new(),
        event_type: crate::INGEST_BATCH_FINISHED.into(),
        schema_version: 1,
        account_id: String::new(),
        source: "core.import".into(),
        correlation_id: id.as_str().to_string(),
        causation_id: String::new(),
        dedup_key: format!("{}:{id}", crate::INGEST_BATCH_FINISHED),
        payload: serde_json::json!({
            "batchId": id.as_str(),
            "state": state.as_str(),
            "source": current.source.as_str(),
            "intakeInstance": current.intake_instance,
            "detail": detail,
        })
        .to_string(),
        ordering_key: id.as_str().to_string(),
        ..PublishDomainEventSpec::default()
    })?;
    store
        .execute_host_batch(batch::request(
            &format!("ingest-finish-{id}"),
            vec![
                batch::exec(
                    "UPDATE ingest_batches SET state = ?, detail = ?, updated_at = ?, \
                     finished_at = ? WHERE id = ? AND state = 'running'",
                    vec![
                        text(state.as_str()),
                        text_opt(detail.as_deref()),
                        text(&now),
                        text(&now),
                        text(id.as_str()),
                    ],
                ),
                batch::exec(
                    "INSERT OR IGNORE INTO domain_events (\
                        id, event_type, schema_version, occurred_at, account_id, source, \
                        correlation_id, causation_id, dedup_key, payload, ordering_key, \
                        dispatch_state, created_at, wake_pending\
                     ) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 1 \
                     WHERE EXISTS (SELECT 1 FROM ingest_batches \
                         WHERE id = ? AND state = ? AND finished_at = ?)",
                    vec![
                        text(&event.id),
                        text(&event.event_type),
                        int(event.schema_version.max(1)),
                        text(&now),
                        text(&event.account_id),
                        text(&event.source),
                        text(&event.correlation_id),
                        text(&event.causation_id),
                        text(&event.dedup_key),
                        text(&event.payload),
                        text(&event.ordering_key),
                        text(&now),
                        text(id.as_str()),
                        text(state.as_str()),
                        text(&now),
                    ],
                ),
            ],
        ))
        .await?;
    Ok(())
}

/// Records outcomes the job decided, `MEMBERS_PER_WRITE` per host batch,
/// while the batch is running.
///
/// # Errors
///
/// Returns the write error.
pub async fn set_member_outcomes(
    store: &LibraryStore,
    id: &IngestBatchId,
    updates: &[OutcomeUpdate],
) -> Result<()> {
    let now = stamp(Utc::now());
    for (chunk_no, chunk) in updates.chunks(MEMBERS_PER_WRITE).enumerate() {
        let statements = chunk
            .iter()
            .map(|update| {
                batch::exec(
                    "UPDATE ingest_members SET outcome = ?, reason = ?, edition_id = ?, \
                     updated_at = ? WHERE batch_id = ? AND member_no = ? \
                     AND EXISTS (SELECT 1 FROM ingest_batches WHERE id = ? AND state = 'running')",
                    vec![
                        text(update.outcome.as_str()),
                        text_opt(update.reason.as_deref().map(short_reason).as_deref()),
                        text_opt(update.edition_id.as_deref()),
                        text(&now),
                        text(id.as_str()),
                        member_int(update.member_no),
                        text(id.as_str()),
                    ],
                )
            })
            .collect();
        store
            .execute_host_batch(batch::request(
                &format!("ingest-outcomes-{id}-{now}-{chunk_no}"),
                statements,
            ))
            .await?;
    }
    Ok(())
}

/// Adds the entries archive member `parent_no` expanded into, numbered
/// after the batch's last member, and marks the archive expanded, with
/// `parent_reason` when the walk stopped early.
///
/// Only the batch's running job calls this, so the numbering read here
/// cannot race another writer.
///
/// # Returns
///
/// The number given to the first entry; the rest follow in order.
///
/// # Errors
///
/// Returns a read or write error.
pub async fn add_expanded_members(
    store: &LibraryStore,
    id: &IngestBatchId,
    parent_no: u32,
    entries: &[ExpandedEntry],
    parent_reason: Option<&str>,
) -> Result<u32> {
    let last: Option<i64> = ingest_members::Entity::find()
        .filter(ingest_members::Column::BatchId.eq(id.as_str()))
        .order_by_desc(ingest_members::Column::MemberNo)
        .limit(1)
        .into_column(ingest_members::Column::MemberNo)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let first = match last {
        Some(last) => u32::try_from(last.saturating_add(1)).map_err(|_| {
            LibraryError::Other(anyhow::anyhow!("an ingest batch ran out of member numbers"))
        })?,
        None => 0,
    };
    let now = stamp(Utc::now());
    let running = "EXISTS (SELECT 1 FROM ingest_batches WHERE id = ? AND state = 'running')";
    for (chunk_no, chunk) in entries.chunks(MEMBERS_PER_WRITE).enumerate() {
        let mut statements = Vec::with_capacity(chunk.len());
        for (offset, entry) in chunk.iter().enumerate() {
            let index = chunk_no * MEMBERS_PER_WRITE + offset;
            let member_no = u32::try_from(index)
                .ok()
                .and_then(|index| first.checked_add(index))
                .ok_or_else(|| {
                    LibraryError::Other(anyhow::anyhow!(
                        "an ingest batch ran out of member numbers"
                    ))
                })?;
            let size = signed(entry.size, "size")?;
            statements.push(batch::exec(
                &format!(
                    "INSERT OR IGNORE INTO ingest_members (batch_id, member_no, parent_no, path, \
                     size, sha256, received, storage_key, signature, outcome, reason, updated_at) \
                     SELECT ?, ?, ?, ?, ?, NULL, ?, NULL, NULL, ?, ?, ? WHERE {running}"
                ),
                vec![
                    text(id.as_str()),
                    member_int(member_no),
                    member_int(parent_no),
                    text(&entry.path),
                    int(size),
                    int(size),
                    text(entry.outcome.as_str()),
                    text_opt(entry.reason.as_deref().map(short_reason).as_deref()),
                    text(&now),
                    text(id.as_str()),
                ],
            ));
        }
        store
            .execute_host_batch(batch::request(
                &format!("ingest-expand-{id}-{parent_no}-{chunk_no}"),
                statements,
            ))
            .await?;
    }
    set_member_outcomes(
        store,
        id,
        &[OutcomeUpdate {
            member_no: parent_no,
            outcome: MemberOutcome::Expanded,
            reason: parent_reason.map(str::to_string),
            edition_id: None,
        }],
    )
    .await?;
    Ok(first)
}

/// Open upload batches on `node_id` idle since before `before`: the
/// prune cancels them and removes their staged bytes.
///
/// # Errors
///
/// Returns a read error.
pub async fn idle_upload_batches(
    store: &LibraryStore,
    node_id: &str,
    before: DateTime<Utc>,
) -> Result<Vec<IngestBatchId>> {
    let rows: Vec<String> = ingest_batches::Entity::find()
        .filter(ingest_batches::Column::NodeId.eq(node_id))
        .filter(ingest_batches::Column::Source.eq(IngestSource::Upload.as_str()))
        .filter(ingest_batches::Column::State.eq(IngestState::Open.as_str()))
        .filter(ingest_batches::Column::UpdatedAt.lt(stamp(before)))
        .order_by_asc(ingest_batches::Column::UpdatedAt)
        .limit(super::trust::result_page(store).max(1))
        .into_column(ingest_batches::Column::Id)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows
        .iter()
        .filter_map(|raw| IngestBatchId::parse(raw))
        .collect())
}

/// Batches on `node_id` in a final state: their staged bytes are no
/// longer needed.
///
/// # Errors
///
/// Returns a read error.
pub async fn final_batches_on(store: &LibraryStore, node_id: &str) -> Result<Vec<IngestBatchId>> {
    let page = super::trust::result_page(store).max(1);
    let mut out: Vec<IngestBatchId> = Vec::new();
    loop {
        let mut select = ingest_batches::Entity::find()
            .filter(ingest_batches::Column::NodeId.eq(node_id))
            .filter(ingest_batches::Column::State.is_in([
                IngestState::Done.as_str(),
                IngestState::Failed.as_str(),
                IngestState::Cancelled.as_str(),
            ]))
            .order_by_asc(ingest_batches::Column::Id)
            .limit(page);
        if let Some(last) = out.last() {
            select = select.filter(ingest_batches::Column::Id.gt(last.as_str()));
        }
        let rows: Vec<String> = select
            .into_column(ingest_batches::Column::Id)
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let full = u64::try_from(rows.len()).unwrap_or(u64::MAX) >= page;
        out.extend(rows.iter().filter_map(|raw| IngestBatchId::parse(raw)));
        if !full {
            return Ok(out);
        }
    }
}

/// Whether a batch row exists for `id`, in any state.
///
/// # Errors
///
/// Returns a read error.
pub async fn batch_exists(store: &LibraryStore, id: &IngestBatchId) -> Result<bool> {
    Ok(ingest_batches::Entity::find_by_id(id.as_str().to_string())
        .into_column::<String, _>(ingest_batches::Column::Id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .is_some())
}

/// Deletes final batches (and their members) that finished before
/// `before`, at most one result page per call.
///
/// # Returns
///
/// How many batches were deleted.
///
/// # Errors
///
/// Returns a read or write error.
pub async fn delete_finished_before(store: &LibraryStore, before: DateTime<Utc>) -> Result<usize> {
    let rows: Vec<String> = ingest_batches::Entity::find()
        .filter(ingest_batches::Column::FinishedAt.lt(stamp(before)))
        .order_by_asc(ingest_batches::Column::FinishedAt)
        .limit(super::trust::result_page(store).clamp(1, 16))
        .into_column(ingest_batches::Column::Id)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut statements = Vec::with_capacity(rows.len() * 2);
    for id in &rows {
        statements.push(batch::exec(
            "DELETE FROM ingest_members WHERE batch_id = ? \
             AND EXISTS (SELECT 1 FROM ingest_batches WHERE id = ? AND finished_at IS NOT NULL)",
            vec![text(id), text(id)],
        ));
        statements.push(batch::exec(
            "DELETE FROM ingest_batches WHERE id = ? AND finished_at IS NOT NULL",
            vec![text(id)],
        ));
    }
    if statements.is_empty() {
        return Ok(0);
    }
    store
        .execute_host_batch(batch::request(
            &format!("ingest-retention-{}", uuid::Uuid::new_v4()),
            statements,
        ))
        .await?;
    Ok(rows.len())
}

/// A person's batches, newest first, at most `limit`.
///
/// # Errors
///
/// Returns a read error, or an error for a corrupt row.
pub async fn user_batches(
    store: &LibraryStore,
    user_id: i64,
    limit: u64,
) -> Result<Vec<IngestBatch>> {
    ingest_batches::Entity::find()
        .filter(ingest_batches::Column::UserId.eq(user_id))
        .order_by_desc(ingest_batches::Column::CreatedAt)
        .limit(limit.clamp(1, super::trust::result_page(store).max(1)))
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(batch_from_model)
        .collect()
}

/// One stored book an Administrator should review: a member stored under
/// a new local edition because nothing strong named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewItem {
    /// The batch.
    pub batch_id: IngestBatchId,
    /// The member.
    pub member_no: u32,
    /// Its path in the batch.
    pub path: String,
    /// The local edition it was stored under.
    pub edition_id: Option<String>,
    /// The patron who uploaded it.
    pub user_id: i64,
}

/// Members stored for review, oldest batch first, after `after` (a
/// `(batch id, member)` cursor), at most one result page.
///
/// # Errors
///
/// Returns a read error, or an error for a corrupt row.
pub async fn review_items(
    store: &LibraryStore,
    after: Option<(&IngestBatchId, u32)>,
) -> Result<Vec<ReviewItem>> {
    let mut select = ingest_members::Entity::find()
        .filter(ingest_members::Column::Outcome.eq(MemberOutcome::NeedsReview.as_str()))
        .order_by_asc(ingest_members::Column::BatchId)
        .order_by_asc(ingest_members::Column::MemberNo)
        .limit(member_page(store));
    if let Some((batch_id, member_no)) = after {
        select = select.filter(
            sea_orm::Condition::any()
                .add(ingest_members::Column::BatchId.gt(batch_id.as_str()))
                .add(
                    sea_orm::Condition::all()
                        .add(ingest_members::Column::BatchId.eq(batch_id.as_str()))
                        .add(ingest_members::Column::MemberNo.gt(i64::from(member_no))),
                ),
        );
    }
    let rows = select.all(store.db()).await.map_err(LibraryError::Orm)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let batch_id = IngestBatchId::parse(&row.batch_id)
            .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("bad ingest member batch id")))?;
        let Some(owner) = load_batch(store, &batch_id).await? else {
            continue;
        };
        let member = member_from_model(row)?;
        out.push(ReviewItem {
            batch_id,
            member_no: member.member_no,
            path: member.path,
            edition_id: member.edition_id,
            user_id: owner.user_id,
        });
    }
    Ok(out)
}

/// One file of a `POST /api/uploads` manifest, as sent.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    /// Relative path, folders kept (`Book/01 Chapter.mp3`); `\` reads as
    /// `/`. Refused when absolute, when it climbs out with `..`, or when it
    /// is too long or too deep.
    #[schema(example = "The Hobbit/01 Chapter One.mp3")]
    pub path: String,
    /// Size in bytes.
    #[schema(example = 52_428_800)]
    pub size: u64,
    /// Lower-case hex SHA-256 of the file, checked before it is stored.
    #[serde(default)]
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    pub sha256: Option<String>,
}

/// `POST /api/uploads` body: the files of one batch.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadManifest {
    /// Every file of the batch, in the order their numbers follow.
    pub files: Vec<ManifestFile>,
}

/// Why a manifest was refused before any batch was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The body is not the documented JSON.
    Malformed(String),
    /// A file's path is not usable.
    Path {
        /// The file's position.
        member_no: usize,
        /// Why.
        refusal: bookclerk_archive::PathRefusal,
    },
    /// A file's digest is not 64 hex digits.
    Digest {
        /// The file's position.
        member_no: usize,
    },
    /// The manifest names more than [`MAX_BATCH_FILES`] files.
    TooManyFiles,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "invalid manifest: {detail}"),
            Self::Path { member_no, refusal } => {
                write!(f, "file {member_no} has an unusable path ({refusal:?})")
            }
            Self::Digest { member_no } => write!(f, "file {member_no} has an invalid sha256"),
            Self::TooManyFiles => write!(f, "the manifest names more than {MAX_BATCH_FILES} files"),
        }
    }
}

/// Parses a `POST /api/uploads` body into the batch's members, checking
/// every path with the archive path policy. Sans-IO: the
/// `upload_manifest` fuzz target drives this same function.
///
/// Duplicate and folding paths, sizes and quotas are checked when the
/// batch is created ([`create_upload_batch`]).
///
/// # Errors
///
/// Returns [`ManifestError`] for a body that is not the documented JSON,
/// a path the policy refuses, a malformed digest or too many files.
pub fn parse_upload_manifest(body: &[u8]) -> std::result::Result<Vec<MemberSpec>, ManifestError> {
    let manifest: UploadManifest =
        serde_json::from_slice(body).map_err(|err| ManifestError::Malformed(err.to_string()))?;
    if manifest.files.len() > MAX_BATCH_FILES {
        return Err(ManifestError::TooManyFiles);
    }
    manifest
        .files
        .into_iter()
        .enumerate()
        .map(|(member_no, file)| {
            let path = EntryPath::admit(&file.path)
                .map_err(|refusal| ManifestError::Path { member_no, refusal })?;
            let sha256 = match file.sha256 {
                Some(raw) => {
                    Some(Sha256Hex::parse(&raw).ok_or(ManifestError::Digest { member_no })?)
                }
                None => None,
            };
            Ok(MemberSpec {
                path,
                size: file.size,
                sha256,
                intake: None,
            })
        })
        .collect()
}

/// Longest `Upload-Offset` value read: `i64::MAX` has 19 digits.
const MAX_OFFSET_DIGITS: usize = 19;

/// Parses an `Upload-Offset` header: plain ASCII digits (no sign, space or
/// leading `+`), at most 19 of them, and no larger than `i64::MAX`.
#[must_use]
pub fn parse_upload_offset(raw: &str) -> Option<u64> {
    if raw.is_empty() || raw.len() > MAX_OFFSET_DIGITS || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = raw.parse::<u64>().ok()?;
    i64::try_from(value).ok()?;
    Some(value)
}

/// Marks an edition acquired by a patron holding after the ingest job
/// wrote its files, and publishes `edition_acquire_changed` so the
/// library's page index picks the book up.
///
/// The holding must still hold the edition; an edition another live
/// holding already acquired keeps that holding's files.
///
/// # Returns
///
/// `true` when the edition is now acquired by `holding_id`.
///
/// # Errors
///
/// Returns the write error.
pub async fn record_ingest_acquired(
    store: &LibraryStore,
    edition_id: &str,
    holding_id: &str,
    storage_key: &str,
) -> Result<bool> {
    use crate::models::PublishDomainEventSpec;
    use crate::store::event_outbox::prepare_publish_domain_event;
    let now = stamp(Utc::now());
    let holds = "EXISTS (SELECT 1 FROM holdings WHERE id = ? \
         AND listing_id IN (SELECT id FROM store_listings WHERE edition_id = ?))";
    let event = prepare_publish_domain_event(PublishDomainEventSpec {
        id: String::new(),
        event_type: crate::EDITION_ACQUIRE_CHANGED.into(),
        schema_version: 1,
        account_id: String::new(),
        source: "core.import".into(),
        correlation_id: holding_id.to_string(),
        causation_id: String::new(),
        dedup_key: format!(
            "{}:{edition_id}:{}",
            crate::EDITION_ACQUIRE_CHANGED,
            uuid::Uuid::new_v4()
        ),
        payload: serde_json::json!({
            "editionId": edition_id,
            "titleId": holding_id,
            "status": "acquired",
        })
        .to_string(),
        ordering_key: holding_id.to_string(),
        ..PublishDomainEventSpec::default()
    })?;
    let statements = vec![
        batch::exec(
            &format!(
                "INSERT OR IGNORE INTO edition_acquisitions \
                 (edition_id, status, storage_key, error_message, holding_id, updated_at) \
                 SELECT ?, 'acquired', ?, NULL, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM editions WHERE id = ?) AND {holds}"
            ),
            vec![
                text(edition_id),
                text(storage_key),
                text(holding_id),
                text(&now),
                text(edition_id),
                text(holding_id),
                text(edition_id),
            ],
        ),
        batch::exec(
            &format!(
                "UPDATE edition_acquisitions SET status = 'acquired', storage_key = ?, \
                 error_message = NULL, holding_id = ?, updated_at = ? \
                 WHERE edition_id = ? \
                 AND (status != 'acquired' OR holding_id = ? \
                      OR NOT EXISTS (SELECT 1 FROM holdings \
                                     WHERE id = edition_acquisitions.holding_id)) \
                 AND {holds}"
            ),
            vec![
                text(storage_key),
                text(holding_id),
                text(&now),
                text(edition_id),
                text(holding_id),
                text(holding_id),
                text(edition_id),
            ],
        ),
        batch::exec(
            "INSERT INTO domain_events (\
                id, event_type, schema_version, occurred_at, account_id, source, \
                correlation_id, causation_id, dedup_key, payload, ordering_key, \
                dispatch_state, created_at, wake_pending\
             ) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 1 \
             WHERE EXISTS (SELECT 1 FROM edition_acquisitions \
                 WHERE edition_id = ? AND holding_id = ? AND status = 'acquired')",
            vec![
                text(&event.id),
                text(&event.event_type),
                int(event.schema_version.max(1)),
                text(&now),
                text(&event.account_id),
                text(&event.source),
                text(&event.correlation_id),
                text(&event.causation_id),
                text(&event.dedup_key),
                text(&event.payload),
                text(&event.ordering_key),
                text(&now),
                text(edition_id),
                text(holding_id),
            ],
        ),
    ];
    let reply = store
        .execute_host_batch(batch::request(
            &format!("ingest-acquired-{edition_id}-{holding_id}"),
            statements,
        ))
        .await?;
    Ok(batch::rows_affected(&reply, 2) == 1)
}

/// The id of the patron holding `user_id` has of `listing_id`, if any.
///
/// # Errors
///
/// Returns a read error.
pub async fn patron_holding_id(
    store: &LibraryStore,
    listing_id: &str,
    user_id: i64,
) -> Result<Option<String>> {
    use crate::entities::holdings;
    holdings::Entity::find()
        .filter(holdings::Column::ListingId.eq(listing_id))
        .filter(holdings::Column::Holder.eq(bookclerk_resolve::Holder::Patron(user_id).stored()))
        .into_column(holdings::Column::Id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)
}

/// The library's first active Owner: who holds what the operator uploads
/// with no person of their own.
///
/// # Errors
///
/// Returns a read error.
pub async fn first_active_owner(store: &LibraryStore) -> Result<Option<i64>> {
    use crate::entities::users;
    users::Entity::find()
        .filter(users::Column::Role.eq("owner"))
        .filter(users::Column::Status.eq("active"))
        .order_by_asc(users::Column::Id)
        .into_column(users::Column::Id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)
}

#[cfg(test)]
#[path = "ingest_tests.rs"]
mod tests;
