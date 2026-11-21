//! The drop folder runner (`docs/server/drop-folders.md`).
//!
//! Every deployed storage instance with the intake role is a drop folder.
//! This node lists each one it runs, groups the listing into items
//! (`bookclerk_intake::ItemGrouper`), and stores how far each item has come
//! (`control_plane::intake_items`). An item whose files have not changed for
//! the folder's quiet period is handed to the ingest job as one batch whose
//! members are storage references into the folder; the job copies them
//! from there. When the batch ends, the item is moved to `_imported/`,
//! deleted, or left, as the folder's settings say.
//!
//! The batch id is chosen and stored on the item before the batch is
//! created, so a restart at any point finds the batch or knows it was never
//! written; an item is never handed to two batches.
//!
//! Watching is the storage plugin's own change feed; it only starts a pass
//! sooner. A full listing every `scan_interval_seconds` is the baseline, so
//! a folder whose plugin sleeps, or a share that delivers no events, still
//! works. Claiming work is the ingest job's; nothing here acts on an event.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_config::{
    AfterIngest, IntakeAttribution, IntakeSettings, FAILED_FOLDER, IMPORTED_FOLDER,
};
use bookclerk_intake::{
    signature_of, step, ItemGrouper, ItemMember, ListedObject, ObservedItem, Step,
};
use bookclerk_library::control_plane::{
    apply_intake_writes, batch_members, cancel_batch, finish_intake_item, intake_settings,
    load_batch, load_intake_items, IngestBatchId, IngestMember, IngestState, IntakeFinish,
    IntakeItemRecord, IntakeRef, IntakeWrite, MemberOutcome, MemberSpec,
};
use bookclerk_library::LibraryStore;
use bookclerk_storage::{
    ListRequest, MoveOptions, StorageBackend, StorageError, WriteCondition, MAX_LIST_PAGE,
};
use tracing::{debug, info, warn};

use crate::api::AppState;

/// How often the runner wakes to see whether a folder is due.
const TICK: Duration = Duration::from_secs(15);

/// Most change-feed entries read per wake; more only means another pass.
const FEED_PAGE: u32 = 256;

/// Most names tried for one failure note before it is left unwritten.
const NOTE_NAMES: u32 = 10;

/// Who holds the books of one drop folder item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IntakeHolder {
    /// This patron, by user id.
    Patron(i64),
}

/// One file of a settled item, as the ingest job is given it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmittedMember {
    /// Its path inside the book: the item's own folder and file names,
    /// without a patron folder (`Dune/01.mp3`, or `Dune.m4b`).
    pub path: String,
    /// Its storage key exactly as the folder plugin lists it (prefix
    /// included), which the job reads.
    pub key: String,
    /// Size in bytes when it settled.
    pub size: u64,
    /// The backend's change signature when it settled, possibly empty.
    pub signature: String,
}

/// One settled item handed to the ingest job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntakeSubmission {
    /// The batch id to create the batch under; already stored on the
    /// item's record.
    pub batch_id: IngestBatchId,
    /// The drop folder's plugin instance id; members are read through it.
    pub plugin_instance_id: String,
    /// The item's key relative to the folder.
    pub item_key: String,
    /// The item's files.
    pub members: Vec<SubmittedMember>,
    /// The signature that settled, as hex; the job compares it before it
    /// reads and fails the item if the files moved.
    pub signature: String,
    /// Who holds the books.
    pub holder: IntakeHolder,
}

/// Why an item was not handed to the ingest job.
#[derive(Debug)]
pub(crate) enum HandOffError {
    /// The item can never be ingested as it is (a path or reference the
    /// job refuses, a holder who is not active); it goes to `_failed/`.
    Refused(String),
    /// The job could not be queued now (queue full, database error). The
    /// batch may be left open; the next finishing pass cancels it and the
    /// item settles again.
    Unavailable(anyhow::Error),
}

/// The ingest job's side of the hand-off.
#[async_trait]
pub(crate) trait IngestHandOff: Send + Sync {
    /// Creates and queues the ingest batch `submission.batch_id`.
    ///
    /// # Errors
    ///
    /// Returns [`HandOffError`] when the batch was refused or not queued.
    async fn submit(&self, submission: IntakeSubmission) -> Result<(), HandOffError>;
}

/// The daemon's hand-off: one `ingest_batch` job per item, through
/// [`crate::uploads::queue_intake_batch`].
pub(crate) struct QueueHandOff<'a> {
    /// The daemon state the job is queued through.
    pub state: &'a AppState,
}

#[async_trait]
impl IngestHandOff for QueueHandOff<'_> {
    async fn submit(&self, submission: IntakeSubmission) -> Result<(), HandOffError> {
        let IntakeHolder::Patron(user_id) = submission.holder;
        let mut specs = Vec::with_capacity(submission.members.len());
        for member in submission.members {
            let path = bookclerk_archive::EntryPath::admit(&member.path).map_err(|err| {
                HandOffError::Refused(format!("{} is not a usable file name: {err}", member.path))
            })?;
            specs.push(MemberSpec {
                path,
                size: member.size,
                sha256: None,
                intake: Some(IntakeRef {
                    key: member.key,
                    signature: Some(member.signature).filter(|s| !s.is_empty()),
                }),
            });
        }
        match crate::uploads::queue_intake_batch(
            self.state,
            &submission.batch_id,
            user_id,
            &submission.plugin_instance_id,
            &specs,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(crate::uploads::IntakeError::Refused(refusal)) => {
                Err(HandOffError::Refused(refusal.to_string()))
            }
            Err(crate::uploads::IntakeError::Queue(refusal)) => {
                Err(HandOffError::Unavailable(anyhow::anyhow!("{refusal:?}")))
            }
        }
    }
}

/// What one pass over a drop folder did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PassReport {
    /// Items the listing showed.
    pub items: usize,
    /// Items handed to the ingest job.
    pub queued: Vec<String>,
    /// Items whose patron folder names nobody; they wait for an
    /// Administrator.
    pub unassigned: Vec<String>,
    /// Items the ingest job refused outright; they went to `_failed/`.
    pub refused: Vec<String>,
}

/// One drop folder as the runner works on it.
#[derive(Clone, Copy)]
pub(crate) struct Folder<'a> {
    /// The library database.
    pub store: &'a LibraryStore,
    /// The drop folder's plugin instance id.
    pub instance_id: &'a str,
    /// Storage over the folder's guest.
    pub backend: &'a dyn StorageBackend,
    /// The folder plugin's key prefix.
    pub prefix: &'a str,
    /// The folder's settings.
    pub settings: &'a IntakeSettings,
}

/// A whole listing of a drop folder.
#[derive(Debug, Default)]
struct Listing {
    /// The listing grouped into items.
    items: Vec<ObservedItem>,
    /// Each listed object's ETag, by key relative to the prefix, where the
    /// backend gave one; a move names it so it touches only that version.
    etags: BTreeMap<String, String>,
}

impl Listing {
    /// The listed item with `key`.
    fn item(&self, key: &str) -> Option<&ObservedItem> {
        self.items
            .binary_search_by(|item| item.key.as_str().cmp(key))
            .ok()
            .and_then(|index| self.items.get(index))
    }
}

/// The change signature the grouper compares for one listed object: the
/// backend's signature, else its ETag, else its modification time, else
/// nothing (then only the size can show a change).
fn object_signature(info: &bookclerk_storage::ObjectInfo) -> String {
    if let Some(signature) = info.signature.as_deref().filter(|s| !s.is_empty()) {
        return signature.to_string();
    }
    if let Some(etag) = info.etag.as_deref().filter(|s| !s.is_empty()) {
        return format!("etag:{etag}");
    }
    info.modified_at_ms
        .map(|ms| format!("mtime:{ms}"))
        .unwrap_or_default()
}

/// The cursor for the next page, or `None` at the end.
///
/// # Errors
///
/// Fails a listing whose backend hands back a cursor it gave before: it
/// would never end, and only complete listings are acted on.
fn next_cursor(
    seen: &mut BTreeSet<String>,
    next: Option<String>,
) -> anyhow::Result<Option<String>> {
    match next {
        None => Ok(None),
        Some(cursor) if seen.insert(cursor.clone()) => Ok(Some(cursor)),
        Some(_) => anyhow::bail!("the folder listing repeated a page cursor"),
    }
}

/// Lists the whole folder under `prefix` and groups it into items, with
/// member keys relative to the prefix.
///
/// # Errors
///
/// Returns the listing error, or an error for a listing that repeats a
/// cursor.
async fn observe(
    backend: &dyn StorageBackend,
    prefix: &str,
    settings: &IntakeSettings,
) -> anyhow::Result<Listing> {
    let mut grouper = ItemGrouper::new(settings);
    let mut etags = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = backend
            .list_hinted(&ListRequest {
                prefix: prefix.to_string(),
                cursor: cursor.clone(),
                limit: MAX_LIST_PAGE,
                with_signatures: true,
                modified_since_ms: None,
            })
            .await?;
        for info in &page.page.objects {
            let Some(relative) = info.key.strip_prefix(prefix) else {
                continue;
            };
            let signature = object_signature(info);
            grouper.add(ListedObject {
                key: relative,
                size: info.size,
                signature: &signature,
            });
            if let Some(etag) = info.etag.as_deref().filter(|etag| !etag.is_empty()) {
                etags.insert(relative.to_string(), etag.to_string());
            }
        }
        cursor = next_cursor(&mut seen, page.page.next_cursor)?;
        if cursor.is_none() {
            break;
        }
    }
    Ok(Listing {
        items: grouper.finish(),
        etags,
    })
}

/// Who holds `item`'s books under `settings`, or `None` when its patron
/// folder names nobody.
async fn holder_of(
    store: &LibraryStore,
    settings: &IntakeSettings,
    item: &ObservedItem,
) -> anyhow::Result<Option<IntakeHolder>> {
    match settings.attribute_to {
        IntakeAttribution::Owner => Ok(settings.patron.map(IntakeHolder::Patron)),
        IntakeAttribution::Subfolder => {
            let Some(folder) = item.patron_folder.as_deref() else {
                return Ok(None);
            };
            Ok(store
                .get_user_by_login_name(folder)
                .await?
                .map(|user| IntakeHolder::Patron(user.id)))
        }
    }
}

/// The submission for a settled `item` of `folder`.
fn submission_of(
    folder: Folder<'_>,
    item: &ObservedItem,
    batch_id: IngestBatchId,
    holder: IntakeHolder,
) -> IntakeSubmission {
    let patron_folder = item
        .patron_folder
        .as_deref()
        .map(|patron| format!("{patron}/"));
    let members = item
        .members
        .iter()
        .map(|member| SubmittedMember {
            path: patron_folder
                .as_deref()
                .and_then(|patron| member.key.strip_prefix(patron))
                .unwrap_or(&member.key)
                .to_string(),
            key: format!("{}{}", folder.prefix, member.key),
            size: member.size,
            signature: member.signature.clone(),
        })
        .collect();
    IntakeSubmission {
        batch_id,
        plugin_instance_id: folder.instance_id.to_string(),
        item_key: item.key.clone(),
        members,
        signature: item.signature.to_hex(),
        holder,
    }
}

/// One pass over a drop folder: list, compare with the stored records,
/// store the changes, and hand settled items to `handoff`.
///
/// # Arguments
///
/// * `folder` - The drop folder.
/// * `now` - The clock, unix milliseconds; read once the listing is
///   complete, so a quiet period never starts before the listing saw the
///   files.
/// * `handoff` - The ingest job's side; `None` keeps settled items waiting.
///
/// # Errors
///
/// Returns a listing or database error; nothing is half-stored, since each
/// record write is idempotent and the next pass reconciles.
pub(crate) async fn run_pass(
    folder: Folder<'_>,
    now: &(dyn Fn() -> u64 + Sync),
    handoff: Option<&dyn IngestHandOff>,
) -> anyhow::Result<PassReport> {
    let listing = observe(folder.backend, folder.prefix, folder.settings).await?;
    let now_ms = now();
    let mut records = load_intake_items(folder.store, folder.instance_id).await?;
    let settle_ms = u64::from(folder.settings.settle_seconds) * 1_000;
    let mut report = PassReport {
        items: listing.items.len(),
        ..PassReport::default()
    };
    let mut writes = Vec::new();
    let mut ready = Vec::new();
    for item in &listing.items {
        let tracked = records.remove(&item.key).map(|record| record.tracked);
        match step(tracked.as_ref(), Some(item), now_ms, settle_ms) {
            Step::Keep | Step::Forget => {}
            Step::Store(record) => writes.push(IntakeWrite::Store {
                item_key: item.key.clone(),
                tracked: record,
            }),
            Step::Ready(_) => ready.push(item),
        }
    }
    for (item_key, record) in records {
        if step(Some(&record.tracked), None, now_ms, settle_ms) == Step::Forget {
            writes.push(IntakeWrite::Forget { item_key });
        }
    }
    apply_intake_writes(folder.store, folder.instance_id, &writes).await?;

    let Some(handoff) = handoff else {
        return Ok(report);
    };
    // Each item's batch id is stored before its batch is created.
    let mut handing = Vec::new();
    for item in ready {
        match holder_of(folder.store, folder.settings, item).await? {
            Some(holder) => handing.push((item, holder, IngestBatchId::new())),
            None => report.unassigned.push(item.key.clone()),
        }
    }
    let queued: Vec<IntakeWrite> = handing
        .iter()
        .map(|(item, _, batch_id)| IntakeWrite::Queue {
            item_key: item.key.clone(),
            signature: item.signature,
            batch_id: batch_id.as_str().to_string(),
        })
        .collect();
    apply_intake_writes(folder.store, folder.instance_id, &queued).await?;
    let day = day_of(now_ms);
    for (item, holder, batch_id) in handing {
        let submission = submission_of(folder, item, batch_id.clone(), holder);
        match handoff.submit(submission).await {
            Ok(()) => report.queued.push(item.key.clone()),
            Err(HandOffError::Refused(reason)) => {
                warn!(
                    plugin_instance_id = folder.instance_id,
                    item = %item.key,
                    reason = %reason,
                    "drop folder item refused by the ingest job"
                );
                let finished = finish_one(
                    folder,
                    &listing,
                    &item.key,
                    batch_id.as_str(),
                    item.signature,
                    &IngestOutcome::Failed(reason),
                    &day,
                )
                .await;
                match finished {
                    Ok(()) => report.refused.push(item.key.clone()),
                    // The record still names a batch that was never
                    // written; the next finishing pass lets it settle
                    // again.
                    Err(err) => warn!(
                        plugin_instance_id = folder.instance_id,
                        item = %item.key,
                        error = %err,
                        "a refused drop folder item could not be finished"
                    ),
                }
            }
            Err(HandOffError::Unavailable(err)) => warn!(
                plugin_instance_id = folder.instance_id,
                item = %item.key,
                error = %err,
                "drop folder item could not be queued for ingest; it settles again"
            ),
        }
    }
    Ok(report)
}

/// The `_imported/` day folder name for the unix-millisecond time `ms`.
fn day_of(ms: u64) -> String {
    let date = i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .unwrap_or_default()
        .date_naive();
    day_folder(date)
}

/// How a finished ingest batch went for its drop folder item, from the
/// batch's final state and its members' outcomes.
///
/// The item counts as added when the batch finished and at least one file
/// was stored (matched, needing review, or already present) with none
/// failing to store. Files the job refused on their own (an extra it does
/// not keep) do not fail the item.
///
/// # Returns
///
/// `None` while the batch has not finished.
fn outcome_of(state: IngestState, members: &[IngestMember]) -> Option<IngestOutcome> {
    if !state.is_final() {
        return None;
    }
    let mut stored = false;
    let mut problems = Vec::new();
    for member in members {
        match member.outcome {
            MemberOutcome::Matched | MemberOutcome::NeedsReview | MemberOutcome::Duplicate => {
                stored = true;
            }
            MemberOutcome::Expanded | MemberOutcome::Refused => {}
            MemberOutcome::Failed | MemberOutcome::Pending => problems.push(format!(
                "{}: {}",
                member.path,
                member.reason.as_deref().unwrap_or("not stored")
            )),
        }
    }
    let ended = match state {
        IngestState::Done => None,
        IngestState::Failed => Some("the ingest job gave up"),
        IngestState::Cancelled => Some("the ingest batch was cancelled"),
        IngestState::Open | IngestState::Committed | IngestState::Running => return None,
    };
    if stored && problems.is_empty() && ended.is_none() {
        return Some(IngestOutcome::Added);
    }
    let mut reasons: Vec<String> = ended.map(str::to_string).into_iter().collect();
    reasons.extend(problems);
    if !stored && reasons.is_empty() {
        reasons = members
            .iter()
            .filter(|member| member.outcome == MemberOutcome::Refused)
            .map(|member| {
                format!(
                    "{}: {}",
                    member.path,
                    member.reason.as_deref().unwrap_or("refused")
                )
            })
            .collect();
    }
    if reasons.is_empty() {
        reasons.push("nothing in the item could be stored".to_string());
    }
    Some(IngestOutcome::Failed(reasons.join("\n")))
}

/// Where a queued item's batch stands.
enum BatchStand {
    /// Still being worked on, or committed and waiting for the job.
    Pending,
    /// It never became a batch the job runs: it was not written (a crash
    /// between storing its id and creating it, or a refusal), or it was
    /// left open when queueing failed and has now been cancelled. The
    /// item's files were not read and are not touched; it settles again.
    NeverRan,
    /// It ended this way.
    Ended(IngestOutcome),
}

/// Where the batch `batch_id` stands, cancelling it if it was left open.
///
/// A batch row that is gone altogether also counts as never run. Final
/// batches are kept for weeks, so that happens only after a crash before
/// the batch was written, or a node offline past that retention; in the
/// second case the item is ingested again and its books found as
/// duplicates.
async fn batch_stand(store: &LibraryStore, batch_id: &str) -> anyhow::Result<BatchStand> {
    let Some(id) = IngestBatchId::parse(batch_id) else {
        return Ok(BatchStand::NeverRan);
    };
    let Some(batch) = load_batch(store, &id).await? else {
        return Ok(BatchStand::NeverRan);
    };
    if batch.state == IngestState::Open {
        // An intake batch is open only between its creation and its
        // queueing in one hand-off, and hand-offs run one at a time in this
        // runner, so an open one here is one whose queueing failed. If it
        // was committed meanwhile, the cancel does nothing and the batch
        // finishes as usual.
        return Ok(if cancel_batch(store, &id).await? {
            BatchStand::NeverRan
        } else {
            BatchStand::Pending
        });
    }
    let members = batch_members(store, &id).await?;
    Ok(outcome_of(batch.state, &members).map_or(BatchStand::Pending, BatchStand::Ended))
}

/// Finishes every item of the drop folder whose ingest batch has ended,
/// and lets every item whose batch never ran settle again.
///
/// The batch's own rows are the record, so an item finishes even when the
/// node missed the `ingest_batch_finished` event or restarted meanwhile.
/// The folder is listed at most once per call, and not at all when its
/// items are left in place.
///
/// # Returns
///
/// How many items were finished or released.
///
/// # Errors
///
/// Returns a database or listing error that stopped the whole pass, or
/// the first error among the items; every other item is still attempted.
pub(crate) async fn finish_ended(folder: Folder<'_>, now_ms: u64) -> anyhow::Result<usize> {
    let queued: Vec<IntakeItemRecord> = load_intake_items(folder.store, folder.instance_id)
        .await?
        .into_values()
        .filter(|record| {
            record.tracked.state == bookclerk_intake::TrackedState::Queued
                && record.batch_id.is_some()
        })
        .collect();
    let day = day_of(now_ms);
    let mut listing: Option<Listing> = None;
    let mut finished = 0;
    let mut first_error = None;
    for record in queued {
        let Some(batch_id) = record.batch_id.as_deref() else {
            continue;
        };
        let result = finish_queued(folder, &record, batch_id, &mut listing, now_ms, &day).await;
        match result {
            Ok(true) => finished += 1,
            Ok(false) => {}
            Err(err) => {
                warn!(
                    plugin_instance_id = folder.instance_id,
                    item = %record.item_key,
                    error = %err,
                    "finishing a drop folder item failed; trying again next pass"
                );
                first_error.get_or_insert(err);
            }
        }
    }
    match first_error {
        Some(err) if finished == 0 => Err(err),
        Some(_) | None => Ok(finished),
    }
}

/// Finishes or releases one queued item, listing the folder into
/// `listing` the first time a finished item needs it.
///
/// # Returns
///
/// Whether the item was finished or released; `false` while its batch is
/// still pending.
///
/// # Errors
///
/// Returns a database or listing error.
async fn finish_queued(
    folder: Folder<'_>,
    record: &IntakeItemRecord,
    batch_id: &str,
    listing: &mut Option<Listing>,
    now_ms: u64,
    day: &str,
) -> anyhow::Result<bool> {
    match batch_stand(folder.store, batch_id).await? {
        BatchStand::Pending => Ok(false),
        BatchStand::NeverRan => {
            finish_intake_item(
                folder.store,
                folder.instance_id,
                &record.item_key,
                batch_id,
                &IntakeFinish::Retry {
                    quiet_since_ms: now_ms,
                },
            )
            .await?;
            Ok(true)
        }
        BatchStand::Ended(outcome) => {
            if listing.is_none() && folder.settings.after_ingest != AfterIngest::Leave {
                *listing = Some(observe(folder.backend, folder.prefix, folder.settings).await?);
            }
            let empty = Listing::default();
            finish_one(
                folder,
                listing.as_ref().unwrap_or(&empty),
                &record.item_key,
                batch_id,
                record.tracked.signature,
                &outcome,
                day,
            )
            .await?;
            Ok(true)
        }
    }
}

/// How an ingest batch ended for one drop folder item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IngestOutcome {
    /// The books were added (matched or not).
    Added,
    /// Nothing was added, for this reason.
    Failed(String),
}

/// Finishes one item after its ingest batch: moves it to `_imported/<day>/`
/// or `_failed/`, deletes it, or leaves it, then records the outcome.
///
/// The item is moved or deleted only if its files still have the signature
/// that was ingested, and each move names the version that was listed; a
/// changed item is left in place and settles again.
///
/// # Arguments
///
/// * `folder` - The drop folder.
/// * `listing` - A listing taken after the batch ended; unused, and may be
///   empty, when items are left in place.
/// * `item_key` - The item.
/// * `batch_id` - The batch the item is queued on; nothing is recorded if
///   the item moved on to another.
/// * `ingested` - The signature that was handed to the batch.
/// * `outcome` - How the batch went.
/// * `day` - The `_imported/` day folder name.
///
/// # Errors
///
/// Returns the database error. A move or delete that fails is recorded on
/// the item with the reason, so it is not ingested again until it changes.
async fn finish_one(
    folder: Folder<'_>,
    listing: &Listing,
    item_key: &str,
    batch_id: &str,
    ingested: bookclerk_intake::ItemSignature,
    outcome: &IngestOutcome,
    day: &str,
) -> anyhow::Result<()> {
    let current = listing.item(item_key);
    let unchanged = current.is_some_and(|item| item.signature == ingested);
    let backend = folder.backend;
    let prefix = folder.prefix;
    let finish = match (outcome, folder.settings.after_ingest, unchanged) {
        (IngestOutcome::Added, AfterIngest::Leave, _) | (IngestOutcome::Added, _, false) => {
            IntakeFinish::Done
        }
        (IngestOutcome::Failed(reason), AfterIngest::Leave, _)
        | (IngestOutcome::Failed(reason), _, false) => IntakeFinish::Failed(reason.clone()),
        (IngestOutcome::Added, AfterIngest::Move, true) => {
            let target = format!("{IMPORTED_FOLDER}/{day}");
            cleanup_finish(
                relocate(backend, prefix, listing, current, item_key, &target).await,
                "added, but ",
            )
        }
        (IngestOutcome::Added, AfterIngest::Delete, true) => cleanup_finish(
            remove(backend, prefix, listing, current).await,
            "added, but ",
        ),
        (IngestOutcome::Failed(reason), AfterIngest::Move | AfterIngest::Delete, true) => {
            let cleanup =
                relocate(backend, prefix, listing, current, item_key, FAILED_FOLDER).await;
            if cleanup.moved_any() {
                write_note(backend, prefix, item_key, reason).await;
            }
            match cleanup_finish(cleanup, &format!("{reason}\n")) {
                IntakeFinish::Removed => IntakeFinish::Failed(reason.clone()),
                other => other,
            }
        }
    };
    let recorded = finish_intake_item(
        folder.store,
        folder.instance_id,
        item_key,
        batch_id,
        &finish,
    )
    .await?;
    info!(
        plugin_instance_id = folder.instance_id,
        item = item_key,
        ?finish,
        recorded,
        "drop folder item finished"
    );
    Ok(())
}

/// Finishes one item the way the runner would, listing the folder for it:
/// the tests' entry point.
#[cfg(test)]
pub(crate) async fn finish_item(
    folder: Folder<'_>,
    item_key: &str,
    batch_id: &str,
    outcome: &IngestOutcome,
    day: &str,
) -> anyhow::Result<()> {
    let Some(record) = load_intake_items(folder.store, folder.instance_id)
        .await?
        .remove(item_key)
    else {
        return Ok(());
    };
    let listing = observe(folder.backend, folder.prefix, folder.settings).await?;
    finish_one(
        folder,
        &listing,
        item_key,
        batch_id,
        record.tracked.signature,
        outcome,
        day,
    )
    .await
}

/// How moving or deleting an item's members went.
#[derive(Debug)]
enum Cleanup {
    /// Every member is gone from the item's place.
    Removed,
    /// A member could not be moved or deleted.
    Stopped {
        /// What went wrong, without the outcome's own prefix.
        reason: String,
        /// The members still in place.
        remaining: Vec<ItemMember>,
        /// Whether any member was moved or deleted before it stopped.
        moved_any: bool,
    },
}

impl Cleanup {
    /// Whether any member left the item's place.
    fn moved_any(&self) -> bool {
        match self {
            Self::Removed => true,
            Self::Stopped { moved_any, .. } => *moved_any,
        }
    }
}

/// The record for a cleanup, its reason led by `lead`. A cleanup that
/// stopped after moving some members keeps the rest under their own
/// signature, so they are not ingested again on their own.
fn cleanup_finish(cleanup: Cleanup, lead: &str) -> IntakeFinish {
    match cleanup {
        Cleanup::Removed => IntakeFinish::Removed,
        Cleanup::Stopped {
            reason,
            remaining,
            moved_any,
        } => {
            let reason = format!("{lead}{reason}");
            if moved_any {
                IntakeFinish::PartlyRemoved {
                    reason,
                    remaining: signature_of(&remaining, None),
                }
            } else {
                IntakeFinish::Failed(reason)
            }
        }
    }
}

/// Moves every member of `item` from `item_key` to `<target>/<item_key>`,
/// never replacing a file already there, and only the version the listing
/// saw where it gave an ETag.
async fn relocate(
    backend: &dyn StorageBackend,
    prefix: &str,
    listing: &Listing,
    item: Option<&ObservedItem>,
    item_key: &str,
    target: &str,
) -> Cleanup {
    let Some(item) = item else {
        return Cleanup::Removed;
    };
    for (index, member) in item.members.iter().enumerate() {
        let Some(rest) = member.key.strip_prefix(item_key) else {
            continue;
        };
        let options = MoveOptions {
            no_overwrite: true,
            if_source_match: listing.etags.get(&member.key).cloned(),
        };
        let from = format!("{prefix}{}", member.key);
        let to = format!("{prefix}{target}/{item_key}{rest}");
        if let Err(err) = backend.move_object(&from, &to, &options).await {
            return Cleanup::Stopped {
                reason: format!("moving {} to {target}/ failed: {err}", member.key),
                remaining: item.members.get(index..).unwrap_or_default().to_vec(),
                moved_any: index > 0,
            };
        }
    }
    Cleanup::Removed
}

/// Deletes every member of `item`, skipping one whose ETag no longer
/// matches the listing (it was replaced since).
async fn remove(
    backend: &dyn StorageBackend,
    prefix: &str,
    listing: &Listing,
    item: Option<&ObservedItem>,
) -> Cleanup {
    let Some(item) = item else {
        return Cleanup::Removed;
    };
    for (index, member) in item.members.iter().enumerate() {
        let key = format!("{prefix}{}", member.key);
        let stopped = |reason: String| Cleanup::Stopped {
            reason,
            remaining: item.members.get(index..).unwrap_or_default().to_vec(),
            moved_any: index > 0,
        };
        if let Some(listed) = listing.etags.get(&member.key) {
            match backend.head(&key).await {
                Ok(Some(probe)) if probe.etag.as_deref() == Some(listed.as_str()) => {}
                Ok(Some(_)) => {
                    return stopped(format!("{} changed since it was listed", member.key));
                }
                Ok(None) => continue,
                Err(err) => return stopped(format!("checking {} failed: {err}", member.key)),
            }
        }
        if let Err(err) = backend.delete(&key).await {
            return stopped(format!("deleting {} failed: {err}", member.key));
        }
    }
    Cleanup::Removed
}

/// Writes `reason` beside an item moved to `_failed/`, never over a file
/// already there: it takes the first free name of
/// `<item>.bookclerk-error.txt`, `<item>.bookclerk-error-2.txt`, ….
async fn write_note(backend: &dyn StorageBackend, prefix: &str, item_key: &str, reason: &str) {
    for attempt in 1..=NOTE_NAMES {
        let suffix = if attempt == 1 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let note = format!("{prefix}{FAILED_FOLDER}/{item_key}.bookclerk-error{suffix}.txt");
        let body = std::io::Cursor::new(format!("{reason}\n").into_bytes());
        match backend
            .put_stream_conditional(
                &note,
                Box::pin(body),
                bookclerk_storage::ObjectMeta {
                    content_type: Some("text/plain".into()),
                    ..bookclerk_storage::ObjectMeta::default()
                },
                &WriteCondition::IfNoneMatch,
            )
            .await
        {
            Ok(_) => return,
            Err(StorageError::Conflict(_)) => {}
            Err(err) => {
                warn!(error = %err, "could not write the drop folder error note");
                return;
            }
        }
    }
    warn!(
        item = item_key,
        "every name for the drop folder error note is taken"
    );
}

/// The name of the `_imported/` day folder for `date`, as `YYYY-MM-DD`.
fn day_folder(date: chrono::NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// Deletes the files of `_imported/` day folders older than
/// `prune_imported_days`, when that setting is on.
///
/// Only keys under `_imported/<YYYY-MM-DD>/` are touched; anything else
/// under `_imported/` was not put there by Bookclerk and stays.
///
/// # Returns
///
/// How many files were deleted.
///
/// # Errors
///
/// Returns the listing error, or an error for a listing that repeats a
/// cursor; a failed delete is logged and retried on the next prune.
async fn prune_imported(
    backend: &dyn StorageBackend,
    prefix: &str,
    settings: &IntakeSettings,
    today: chrono::NaiveDate,
) -> anyhow::Result<usize> {
    let Some(days) = settings.prune_imported_days else {
        return Ok(0);
    };
    if settings.after_ingest != AfterIngest::Move {
        return Ok(0);
    }
    let Some(oldest_kept) = today.checked_sub_days(chrono::Days::new(u64::from(days))) else {
        return Ok(0);
    };
    let root = format!("{prefix}{IMPORTED_FOLDER}/");
    let mut expired = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = backend
            .list_hinted(&ListRequest {
                prefix: root.clone(),
                cursor: cursor.clone(),
                limit: MAX_LIST_PAGE,
                with_signatures: false,
                modified_since_ms: None,
            })
            .await?;
        for info in &page.page.objects {
            let Some((day, _)) = info
                .key
                .strip_prefix(&root)
                .and_then(|rest| rest.split_once('/'))
            else {
                continue;
            };
            let Ok(date) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
                continue;
            };
            if day_folder(date) == day && date < oldest_kept {
                expired.push(info.key.clone());
            }
        }
        cursor = next_cursor(&mut seen, page.page.next_cursor)?;
        if cursor.is_none() {
            break;
        }
    }
    let mut deleted = 0;
    for key in expired {
        match backend.delete(&key).await {
            Ok(()) => deleted += 1,
            Err(err) => warn!(key = %key, error = %err, "could not prune an imported file"),
        }
    }
    Ok(deleted)
}

/// How often a folder's `_imported/` is pruned, at most.
const PRUNE_EVERY_MS: u64 = 60 * 60 * 1_000;

/// When each drop folder on this node was last listed, and its change-feed
/// cursor.
#[derive(Debug, Default)]
struct FolderClock {
    /// Unix milliseconds of the last full pass; zero before the first.
    last_pass_ms: u64,
    /// The folder's change-feed cursor, when it has a feed.
    feed_cursor: Option<String>,
    /// Earliest time an item that was settling at the last pass can be
    /// ready, unix milliseconds; zero when none was settling.
    next_ready_ms: u64,
    /// Unix milliseconds of the last `_imported/` prune; zero before the
    /// first.
    last_prune_ms: u64,
}

/// Starts the runner: every [`TICK`] it passes over each drop folder this
/// node runs whose listing is due, whose change feed has news, or whose
/// settling item may now be ready.
pub(crate) fn spawn_intake_runner(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut clocks: BTreeMap<String, FolderClock> = BTreeMap::new();
        loop {
            tokio::time::sleep(TICK).await;
            let intakes = state.destinations.read().await.intakes();
            clocks.retain(|id, _| intakes.iter().any(|intake| intake.id.as_str() == id));
            let library = state.library_snapshot().await;
            for intake in intakes {
                let id = intake.id.as_str().to_string();
                let settings = match intake_settings(&library, &id).await {
                    Ok(Some(settings)) => settings,
                    Ok(None) => continue,
                    Err(err) => {
                        warn!(plugin_instance_id = %id, error = %err, "drop folder settings unreadable");
                        continue;
                    }
                };
                let folder = Folder {
                    store: &library,
                    instance_id: &id,
                    backend: intake.backend.as_ref(),
                    prefix: &intake.settings.prefix,
                    settings: &settings,
                };
                let clock = clocks.entry(id.clone()).or_default();
                let feed_news = match intake
                    .backend
                    .changes(clock.feed_cursor.as_deref(), FEED_PAGE)
                    .await
                {
                    Ok(Some(page)) => {
                        let news = !page.changes.is_empty() || !page.complete;
                        clock.feed_cursor = Some(page.cursor);
                        news
                    }
                    Ok(None) => false,
                    Err(err) => {
                        debug!(plugin_instance_id = %id, error = %err, "drop folder change feed");
                        clock.feed_cursor = None;
                        true
                    }
                };
                if settings.prune_imported_days.is_some()
                    && now_ms().saturating_sub(clock.last_prune_ms) >= PRUNE_EVERY_MS
                {
                    clock.last_prune_ms = now_ms();
                    let today = chrono::Utc::now().date_naive();
                    match prune_imported(folder.backend, folder.prefix, &settings, today).await {
                        Ok(0) => {}
                        Ok(deleted) => {
                            info!(plugin_instance_id = %id, deleted, "pruned imported files");
                        }
                        Err(err) => {
                            warn!(plugin_instance_id = %id, error = %err, "drop folder prune failed");
                        }
                    }
                }
                match finish_ended(folder, now_ms()).await {
                    Ok(0) => {}
                    Ok(finished) => {
                        info!(plugin_instance_id = %id, finished, "drop folder items finished");
                    }
                    Err(err) => {
                        warn!(plugin_instance_id = %id, error = %err, "finishing drop folder items failed");
                    }
                }
                let started_ms = now_ms();
                let interval_ms = u64::from(settings.scan_interval_seconds) * 1_000;
                let due = clock.last_pass_ms == 0
                    || started_ms.saturating_sub(clock.last_pass_ms) >= interval_ms
                    || feed_news
                    || (clock.next_ready_ms != 0 && started_ms >= clock.next_ready_ms);
                if !due {
                    continue;
                }
                match run_pass(folder, &now_ms, Some(&QueueHandOff { state: &state })).await {
                    Ok(report) => {
                        clock.last_pass_ms = started_ms;
                        clock.next_ready_ms = next_ready(&library, &id, &settings, now_ms()).await;
                        if !report.queued.is_empty() || !report.unassigned.is_empty() {
                            info!(plugin_instance_id = %id, ?report, "drop folder pass");
                        }
                    }
                    Err(err) => {
                        warn!(plugin_instance_id = %id, error = %err, "drop folder pass failed");
                    }
                }
            }
        }
    });
}

/// The earliest time still ahead of `now_ms` at which a settling item of
/// the folder may be ready, or zero.
///
/// A deadline already past belongs to an item the pass just looked at and
/// could not hand off (its patron folder names nobody, or it is over the
/// member cap); it waits for the next listing on the folder's interval
/// rather than starting a listing every tick.
async fn next_ready(
    store: &LibraryStore,
    instance_id: &str,
    settings: &IntakeSettings,
    now_ms: u64,
) -> u64 {
    let settle_ms = u64::from(settings.settle_seconds) * 1_000;
    load_intake_items(store, instance_id)
        .await
        .map(|records| {
            records
                .values()
                .filter(|record| record.tracked.state == bookclerk_intake::TrackedState::Settling)
                .map(|record| record.tracked.quiet_since_ms.saturating_add(settle_ms))
                .filter(|ready_ms| *ready_ms > now_ms)
                .min()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// The current time in unix milliseconds.
fn now_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_intake::TrackedState;
    use bookclerk_library::control_plane::{create_plugin_instance, ConfigActor};
    use bookclerk_storage::LocalFsBackend;
    use std::sync::Mutex;

    /// Records submissions and accepts them.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<IntakeSubmission>>);

    impl Recorder {
        /// The batch id the item with `key` was handed to.
        fn batch_of(&self, key: &str) -> String {
            self.0
                .lock()
                .unwrap()
                .iter()
                .find(|submission| submission.item_key == key)
                .map(|submission| submission.batch_id.as_str().to_string())
                .unwrap()
        }
    }

    #[async_trait]
    impl IngestHandOff for Recorder {
        async fn submit(&self, submission: IntakeSubmission) -> Result<(), HandOffError> {
            self.0
                .lock()
                .map_err(|_| HandOffError::Unavailable(anyhow::anyhow!("poisoned")))?
                .push(submission);
            Ok(())
        }
    }

    /// The drop folder `id` over `backend`, with no prefix.
    fn folder<'a>(
        store: &'a LibraryStore,
        id: &'a str,
        backend: &'a LocalFsBackend,
        settings: &'a IntakeSettings,
    ) -> Folder<'a> {
        Folder {
            store,
            instance_id: id,
            backend,
            prefix: "",
            settings,
        }
    }

    async fn store_and_folder() -> (LibraryStore, String) {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let operator = ConfigActor::Operator {
            id: "operator:test".into(),
        };
        let id = create_plugin_instance(&store, &operator, "local", "Drop")
            .await
            .unwrap()
            .id
            .as_str()
            .to_string();
        (store, id)
    }

    fn write(dir: &std::path::Path, rel: &str, body: &[u8]) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[tokio::test]
    async fn a_settled_item_is_handed_off_once_then_moved() {
        let (store, id) = store_and_folder().await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Book A/01.mp3", b"one");
        write(dir.path(), "Book A/02.mp3", b"two");
        write(dir.path(), "Loose.m4b.part", b"partial");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let settings = IntakeSettings::for_patron(5);
        let recorder = Recorder::default();

        let first = run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 0,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert_eq!(first.items, 1, "the partial download is ignored");
        assert!(first.queued.is_empty());

        // Still inside the quiet period.
        let early = run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 59_999,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert!(early.queued.is_empty());

        let ready = run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 60_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert_eq!(ready.queued, ["Book A"]);
        let again = run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 120_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert!(
            again.queued.is_empty(),
            "a queued item is not handed off twice"
        );
        let submissions = recorder.0.lock().unwrap().clone();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].holder, IntakeHolder::Patron(5));
        assert_eq!(submissions[0].members.len(), 2);

        finish_item(
            folder(&store, &id, &backend, &settings),
            "Book A",
            &recorder.batch_of("Book A"),
            &IngestOutcome::Added,
            "2026-11-20",
        )
        .await
        .unwrap();
        assert!(dir
            .path()
            .join("_imported/2026-11-20/Book A/01.mp3")
            .is_file());
        assert!(!dir.path().join("Book A/01.mp3").exists());
        assert!(load_intake_items(&store, &id).await.unwrap().is_empty());

        // The moved item is not seen again.
        let after = run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 200_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert_eq!(after.items, 0);
    }

    #[tokio::test]
    async fn a_growing_file_restarts_the_quiet_period() {
        let (store, id) = store_and_folder().await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Big.m4b", b"part");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let settings = IntakeSettings::for_patron(1);
        let recorder = Recorder::default();
        run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 0,
            Some(&recorder),
        )
        .await
        .unwrap();
        write(dir.path(), "Big.m4b", b"part and more");
        let grown = run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 60_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert!(grown.queued.is_empty());
        let records = load_intake_items(&store, &id).await.unwrap();
        assert_eq!(records["Big.m4b"].tracked.quiet_since_ms, 60_000);
        assert_eq!(records["Big.m4b"].tracked.state, TrackedState::Settling);
    }

    #[tokio::test]
    async fn failures_go_to_failed_with_a_note_and_leave_keeps_the_file() {
        let (store, id) = store_and_folder().await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Bad.m4b", b"x");
        write(dir.path(), "Kept.m4b", b"y");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let recorder = Recorder::default();
        let settings = IntakeSettings::for_patron(1);
        run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 0,
            Some(&recorder),
        )
        .await
        .unwrap();
        run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 60_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        let batch = |key: &str| recorder.batch_of(key);
        finish_item(
            folder(&store, &id, &backend, &settings),
            "Bad.m4b",
            &batch("Bad.m4b"),
            &IngestOutcome::Failed("not audio".into()),
            "2026-11-20",
        )
        .await
        .unwrap();
        assert!(dir.path().join("_failed/Bad.m4b").is_file());
        let note = std::fs::read_to_string(dir.path().join("_failed/Bad.m4b.bookclerk-error.txt"))
            .unwrap();
        assert_eq!(note.trim(), "not audio");

        let leave = IntakeSettings {
            after_ingest: AfterIngest::Leave,
            ..settings.clone()
        };
        finish_item(
            folder(&store, &id, &backend, &leave),
            "Kept.m4b",
            &batch("Kept.m4b"),
            &IngestOutcome::Added,
            "2026-11-20",
        )
        .await
        .unwrap();
        assert!(dir.path().join("Kept.m4b").is_file());
        let records = load_intake_items(&store, &id).await.unwrap();
        assert_eq!(records["Kept.m4b"].tracked.state, TrackedState::Done);
        let later = run_pass(
            folder(&store, &id, &backend, &leave),
            &|| 900_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert!(later.queued.is_empty(), "a done item is not ingested again");
    }

    #[tokio::test]
    async fn an_unknown_patron_folder_waits() {
        let (store, id) = store_and_folder().await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "nobody/Book.m4b", b"z");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let settings = IntakeSettings {
            attribute_to: IntakeAttribution::Subfolder,
            patron: None,
            ..IntakeSettings::for_patron(1)
        };
        let recorder = Recorder::default();
        run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 0,
            Some(&recorder),
        )
        .await
        .unwrap();
        let report = run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 60_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert_eq!(report.unassigned, ["nobody/Book.m4b"]);
        assert!(recorder.0.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn old_imported_days_are_pruned_and_other_files_kept() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "_imported/2026-10-01/Old/01.mp3", b"old");
        write(dir.path(), "_imported/2026-11-19/New/01.mp3", b"new");
        write(dir.path(), "_imported/notes/keep.txt", b"mine");
        write(dir.path(), "_imported/2026-1-01/odd.txt", b"not ours");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let off = IntakeSettings::for_patron(1);
        assert_eq!(prune_imported(&backend, "", &off, today).await.unwrap(), 0);
        let on = IntakeSettings {
            prune_imported_days: Some(30),
            ..IntakeSettings::for_patron(1)
        };
        assert_eq!(prune_imported(&backend, "", &on, today).await.unwrap(), 1);
        assert!(!dir.path().join("_imported/2026-10-01/Old/01.mp3").exists());
        assert!(dir.path().join("_imported/2026-11-19/New/01.mp3").is_file());
        assert!(dir.path().join("_imported/notes/keep.txt").is_file());
        assert!(dir.path().join("_imported/2026-1-01/odd.txt").is_file());
    }
    fn member(path: &str, outcome: MemberOutcome, reason: Option<&str>) -> IngestMember {
        IngestMember {
            member_no: 0,
            parent_no: None,
            path: path.to_string(),
            size: 1,
            sha256: None,
            received: 1,
            storage_key: None,
            signature: None,
            outcome,
            reason: reason.map(str::to_string),
            edition_id: None,
        }
    }

    #[test]
    fn a_batch_outcome_decides_imported_or_failed() {
        let stored = member("Dune/01.mp3", MemberOutcome::Matched, None);
        let extra = member(
            "Dune/notes.exe",
            MemberOutcome::Refused,
            Some("unsupported"),
        );
        let broken = member(
            "Dune/02.mp3",
            MemberOutcome::Failed,
            Some("intake_unavailable"),
        );
        assert_eq!(
            outcome_of(IngestState::Running, std::slice::from_ref(&stored)),
            None
        );
        assert_eq!(
            outcome_of(IngestState::Done, &[stored.clone(), extra.clone()]),
            Some(IngestOutcome::Added),
            "a refused extra does not fail the book"
        );
        assert_eq!(
            outcome_of(IngestState::Done, &[stored.clone(), broken]),
            Some(IngestOutcome::Failed(
                "Dune/02.mp3: intake_unavailable".to_string()
            ))
        );
        assert_eq!(
            outcome_of(IngestState::Done, &[extra]),
            Some(IngestOutcome::Failed(
                "Dune/notes.exe: unsupported".to_string()
            ))
        );
        assert_eq!(
            outcome_of(IngestState::Cancelled, &[stored]),
            Some(IngestOutcome::Failed(
                "the ingest batch was cancelled".to_string()
            ))
        );
        assert_eq!(
            outcome_of(IngestState::Done, &[]),
            Some(IngestOutcome::Failed(
                "nothing in the item could be stored".to_string()
            ))
        );
    }

    /// Refuses every submission.
    struct Refuser;

    #[async_trait]
    impl IngestHandOff for Refuser {
        async fn submit(&self, _: IntakeSubmission) -> Result<(), HandOffError> {
            Err(HandOffError::Refused("the holder is not active".into()))
        }
    }

    #[tokio::test]
    async fn a_refused_item_goes_to_failed_with_the_reason() {
        let (store, id) = store_and_folder().await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Book.m4b", b"book");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let settings = IntakeSettings::for_patron(5);
        run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 0,
            Some(&Refuser),
        )
        .await
        .unwrap();
        let report = run_pass(
            folder(&store, &id, &backend, &settings),
            &|| 60_000,
            Some(&Refuser),
        )
        .await
        .unwrap();
        assert_eq!(report.refused, ["Book.m4b"]);
        assert!(dir.path().join("_failed/Book.m4b").is_file());
        let note = std::fs::read_to_string(dir.path().join("_failed/Book.m4b.bookclerk-error.txt"))
            .unwrap();
        assert_eq!(note, "the holder is not active\n");
    }

    #[test]
    fn day_folders_are_utc_dates() {
        assert_eq!(day_of(1_795_132_800_000), "2026-11-20");
    }

    /// Never queues anything.
    struct Unavailable;

    #[async_trait]
    impl IngestHandOff for Unavailable {
        async fn submit(&self, _: IntakeSubmission) -> Result<(), HandOffError> {
            Err(HandOffError::Unavailable(anyhow::anyhow!("queue full")))
        }
    }

    #[tokio::test]
    async fn an_item_whose_batch_never_ran_settles_again_untouched() {
        let (store, id) = store_and_folder().await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Book.m4b", b"book");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let settings = IntakeSettings::for_patron(5);
        let drop = folder(&store, &id, &backend, &settings);
        run_pass(drop, &|| 0, Some(&Unavailable)).await.unwrap();
        let report = run_pass(drop, &|| 60_000, Some(&Unavailable))
            .await
            .unwrap();
        assert!(report.queued.is_empty());
        // The record names a batch that was never written.
        let record = load_intake_items(&store, &id).await.unwrap()["Book.m4b"].clone();
        assert_eq!(record.tracked.state, TrackedState::Queued);
        let first_batch = record.batch_id.unwrap();
        assert_eq!(finish_ended(drop, 70_000).await.unwrap(), 1);
        let record = load_intake_items(&store, &id).await.unwrap()["Book.m4b"].clone();
        assert_eq!(record.tracked.state, TrackedState::Settling);
        assert_eq!(record.tracked.quiet_since_ms, 70_000);
        assert!(dir.path().join("Book.m4b").is_file(), "the file stays");
        // It is handed off again, under a new batch id, once quiet.
        let recorder = Recorder::default();
        let again = run_pass(drop, &|| 130_000, Some(&recorder)).await.unwrap();
        assert_eq!(again.queued, ["Book.m4b"]);
        assert_ne!(recorder.batch_of("Book.m4b"), first_batch);
    }

    #[tokio::test]
    async fn a_partial_move_keeps_the_rest_from_being_ingested_again() {
        let (store, id) = store_and_folder().await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Book/01.mp3", b"one");
        write(dir.path(), "Book/02.mp3", b"two");
        // Someone else's file already sits where track 02 would go.
        write(dir.path(), "_imported/2026-11-20/Book/02.mp3", b"theirs");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let settings = IntakeSettings::for_patron(5);
        let drop = folder(&store, &id, &backend, &settings);
        let recorder = Recorder::default();
        run_pass(drop, &|| 0, Some(&recorder)).await.unwrap();
        run_pass(drop, &|| 60_000, Some(&recorder)).await.unwrap();
        finish_item(
            drop,
            "Book",
            &recorder.batch_of("Book"),
            &IngestOutcome::Added,
            "2026-11-20",
        )
        .await
        .unwrap();
        assert!(dir
            .path()
            .join("_imported/2026-11-20/Book/01.mp3")
            .is_file());
        assert!(dir.path().join("Book/02.mp3").is_file());
        assert_eq!(
            std::fs::read(dir.path().join("_imported/2026-11-20/Book/02.mp3")).unwrap(),
            b"theirs"
        );
        let record = load_intake_items(&store, &id).await.unwrap()["Book"].clone();
        assert_eq!(record.tracked.state, TrackedState::Failed);
        assert!(record.error.unwrap().contains("Book/02.mp3"));
        // The track left behind is not handed off on its own.
        let later = run_pass(drop, &|| 900_000, Some(&recorder)).await.unwrap();
        assert!(later.queued.is_empty());
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failure_note_never_replaces_a_file() {
        let (store, id) = store_and_folder().await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Bad.m4b", b"x");
        write(dir.path(), "_failed/Bad.m4b.bookclerk-error.txt", b"mine");
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let settings = IntakeSettings::for_patron(5);
        let drop = folder(&store, &id, &backend, &settings);
        run_pass(drop, &|| 0, Some(&Refuser)).await.unwrap();
        let report = run_pass(drop, &|| 60_000, Some(&Refuser)).await.unwrap();
        assert_eq!(report.refused, ["Bad.m4b"]);
        assert_eq!(
            std::fs::read(dir.path().join("_failed/Bad.m4b.bookclerk-error.txt")).unwrap(),
            b"mine"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("_failed/Bad.m4b.bookclerk-error-2.txt"))
                .unwrap(),
            "the holder is not active\n"
        );
    }

    #[test]
    fn a_repeated_listing_cursor_fails_the_listing() {
        let mut seen = BTreeSet::new();
        assert_eq!(
            next_cursor(&mut seen, Some("A".into())).unwrap().as_deref(),
            Some("A")
        );
        assert_eq!(
            next_cursor(&mut seen, Some("B".into())).unwrap().as_deref(),
            Some("B")
        );
        assert!(next_cursor(&mut seen, Some("A".into())).is_err());
        assert_eq!(next_cursor(&mut seen, None).unwrap(), None);
    }

    #[tokio::test]
    async fn a_past_deadline_does_not_start_a_listing_every_tick() {
        let (store, id) = store_and_folder().await;
        let settings = IntakeSettings::for_patron(5);
        let settling = |quiet_since_ms| bookclerk_intake::TrackedItem {
            state: TrackedState::Settling,
            signature: bookclerk_intake::ItemSignature::from_hex(&"ab".repeat(32)).unwrap(),
            quiet_since_ms,
        };
        apply_intake_writes(
            &store,
            &id,
            &[
                IntakeWrite::Store {
                    item_key: "waiting for a patron".into(),
                    tracked: settling(0),
                },
                IntakeWrite::Store {
                    item_key: "still copying".into(),
                    tracked: settling(100_000),
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(next_ready(&store, &id, &settings, 120_000).await, 160_000);
        assert_eq!(next_ready(&store, &id, &settings, 170_000).await, 0);
    }
}
