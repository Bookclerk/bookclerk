//! What each output destination holds, as its change feed and listings
//! report it (`destination_objects`), and how those reports are followed
//! (`destination_feeds`).
//!
//! Every destination has a fallback for noticing outside changes: a paged
//! listing compared with the previous one (storage destinations ADR,
//! `docs/adr/storage-destinations.md`, decision 5). A change feed, when the
//! destination's plugin has one and it is healthy, reports changes sooner
//! and lets the listing run rarely. The destination sync pass in the daemon
//! drives both and writes them here; the pass holds the destination's lease,
//! so one writer at a time changes a destination's rows.
//!
//! Each change is stamped with the destination's change sequence
//! (`destination_feeds.seq`): a feed page or a listing takes the next number
//! and every row it changes records it as `changed_seq`. A reader keeps the
//! last sequence it handled and pages what changed after it
//! ([`LibraryStore::changed_objects`]); a removed object stays as a
//! tombstone for [`OBJECT_TOMBSTONE_DAYS`] so a reader that runs later still
//! sees the removal.

use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, Utc};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect,
};

use super::LibraryStore;
use crate::entities::{destination_feeds, destination_objects, edition_copies, edition_files};
use crate::error::{LibraryError, Result};
use bookclerk_config::MissingFileAction;

use crate::{AcquireStatus, CopyState, FileRole};

/// Days a removed object's tombstone is kept for readers that page changes.
pub const OBJECT_TOMBSTONE_DAYS: i64 = 30;

/// Columns one `destination_objects` row binds on insert.
const OBJECT_COLUMNS: usize = 9;

/// Most keys in one `IN (...)` list, whatever the adapter allows.
const MAX_KEYS_PER_STATEMENT: usize = 200;

/// The state of a destination's change feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FeedState {
    /// The destination's plugin has no change feed (or it is not turned
    /// on); scheduled listings alone notice outside changes.
    Off,
    /// The feed answered and said it missed nothing; listings run rarely.
    Healthy,
    /// The feed said it may have missed changes; a listing runs at once.
    Gap,
    /// Reading the feed failed; a listing runs at once and the feed is
    /// tried again on the next pass.
    Failing,
}

impl FeedState {
    /// Every state, in stored order (the `destination_feeds.feed_state`
    /// CHECK list).
    pub const ALL: [Self; 4] = [Self::Off, Self::Healthy, Self::Gap, Self::Failing];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Healthy => "healthy",
            Self::Gap => "gap",
            Self::Failing => "failing",
        }
    }

    /// The state a stored spelling names; `None` for one this build does
    /// not know.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == stored)
    }
}

/// How one destination's outside changes are followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationFeed {
    /// The destination's plugin instance id.
    pub destination: String,
    /// The last change sequence number handed out.
    pub seq: i64,
    /// The change feed's state.
    pub feed_state: FeedState,
    /// The feed cursor to read from next; `None` before the first read.
    pub feed_cursor: Option<String>,
    /// The last feed failure.
    pub feed_error: Option<String>,
    /// When the feed was last read.
    pub feed_checked_at: Option<DateTime<Utc>>,
    /// Sequence of the listing in progress; `None` between listings.
    pub listing_seq: Option<i64>,
    /// Cursor of the next page of the listing in progress.
    pub listing_cursor: Option<String>,
    /// When the listing in progress, or the last one, started.
    pub listing_started_at: Option<DateTime<Utc>>,
    /// When the last listing finished.
    pub listed_at: Option<DateTime<Utc>>,
    /// When the next scheduled listing is due.
    pub next_listing_at: Option<DateTime<Utc>>,
    /// Why the last listing failed or its removals were not applied.
    pub listing_error: Option<String>,
    /// Objects the destination held after the last listing or feed page.
    pub objects: u64,
    /// The last sequence the copy check read changes up to.
    pub checked_seq: i64,
    /// Objects known when the listing in progress started, which its
    /// removal guard is judged against; `None` between listings.
    pub listing_base: Option<u64>,
    /// The row revision this value was read at (0 before the first save);
    /// [`LibraryStore::save_destination_feed`] refuses a save from an older
    /// one.
    pub revision: i64,
}

impl DestinationFeed {
    /// A destination never followed before: no feed read, no listing.
    #[must_use]
    pub fn new(destination: &str) -> Self {
        Self {
            destination: destination.to_owned(),
            seq: 0,
            feed_state: FeedState::Off,
            feed_cursor: None,
            feed_error: None,
            feed_checked_at: None,
            listing_seq: None,
            listing_cursor: None,
            listing_started_at: None,
            listed_at: None,
            next_listing_at: None,
            listing_error: None,
            objects: 0,
            checked_seq: 0,
            listing_base: None,
            revision: 0,
        }
    }

    /// Takes the next change sequence number.
    pub fn next_seq(&mut self) -> i64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    /// Whether a listing should run at `now`: one is in progress, none has
    /// finished yet, the feed is not healthy while a feed is in use, or the
    /// scheduled one is due.
    #[must_use]
    pub fn listing_due(&self, now: DateTime<Utc>) -> bool {
        if self.listing_seq.is_some() || self.listed_at.is_none() {
            return true;
        }
        match self.feed_state {
            FeedState::Gap | FeedState::Failing => true,
            FeedState::Off | FeedState::Healthy => {
                self.next_listing_at.is_none_or(|due| due <= now)
            }
        }
    }

    /// The feed a stored row describes; `None` for a state this build does
    /// not know.
    fn from_row(row: destination_feeds::Model) -> Option<Self> {
        Some(Self {
            feed_state: FeedState::parse(&row.feed_state)?,
            destination: row.destination,
            seq: row.seq,
            feed_cursor: row.feed_cursor,
            feed_error: row.feed_error,
            feed_checked_at: row.feed_checked_at.as_deref().and_then(parse_time),
            listing_seq: row.listing_seq,
            listing_cursor: row.listing_cursor,
            listing_started_at: row.listing_started_at.as_deref().and_then(parse_time),
            listed_at: row.listed_at.as_deref().and_then(parse_time),
            next_listing_at: row.next_listing_at.as_deref().and_then(parse_time),
            listing_error: row.listing_error,
            objects: u64::try_from(row.objects).unwrap_or(0),
            checked_seq: row.checked_seq,
            listing_base: row.listing_base.and_then(|base| u64::try_from(base).ok()),
            revision: row.revision,
        })
    }
}

/// One object as a listing or a change feed reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenObject {
    /// The object's key at the destination.
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// The backend's opaque change signature, else its entity tag; `None`
    /// when it gave neither, and then a change of size alone counts as a
    /// change.
    pub signature: Option<String>,
    /// Last modification time in UTC unix milliseconds, when known.
    pub modified_at_ms: Option<u64>,
}

/// One object whose row changed after a reader's last sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedObject {
    /// The object's key at the destination.
    pub key: String,
    /// The change sequence that changed the row.
    pub changed_seq: i64,
    /// True when the object is gone.
    pub removed: bool,
    /// Size in bytes when last seen.
    pub size: u64,
}

/// What [`LibraryStore::lose_copies`] did with the stored copies whose file
/// is gone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LostCopies {
    /// Copies that turned pending for the copy retry to write again
    /// (`output.missing_files = restore`).
    pub restoring: u64,
    /// Copies marked missing (`output.missing_files = mark_missing`).
    pub missing: u64,
    /// Editions that turned not acquired because no destination holds a
    /// stored copy of them any more.
    pub not_acquired: u64,
}

/// `at` in the stored form: RFC 3339 in UTC to the second.
fn stored_time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A stored time; `None` when the text is not RFC 3339.
fn parse_time(stored: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(stored)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// `value` as a stored integer, saturating at `i64::MAX`.
fn stored_int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl LibraryStore {
    /// Keys per `IN (...)` list on this store's database plugin, leaving
    /// room for `fixed` other binds.
    fn object_keys_per_statement(&self, fixed: usize) -> usize {
        usize::try_from(self.db_capabilities().max_binds)
            .unwrap_or(usize::MAX)
            .saturating_sub(fixed)
            .clamp(1, MAX_KEYS_PER_STATEMENT)
    }

    /// How `destination`'s outside changes are followed; `None` before its
    /// first sync pass (or for a stored state this build does not know).
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn destination_feed(&self, destination: &str) -> Result<Option<DestinationFeed>> {
        let row = destination_feeds::Entity::find_by_id(destination.to_owned())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.and_then(DestinationFeed::from_row))
    }

    /// Every destination's feed row, ordered by destination.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn destination_feeds(&self) -> Result<Vec<DestinationFeed>> {
        let rows = destination_feeds::Entity::find()
            .order_by_asc(destination_feeds::Column::Destination)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .filter_map(DestinationFeed::from_row)
            .collect())
    }

    /// Writes `feed`, replacing the destination's row, and moves
    /// `feed.revision` to the revision written.
    ///
    /// The write is a compare-and-swap on the revision `feed` was read at:
    /// a first save inserts the row, and a later one updates it only while
    /// nobody else saved since. A sync pass whose lease another node took
    /// therefore fails at its next save instead of overwriting the new
    /// holder's progress.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Conflict`] when the row changed since
    /// `feed` was read, or the error of a failed write.
    pub async fn save_destination_feed(&self, feed: &mut DestinationFeed) -> Result<()> {
        let next = feed.revision.saturating_add(1);
        let now = stored_time(Utc::now());
        let written = if feed.revision == 0 {
            let row = destination_feeds::ActiveModel {
                destination: Set(feed.destination.clone()),
                seq: Set(feed.seq),
                feed_state: Set(feed.feed_state.as_str().to_owned()),
                feed_cursor: Set(feed.feed_cursor.clone()),
                feed_error: Set(feed.feed_error.clone()),
                feed_checked_at: Set(feed.feed_checked_at.map(stored_time)),
                listing_seq: Set(feed.listing_seq),
                listing_cursor: Set(feed.listing_cursor.clone()),
                listing_started_at: Set(feed.listing_started_at.map(stored_time)),
                listed_at: Set(feed.listed_at.map(stored_time)),
                next_listing_at: Set(feed.next_listing_at.map(stored_time)),
                listing_error: Set(feed.listing_error.clone()),
                objects: Set(stored_int(feed.objects)),
                checked_seq: Set(feed.checked_seq),
                listing_base: Set(feed.listing_base.map(stored_int)),
                revision: Set(next),
                updated_at: Set(now),
            };
            destination_feeds::Entity::insert(row)
                .on_conflict(
                    OnConflict::column(destination_feeds::Column::Destination)
                        .do_nothing()
                        .to_owned(),
                )
                .exec_without_returning(self.db())
                .await
                .map_err(LibraryError::Orm)?
        } else {
            destination_feeds::Entity::update_many()
                .col_expr(destination_feeds::Column::Seq, Expr::value(feed.seq))
                .col_expr(
                    destination_feeds::Column::FeedState,
                    Expr::value(feed.feed_state.as_str()),
                )
                .col_expr(
                    destination_feeds::Column::FeedCursor,
                    Expr::value(feed.feed_cursor.clone()),
                )
                .col_expr(
                    destination_feeds::Column::FeedError,
                    Expr::value(feed.feed_error.clone()),
                )
                .col_expr(
                    destination_feeds::Column::FeedCheckedAt,
                    Expr::value(feed.feed_checked_at.map(stored_time)),
                )
                .col_expr(
                    destination_feeds::Column::ListingSeq,
                    Expr::value(feed.listing_seq),
                )
                .col_expr(
                    destination_feeds::Column::ListingCursor,
                    Expr::value(feed.listing_cursor.clone()),
                )
                .col_expr(
                    destination_feeds::Column::ListingStartedAt,
                    Expr::value(feed.listing_started_at.map(stored_time)),
                )
                .col_expr(
                    destination_feeds::Column::ListedAt,
                    Expr::value(feed.listed_at.map(stored_time)),
                )
                .col_expr(
                    destination_feeds::Column::NextListingAt,
                    Expr::value(feed.next_listing_at.map(stored_time)),
                )
                .col_expr(
                    destination_feeds::Column::ListingError,
                    Expr::value(feed.listing_error.clone()),
                )
                .col_expr(
                    destination_feeds::Column::Objects,
                    Expr::value(stored_int(feed.objects)),
                )
                .col_expr(
                    destination_feeds::Column::CheckedSeq,
                    Expr::value(feed.checked_seq),
                )
                .col_expr(
                    destination_feeds::Column::ListingBase,
                    Expr::value(feed.listing_base.map(stored_int)),
                )
                .col_expr(destination_feeds::Column::Revision, Expr::value(next))
                .col_expr(destination_feeds::Column::UpdatedAt, Expr::value(now))
                .filter(destination_feeds::Column::Destination.eq(feed.destination.as_str()))
                .filter(destination_feeds::Column::Revision.eq(feed.revision))
                .exec(self.db())
                .await
                .map_err(LibraryError::Orm)?
                .rows_affected
        };
        if written == 0 {
            return Err(LibraryError::Conflict(format!(
                "destination {} sync state changed since it was read",
                feed.destination
            )));
        }
        feed.revision = next;
        Ok(())
    }

    /// Records that `destination` holds `objects`, as a listing page or a
    /// feed page numbered `seq` reported them.
    ///
    /// Each object is marked seen at `seq`. One that is new, was removed,
    /// or whose size or signature differs from its row is recorded as
    /// changed at `seq`; an unchanged one keeps its `changed_seq`. A row a
    /// later page or listing already wrote is left as it is.
    ///
    /// # Returns
    ///
    /// How many objects changed.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a write fails.
    pub async fn record_seen_objects(
        &self,
        destination: &str,
        seq: i64,
        objects: &[SeenObject],
        now: DateTime<Utc>,
    ) -> Result<u64> {
        let mut changed_count = 0;
        for chunk in objects.chunks(self.object_keys_per_statement(3)) {
            let known: HashMap<String, destination_objects::Model> =
                destination_objects::Entity::find()
                    .filter(destination_objects::Column::Destination.eq(destination))
                    .filter(
                        destination_objects::Column::ObjectKey
                            .is_in(chunk.iter().map(|object| object.key.clone())),
                    )
                    .all(self.db())
                    .await
                    .map_err(LibraryError::Orm)?
                    .into_iter()
                    .map(|row| (row.object_key.clone(), row))
                    .collect();
            let mut unchanged = Vec::new();
            let mut changed = Vec::new();
            for object in chunk {
                // A row a later page or listing already wrote is newer than
                // what this one saw (a node whose lease lapsed finishing
                // late); it is left as it is.
                if known
                    .get(&object.key)
                    .is_some_and(|row| row.changed_seq > seq || row.listed_seq > seq)
                {
                    continue;
                }
                let same = known.get(&object.key).is_some_and(|row| {
                    row.removed == 0
                        && row.size == stored_int(object.size)
                        && row.signature == object.signature
                });
                if same {
                    unchanged.push(object.key.clone());
                } else {
                    changed.push(object);
                }
            }
            if !unchanged.is_empty() {
                destination_objects::Entity::update_many()
                    .col_expr(destination_objects::Column::ListedSeq, Expr::value(seq))
                    .filter(destination_objects::Column::Destination.eq(destination))
                    .filter(destination_objects::Column::ListedSeq.lte(seq))
                    .filter(destination_objects::Column::ObjectKey.is_in(unchanged))
                    .exec(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
            }
            changed_count += u64::try_from(changed.len()).unwrap_or(u64::MAX);
            let rows_per_insert = (self.object_keys_per_statement(0) / OBJECT_COLUMNS).max(1);
            for rows in changed.chunks(rows_per_insert) {
                let models = rows.iter().map(|object| destination_objects::ActiveModel {
                    destination: Set(destination.to_owned()),
                    object_key: Set(object.key.clone()),
                    size: Set(stored_int(object.size)),
                    signature: Set(object.signature.clone()),
                    modified_at_ms: Set(object.modified_at_ms.map(stored_int)),
                    listed_seq: Set(seq),
                    changed_seq: Set(seq),
                    removed: Set(0),
                    changed_at: Set(stored_time(now)),
                });
                destination_objects::Entity::insert_many(models)
                    .on_conflict(
                        OnConflict::columns([
                            destination_objects::Column::Destination,
                            destination_objects::Column::ObjectKey,
                        ])
                        .update_columns([
                            destination_objects::Column::Size,
                            destination_objects::Column::Signature,
                            destination_objects::Column::ModifiedAtMs,
                            destination_objects::Column::ListedSeq,
                            destination_objects::Column::ChangedSeq,
                            destination_objects::Column::Removed,
                            destination_objects::Column::ChangedAt,
                        ])
                        .to_owned(),
                    )
                    .exec_without_returning(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
            }
        }
        Ok(changed_count)
    }

    /// Records that the objects at `keys` are gone from `destination`, as
    /// a feed page numbered `seq` reported. A key with no row, already
    /// removed, or written by a later page or listing is left alone.
    ///
    /// # Returns
    ///
    /// How many objects were marked removed.
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    pub async fn remove_seen_objects(
        &self,
        destination: &str,
        seq: i64,
        keys: &[String],
        now: DateTime<Utc>,
    ) -> Result<u64> {
        let mut removed = 0;
        for chunk in keys.chunks(self.object_keys_per_statement(8)) {
            let result = destination_objects::Entity::update_many()
                .col_expr(destination_objects::Column::Removed, Expr::value(1))
                .col_expr(destination_objects::Column::ChangedSeq, Expr::value(seq))
                .col_expr(destination_objects::Column::ListedSeq, Expr::value(seq))
                .col_expr(
                    destination_objects::Column::ChangedAt,
                    Expr::value(stored_time(now)),
                )
                .filter(destination_objects::Column::Destination.eq(destination))
                .filter(destination_objects::Column::Removed.eq(0))
                // Never over a row a later page or listing wrote.
                .filter(destination_objects::Column::ChangedSeq.lte(seq))
                .filter(destination_objects::Column::ListedSeq.lte(seq))
                .filter(destination_objects::Column::ObjectKey.is_in(chunk.iter().cloned()))
                .exec(self.db())
                .await
                .map_err(LibraryError::Orm)?;
            removed += result.rows_affected;
        }
        Ok(removed)
    }

    /// Objects `destination` is known to hold that the listing numbered
    /// `listing_seq` has not seen: the ones [`Self::finish_listing`] would
    /// mark removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn unlisted_object_count(&self, destination: &str, listing_seq: i64) -> Result<u64> {
        destination_objects::Entity::find()
            .filter(destination_objects::Column::Destination.eq(destination))
            .filter(destination_objects::Column::Removed.eq(0))
            .filter(destination_objects::Column::ListedSeq.lt(listing_seq))
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)
    }

    /// Objects `destination` is known to hold now.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn known_object_count(&self, destination: &str) -> Result<u64> {
        destination_objects::Entity::find()
            .filter(destination_objects::Column::Destination.eq(destination))
            .filter(destination_objects::Column::Removed.eq(0))
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)
    }

    /// Ends the listing numbered `listing_seq` of `destination`: every
    /// object it did not see is marked removed at `listing_seq`.
    ///
    /// # Returns
    ///
    /// How many objects were marked removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn finish_listing(
        &self,
        destination: &str,
        listing_seq: i64,
        now: DateTime<Utc>,
    ) -> Result<u64> {
        let result = destination_objects::Entity::update_many()
            .col_expr(destination_objects::Column::Removed, Expr::value(1))
            .col_expr(
                destination_objects::Column::ChangedSeq,
                Expr::value(listing_seq),
            )
            .col_expr(
                destination_objects::Column::ChangedAt,
                Expr::value(stored_time(now)),
            )
            .filter(destination_objects::Column::Destination.eq(destination))
            .filter(destination_objects::Column::Removed.eq(0))
            .filter(destination_objects::Column::ListedSeq.lt(listing_seq))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }

    /// Marks every object `destination` holds as seen by the listing
    /// numbered `listing_seq`, without recording a change: used when a
    /// listing's removals are held back, so the next listing compares
    /// against the same set.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn keep_unlisted_objects(&self, destination: &str, listing_seq: i64) -> Result<()> {
        destination_objects::Entity::update_many()
            .col_expr(
                destination_objects::Column::ListedSeq,
                Expr::value(listing_seq),
            )
            .filter(destination_objects::Column::Destination.eq(destination))
            .filter(destination_objects::Column::Removed.eq(0))
            .filter(destination_objects::Column::ListedSeq.lt(listing_seq))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Up to `limit` objects of `destination` whose rows changed after
    /// `after`: a change sequence, or a sequence and the last key read in
    /// it to continue a page. Ordered by sequence, then key.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn changed_objects(
        &self,
        destination: &str,
        after: (i64, Option<&str>),
        limit: u64,
    ) -> Result<Vec<ChangedObject>> {
        let (seq, key) = after;
        let position = match key {
            Some(key) => Condition::any()
                .add(destination_objects::Column::ChangedSeq.gt(seq))
                .add(
                    Condition::all()
                        .add(destination_objects::Column::ChangedSeq.eq(seq))
                        .add(destination_objects::Column::ObjectKey.gt(key)),
                ),
            None => Condition::all().add(destination_objects::Column::ChangedSeq.gt(seq)),
        };
        let rows = destination_objects::Entity::find()
            .filter(destination_objects::Column::Destination.eq(destination))
            .filter(position)
            .order_by_asc(destination_objects::Column::ChangedSeq)
            .order_by_asc(destination_objects::Column::ObjectKey)
            .limit(limit)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .map(|row| ChangedObject {
                key: row.object_key,
                changed_seq: row.changed_seq,
                removed: row.removed != 0,
                size: u64::try_from(row.size).unwrap_or(0),
            })
            .collect())
    }

    /// Deletes tombstones of objects removed before `before`.
    ///
    /// # Returns
    ///
    /// How many tombstones were deleted.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn prune_object_tombstones(&self, before: DateTime<Utc>) -> Result<u64> {
        let result = destination_objects::Entity::delete_many()
            .filter(destination_objects::Column::Removed.eq(1))
            .filter(destination_objects::Column::ChangedAt.lt(stored_time(before)))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }

    /// Records that the files at `keys` are gone from `destination`.
    ///
    /// Every stored copy whose primary audio key is one of them is looked
    /// at. When another audio file of its edition is recorded at the
    /// destination (`edition_files`) and the destination still holds it
    /// (the file was moved or renamed), the copy now names that key.
    /// Otherwise `action` decides:
    ///
    /// - [`MissingFileAction::Restore`]: the copy turns pending, due at
    ///   `now`, so the next copy retry writes it again.
    /// - [`MissingFileAction::MarkMissing`]: the copy turns missing and is
    ///   left alone. When no destination holds a stored copy of the edition
    ///   any more, the edition is no longer acquired (its title's acquire
    ///   status turns not acquired), so a member can ask for it again.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a write fails.
    pub async fn lose_copies(
        &self,
        destination: &str,
        keys: &[String],
        action: MissingFileAction,
        now: DateTime<Utc>,
    ) -> Result<LostCopies> {
        let mut lost = LostCopies::default();
        for chunk in keys.chunks(self.object_keys_per_statement(2)) {
            let copies = edition_copies::Entity::find()
                .filter(edition_copies::Column::Destination.eq(destination))
                .filter(edition_copies::Column::State.eq(CopyState::Stored.as_str()))
                .filter(edition_copies::Column::StorageKey.is_in(chunk.iter().cloned()))
                .all(self.db())
                .await
                .map_err(LibraryError::Orm)?;
            for copy in copies {
                let edition = copy.edition_id.clone();
                let moved_to = self
                    .present_audio_file(destination, &copy.edition_id, copy.storage_key.as_deref())
                    .await?;
                let mut release_after = None;
                let mut row: edition_copies::ActiveModel = copy.into();
                row.updated_at = Set(stored_time(now));
                row.last_error = Set(Some("the file is no longer at the destination".to_owned()));
                match (moved_to, action) {
                    (Some(key), _) => {
                        row.storage_key = Set(Some(key));
                        row.last_error = Set(None);
                    }
                    (None, MissingFileAction::Restore) => {
                        row.state = Set(CopyState::Pending.as_str().to_owned());
                        row.next_attempt_at = Set(Some(stored_time(now)));
                        lost.restoring += 1;
                    }
                    (None, MissingFileAction::MarkMissing) => {
                        // The edition is released before its copy turns
                        // missing: if either write fails, the copy is still
                        // stored and the next pass retries both.
                        if self
                            .release_unstored_edition(&edition, Some(destination))
                            .await?
                        {
                            lost.not_acquired += 1;
                        }
                        row.state = Set(CopyState::Missing.as_str().to_owned());
                        row.next_attempt_at = Set(None);
                        lost.missing += 1;
                        release_after = Some(edition);
                    }
                }
                edition_copies::Entity::update(row)
                    .exec(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
                // Checked again once the copy is missing: another node
                // syncing another destination may have lost the edition's
                // other stored copy since the check above. Whichever of the
                // two checks last sees no stored copy left and releases it.
                if let Some(edition) = release_after {
                    if self
                        .release_unstored_edition(&edition, Some(destination))
                        .await?
                    {
                        lost.not_acquired += 1;
                    }
                }
            }
        }
        Ok(lost)
    }

    /// Releases every edition with a missing copy at `destination` that is
    /// still acquired although no destination holds a stored copy of it:
    /// the state a failed release in [`Self::lose_copies`] leaves behind,
    /// since the copy is already missing and no later loss names it.
    ///
    /// # Returns
    ///
    /// How many editions turned not acquired.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a write fails.
    pub async fn release_missing_editions(&self, destination: &str) -> Result<u64> {
        let mut editions: Vec<String> = edition_copies::Entity::find()
            .filter(edition_copies::Column::Destination.eq(destination))
            .filter(edition_copies::Column::State.eq(CopyState::Missing.as_str()))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|copy| copy.edition_id)
            .collect();
        editions.sort();
        editions.dedup();
        let mut released = 0;
        for edition in editions {
            // No destination is set aside here: a copy at `destination`
            // stored again since it was read (a forced acquire) keeps the
            // edition acquired.
            if self.release_unstored_edition(&edition, None).await? {
                released += 1;
            }
        }
        Ok(released)
    }

    /// Marks `edition_id` not acquired when no destination (other than
    /// `losing`, when given) holds a stored copy of it: the title whose acquire recorded it (its
    /// `edition_acquisitions.holding_id`) turns not acquired.
    ///
    /// # Returns
    ///
    /// True when the edition was acquired and now is not.
    async fn release_unstored_edition(
        &self,
        edition_id: &str,
        losing: Option<&str>,
    ) -> Result<bool> {
        let edition = edition_id.to_owned();
        let copies = self
            .copies_of_editions(std::slice::from_ref(&edition))
            .await?;
        if copies.iter().any(|copy| {
            copy.state == CopyState::Stored && Some(copy.destination.as_str()) != losing
        }) {
            return Ok(false);
        }
        let Some(acquisition) = self.edition_acquisition(edition_id).await? else {
            return Ok(false);
        };
        if acquisition.status != AcquireStatus::Acquired {
            return Ok(false);
        }
        let Some(title) = self
            .acquire_title_of_holding(&acquisition.holding_id)
            .await?
        else {
            return Ok(false);
        };
        let title_id = title.book_uuid.as_deref().unwrap_or(&title.product_id);
        self.set_acquire_status(
            title_id,
            &title.account_id,
            AcquireStatus::NotAcquired,
            None,
            None,
        )
        .await?;
        Ok(true)
    }

    /// The primary audio keys of `destination`'s stored copies that have no
    /// present object row there: files a finished listing did not see
    /// (including ones that were gone before the destination was first
    /// listed, which leave no tombstone), in key order.
    ///
    /// # Returns
    ///
    /// The missing keys, and how many distinct keys the destination's stored
    /// copies name.
    ///
    /// # Errors
    ///
    /// Returns an error when a read fails.
    pub async fn unlisted_copy_keys(&self, destination: &str) -> Result<(Vec<String>, u64)> {
        let mut keys: Vec<String> = edition_copies::Entity::find()
            .filter(edition_copies::Column::Destination.eq(destination))
            .filter(edition_copies::Column::State.eq(CopyState::Stored.as_str()))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .filter_map(|copy| copy.storage_key)
            .collect();
        keys.sort();
        keys.dedup();
        let stored = u64::try_from(keys.len()).unwrap_or(u64::MAX);
        let mut missing = Vec::new();
        for chunk in keys.chunks(self.object_keys_per_statement(2)) {
            let present: std::collections::HashSet<String> = destination_objects::Entity::find()
                .filter(destination_objects::Column::Destination.eq(destination))
                .filter(destination_objects::Column::Removed.eq(0))
                .filter(destination_objects::Column::ObjectKey.is_in(chunk.iter().cloned()))
                .all(self.db())
                .await
                .map_err(LibraryError::Orm)?
                .into_iter()
                .map(|row| row.object_key)
                .collect();
            missing.extend(chunk.iter().filter(|key| !present.contains(*key)).cloned());
        }
        Ok((missing, stored))
    }

    /// Another audio file of `edition_id` recorded at `destination` (not
    /// `gone`) that the destination still holds, by its last listing or
    /// feed page; `None` when there is none.
    async fn present_audio_file(
        &self,
        destination: &str,
        edition_id: &str,
        gone: Option<&str>,
    ) -> Result<Option<String>> {
        let files = edition_files::Entity::find()
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::OwnerKind.eq("edition"))
            .filter(edition_files::Column::OwnerId.eq(edition_id))
            .filter(edition_files::Column::Role.eq(FileRole::Audio.as_str()))
            .order_by_asc(edition_files::Column::StorageKey)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        for file in files {
            if Some(file.storage_key.as_str()) == gone {
                continue;
            }
            let present = destination_objects::Entity::find_by_id((
                destination.to_owned(),
                file.storage_key.clone(),
            ))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .is_some_and(|row| row.removed == 0);
            if present {
                return Ok(Some(file.storage_key));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
#[path = "destination_objects_tests.rs"]
mod tests;
