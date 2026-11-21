//! Noticing changes made outside Bookclerk at each output destination
//! (storage destinations ADR, `docs/adr/storage-destinations.md`,
//! decision 5).
//!
//! Every destination has a fallback: a paged listing compared with what the
//! last one saw, run on a schedule. A destination whose plugin has a change
//! feed is read through it on every pass while it stays healthy, and its
//! listing then runs rarely. A feed that fails, or says it may have missed
//! changes, hands over to the listing at once.
//!
//! What each destination holds lands in `destination_objects`, stamped with
//! the destination's change sequence, and the feed's state in
//! `destination_feeds` (see `bookclerk_library::DestinationFeed`). After a
//! pass, the copy check reads what changed: a stored copy whose primary
//! audio file is gone is marked missing (and its edition no longer
//! acquired once no destination holds it), or turns pending so the copy
//! retry loop writes it again, as `output.missing_files` says.
//!
//! One node at a time syncs a destination: each pass takes the
//! destination's library lease first.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_config::MissingFileAction;
use bookclerk_library::control_plane::{
    acquire_lease, load_or_create_node_identity, release_lease, renew_lease, LibraryLease,
};
use bookclerk_library::{DestinationFeed, FeedState, LibraryStore, SeenObject};
use bookclerk_storage::{ListRequest, ObjectChangeKind, StorageBackend};
use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

use crate::api::AppState;
use crate::job_worker::jittered_delay;

/// How often each node runs a sync pass over its deployed destinations.
/// A pass reads a healthy feed and starts a listing only when one is due,
/// so most passes wake no listing.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Time between scheduled listings of a destination without a healthy
/// change feed.
pub const LISTING_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Time between scheduled listings of a destination whose change feed is
/// healthy: the listing is then only a safety net.
pub const LISTING_INTERVAL_WITH_FEED: Duration = Duration::from_secs(24 * 60 * 60);

/// Objects asked for per listing page.
const LIST_PAGE: u32 = 1000;

/// Listing pages one pass reads at most; a longer listing continues on the
/// next pass from its saved cursor.
const MAX_LIST_PAGES_PER_PASS: usize = 200;

/// Changes asked for per feed page.
const FEED_PAGE: u32 = 256;

/// Feed pages one pass reads at most.
const MAX_FEED_PAGES_PER_PASS: usize = 64;

/// Changed rows the copy check reads per page.
const CHECK_PAGE: u64 = 500;

/// How long a pass holds a destination's lease without renewing it.
const LEASE_TTL: Duration = Duration::from_secs(10 * 60);

/// A listing whose removals reach this share of the objects known before
/// it (in percent) is held back: a listing that misses that much more
/// likely failed partway (a wrong prefix, a missing permission, a mount
/// that is not there) than saw the files really go.
const REMOVAL_GUARD_PERCENT: u64 = 20;

/// Removals a listing may always apply, however small the destination.
const REMOVAL_GUARD_MIN: u64 = 10;

/// What one destination's sync pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Feed pages read.
    pub feed_pages: usize,
    /// Listing pages read.
    pub listing_pages: usize,
    /// True when a listing finished in this pass.
    pub listing_finished: bool,
    /// Objects added, changed or removed.
    pub changes: u64,
    /// Removals a listing held back by the removal guard.
    pub held_back: u64,
    /// Stored copies whose file is gone: turned pending or marked
    /// missing, as `output.missing_files` says.
    pub lost_copies: u64,
    /// Editions that turned not acquired because no destination holds
    /// them any more.
    pub not_acquired: u64,
}

/// The lease a destination's sync pass holds.
fn lease_name(destination: &str) -> String {
    format!("destination-sync:{destination}")
}

/// Spawns the loop that syncs this node's deployed destinations every
/// [`SYNC_INTERVAL`] (jittered), and prunes old tombstones.
pub fn spawn_destination_sync_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(jittered_delay(SYNC_INTERVAL)).await;
            sync_deployed_destinations(&state).await;
        }
    });
}

/// One pass over every destination this node has deployed: each one whose
/// lease this node takes is synced; the others are left to the node that
/// holds them.
pub async fn sync_deployed_destinations(state: &AppState) {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let node_id = match load_or_create_node_identity(&files_dir) {
        Ok(identity) => identity.node_id,
        Err(err) => {
            warn!(error = %err, "node identity unavailable; destination sync idle");
            return;
        }
    };
    let missing_files = state.config.read().await.output.missing_files;
    let destinations = state.destinations.read().await.destinations();
    let library = state.library_snapshot().await;
    for destination in destinations {
        let id = destination.id.as_str().to_owned();
        let lease = match acquire_lease(&library, &lease_name(&id), &node_id, LEASE_TTL).await {
            Ok(Some(lease)) => lease,
            Ok(None) => continue,
            Err(err) => {
                warn!(destination = %id, error = %err, "destination sync lease unavailable");
                continue;
            }
        };
        let backend = destination.backend.as_ref();
        match sync_destination(&library, &id, backend, missing_files, Some(&lease)).await {
            Ok(report) if report.changes + report.lost_copies + report.held_back > 0 => info!(
                destination = %id,
                changes = report.changes,
                lost_copies = report.lost_copies,
                not_acquired = report.not_acquired,
                held_back = report.held_back,
                "destination sync noticed outside changes"
            ),
            Ok(report) => debug!(destination = %id, ?report, "destination sync pass"),
            Err(err) => warn!(destination = %id, error = %err, "destination sync failed"),
        }
        if let Err(err) = release_lease(&library, &lease).await {
            debug!(destination = %id, error = %err, "destination sync lease not released");
        }
    }
    let before = Utc::now() - chrono::Duration::days(bookclerk_library::OBJECT_TOMBSTONE_DAYS);
    if let Err(err) = library.prune_object_tombstones(before).await {
        debug!(error = %err, "object tombstones not pruned");
    }
}

/// Renews `lease` between pages; `false` once another node took it.
async fn keep_lease(library: &LibraryStore, lease: Option<&LibraryLease>) -> bool {
    let Some(lease) = lease else {
        return true;
    };
    matches!(renew_lease(library, lease, LEASE_TTL).await, Ok(Some(_)))
}

/// Syncs one destination: reads its change feed when it has one, runs or
/// continues its listing when one is due, then checks its stored copies
/// against what changed.
///
/// The caller holds the destination's lease (`lease`, renewed between
/// pages; `None` in tests).
///
/// # Errors
///
/// Returns a library read or write error, including a conflict when
/// another node saved the destination's sync state since this pass read it
/// (it took the lease). A storage error is recorded on the feed row instead
/// (the feed turns failing, or the listing keeps its cursor and error for
/// the next pass).
pub async fn sync_destination(
    library: &LibraryStore,
    destination: &str,
    backend: &dyn StorageBackend,
    missing_files: MissingFileAction,
    lease: Option<&LibraryLease>,
) -> anyhow::Result<SyncReport> {
    let mut report = SyncReport::default();
    let mut feed = library
        .destination_feed(destination)
        .await?
        .unwrap_or_else(|| DestinationFeed::new(destination));
    if backend.capabilities().changes {
        read_feed(library, backend, &mut feed, &mut report).await?;
    } else {
        feed.feed_state = FeedState::Off;
        feed.feed_cursor = None;
        feed.feed_error = None;
    }
    library.save_destination_feed(&mut feed).await?;
    if feed.listing_due(Utc::now()) {
        run_listing(
            library,
            backend,
            &mut feed,
            lease,
            missing_files,
            &mut report,
        )
        .await?;
    }
    check_copies(library, &mut feed, missing_files, &mut report).await?;
    Ok(report)
}

/// Reads the destination's change feed from its cursor, recording each
/// page, until it has nothing more ready or [`MAX_FEED_PAGES_PER_PASS`]
/// pages were read.
async fn read_feed(
    library: &LibraryStore,
    backend: &dyn StorageBackend,
    feed: &mut DestinationFeed,
    report: &mut SyncReport,
) -> anyhow::Result<()> {
    feed.feed_checked_at = Some(Utc::now());
    for _ in 0..MAX_FEED_PAGES_PER_PASS {
        let page = match backend
            .changes(feed.feed_cursor.as_deref(), FEED_PAGE)
            .await
        {
            Ok(Some(page)) => page,
            Ok(None) => {
                feed.feed_state = FeedState::Off;
                feed.feed_cursor = None;
                feed.feed_error = None;
                return Ok(());
            }
            Err(err) => {
                feed.feed_state = FeedState::Failing;
                feed.feed_error = Some(err.to_string());
                return Ok(());
            }
        };
        report.feed_pages += 1;
        let seq = feed.next_seq();
        // The number is claimed before any object is written: if another
        // node saved the feed since this one read it, the save conflicts
        // and nothing of this page is recorded.
        library.save_destination_feed(feed).await?;
        match apply_feed_page(library, backend, feed, seq, &page.changes).await {
            Ok(changed) => report.changes += changed,
            // Probing the page's keys failed: the cursor stays where it was,
            // so the page is read again, and a listing covers it meanwhile.
            Err(err)
                if err
                    .downcast_ref::<bookclerk_storage::StorageError>()
                    .is_some() =>
            {
                feed.feed_state = FeedState::Failing;
                feed.feed_error = Some(err.to_string());
                return Ok(());
            }
            Err(err) => return Err(err),
        }
        // The cursor is saved with what the page brought, so the next read
        // starts after it (a queue feed deletes the page's messages then).
        feed.feed_cursor = Some(page.cursor);
        feed.feed_error = None;
        feed.feed_state = if page.complete {
            match feed.feed_state {
                // A gap stays until a listing has covered it.
                FeedState::Gap => FeedState::Gap,
                FeedState::Off | FeedState::Healthy | FeedState::Failing => FeedState::Healthy,
            }
        } else {
            FeedState::Gap
        };
        feed.objects = library.known_object_count(&feed.destination).await?;
        library.save_destination_feed(feed).await?;
        if !page.more {
            break;
        }
    }
    Ok(())
}

/// Records one feed page at `seq`: an upsert that carries listing fields
/// is recorded as given; any other change is probed to learn whether the
/// object is there now.
///
/// # Returns
///
/// How many objects changed.
async fn apply_feed_page(
    library: &LibraryStore,
    backend: &dyn StorageBackend,
    feed: &DestinationFeed,
    seq: i64,
    changes: &[bookclerk_storage::ObjectChange],
) -> anyhow::Result<u64> {
    let now = Utc::now();
    // The feed lists changes oldest first and a key may repeat: only the
    // last change of each key says where it stands now.
    let mut last: HashMap<&str, usize> = HashMap::new();
    for (index, change) in changes.iter().enumerate() {
        last.insert(change.key.as_str(), index);
    }
    let mut seen = Vec::new();
    let mut removed = Vec::new();
    let mut probe = Vec::new();
    for (_, change) in changes
        .iter()
        .enumerate()
        .filter(|(index, change)| last.get(change.key.as_str()) == Some(index))
    {
        match (change.kind, &change.info) {
            (ObjectChangeKind::Removed, _) => removed.push(change.key.clone()),
            (ObjectChangeKind::Upserted, Some(info)) => seen.push(SeenObject {
                key: change.key.clone(),
                size: info.size,
                signature: info.signature.clone().or_else(|| info.etag.clone()),
                modified_at_ms: info.modified_at_ms,
            }),
            (ObjectChangeKind::Upserted | ObjectChangeKind::Changed, _) => {
                probe.push(change.key.clone());
            }
        }
    }
    if !probe.is_empty() {
        let probes = backend.head_many(&probe).await?;
        for (key, found) in probe.into_iter().zip(probes) {
            match found {
                Some(found) => seen.push(SeenObject {
                    key,
                    size: found.size,
                    signature: found.signature.or(found.etag),
                    modified_at_ms: found.modified_at_ms,
                }),
                None => removed.push(key),
            }
        }
    }
    let mut changed = library
        .remove_seen_objects(&feed.destination, seq, &removed, now)
        .await?;
    changed += library
        .record_seen_objects(&feed.destination, seq, &seen, now)
        .await?;
    Ok(changed)
}

/// Runs or continues the destination's listing for up to
/// [`MAX_LIST_PAGES_PER_PASS`] pages, and finishes it when the last page
/// is read.
async fn run_listing(
    library: &LibraryStore,
    backend: &dyn StorageBackend,
    feed: &mut DestinationFeed,
    lease: Option<&LibraryLease>,
    missing_files: MissingFileAction,
    report: &mut SyncReport,
) -> anyhow::Result<()> {
    let seq = match feed.listing_seq {
        Some(seq) => seq,
        None => {
            let seq = feed.next_seq();
            feed.listing_seq = Some(seq);
            feed.listing_cursor = None;
            feed.listing_started_at = Some(Utc::now());
            // The removal guard is judged against what was known before
            // this listing, not what it adds.
            feed.listing_base = Some(library.known_object_count(&feed.destination).await?);
            library.save_destination_feed(feed).await?;
            seq
        }
    };
    for _ in 0..MAX_LIST_PAGES_PER_PASS {
        if !keep_lease(library, lease).await {
            return Ok(());
        }
        let request = ListRequest {
            prefix: String::new(),
            cursor: feed.listing_cursor.clone(),
            limit: LIST_PAGE,
            with_signatures: true,
            modified_since_ms: None,
        };
        let page = match backend.list_hinted(&request).await {
            Ok(page) => page.page,
            Err(err) => {
                feed.listing_error = Some(err.to_string());
                library.save_destination_feed(feed).await?;
                return Ok(());
            }
        };
        report.listing_pages += 1;
        let seen: Vec<SeenObject> = page
            .objects
            .into_iter()
            .map(|object| SeenObject {
                signature: object.signature.or(object.etag),
                key: object.key,
                size: object.size,
                modified_at_ms: object.modified_at_ms,
            })
            .collect();
        report.changes += library
            .record_seen_objects(&feed.destination, seq, &seen, Utc::now())
            .await?;
        match page.next_cursor {
            Some(cursor) => {
                feed.listing_cursor = Some(cursor);
                library.save_destination_feed(feed).await?;
            }
            None => return finish_listing(library, feed, seq, missing_files, report).await,
        }
    }
    Ok(())
}

/// Ends the listing numbered `seq`: the objects it did not see are marked
/// removed, unless the removal guard holds them back; stored copies whose
/// file the listing did not see are handed to
/// [`LibraryStore::lose_copies`] under the same guard; and the next listing
/// is scheduled.
async fn finish_listing(
    library: &LibraryStore,
    feed: &mut DestinationFeed,
    seq: i64,
    missing_files: MissingFileAction,
    report: &mut SyncReport,
) -> anyhow::Result<()> {
    let now = Utc::now();
    let known = match feed.listing_base {
        Some(base) => base,
        None => library.known_object_count(&feed.destination).await?,
    };
    let unlisted = library
        .unlisted_object_count(&feed.destination, seq)
        .await?;
    let guard = (known.saturating_mul(REMOVAL_GUARD_PERCENT) / 100).max(REMOVAL_GUARD_MIN);
    if feed.listed_at.is_some() && unlisted > guard {
        // The previous listing's view stays; an operator who really moved
        // or deleted that much sees why here, and the next listing applies
        // the removals once they are a smaller share (or the files return).
        library
            .keep_unlisted_objects(&feed.destination, seq)
            .await?;
        report.held_back = unlisted;
        feed.listing_error = Some(format!(
            "the listing missed {unlisted} of {known} known files; removals were not applied"
        ));
    } else {
        report.changes += library.finish_listing(&feed.destination, seq, now).await?;
        feed.listing_error = None;
        check_unlisted_copies(library, feed, missing_files, report).await?;
    }
    report.listing_finished = true;
    feed.listing_seq = None;
    feed.listing_cursor = None;
    feed.listing_base = None;
    feed.listed_at = Some(now);
    if feed.feed_state == FeedState::Gap {
        feed.feed_state = FeedState::Healthy;
    }
    let interval = if feed.feed_state == FeedState::Healthy {
        LISTING_INTERVAL_WITH_FEED
    } else {
        LISTING_INTERVAL
    };
    feed.next_listing_at = Some(now + chrono::Duration::from_std(interval)?);
    feed.objects = library.known_object_count(&feed.destination).await?;
    library.save_destination_feed(feed).await?;
    Ok(())
}

/// Hands stored copies whose file a finished listing did not see to
/// [`LibraryStore::lose_copies`]. This covers files that were gone before
/// the destination was first listed, which leave no tombstone for
/// [`check_copies`] to read. The removal guard applies to them too: a
/// listing that misses that many copies more likely looked in the wrong
/// place, so they are left alone and `listing_error` says why.
async fn check_unlisted_copies(
    library: &LibraryStore,
    feed: &mut DestinationFeed,
    missing_files: MissingFileAction,
    report: &mut SyncReport,
) -> anyhow::Result<()> {
    let (gone, stored) = library.unlisted_copy_keys(&feed.destination).await?;
    let missed = u64::try_from(gone.len()).unwrap_or(u64::MAX);
    if missed == 0 {
        return Ok(());
    }
    let guard = (stored.saturating_mul(REMOVAL_GUARD_PERCENT) / 100).max(REMOVAL_GUARD_MIN);
    // A listing that found nothing at all while copies are stored there
    // (a wrong prefix, revoked access) is not trusted even on a destination
    // small enough to sit under the guard's minimum.
    let empty = library.known_object_count(&feed.destination).await? == 0;
    if missed > guard || empty {
        report.held_back += missed;
        feed.listing_error = Some(format!(
            "the listing missed {missed} of {stored} stored copies; they were left as they were"
        ));
        return Ok(());
    }
    let lost = library
        .lose_copies(&feed.destination, &gone, missing_files, Utc::now())
        .await?;
    report.lost_copies += lost.restoring + lost.missing;
    report.not_acquired += lost.not_acquired;
    Ok(())
}

/// Reads what changed since the copy check last ran, and hands every
/// stored copy whose primary audio file is gone to
/// [`LibraryStore::lose_copies`] with `missing_files`.
async fn check_copies(
    library: &LibraryStore,
    feed: &mut DestinationFeed,
    missing_files: MissingFileAction,
    report: &mut SyncReport,
) -> anyhow::Result<()> {
    let now: DateTime<Utc> = Utc::now();
    let mut after: (i64, Option<String>) = (feed.checked_seq, None);
    loop {
        let page = library
            .changed_objects(&feed.destination, (after.0, after.1.as_deref()), CHECK_PAGE)
            .await?;
        let Some(last) = page.last() else {
            break;
        };
        after = (last.changed_seq, Some(last.key.clone()));
        let gone: Vec<String> = page
            .iter()
            .filter(|change| change.removed)
            .map(|change| change.key.clone())
            .collect();
        let lost = library
            .lose_copies(&feed.destination, &gone, missing_files, now)
            .await?;
        report.lost_copies += lost.restoring + lost.missing;
        report.not_acquired += lost.not_acquired;
        if u64::try_from(page.len()).unwrap_or(u64::MAX) < CHECK_PAGE {
            break;
        }
    }
    // Only sequences that are complete are marked read: a listing in
    // progress may still change rows at its own sequence.
    let done = feed.listing_seq.map_or(feed.seq, |listing| listing - 1);
    if done > feed.checked_seq {
        feed.checked_seq = done;
        library.save_destination_feed(feed).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "destination_sync_tests.rs"]
mod tests;
