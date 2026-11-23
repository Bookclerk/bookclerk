//! On a branch: the record of files a host job copies to this install's
//! own destinations for a library it joined (#314).
//!
//! A host job that keeps a copy of a title (the branch owner's own books,
//! or every member's when the link keeps member copies) writes it to this
//! install's destinations as well as streaming it to the library. Each key
//! is claimed here first ([`claim_link_copy`]): a key another title holds,
//! whether another title's copy or a file this install's own library
//! recorded for another edition, is refused, so a copy never overwrites
//! another title's file. Once the library commits the title, the attempt's
//! keys are kept ([`keep_link_copies`]); when it does not, the caller
//! deletes what it created and gives the keys back ([`release_link_copies`]).
//!
//! The title is named by its call number on the library, which names one
//! edition, so two libraries the branch joined that send the same edition
//! share its copies; a title sent without one is named by its holding
//! there, and only that library's attempts share it.
//!
//! The writer names the node, the process incarnation and the attempt, so a
//! copy whose daemon stopped mid-title is found on the next start
//! ([`stale_link_copies`]) and removed: nothing else would ever learn it
//! was not committed.

use chrono::{DateTime, Duration, Utc};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait as _, EntityTrait as _, QueryFilter as _, QueryOrder as _,
    QuerySelect as _,
};

use crate::call_number;
use crate::entities::link_copy_files;
use crate::error::{LibraryError, Result};
use crate::store::{FileOwner, LibraryStore};

/// Record state: the attempt is still copying.
const STATE_COPYING: &str = "copying";
/// Record state: the library committed the title; the copy stays.
const STATE_KEPT: &str = "kept";

/// How long a `copying` record of an earlier process waits before
/// [`stale_link_copies`] answers it: two processes can share a node id for
/// a moment during an overlapping restart, and the older one may still be
/// finishing its copy.
pub const STALE_LINK_COPY_AGE_SECS: i64 = 60 * 60;

/// How many stale records one [`stale_link_copies`] call answers.
const STALE_LINK_COPY_BATCH: u64 = 128;

/// One copy attempt of one title: what its claims are recorded under.
#[derive(Debug, Clone, Copy)]
pub struct LinkCopyAttempt<'a> {
    /// The library the title was acquired for.
    pub library_id: &'a str,
    /// The title: its call number on that library, else its holding id
    /// there ([`link_copy_title_ref`]).
    pub title_ref: &'a str,
    /// This attempt's writer ([`link_copy_writer`]).
    pub writer: &'a str,
}

/// Whether a key held a file before an attempt wrote its copy there.
///
/// Only a key the attempt finds empty is its own to delete when the title
/// is not committed; a file already there (an earlier copy of the title,
/// say, recorded before this ledger or by an attempt that took it over)
/// is overwritten by the copy and stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyTarget {
    /// No file was at the key: the attempt's copy is the only one, and
    /// goes when the title is not committed.
    New,
    /// A file was already at the key: it stays, and its record is kept
    /// again, when the title is not committed.
    Existing,
}

impl CopyTarget {
    /// The `replaced` column's value: 1 for [`Self::Existing`].
    fn column(self) -> i64 {
        match self {
            Self::New => 0,
            Self::Existing => 1,
        }
    }

    /// The target the `replaced` column records.
    fn from_column(replaced: i64) -> Self {
        if replaced == 0 {
            Self::New
        } else {
            Self::Existing
        }
    }
}

/// What [`claim_link_copy`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCopyClaim {
    /// The key was unrecorded (or another attempt of the same title was
    /// still copying it); the attempt holds it now.
    Fresh,
    /// The key held a kept copy of the same title (from any library that
    /// sent the same edition), which the attempt took over: a failed
    /// attempt leaves that file and its record kept.
    Replaced,
    /// Another title holds the key; nothing was written.
    HeldByAnotherTitle,
}

/// A copy an earlier process left mid-title ([`stale_link_copies`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleLinkCopy {
    /// This install's destination id.
    pub destination: String,
    /// The storage key there.
    pub key: String,
    /// The attempt that wrote it.
    pub writer: String,
    /// Whether a file was at the key before the attempt: an
    /// [`CopyTarget::Existing`] one is left and its record kept again
    /// ([`settle_stale_link_copy`]), a [`CopyTarget::New`] one deleted.
    pub target: CopyTarget,
}

/// The title reference a copy is recorded under: the title's call number
/// on the library (`call_number`, when it sent one), which names one
/// edition there, else its holding id there.
#[must_use]
pub fn link_copy_title_ref(call_number: Option<&str>, holding_id: &str) -> String {
    match call_number.filter(|number| !number.is_empty()) {
        Some(number) => format!("cn:{number}"),
        None => format!("holding:{holding_id}"),
    }
}

/// A new attempt's writer: `<node id>/<process incarnation>/<attempt id>`.
#[must_use]
pub fn link_copy_writer(node_id: &str, incarnation: &str) -> String {
    format!(
        "{node_id}/{incarnation}/{}",
        uuid::Uuid::new_v4().hyphenated()
    )
}

/// True when `title_ref` names the title by its call number, which names
/// one edition whichever library sent it ([`link_copy_title_ref`]).
fn names_an_edition(title_ref: &str) -> bool {
    title_ref.starts_with("cn:")
}

/// True when the copy recorded in `row` is of the title `attempt` copies:
/// the same call number from any library, or the same holding of the same
/// library.
fn same_title(row: &link_copy_files::Model, attempt: LinkCopyAttempt<'_>) -> bool {
    row.title_ref == attempt.title_ref
        && (names_an_edition(attempt.title_ref) || row.library_id == attempt.library_id)
}

/// RFC 3339 stamp, as the other control-plane records write it.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339()
}

/// True when `key` on `destination` is a file this install's own library
/// recorded for an edition other than the title `title_ref` names: a local
/// acquire's or an import's file of another book. The same edition (its
/// call number matches) shares its files with the title, so it is not.
///
/// # Errors
///
/// Returns an error when the read fails.
async fn held_by_another_edition(
    store: &LibraryStore,
    destination: &str,
    key: &str,
    title_ref: &str,
) -> Result<bool> {
    let Some(file) = store.edition_file(destination, key).await? else {
        return Ok(false);
    };
    let FileOwner::Edition(edition) = &file.owner;
    let same = call_number(edition).is_some_and(|number| format!("cn:{number}") == title_ref);
    Ok(!same)
}

/// Claims `key` on `destination` for `attempt` before the copy is written;
/// `target` says whether a file is already there.
///
/// The key is the attempt's when it is unrecorded, or held by a copy of
/// the same title (an earlier attempt's kept copy, or one still copying,
/// which the newest attempt takes over); a title named by its call number
/// is the same title whichever joined library sent it. It is refused,
/// changing nothing, when another title's copy holds it, or when this
/// install's own library recorded it for another edition (whose call
/// number differs) and no copy of this title holds it: the copy must not
/// overwrite that file. A key this title's copy holds may be in this
/// install's library too (the branch owner's title, imported once
/// committed), and stays the title's own.
///
/// # Errors
///
/// Returns an error when a read or write fails.
pub async fn claim_link_copy(
    store: &LibraryStore,
    attempt: LinkCopyAttempt<'_>,
    destination: &str,
    key: &str,
    target: CopyTarget,
) -> Result<LinkCopyClaim> {
    let recorded = link_copy_files::Entity::find_by_id((destination.to_string(), key.to_string()))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let ours = recorded
        .as_ref()
        .is_some_and(|row| same_title(row, attempt));
    if !ours && held_by_another_edition(store, destination, key, attempt.title_ref).await? {
        return Ok(LinkCopyClaim::HeldByAnotherTitle);
    }
    let now = stamp(Utc::now());
    let inserted = link_copy_files::Entity::insert(link_copy_files::ActiveModel {
        destination: Set(destination.to_string()),
        storage_key: Set(key.to_string()),
        library_id: Set(attempt.library_id.to_string()),
        title_ref: Set(attempt.title_ref.to_string()),
        writer: Set(attempt.writer.to_string()),
        state: Set(STATE_COPYING.into()),
        replaced: Set(target.column()),
        written_at: Set(now.clone()),
    })
    .on_conflict(
        OnConflict::columns([
            link_copy_files::Column::Destination,
            link_copy_files::Column::StorageKey,
        ])
        .do_nothing()
        .to_owned(),
    )
    .exec_without_returning(store.db())
    .await
    .map_err(LibraryError::Orm)?;
    if inserted == 1 {
        return Ok(LinkCopyClaim::Fresh);
    }
    // Taken over only from the same title, each in one conditional write:
    // a kept copy (marked replaced, so a failed attempt keeps it again),
    // else another attempt's copy still in progress (the newest wins; a
    // file it found stays one this attempt found too).
    let take = |state: &'static str, replaced: Option<i64>| {
        let mut update = link_copy_files::Entity::update_many()
            .col_expr(link_copy_files::Column::Writer, Expr::value(attempt.writer))
            .col_expr(
                link_copy_files::Column::LibraryId,
                Expr::value(attempt.library_id),
            )
            .col_expr(link_copy_files::Column::State, Expr::value(STATE_COPYING))
            .col_expr(link_copy_files::Column::WrittenAt, Expr::value(now.clone()));
        if let Some(replaced) = replaced {
            update = update.col_expr(link_copy_files::Column::Replaced, Expr::value(replaced));
        }
        let update = update
            .filter(link_copy_files::Column::Destination.eq(destination))
            .filter(link_copy_files::Column::StorageKey.eq(key))
            .filter(link_copy_files::Column::TitleRef.eq(attempt.title_ref))
            .filter(link_copy_files::Column::State.eq(state));
        if names_an_edition(attempt.title_ref) {
            update
        } else {
            update.filter(link_copy_files::Column::LibraryId.eq(attempt.library_id))
        }
    };
    let kept = take(STATE_KEPT, Some(CopyTarget::Existing.column()))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if kept.rows_affected == 1 {
        return Ok(LinkCopyClaim::Replaced);
    }
    let copying = take(
        STATE_COPYING,
        (target == CopyTarget::Existing).then(|| target.column()),
    )
    .exec(store.db())
    .await
    .map_err(LibraryError::Orm)?;
    if copying.rows_affected == 1 {
        return Ok(LinkCopyClaim::Fresh);
    }
    Ok(LinkCopyClaim::HeldByAnotherTitle)
}

/// Keeps every copy `writer` claimed: the library committed the title, so
/// they stay on this install's destinations.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn keep_link_copies(store: &LibraryStore, writer: &str) -> Result<()> {
    link_copy_files::Entity::update_many()
        .col_expr(link_copy_files::Column::State, Expr::value(STATE_KEPT))
        .col_expr(link_copy_files::Column::Replaced, Expr::value(0))
        .filter(link_copy_files::Column::Writer.eq(writer))
        .filter(link_copy_files::Column::State.eq(STATE_COPYING))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// Gives back every key `writer`'s attempt claimed once it failed (the
/// caller removed the files it created): a key the attempt took over from
/// a kept copy of its title is kept again (that file stays), and a key it
/// claimed fresh is forgotten. A key another attempt claimed since is left
/// alone.
///
/// # Errors
///
/// Returns an error when a write fails.
pub async fn release_link_copies(store: &LibraryStore, writer: &str) -> Result<()> {
    link_copy_files::Entity::update_many()
        .col_expr(link_copy_files::Column::State, Expr::value(STATE_KEPT))
        .col_expr(link_copy_files::Column::Replaced, Expr::value(0))
        .filter(link_copy_files::Column::Writer.eq(writer))
        .filter(link_copy_files::Column::State.eq(STATE_COPYING))
        .filter(link_copy_files::Column::Replaced.eq(1))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    link_copy_files::Entity::delete_many()
        .filter(link_copy_files::Column::Writer.eq(writer))
        .filter(link_copy_files::Column::State.eq(STATE_COPYING))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// Copies of `node_id` that an earlier process left mid-title: `copying`
/// records whose writer is this node under another incarnation than
/// `incarnation`, older than [`STALE_LINK_COPY_AGE_SECS`] at `now`, oldest
/// first, at most a batch. The caller deletes each new one's file (or
/// keeps an existing one's) and then settles that key
/// ([`settle_stale_link_copy`]); a key whose file could not be deleted is
/// left for a later sweep.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn stale_link_copies(
    store: &LibraryStore,
    node_id: &str,
    incarnation: &str,
    now: DateTime<Utc>,
) -> Result<Vec<StaleLinkCopy>> {
    let ours = format!("{node_id}/");
    let live = format!("{node_id}/{incarnation}/");
    let before = stamp(now - Duration::seconds(STALE_LINK_COPY_AGE_SECS));
    let rows = link_copy_files::Entity::find()
        .filter(link_copy_files::Column::State.eq(STATE_COPYING))
        .filter(link_copy_files::Column::WrittenAt.lt(before))
        .filter(link_copy_files::Column::Writer.starts_with(ours.as_str()))
        .filter(link_copy_files::Column::Writer.not_like(format!("{live}%")))
        .order_by_asc(link_copy_files::Column::WrittenAt)
        .limit(STALE_LINK_COPY_BATCH)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows
        .into_iter()
        // `starts_with` binds a LIKE pattern, where `_` matches any one
        // character: confirm the prefixes exactly.
        .filter(|row| row.writer.starts_with(&ours) && !row.writer.starts_with(&live))
        .map(|row| StaleLinkCopy {
            destination: row.destination,
            key: row.storage_key,
            writer: row.writer,
            target: CopyTarget::from_column(row.replaced),
        })
        .collect())
}

/// Settles one copy [`stale_link_copies`] answered, once the caller deleted
/// its file (a [`CopyTarget::New`] one) or left it (a
/// [`CopyTarget::Existing`] one): the record is forgotten, or kept again.
/// Changes nothing when another attempt claimed the key since.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn settle_stale_link_copy(store: &LibraryStore, copy: &StaleLinkCopy) -> Result<()> {
    let ours = |destination: &str, key: &str, writer: &str| {
        sea_orm::Condition::all()
            .add(link_copy_files::Column::Destination.eq(destination.to_string()))
            .add(link_copy_files::Column::StorageKey.eq(key.to_string()))
            .add(link_copy_files::Column::Writer.eq(writer.to_string()))
            .add(link_copy_files::Column::State.eq(STATE_COPYING))
    };
    match copy.target {
        CopyTarget::Existing => {
            link_copy_files::Entity::update_many()
                .col_expr(link_copy_files::Column::State, Expr::value(STATE_KEPT))
                .col_expr(
                    link_copy_files::Column::Replaced,
                    Expr::value(CopyTarget::New.column()),
                )
                .filter(ours(&copy.destination, &copy.key, &copy.writer))
                .exec(store.db())
                .await
                .map_err(LibraryError::Orm)?;
        }
        CopyTarget::New => {
            link_copy_files::Entity::delete_many()
                .filter(ours(&copy.destination, &copy.key, &copy.writer))
                .exec(store.db())
                .await
                .map_err(LibraryError::Orm)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "link_copies_tests.rs"]
mod tests;
