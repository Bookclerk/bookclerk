//! Each edition's copy at each output destination (`edition_copies`).
//!
//! An acquire writes to every destination in its target set at once and
//! succeeds as soon as one of them holds the complete files (storage
//! destinations ADR, `docs/adr/storage-destinations.md`, decision 3). Each
//! destination's outcome is recorded here: [`CopyState::Stored`] with the
//! edition's primary audio key there, or [`CopyState::Pending`] with the
//! failure and the time the next background attempt is due. A copy that
//! keeps failing turns [`CopyState::Failed`] after [`MAX_COPY_ATTEMPTS`]
//! and is left for the operator (a forced acquire tries it again).
//!
//! Acquire candidate selection reads these rows: an acquired edition is
//! visited again while a destination whose filter takes it has no stored
//! copy and is not waiting out a retry delay
//! ([`EditionCopy::wants_attempt`]). A destination with no row has never
//! been given the edition, so an edition acquired before the destination
//! existed, or before its filter took the edition, is copied there by the
//! next acquire run.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::LibraryStore;
use crate::entities::{edition_copies, edition_files};
use crate::error::{LibraryError, Result};
use crate::FileRole;

/// Failed attempts after which a pending copy turns [`CopyState::Failed`]
/// and is no longer retried in the background.
pub const MAX_COPY_ATTEMPTS: u32 = 8;

/// Delay before the first background retry of a failed copy; each later
/// failure doubles it, up to [`COPY_RETRY_MAX_DELAY`].
pub const COPY_RETRY_BASE_DELAY: Duration = Duration::from_secs(5 * 60);

/// The longest delay between two background retries of one copy.
pub const COPY_RETRY_MAX_DELAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Where an edition's copy at one destination stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CopyState {
    /// The destination holds the complete files.
    Stored,
    /// A write failed or the destination was unavailable; the copy is
    /// retried once its delay has passed.
    Pending,
    /// The copy failed [`MAX_COPY_ATTEMPTS`] times in a row and is no
    /// longer retried on its own.
    Failed,
}

impl CopyState {
    /// Every state, in stored order (the `edition_copies.state` CHECK list).
    pub const ALL: [Self; 3] = [Self::Stored, Self::Pending, Self::Failed];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Pending => "pending",
            Self::Failed => "failed",
        }
    }

    /// The state a stored spelling names; `None` for one this build does
    /// not know.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == stored)
    }
}

/// One edition's copy at one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionCopy {
    /// The edition (`editions.id`).
    pub edition_id: String,
    /// The destination's plugin instance id.
    pub destination: String,
    /// Where the copy stands.
    pub state: CopyState,
    /// The edition's primary audio key on the destination, once a write
    /// there was stored.
    pub storage_key: Option<String>,
    /// Failed writes since the copy was last stored.
    pub attempts: u32,
    /// When the next background attempt is due; `None` unless pending.
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// The last write failure; `None` once stored.
    pub last_error: Option<String>,
}

impl EditionCopy {
    /// Whether an acquire at `now` should write this copy: a pending copy
    /// whose delay has passed. A stored copy needs nothing, a failed one
    /// waits for the operator, and a pending one waits out its delay.
    #[must_use]
    pub fn wants_attempt(&self, now: DateTime<Utc>) -> bool {
        match self.state {
            CopyState::Stored | CopyState::Failed => false,
            CopyState::Pending => self.next_attempt_at.is_none_or(|due| due <= now),
        }
    }

    /// The copy a stored row describes; `None` for a state this build does
    /// not know.
    fn from_row(row: edition_copies::Model) -> Option<Self> {
        Some(Self {
            state: CopyState::parse(&row.state)?,
            attempts: u32::try_from(row.attempts).unwrap_or(u32::MAX),
            next_attempt_at: row.next_attempt_at.as_deref().and_then(parse_time),
            edition_id: row.edition_id,
            destination: row.destination,
            storage_key: row.storage_key,
            last_error: row.last_error,
        })
    }
}

/// The delay before the next background attempt of a copy that has now
/// failed `attempts` times (1 for the first failure):
/// [`COPY_RETRY_BASE_DELAY`] doubled per earlier failure, capped at
/// [`COPY_RETRY_MAX_DELAY`].
#[must_use]
pub fn copy_retry_delay(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(16);
    COPY_RETRY_BASE_DELAY
        .saturating_mul(1u32 << doublings)
        .min(COPY_RETRY_MAX_DELAY)
}

/// `at` in the stored form: RFC 3339 in UTC to the second, so stored times
/// order the same as text and as instants.
fn stored_time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A stored time; `None` when the text is not RFC 3339.
fn parse_time(stored: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(stored)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

impl LibraryStore {
    /// Records that `destination` holds the complete files of `edition_id`,
    /// with its primary audio at `storage_key` there. Clears any pending
    /// retry.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn set_copy_stored(
        &self,
        edition_id: &str,
        destination: &str,
        storage_key: &str,
    ) -> Result<()> {
        let row = edition_copies::ActiveModel {
            edition_id: Set(edition_id.to_owned()),
            destination: Set(destination.to_owned()),
            state: Set(CopyState::Stored.as_str().to_owned()),
            storage_key: Set(Some(storage_key.to_owned())),
            attempts: Set(0),
            next_attempt_at: Set(None),
            last_error: Set(None),
            updated_at: Set(stored_time(Utc::now())),
        };
        self.upsert_copy(row).await
    }

    /// Records that writing `edition_id` to `destination` failed with
    /// `error` at `now`. The copy turns pending with its next attempt due
    /// after [`copy_retry_delay`], or failed once it has failed
    /// [`MAX_COPY_ATTEMPTS`] times. A key an earlier stored copy recorded is
    /// kept.
    ///
    /// The attempt count is read, then written: the acquire job that writes
    /// an edition holds the node's work lock, so two writes of one copy do
    /// not race.
    ///
    /// # Returns
    ///
    /// The copy's state after the failure.
    ///
    /// # Errors
    ///
    /// Returns an error when the read or the write fails.
    pub async fn set_copy_pending(
        &self,
        edition_id: &str,
        destination: &str,
        error: &str,
        now: DateTime<Utc>,
    ) -> Result<CopyState> {
        let before =
            edition_copies::Entity::find_by_id((edition_id.to_owned(), destination.to_owned()))
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?;
        let (attempts, storage_key) = match before {
            Some(row) => (
                u32::try_from(row.attempts)
                    .unwrap_or(u32::MAX)
                    .saturating_add(1),
                row.storage_key,
            ),
            None => (1, None),
        };
        let (state, next_attempt_at) = if attempts >= MAX_COPY_ATTEMPTS {
            (CopyState::Failed, None)
        } else {
            let delay = chrono::Duration::from_std(copy_retry_delay(attempts))
                .unwrap_or(chrono::Duration::MAX);
            (CopyState::Pending, Some(stored_time(now + delay)))
        };
        let row = edition_copies::ActiveModel {
            edition_id: Set(edition_id.to_owned()),
            destination: Set(destination.to_owned()),
            state: Set(state.as_str().to_owned()),
            storage_key: Set(storage_key),
            attempts: Set(i64::from(attempts)),
            next_attempt_at: Set(next_attempt_at),
            last_error: Set(Some(error.to_owned())),
            updated_at: Set(stored_time(now)),
        };
        self.upsert_copy(row).await?;
        Ok(state)
    }

    /// Writes `row`, replacing the copy it names.
    async fn upsert_copy(&self, row: edition_copies::ActiveModel) -> Result<()> {
        edition_copies::Entity::insert(row)
            .on_conflict(
                OnConflict::columns([
                    edition_copies::Column::EditionId,
                    edition_copies::Column::Destination,
                ])
                .update_columns([
                    edition_copies::Column::State,
                    edition_copies::Column::StorageKey,
                    edition_copies::Column::Attempts,
                    edition_copies::Column::NextAttemptAt,
                    edition_copies::Column::LastError,
                    edition_copies::Column::UpdatedAt,
                ])
                .to_owned(),
            )
            .exec_without_returning(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Records a stored copy of `edition_id` on each destination where its
    /// primary audio file is recorded (`edition_files`, role `audio`) and
    /// no copy row exists yet.
    ///
    /// An edition acquired before copies were recorded has files but no
    /// copy rows; its audio rows say where the acquire wrote it. Call this
    /// only for an acquired edition: an acquire records each key just
    /// before writing it, so a row alone does not prove the write
    /// finished. Existing copy rows are never changed.
    ///
    /// # Returns
    ///
    /// How many copies were recorded.
    ///
    /// # Errors
    ///
    /// Returns an error when the read or a write fails.
    pub async fn record_copies_from_files(&self, edition_id: &str) -> Result<usize> {
        let files = edition_files::Entity::find()
            .filter(edition_files::Column::OwnerKind.eq("edition"))
            .filter(edition_files::Column::OwnerId.eq(edition_id))
            .filter(edition_files::Column::Role.eq(FileRole::Audio.as_str()))
            .order_by_asc(edition_files::Column::Destination)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        let now = stored_time(Utc::now());
        let mut recorded = 0;
        for file in files {
            let inserted = edition_copies::Entity::insert(edition_copies::ActiveModel {
                edition_id: Set(edition_id.to_owned()),
                destination: Set(file.destination),
                state: Set(CopyState::Stored.as_str().to_owned()),
                storage_key: Set(Some(file.storage_key)),
                attempts: Set(0),
                next_attempt_at: Set(None),
                last_error: Set(None),
                updated_at: Set(now.clone()),
            })
            .on_conflict(
                OnConflict::columns([
                    edition_copies::Column::EditionId,
                    edition_copies::Column::Destination,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(self.db())
            .await
            .map_err(LibraryError::Orm)?;
            if inserted > 0 {
                recorded += 1;
            }
        }
        Ok(recorded)
    }

    /// Every recorded copy of the editions in `edition_ids`, ordered by
    /// edition then destination. Rows in a state this build does not know
    /// are left out.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn copies_of_editions(&self, edition_ids: &[String]) -> Result<Vec<EditionCopy>> {
        let mut copies = Vec::new();
        // One page of acquire candidates at a time, so the bound list stays
        // well inside every adapter's bind limit.
        for chunk in edition_ids.chunks(COPIES_READ_CHUNK) {
            let rows = edition_copies::Entity::find()
                .filter(edition_copies::Column::EditionId.is_in(chunk.iter().cloned()))
                .order_by_asc(edition_copies::Column::EditionId)
                .order_by_asc(edition_copies::Column::Destination)
                .all(self.db())
                .await
                .map_err(LibraryError::Orm)?;
            copies.extend(rows.into_iter().filter_map(EditionCopy::from_row));
        }
        Ok(copies)
    }

    /// Up to `limit` editions with a pending copy on one of `destinations`
    /// whose next attempt is due at `now`, soonest due first, each named
    /// once. A copy on a destination not in `destinations` (one no longer
    /// deployed) is not due anywhere.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn due_copy_editions(
        &self,
        now: DateTime<Utc>,
        destinations: &[String],
        limit: u64,
    ) -> Result<Vec<String>> {
        if destinations.is_empty() {
            return Ok(Vec::new());
        }
        let rows = edition_copies::Entity::find()
            .filter(edition_copies::Column::State.eq(CopyState::Pending.as_str()))
            .filter(edition_copies::Column::NextAttemptAt.lte(stored_time(now)))
            .filter(edition_copies::Column::Destination.is_in(destinations.iter().cloned()))
            .order_by_asc(edition_copies::Column::NextAttemptAt)
            .order_by_asc(edition_copies::Column::EditionId)
            .limit(limit)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        let mut editions: Vec<String> = Vec::with_capacity(rows.len());
        for edition in rows.into_iter().map(|row| row.edition_id) {
            if !editions.contains(&edition) {
                editions.push(edition);
            }
        }
        Ok(editions)
    }

    /// Moves the due retries of `edition_id`'s pending copies on
    /// `destinations` to their next delay from `now`, without counting an
    /// attempt: called once an acquire for them is queued, so the next
    /// pass does not queue another while that one waits. The acquire's own
    /// outcome then records the copy as usual.
    ///
    /// # Errors
    ///
    /// Returns an error when the read or a write fails.
    pub async fn defer_copy_retries(
        &self,
        edition_id: &str,
        destinations: &[String],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let due = edition_copies::Entity::find()
            .filter(edition_copies::Column::EditionId.eq(edition_id))
            .filter(edition_copies::Column::State.eq(CopyState::Pending.as_str()))
            .filter(edition_copies::Column::NextAttemptAt.lte(stored_time(now)))
            .filter(edition_copies::Column::Destination.is_in(destinations.iter().cloned()))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        for row in due {
            let attempts = u32::try_from(row.attempts).unwrap_or(u32::MAX);
            let delay = chrono::Duration::from_std(copy_retry_delay(attempts.saturating_add(1)))
                .unwrap_or(chrono::Duration::MAX);
            edition_copies::Entity::update_many()
                .col_expr(
                    edition_copies::Column::NextAttemptAt,
                    sea_orm::sea_query::Expr::value(stored_time(now + delay)),
                )
                .filter(edition_copies::Column::EditionId.eq(edition_id))
                .filter(edition_copies::Column::Destination.eq(row.destination))
                .filter(edition_copies::Column::State.eq(CopyState::Pending.as_str()))
                .exec(self.db())
                .await
                .map_err(LibraryError::Orm)?;
        }
        Ok(())
    }
}

/// Editions read per statement by [`LibraryStore::copies_of_editions`].
const COPIES_READ_CHUNK: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{sqlite, Profile};

    #[test]
    fn retries_back_off_to_a_day() {
        assert_eq!(copy_retry_delay(1), COPY_RETRY_BASE_DELAY);
        assert_eq!(copy_retry_delay(2), COPY_RETRY_BASE_DELAY * 2);
        assert_eq!(copy_retry_delay(3), COPY_RETRY_BASE_DELAY * 4);
        assert_eq!(copy_retry_delay(40), COPY_RETRY_MAX_DELAY);
        assert_eq!(copy_retry_delay(0), COPY_RETRY_BASE_DELAY);
    }

    #[test]
    fn states_round_trip_and_say_when_to_write() {
        for state in CopyState::ALL {
            assert_eq!(CopyState::parse(state.as_str()), Some(state));
        }
        assert_eq!(CopyState::parse("lost"), None);
        let now = Utc::now();
        let copy = |state, next_attempt_at| EditionCopy {
            edition_id: "e".into(),
            destination: "d".into(),
            state,
            storage_key: None,
            attempts: 1,
            next_attempt_at,
            last_error: None,
        };
        assert!(!copy(CopyState::Stored, None).wants_attempt(now));
        assert!(!copy(CopyState::Failed, None).wants_attempt(now));
        assert!(copy(CopyState::Pending, None).wants_attempt(now));
        assert!(copy(CopyState::Pending, Some(now)).wants_attempt(now));
        assert!(
            !copy(CopyState::Pending, Some(now + chrono::Duration::seconds(1))).wants_attempt(now)
        );
    }

    /// An edition `store` holds, created through a minimal title.
    async fn edition(store: &LibraryStore, product_id: &str) -> String {
        store
            .upsert_account("audible", "user-1", "us", None, true, "audible")
            .await
            .unwrap();
        let book = crate::NewBook::minimal(product_id, "user-1", "us", "Title");
        store.upsert_book(&book).await.unwrap();
        store
            .acquire_titles_named(None, product_id)
            .await
            .unwrap()
            .remove(0)
            .edition_id
    }

    /// A copy is stored, fails into pending with a growing delay, keeps its
    /// stored key, turns failed at the limit, and is stored again; due
    /// editions are listed once each. Runs under every database test
    /// profile.
    #[tokio::test]
    async fn a_copy_moves_between_stored_pending_and_failed() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store().clone();
            let one = edition(&store, "B00ONE0001").await;
            let two = edition(&store, "B00TWO0002").await;
            let now = Utc::now();
            let dests = vec!["local".to_owned(), "s3".to_owned()];

            store
                .set_copy_stored(&one, "local", "A/One.m4b")
                .await
                .unwrap();
            let first = store
                .set_copy_pending(&one, "s3", "bucket offline", now)
                .await
                .unwrap();
            assert_eq!(first, CopyState::Pending);
            assert_eq!(
                store
                    .set_copy_pending(&one, "local", "disk full", now)
                    .await
                    .unwrap(),
                CopyState::Pending
            );

            let copies = store
                .copies_of_editions(&[one.clone(), two.clone()])
                .await
                .unwrap();
            assert_eq!(copies.len(), 2, "{profile:?}");
            let local = &copies[0];
            assert_eq!(local.destination, "local");
            assert_eq!(local.state, CopyState::Pending);
            assert_eq!(
                local.storage_key.as_deref(),
                Some("A/One.m4b"),
                "the stored key is kept"
            );
            let s3 = &copies[1];
            assert_eq!(s3.attempts, 1);
            assert_eq!(s3.last_error.as_deref(), Some("bucket offline"));
            assert!(!s3.wants_attempt(now));
            assert!(
                s3.wants_attempt(now + chrono::Duration::from_std(COPY_RETRY_BASE_DELAY).unwrap())
            );

            assert!(store
                .due_copy_editions(now, &dests, 10)
                .await
                .unwrap()
                .is_empty());
            let later = now + chrono::Duration::days(2);
            assert_eq!(
                store.due_copy_editions(later, &dests, 10).await.unwrap(),
                vec![one.clone()]
            );
            let only_s3 = vec!["s3".to_owned()];
            assert_eq!(
                store.due_copy_editions(later, &only_s3, 10).await.unwrap(),
                vec![one.clone()]
            );
            assert!(store
                .due_copy_editions(later, &["nas".to_owned()], 10)
                .await
                .unwrap()
                .is_empty());
            // Once an acquire is queued for them, the retries are not due
            // again until another delay has passed, and no attempt is
            // counted.
            store
                .defer_copy_retries(&one, &only_s3, later)
                .await
                .unwrap();
            assert!(store
                .due_copy_editions(later, &only_s3, 10)
                .await
                .unwrap()
                .is_empty());
            let deferred = store
                .copies_of_editions(std::slice::from_ref(&one))
                .await
                .unwrap();
            assert_eq!(
                deferred
                    .iter()
                    .find(|c| c.destination == "s3")
                    .unwrap()
                    .attempts,
                1
            );

            for _ in 1..MAX_COPY_ATTEMPTS {
                store
                    .set_copy_pending(&one, "s3", "bucket offline", now)
                    .await
                    .unwrap();
            }
            let copies = store
                .copies_of_editions(std::slice::from_ref(&one))
                .await
                .unwrap();
            let s3 = copies.iter().find(|c| c.destination == "s3").unwrap();
            assert_eq!(s3.state, CopyState::Failed, "{profile:?}");
            assert_eq!(s3.next_attempt_at, None);

            store
                .set_copy_stored(&one, "s3", "library/one.m4b")
                .await
                .unwrap();
            let copies = store
                .copies_of_editions(std::slice::from_ref(&one))
                .await
                .unwrap();
            let s3 = copies.iter().find(|c| c.destination == "s3").unwrap();
            assert_eq!(s3.state, CopyState::Stored);
            assert_eq!(s3.attempts, 0);
            assert_eq!(s3.last_error, None);
            assert_eq!(
                kit.refused_begins(),
                0,
                "{profile:?} took an interactive transaction"
            );
        }
    }

    /// An edition with audio file rows and no copies gets a stored copy per
    /// destination its audio is on; existing copies are kept. Runs under
    /// every database test profile.
    #[tokio::test]
    async fn copies_are_recorded_from_an_editions_audio_files() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store().clone();
            let one = edition(&store, "B00ONE0001").await;
            let owner = crate::FileOwner::Edition(one.clone());
            for (destination, key, role) in [
                ("local", "A/One.m4b", FileRole::Audio),
                ("local", "A/One.jpg", FileRole::Cover),
                ("s3", "lib/one.m4b", FileRole::Audio),
                ("nas", "One/One.m4b", FileRole::Audio),
            ] {
                assert!(store
                    .record_edition_file(destination, key, &owner, role)
                    .await
                    .unwrap());
            }
            let now = Utc::now();
            store
                .set_copy_pending(&one, "nas", "offline", now)
                .await
                .unwrap();

            assert_eq!(
                store.record_copies_from_files(&one).await.unwrap(),
                2,
                "{profile:?}"
            );
            assert_eq!(store.record_copies_from_files(&one).await.unwrap(), 0);
            let copies = store
                .copies_of_editions(std::slice::from_ref(&one))
                .await
                .unwrap();
            let summary: Vec<(&str, CopyState, Option<&str>)> = copies
                .iter()
                .map(|c| (c.destination.as_str(), c.state, c.storage_key.as_deref()))
                .collect();
            assert_eq!(
                summary,
                vec![
                    ("local", CopyState::Stored, Some("A/One.m4b")),
                    ("nas", CopyState::Pending, None),
                    ("s3", CopyState::Stored, Some("lib/one.m4b")),
                ]
            );
            assert_eq!(kit.refused_begins(), 0, "{profile:?}");
        }
    }
}
