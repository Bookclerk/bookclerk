//! The drop folder runner (`docs/server/drop-folders.md`).
//!
//! Every deployed storage instance with the intake role is a drop folder.
//! This node lists each one it runs, groups the listing into items
//! (`bookclerk_intake::ItemGrouper`), and stores how far each item has come
//! (`control_plane::intake_items`). An item whose files have not changed for
//! the folder's quiet period is handed to the ingest job as one batch whose
//! members are storage references into the folder, so nothing is copied to
//! staging first. When the batch ends, the item is moved to `_imported/`,
//! deleted, or left, as the folder's settings say.
//!
//! Watching is the storage plugin's own change feed; it only starts a pass
//! sooner. A full listing every `scan_interval_seconds` is the baseline, so
//! a folder whose plugin sleeps, or a share that delivers no events, still
//! works. Claiming work is the ingest job's; nothing here acts on an event.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_config::{
    AfterIngest, IntakeAttribution, IntakeSettings, FAILED_FOLDER, IMPORTED_FOLDER,
};
use bookclerk_intake::{step, ItemGrouper, ItemMember, ListedObject, ObservedItem, Step};
use bookclerk_library::control_plane::{
    apply_intake_writes, finish_intake_item, intake_settings, load_intake_items, IntakeFinish,
    IntakeWrite,
};
use bookclerk_library::LibraryStore;
use bookclerk_storage::{ListRequest, MoveOptions, StorageBackend, MAX_LIST_PAGE};
use tracing::{debug, info, warn};

use crate::api::AppState;

/// How often the runner wakes to see whether a folder is due.
const TICK: Duration = Duration::from_secs(15);

/// Most change-feed entries read per wake; more only means another pass.
const FEED_PAGE: u32 = 256;

/// Who holds the books of one drop folder item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IntakeHolder {
    /// This patron, by user id.
    Patron(i64),
}

/// One settled item handed to the ingest job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntakeSubmission {
    /// The drop folder's plugin instance id; members are read through it.
    pub plugin_instance_id: String,
    /// The item's key relative to the folder.
    pub item_key: String,
    /// The item's members, with storage keys as the folder plugin names
    /// them (its prefix included), sizes and change signatures.
    pub members: Vec<ItemMember>,
    /// The signature that settled, as hex; the job compares it before it
    /// reads and fails the item if the files moved.
    pub signature: String,
    /// Who holds the books.
    pub holder: IntakeHolder,
}

/// The ingest job's side of the hand-off.
#[async_trait]
pub(crate) trait IngestHandOff: Send + Sync {
    /// Queues one ingest batch for `submission`.
    ///
    /// # Returns
    ///
    /// The batch id the job reports the outcome under.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch could not be queued; the item stays
    /// settled and is offered again on the next pass.
    async fn submit(&self, submission: IntakeSubmission) -> anyhow::Result<String>;
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

/// Lists the whole folder under `prefix` and groups it into items, with
/// member keys relative to the prefix.
///
/// # Errors
///
/// Returns the listing error.
async fn observe(
    backend: &dyn StorageBackend,
    prefix: &str,
    settings: &IntakeSettings,
) -> anyhow::Result<Vec<ObservedItem>> {
    let mut grouper = ItemGrouper::new(settings);
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
        }
        match page.page.next_cursor {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            Some(_) | None => break,
        }
    }
    Ok(grouper.finish())
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

/// One pass over the drop folder `instance_id`: list, compare with the
/// stored records, store the changes, and hand settled items to `handoff`.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `instance_id` - The drop folder's plugin instance id.
/// * `backend` - Storage over the folder's guest.
/// * `prefix` - The folder plugin's key prefix.
/// * `settings` - The folder's settings.
/// * `now_ms` - The current time, unix milliseconds.
/// * `handoff` - The ingest job's side; `None` keeps settled items waiting.
///
/// # Errors
///
/// Returns a listing or database error; nothing is half-stored, since each
/// record write is idempotent and the next pass reconciles.
pub(crate) async fn run_pass(
    store: &LibraryStore,
    instance_id: &str,
    backend: &dyn StorageBackend,
    prefix: &str,
    settings: &IntakeSettings,
    now_ms: u64,
    handoff: Option<&dyn IngestHandOff>,
) -> anyhow::Result<PassReport> {
    let observed = observe(backend, prefix, settings).await?;
    let mut records = load_intake_items(store, instance_id).await?;
    let settle_ms = u64::from(settings.settle_seconds) * 1_000;
    let mut report = PassReport {
        items: observed.len(),
        ..PassReport::default()
    };
    let mut writes = Vec::new();
    let mut ready = Vec::new();
    for item in &observed {
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
    apply_intake_writes(store, instance_id, &writes).await?;

    let Some(handoff) = handoff else {
        return Ok(report);
    };
    let mut queued = Vec::new();
    for item in ready {
        let Some(holder) = holder_of(store, settings, item).await? else {
            report.unassigned.push(item.key.clone());
            continue;
        };
        let members = item
            .members
            .iter()
            .map(|member| ItemMember {
                key: format!("{prefix}{}", member.key),
                ..member.clone()
            })
            .collect();
        let submission = IntakeSubmission {
            plugin_instance_id: instance_id.to_string(),
            item_key: item.key.clone(),
            members,
            signature: item.signature.to_hex(),
            holder,
        };
        match handoff.submit(submission).await {
            Ok(batch_id) => {
                report.queued.push(item.key.clone());
                queued.push(IntakeWrite::Queue {
                    item_key: item.key.clone(),
                    signature: item.signature,
                    batch_id,
                });
            }
            Err(err) => warn!(
                plugin_instance_id = instance_id,
                item = %item.key,
                error = %err,
                "drop folder item could not be queued for ingest; trying again next pass"
            ),
        }
    }
    apply_intake_writes(store, instance_id, &queued).await?;
    Ok(report)
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
/// that was ingested; a changed item is left in place and settles again.
///
/// # Errors
///
/// Returns a listing, storage or database error. A move that fails leaves
/// the item recorded as failed with the reason, so it is not ingested again
/// until it changes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_item(
    store: &LibraryStore,
    instance_id: &str,
    backend: &dyn StorageBackend,
    prefix: &str,
    settings: &IntakeSettings,
    item_key: &str,
    batch_id: &str,
    outcome: &IngestOutcome,
    day: &str,
) -> anyhow::Result<()> {
    let ingested = load_intake_items(store, instance_id)
        .await?
        .remove(item_key)
        .filter(|record| record.batch_id.as_deref() == Some(batch_id));
    let Some(ingested) = ingested else {
        debug!(
            plugin_instance_id = instance_id,
            item = item_key,
            "drop folder item is no longer queued on this batch"
        );
        return Ok(());
    };
    let current = observe(backend, prefix, settings)
        .await?
        .into_iter()
        .find(|item| item.key == item_key);
    let unchanged = current
        .as_ref()
        .is_some_and(|item| item.signature == ingested.tracked.signature);
    let finish = match (outcome, settings.after_ingest, unchanged) {
        (IngestOutcome::Added, AfterIngest::Leave, _) | (IngestOutcome::Added, _, false) => {
            IntakeFinish::Done
        }
        (IngestOutcome::Failed(reason), AfterIngest::Leave, _)
        | (IngestOutcome::Failed(reason), _, false) => IntakeFinish::Failed(reason.clone()),
        (IngestOutcome::Added, AfterIngest::Move, true) => {
            let target = format!("{IMPORTED_FOLDER}/{day}");
            relocate(backend, prefix, current.as_ref(), item_key, &target).await
        }
        (IngestOutcome::Added, AfterIngest::Delete, true) => {
            remove(backend, prefix, current.as_ref()).await
        }
        (IngestOutcome::Failed(reason), AfterIngest::Move | AfterIngest::Delete, true) => {
            match relocate(backend, prefix, current.as_ref(), item_key, FAILED_FOLDER).await {
                IntakeFinish::Removed => {
                    let note = format!("{prefix}{FAILED_FOLDER}/{item_key}.bookclerk-error.txt");
                    if let Err(err) = backend
                        .put(
                            &note,
                            bytes::Bytes::from(format!("{reason}\n")),
                            bookclerk_storage::ObjectMeta {
                                content_type: Some("text/plain".into()),
                                ..bookclerk_storage::ObjectMeta::default()
                            },
                        )
                        .await
                    {
                        warn!(error = %err, "could not write the drop folder error note");
                    }
                    IntakeFinish::Failed(reason.clone())
                }
                other => other,
            }
        }
    };
    let recorded = finish_intake_item(store, instance_id, item_key, batch_id, &finish).await?;
    info!(
        plugin_instance_id = instance_id,
        item = item_key,
        ?finish,
        recorded,
        "drop folder item finished"
    );
    Ok(())
}

/// Moves every member of `item` from `item_key` to `<target>/<item_key>`,
/// never replacing a file already there.
async fn relocate(
    backend: &dyn StorageBackend,
    prefix: &str,
    item: Option<&ObservedItem>,
    item_key: &str,
    target: &str,
) -> IntakeFinish {
    let Some(item) = item else {
        return IntakeFinish::Removed;
    };
    let options = MoveOptions {
        no_overwrite: true,
        if_source_match: None,
    };
    for member in &item.members {
        let Some(rest) = member.key.strip_prefix(item_key) else {
            continue;
        };
        let from = format!("{prefix}{}", member.key);
        let to = format!("{prefix}{target}/{item_key}{rest}");
        if let Err(err) = backend.move_object(&from, &to, &options).await {
            return IntakeFinish::Failed(format!(
                "added, but moving {} to {target}/ failed: {err}",
                member.key
            ));
        }
    }
    IntakeFinish::Removed
}

/// Deletes every member of `item`.
async fn remove(
    backend: &dyn StorageBackend,
    prefix: &str,
    item: Option<&ObservedItem>,
) -> IntakeFinish {
    let Some(item) = item else {
        return IntakeFinish::Removed;
    };
    for member in &item.members {
        let key = format!("{prefix}{}", member.key);
        if let Err(err) = backend.delete(&key).await {
            return IntakeFinish::Failed(format!(
                "added, but deleting {} failed: {err}",
                member.key
            ));
        }
    }
    IntakeFinish::Removed
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
/// Returns the listing error; a failed delete is logged and retried on the
/// next prune.
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
        match page.page.next_cursor {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            Some(_) | None => break,
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
                let clock = clocks.entry(id.clone()).or_default();
                let now_ms = now_ms();
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
                    && now_ms.saturating_sub(clock.last_prune_ms) >= PRUNE_EVERY_MS
                {
                    clock.last_prune_ms = now_ms;
                    let today = chrono::Utc::now().date_naive();
                    match prune_imported(
                        intake.backend.as_ref(),
                        &intake.settings.prefix,
                        &settings,
                        today,
                    )
                    .await
                    {
                        Ok(0) => {}
                        Ok(deleted) => {
                            info!(plugin_instance_id = %id, deleted, "pruned imported files");
                        }
                        Err(err) => {
                            warn!(plugin_instance_id = %id, error = %err, "drop folder prune failed");
                        }
                    }
                }
                let interval_ms = u64::from(settings.scan_interval_seconds) * 1_000;
                let due = clock.last_pass_ms == 0
                    || now_ms.saturating_sub(clock.last_pass_ms) >= interval_ms
                    || feed_news
                    || (clock.next_ready_ms != 0 && now_ms >= clock.next_ready_ms);
                if !due {
                    continue;
                }
                match run_pass(
                    &library,
                    &id,
                    intake.backend.as_ref(),
                    &intake.settings.prefix,
                    &settings,
                    now_ms,
                    None,
                )
                .await
                {
                    Ok(report) => {
                        clock.last_pass_ms = now_ms;
                        clock.next_ready_ms = next_ready(&library, &id, &settings).await;
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

/// The earliest time a settling item of the folder may be ready, or zero.
async fn next_ready(store: &LibraryStore, instance_id: &str, settings: &IntakeSettings) -> u64 {
    let settle_ms = u64::from(settings.settle_seconds) * 1_000;
    load_intake_items(store, instance_id)
        .await
        .map(|records| {
            records
                .values()
                .filter(|record| record.tracked.state == bookclerk_intake::TrackedState::Settling)
                .map(|record| record.tracked.quiet_since_ms.saturating_add(settle_ms))
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

    /// Records submissions and answers with numbered batch ids.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<IntakeSubmission>>);

    #[async_trait]
    impl IngestHandOff for Recorder {
        async fn submit(&self, submission: IntakeSubmission) -> anyhow::Result<String> {
            let mut seen = self.0.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
            seen.push(submission);
            Ok(format!("batch-{}", seen.len()))
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

        let first = run_pass(&store, &id, &backend, "", &settings, 0, Some(&recorder))
            .await
            .unwrap();
        assert_eq!(first.items, 1, "the partial download is ignored");
        assert!(first.queued.is_empty());

        // Still inside the quiet period.
        let early = run_pass(
            &store,
            &id,
            &backend,
            "",
            &settings,
            59_999,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert!(early.queued.is_empty());

        let ready = run_pass(
            &store,
            &id,
            &backend,
            "",
            &settings,
            60_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        assert_eq!(ready.queued, ["Book A"]);
        let again = run_pass(
            &store,
            &id,
            &backend,
            "",
            &settings,
            120_000,
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
            &store,
            &id,
            &backend,
            "",
            &settings,
            "Book A",
            "batch-1",
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
            &store,
            &id,
            &backend,
            "",
            &settings,
            200_000,
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
        run_pass(&store, &id, &backend, "", &settings, 0, Some(&recorder))
            .await
            .unwrap();
        write(dir.path(), "Big.m4b", b"part and more");
        let grown = run_pass(
            &store,
            &id,
            &backend,
            "",
            &settings,
            60_000,
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
        run_pass(&store, &id, &backend, "", &settings, 0, Some(&recorder))
            .await
            .unwrap();
        run_pass(
            &store,
            &id,
            &backend,
            "",
            &settings,
            60_000,
            Some(&recorder),
        )
        .await
        .unwrap();
        let batch = |key: &str| {
            recorder
                .0
                .lock()
                .unwrap()
                .iter()
                .position(|s| s.item_key == key)
                .map(|i| format!("batch-{}", i + 1))
                .unwrap()
        };
        finish_item(
            &store,
            &id,
            &backend,
            "",
            &settings,
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
            &store,
            &id,
            &backend,
            "",
            &leave,
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
        let later = run_pass(&store, &id, &backend, "", &leave, 900_000, Some(&recorder))
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
        run_pass(&store, &id, &backend, "", &settings, 0, Some(&recorder))
            .await
            .unwrap();
        let report = run_pass(
            &store,
            &id,
            &backend,
            "",
            &settings,
            60_000,
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
}
