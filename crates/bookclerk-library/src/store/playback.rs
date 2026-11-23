//! Listening state for the built-in player (`docs/adr/playback.md`):
//! per-person progress (`playback_progress`), play sessions
//! (`playback_sessions`) and bookmarks (`bookmarks`).
//!
//! Progress is keyed `(user_id, edition_id)`: a position is a place on one
//! recording's timeline, so two recordings of one book each keep their
//! own. Reaching the end of any of them also sets the work's finished
//! flag (`user_title_state.is_finished`) in the same batch, which is what
//! the library page and Discover read.
//!
//! A play session is one stretch of listening on one player. Its row is
//! also the leave to stream the edition's files
//! ([`crate::control_plane::authorize_playback`] checks it afresh on every
//! request), and it stays as listening history once closed.
//!
//! Every write is one atomic batch through the database plugin, never an
//! interactive transaction. The `playback` live topic moves when a session
//! opens or closes, an edition is finished, progress is set outright or a
//! bookmark changes; the position syncs of a playing session do not move
//! it, so open pages are not told to re-read every few seconds.

use bookclerk_plugin_abi::{DbType, DbValue, TypedDbStatement};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder};

use super::{now_str, parse_dt, parse_dt_opt, LibraryStore};
use crate::control_plane::{batch, bump, bump_if_any, read_paged, LiveTopic};
use crate::entities::{bookmarks, editions, expressions, playback_progress, playback_sessions};
use crate::error::{LibraryError, Result};

/// The longest position or duration the player stores: sixty days, far
/// past any recording, and small enough that sums of two stay exact in
/// every database profile's integer.
pub const MAX_PLAYBACK_MS: u64 = 60 * 24 * 60 * 60 * 1000;

/// Most bookmarks one person keeps in one edition.
pub const MAX_BOOKMARKS_PER_EDITION: u64 = 200;

/// Longest bookmark title, in bytes (well inside the minimum database
/// profile's cell size).
pub const MAX_BOOKMARK_TITLE_BYTES: usize = 200;

/// Longest player name a session stores, in bytes.
pub const MAX_PLAYER_NAME_BYTES: usize = 64;

/// How close to the end, in milliseconds, counts as finished for a
/// recording longer than a minute.
const FINISHED_TAIL_MS: u64 = 30_000;

/// A length of time on a recording's timeline, in milliseconds: a
/// position, a duration or time listened. At most [`MAX_PLAYBACK_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct PlaybackMs(u64);

impl PlaybackMs {
    /// The start of a recording.
    pub const ZERO: Self = Self(0);

    /// `ms` as a timeline length.
    ///
    /// # Returns
    ///
    /// `None` when `ms` is past [`MAX_PLAYBACK_MS`].
    #[must_use]
    pub fn new(ms: u64) -> Option<Self> {
        (ms <= MAX_PLAYBACK_MS).then_some(Self(ms))
    }

    /// The length in milliseconds.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// The stored integer. Always fits: the value is at most
    /// [`MAX_PLAYBACK_MS`].
    fn stored(self) -> i64 {
        i64::try_from(self.0).unwrap_or(i64::MAX)
    }

    /// The length a stored integer states, clamped into range (a negative
    /// or oversized cell reads as the nearest valid length).
    fn from_stored(stored: i64) -> Self {
        Self(u64::try_from(stored).unwrap_or(0).min(MAX_PLAYBACK_MS))
    }
}

/// True when `position` in a recording `duration` long counts as having
/// reached its end: within the last one percent, or within the last
/// thirty seconds of a recording longer than a minute.
///
/// # Arguments
///
/// * `position` - Where the listener is.
/// * `duration` - The recording's whole runtime; a zero duration is
///   unknown and never finished.
#[must_use]
pub fn reaches_end(position: PlaybackMs, duration: PlaybackMs) -> bool {
    let (position, duration) = (position.get(), duration.get());
    if duration == 0 {
        return false;
    }
    let last_percent = duration - duration / 100;
    let tail = duration > 60_000 && position.saturating_add(FINISHED_TAIL_MS) >= duration;
    position >= last_percent || tail
}

/// What kind of player opened a play session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlayerKind {
    /// The web app in a browser.
    Web,
    /// The desktop window's native engine.
    Desktop,
    /// An outside player app, through the player API.
    App,
}

impl PlayerKind {
    /// Every kind, in stored order (the `playback_sessions.client_kind`
    /// CHECK list).
    pub const ALL: [Self; 3] = [Self::Web, Self::Desktop, Self::App];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::App => "app",
        }
    }

    /// The kind a stored spelling names; [`Self::App`] for an unknown one.
    #[must_use]
    pub fn parse(stored: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == stored)
            .unwrap_or(Self::App)
    }
}

/// A host event about someone's listening, published after the write it
/// describes ([`LibraryStore::publish_playback_event`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackEvent {
    /// A play session opened (`playback_started`).
    Started,
    /// A play session closed, or progress was set outright
    /// (`playback_progress`). The position syncs of a playing session
    /// publish nothing.
    Progress,
    /// The listener reached the end of an edition (`playback_finished`).
    Finished,
}

impl PlaybackEvent {
    /// Every event, in the order of a listen.
    pub const ALL: [Self; 3] = [Self::Started, Self::Progress, Self::Finished];

    /// The event type plugins subscribe to.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "playback_started",
            Self::Progress => "playback_progress",
            Self::Finished => "playback_finished",
        }
    }
}

/// How far one person has listened to one edition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackProgress {
    /// The listener (`users.id`).
    pub user_id: i64,
    /// The edition.
    pub edition_id: String,
    /// Where they are on the edition's timeline.
    pub position: PlaybackMs,
    /// The edition's runtime as the last player reported it.
    pub duration: Option<PlaybackMs>,
    /// True once they reached the end of this edition.
    pub is_finished: bool,
    /// When they first played it.
    pub started_at: DateTime<Utc>,
    /// When they last reached the end.
    pub finished_at: Option<DateTime<Utc>>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

/// One stretch of listening on one player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackSession {
    /// Session id (hyphenated UUID).
    pub id: String,
    /// The listener (`users.id`).
    pub user_id: i64,
    /// The edition being played.
    pub edition_id: String,
    /// The node that opened the session and streams its files.
    pub serving_node_id: String,
    /// The destination the files are read from.
    pub destination: String,
    /// Digest of the edition's file rows when the session opened.
    pub files_digest: String,
    /// What kind of player opened it.
    pub client_kind: PlayerKind,
    /// The player's own name for itself.
    pub client_name: String,
    /// Where the session started from.
    pub start_position: PlaybackMs,
    /// Where it was at the last sync.
    pub position: PlaybackMs,
    /// Time spent listening in it, as the player counted.
    pub listened: PlaybackMs,
    /// When it opened.
    pub started_at: DateTime<Utc>,
    /// When it last synced.
    pub updated_at: DateTime<Utc>,
    /// When the player closed it; `None` while open.
    pub closed_at: Option<DateTime<Utc>>,
}

/// A place one person marked in one edition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    /// Bookmark id (hyphenated UUID).
    pub id: String,
    /// The person who made it (`users.id`).
    pub user_id: i64,
    /// The edition it is in.
    pub edition_id: String,
    /// The marked place.
    pub position: PlaybackMs,
    /// The person's note; may be empty.
    pub title: String,
    /// When it was made.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// What the player needs to know of an edition before it plays
/// ([`LibraryStore::playback_edition`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackEdition {
    /// The edition.
    pub edition_id: String,
    /// The work it belongs to; `None` only while the edition is being
    /// moved between works.
    pub work_id: Option<String>,
    /// The edition's display title.
    pub title: String,
    /// Its runtime as the catalog states it, to the minute; players
    /// report the exact one once they have loaded the audio.
    pub runtime: Option<PlaybackMs>,
}

/// A play session to open ([`LibraryStore::open_playback_session`]).
#[derive(Debug, Clone, Copy)]
pub struct NewPlaybackSession<'a> {
    /// The listener (`users.id`).
    pub user_id: i64,
    /// The edition to play.
    pub edition_id: &'a str,
    /// This node's id: it streams the files.
    pub serving_node_id: &'a str,
    /// The destination the files will be read from.
    pub destination: &'a str,
    /// Digest of the edition's file rows on that destination
    /// ([`crate::control_plane::PlaybackPlan::digest`]).
    pub files_digest: &'a str,
    /// What kind of player asks.
    pub client_kind: PlayerKind,
    /// The player's own name; at most [`MAX_PLAYER_NAME_BYTES`] bytes are
    /// kept.
    pub client_name: &'a str,
}

/// What a player reports about a playing session
/// ([`LibraryStore::sync_playback_session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackSync {
    /// Where the listener is now.
    pub position: PlaybackMs,
    /// Time listened since the last sync of this session.
    pub listened: PlaybackMs,
    /// The recording's whole runtime, when the player knows it.
    pub duration: Option<PlaybackMs>,
    /// True when this is the session's last report: it closes.
    pub close: bool,
}

/// The result of a sync or of setting progress outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackChange {
    /// The person's progress after the write.
    pub progress: PlaybackProgress,
    /// True when this write is the one that reached the edition's end.
    pub became_finished: bool,
}

/// Progress to set outright ([`LibraryStore::set_playback_progress`]): a
/// seek from a page with no session, Mark finished, or Start over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackProgressEdit {
    /// The position to store; `None` keeps it.
    pub position: Option<PlaybackMs>,
    /// The runtime to store; `None` keeps it.
    pub duration: Option<PlaybackMs>,
    /// `Some(true)` marks the edition (and its work) finished,
    /// `Some(false)` clears the edition's flag only, `None` decides from
    /// the position and runtime.
    pub finished: Option<bool>,
}

/// A page position of [`LibraryStore::list_playback_progress`]: the last
/// row of the page before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressCursor {
    /// That row's `updated_at`, as stored.
    pub updated_at: String,
    /// That row's edition.
    pub edition_id: String,
}

/// Maps a stored progress row.
fn map_progress(m: playback_progress::Model) -> PlaybackProgress {
    PlaybackProgress {
        user_id: m.user_id,
        edition_id: m.edition_id,
        position: PlaybackMs::from_stored(m.position_ms),
        duration: m.duration_ms.map(PlaybackMs::from_stored),
        is_finished: m.is_finished != 0,
        started_at: parse_dt(&m.started_at),
        finished_at: parse_dt_opt(m.finished_at.as_deref()),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// Maps a stored session row.
pub(super) fn map_session(m: playback_sessions::Model) -> PlaybackSession {
    PlaybackSession {
        id: m.id,
        user_id: m.user_id,
        edition_id: m.edition_id,
        serving_node_id: m.serving_node_id,
        destination: m.destination,
        files_digest: m.files_digest,
        client_kind: PlayerKind::parse(&m.client_kind),
        client_name: m.client_name,
        start_position: PlaybackMs::from_stored(m.start_position_ms),
        position: PlaybackMs::from_stored(m.position_ms),
        listened: PlaybackMs::from_stored(m.listened_ms),
        started_at: parse_dt(&m.started_at),
        updated_at: parse_dt(&m.updated_at),
        closed_at: parse_dt_opt(m.closed_at.as_deref()),
    }
}

/// Maps a stored bookmark row.
pub(super) fn map_bookmark(m: bookmarks::Model) -> Bookmark {
    Bookmark {
        id: m.id,
        user_id: m.user_id,
        edition_id: m.edition_id,
        position: PlaybackMs::from_stored(m.position_ms),
        title: m.title,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
fn truncated(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The condition, for a statement later in a sync's batch, that the
/// batch's own session update took effect: the session row carries the
/// batch's timestamp. Binds the session id, then that timestamp.
const SESSION_MOVED: &str =
    "EXISTS (SELECT 1 FROM playback_sessions WHERE id = ? AND updated_at = ?)";

/// The two statements that set a work's finished flag for one person,
/// keeping their ratings and tags: `UPDATE`, then `INSERT OR IGNORE`
/// guarded on the work existing.
///
/// Both run only when the same batch finished the edition: its progress
/// update stamped `finished_at` with the batch's timestamp
/// ([`FINISHED_HERE`]). The decision is the row's as the batch found it,
/// so a report that lost a race with another one, or with the session's
/// close, does not finish the work.
///
/// # Arguments
///
/// * `user_id` - The person.
/// * `work_id` - The work.
/// * `edition_id` - The edition whose progress row decides.
/// * `now` - The batch's timestamp.
fn work_finished(
    user_id: i64,
    work_id: &str,
    edition_id: &str,
    now: &str,
) -> [TypedDbStatement; 2] {
    let mark = || {
        vec![
            batch::int(user_id),
            batch::text(edition_id),
            batch::text(now),
        ]
    };
    let mut update = vec![batch::text(now), batch::int(user_id), batch::text(work_id)];
    update.extend(mark());
    let mut insert = vec![
        batch::int(user_id),
        batch::text(work_id),
        batch::text(now),
        batch::text(work_id),
    ];
    insert.extend(mark());
    [
        batch::exec(
            &format!(
                "UPDATE user_title_state SET is_finished = 1, updated_at = ? \
                 WHERE user_id = ? AND work_id = ? AND {FINISHED_HERE}"
            ),
            update,
        ),
        batch::exec(
            &format!(
                "INSERT OR IGNORE INTO user_title_state \
                 (user_id, work_id, is_finished, updated_at) \
                 SELECT ?, ?, 1, ? WHERE EXISTS (SELECT 1 FROM works WHERE id = ?) \
                 AND {FINISHED_HERE}"
            ),
            insert,
        ),
    ]
}

/// True while the person's progress row in the edition was finished by the
/// batch whose timestamp is bound: binds `user_id`, `edition_id`, the
/// timestamp. Only the statement that finishes a row sets `finished_at`,
/// and only on the unfinished-to-finished transition.
const FINISHED_HERE: &str = "EXISTS (SELECT 1 FROM playback_progress \
     WHERE user_id = ? AND edition_id = ? AND is_finished = 1 AND finished_at = ?)";

/// A query statement that answers one row when the batch finished the
/// edition ([`FINISHED_HERE`]), read back for [`PlaybackChange`].
fn finished_here_probe(user_id: i64, edition_id: &str, now: &str) -> TypedDbStatement {
    batch::query(
        "SELECT user_id FROM playback_progress \
         WHERE user_id = ? AND edition_id = ? AND is_finished = 1 AND finished_at = ?",
        vec![
            batch::int(user_id),
            batch::text(edition_id),
            batch::text(now),
        ],
        1,
    )
}

/// One side of a [`reaches_end_sql`] comparison: an SQL expression and the
/// binds it takes, in order.
struct SqlOperand {
    /// The expression.
    sql: String,
    /// Its binds.
    binds: Vec<DbValue>,
}

impl SqlOperand {
    /// `COALESCE(?, column)`: the bound value, or the row's when absent.
    fn bound_or(value: Option<PlaybackMs>, column: &str) -> Self {
        Self {
            sql: format!("COALESCE(?, {column})"),
            binds: vec![ms_opt(value)],
        }
    }
}

/// [`reaches_end`] written in SQL-v1, for a statement to decide from the
/// row it changes: true when `duration` is known and above zero and
/// `position` is within its last one percent, or within the last thirty
/// seconds of a recording longer than a minute. A `NULL` duration is
/// unknown and never finished (the predicate is then `NULL`, which a
/// `CASE WHEN` reads as false).
///
/// # Returns
///
/// The predicate and its binds, in order.
fn reaches_end_sql(position: &SqlOperand, duration: &SqlOperand) -> (String, Vec<DbValue>) {
    let (p, d) = (&position.sql, &duration.sql);
    let sql = format!(
        "({d} > 0 AND ({p} >= {d} - {d} / 100 OR ({d} > 60000 AND {p} + {FINISHED_TAIL_MS} >= {d})))"
    );
    let mut binds = Vec::new();
    // In the order the operands appear above.
    for operand in [
        duration, position, duration, duration, duration, position, duration,
    ] {
        binds.extend(operand.binds.iter().cloned());
    }
    (sql, binds)
}

/// An optional timeline length as a bind.
fn ms_opt(value: Option<PlaybackMs>) -> DbValue {
    batch::int_opt(value.map(PlaybackMs::stored))
}

impl LibraryStore {
    /// The edition a player asks about, with its work and catalog runtime.
    ///
    /// # Returns
    ///
    /// `None` for an unknown edition.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn playback_edition(&self, edition_id: &str) -> Result<Option<PlaybackEdition>> {
        let Some(edition) = editions::Entity::find_by_id(edition_id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let work_id = expressions::Entity::find_by_id(edition.expression_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|expression| expression.work_id);
        let runtime = edition
            .duration_minutes
            .and_then(|minutes| u64::try_from(minutes).ok())
            .and_then(|minutes| minutes.checked_mul(60_000))
            .and_then(PlaybackMs::new)
            .filter(|runtime| runtime.get() > 0);
        Ok(Some(PlaybackEdition {
            edition_id: edition.id,
            work_id,
            title: edition.title,
            runtime,
        }))
    }

    /// One person's progress in one edition.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn playback_progress(
        &self,
        user_id: i64,
        edition_id: &str,
    ) -> Result<Option<PlaybackProgress>> {
        Ok(
            playback_progress::Entity::find_by_id((user_id, edition_id.to_string()))
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
                .map(map_progress),
        )
    }

    /// A page of one person's progress rows, most recently played first.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The listener.
    /// * `include_finished` - False lists only editions not yet finished
    ///   (Continue listening).
    /// * `after` - The last row of the page before; `None` for the first.
    /// * `limit` - Most rows to return.
    ///
    /// # Returns
    ///
    /// The rows, and the cursor of the next page when there may be one.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn list_playback_progress(
        &self,
        user_id: i64,
        include_finished: bool,
        after: Option<&ProgressCursor>,
        limit: u64,
    ) -> Result<(Vec<PlaybackProgress>, Option<ProgressCursor>)> {
        use playback_progress::Column as P;
        let mut select = playback_progress::Entity::find().filter(P::UserId.eq(user_id));
        if !include_finished {
            select = select.filter(P::IsFinished.eq(0));
        }
        if let Some(after) = after {
            select = select.filter(
                Condition::any()
                    .add(P::UpdatedAt.lt(after.updated_at.as_str()))
                    .add(
                        Condition::all()
                            .add(P::UpdatedAt.eq(after.updated_at.as_str()))
                            .add(P::EditionId.gt(after.edition_id.as_str())),
                    ),
            );
        }
        // Read in pages of the database plugin's own result size, so a
        // page of the list is never one reply larger than it allows.
        let mut rows = read_paged(
            self,
            select
                .order_by_desc(P::UpdatedAt)
                .order_by_asc(P::EditionId),
            Some(limit.saturating_add(1)),
        )
        .await?;
        let more = u64::try_from(rows.len()).unwrap_or(u64::MAX) > limit;
        if more {
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        let next = if more {
            rows.last().map(|row| ProgressCursor {
                updated_at: row.updated_at.clone(),
                edition_id: row.edition_id.clone(),
            })
        } else {
            None
        };
        Ok((rows.into_iter().map(map_progress).collect(), next))
    }

    /// Sets one person's progress in one edition outright, with no
    /// session: a seek from the book's page, Mark finished or Start over.
    ///
    /// The row is created when the person has none. Finishing also sets
    /// the work's finished flag when `work_id` is given. One atomic batch;
    /// nothing is written for an unknown edition on an engine that checks
    /// foreign keys, which is then the returned error.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The listener.
    /// * `edition_id` - The edition.
    /// * `work_id` - The edition's work, for the finished flag.
    /// * `edit` - What to store.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn set_playback_progress(
        &self,
        user_id: i64,
        edition_id: &str,
        work_id: Option<&str>,
        edit: &PlaybackProgressEdit,
    ) -> Result<PlaybackChange> {
        let now = now_str();
        // Whether the edit finishes the edition is decided by the update
        // from the row as it finds it, not from an earlier read: a field
        // the edit leaves out is the row's, and an edit that leaves the
        // finished flag out finishes the edition when the row was finished
        // or the position (the edit's, else the row's) reaches the end of
        // the runtime (the edit's, else the row's).
        let (reaches, reaches_binds) = reaches_end_sql(
            &SqlOperand::bound_or(edit.position, "position_ms"),
            &SqlOperand::bound_or(edit.duration, "duration_ms"),
        );
        let finished_sql =
            format!("COALESCE(?, CASE WHEN is_finished = 1 OR {reaches} THEN 1 ELSE 0 END)");
        let finished_binds = || {
            let mut binds = vec![batch::int_opt(edit.finished.map(i64::from))];
            binds.extend(reaches_binds.iter().cloned());
            binds
        };
        let mut update_binds = vec![ms_opt(edit.position), ms_opt(edit.duration)];
        update_binds.extend(finished_binds());
        update_binds.extend(finished_binds());
        update_binds.extend([
            batch::text(&now),
            batch::text(&now),
            batch::int(user_id),
            batch::text(edition_id),
        ]);
        let mut statements = vec![
            batch::exec(
                "INSERT OR IGNORE INTO playback_progress \
                 (user_id, edition_id, position_ms, is_finished, started_at, updated_at) \
                 VALUES (?, ?, 0, 0, ?, ?)",
                vec![
                    batch::int(user_id),
                    batch::text(edition_id),
                    batch::text(&now),
                    batch::text(&now),
                ],
            ),
            batch::exec(
                // Every `SET` expression reads the row as it was before
                // this statement, so `finished_at` is stamped exactly on
                // the unfinished-to-finished transition.
                &format!(
                    "UPDATE playback_progress SET position_ms = COALESCE(?, position_ms), \
                     duration_ms = COALESCE(?, duration_ms), \
                     is_finished = {finished_sql}, \
                     finished_at = CASE WHEN is_finished = 0 AND {finished_sql} = 1 \
                     THEN ? ELSE finished_at END, updated_at = ? \
                     WHERE user_id = ? AND edition_id = ?"
                ),
                update_binds,
            ),
        ];
        if let Some(work_id) = work_id {
            statements.extend(work_finished(user_id, work_id, edition_id, &now));
        }
        let probe = statements.len();
        statements.push(finished_here_probe(user_id, edition_id, &now));
        statements.push(bump(LiveTopic::Playback));
        let operation_id = format!("playback-progress-{}", uuid::Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        let became_finished = !batch::rows(&reply, probe).is_empty();
        let progress = self
            .playback_progress(user_id, edition_id)
            .await?
            .ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!("playback progress was not stored"))
            })?;
        Ok(PlaybackChange {
            progress,
            became_finished,
        })
    }

    /// Forgets one person's progress in one edition. The work's finished
    /// flag is left as it is.
    ///
    /// # Returns
    ///
    /// True when a row was removed.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn delete_playback_progress(&self, user_id: i64, edition_id: &str) -> Result<bool> {
        let statements = vec![
            batch::exec(
                "DELETE FROM playback_progress WHERE user_id = ? AND edition_id = ?",
                vec![batch::int(user_id), batch::text(edition_id)],
            ),
            bump(LiveTopic::Playback),
        ];
        let operation_id = format!("playback-progress-delete-{}", uuid::Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(batch::rows_affected(&reply, 0) > 0)
    }

    /// Opens a play session and answers it.
    ///
    /// The session starts where the person's progress is, or at the start
    /// when they have none or had finished the edition (playing a finished
    /// edition again starts it over and clears the edition's finished
    /// flag; the work stays finished). One atomic batch.
    ///
    /// # Errors
    ///
    /// Returns the database error, for example for an unknown edition on
    /// an engine that checks foreign keys.
    pub async fn open_playback_session(
        &self,
        new: NewPlaybackSession<'_>,
    ) -> Result<PlaybackSession> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_str();
        let client_name = truncated(new.client_name.trim(), MAX_PLAYER_NAME_BYTES);
        let statements = vec![
            batch::exec(
                "INSERT OR IGNORE INTO playback_progress \
                 (user_id, edition_id, position_ms, is_finished, started_at, updated_at) \
                 VALUES (?, ?, 0, 0, ?, ?)",
                vec![
                    batch::int(new.user_id),
                    batch::text(new.edition_id),
                    batch::text(&now),
                    batch::text(&now),
                ],
            ),
            // The start is decided from the row as this statement finds
            // it, not from an earlier read, so a report landing just
            // before the batch is where the session starts: a finished
            // edition starts over, any other goes on from its position.
            batch::exec(
                "UPDATE playback_progress SET \
                 position_ms = CASE WHEN is_finished = 1 THEN 0 ELSE position_ms END, \
                 is_finished = 0, updated_at = ? \
                 WHERE user_id = ? AND edition_id = ?",
                vec![
                    batch::text(&now),
                    batch::int(new.user_id),
                    batch::text(new.edition_id),
                ],
            ),
            batch::exec(
                "INSERT INTO playback_sessions \
                 (id, user_id, edition_id, serving_node_id, destination, files_digest, \
                  client_kind, client_name, start_position_ms, position_ms, listened_ms, \
                  started_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ?, ?, position_ms, position_ms, 0, ?, ? \
                 FROM playback_progress WHERE user_id = ? AND edition_id = ?",
                vec![
                    batch::text(&id),
                    batch::int(new.user_id),
                    batch::text(new.edition_id),
                    batch::text(new.serving_node_id),
                    batch::text(new.destination),
                    batch::text(new.files_digest),
                    batch::text(new.client_kind.as_str()),
                    batch::text(client_name),
                    batch::text(&now),
                    batch::text(&now),
                    batch::int(new.user_id),
                    batch::text(new.edition_id),
                ],
            ),
            bump(LiveTopic::Playback),
        ];
        let operation_id = format!("playback-open-{id}");
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        self.playback_session(&id)
            .await?
            .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("the play session was not stored")))
    }

    /// One play session by id.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn playback_session(&self, id: &str) -> Result<Option<PlaybackSession>> {
        Ok(playback_sessions::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_session))
    }

    /// Records a player's report for an open session: the position, the
    /// time listened since the last report and, with `close`, its end.
    ///
    /// The person's progress moves with it. When the position reaches the
    /// recording's end ([`reaches_end`]) the edition is marked finished,
    /// and so is its work when `work_id` is given. One atomic batch, in
    /// which the progress only moves if the session was still open.
    ///
    /// # Arguments
    ///
    /// * `session` - The session, as read just before.
    /// * `work_id` - The edition's work, for the finished flag.
    /// * `sync` - The player's report.
    ///
    /// # Returns
    ///
    /// `None` when the session had already closed; nothing is written
    /// then.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn sync_playback_session(
        &self,
        session: &PlaybackSession,
        work_id: Option<&str>,
        sync: &PlaybackSync,
    ) -> Result<Option<PlaybackChange>> {
        if session.closed_at.is_some() {
            return Ok(None);
        }
        let now = now_str();
        let open_session = SESSION_MOVED;
        // Whether the report finishes the edition is decided by the
        // progress update from the row as it finds it: the runtime is the
        // report's, else the row's (which an overlapping report may just
        // have stored), and only an unfinished row is finished.
        let (reaches, reaches_binds) = reaches_end_sql(
            &SqlOperand {
                sql: "?".into(),
                binds: vec![batch::int(sync.position.stored())],
            },
            &SqlOperand::bound_or(sync.duration, "duration_ms"),
        );
        let finishes = format!("is_finished = 0 AND {reaches}");
        let mut progress_binds = vec![batch::int(sync.position.stored()), ms_opt(sync.duration)];
        progress_binds.extend(reaches_binds.iter().cloned());
        progress_binds.extend(reaches_binds.iter().cloned());
        progress_binds.extend([
            batch::text(&now),
            batch::text(&now),
            batch::int(session.user_id),
            batch::text(&session.edition_id),
            batch::text(&session.id),
            batch::text(&now),
        ]);
        let mut statements = vec![
            // The person may have forgotten their progress while the
            // session played: the next report starts the row again, as
            // long as the session is still open.
            batch::exec(
                "INSERT OR IGNORE INTO playback_progress \
                 (user_id, edition_id, position_ms, is_finished, started_at, updated_at) \
                 SELECT ?, ?, 0, 0, ?, ? WHERE EXISTS (SELECT 1 FROM playback_sessions \
                 WHERE id = ? AND user_id = ? AND closed_at IS NULL)",
                vec![
                    batch::int(session.user_id),
                    batch::text(&session.edition_id),
                    batch::text(&now),
                    batch::text(&now),
                    batch::text(&session.id),
                    batch::int(session.user_id),
                ],
            ),
            // Time listened is added to the stored total inside the
            // statement, so two reports that overlap both count.
            batch::exec(
                "UPDATE playback_sessions SET position_ms = ?, \
                 listened_ms = CASE WHEN listened_ms + ? > ? THEN ? ELSE listened_ms + ? END, \
                 updated_at = ?, \
                 closed_at = CASE WHEN ? = 1 THEN ? ELSE closed_at END \
                 WHERE id = ? AND user_id = ? AND closed_at IS NULL",
                vec![
                    batch::int(sync.position.stored()),
                    batch::int(sync.listened.stored()),
                    batch::int(PlaybackMs(MAX_PLAYBACK_MS).stored()),
                    batch::int(PlaybackMs(MAX_PLAYBACK_MS).stored()),
                    batch::int(sync.listened.stored()),
                    batch::text(&now),
                    batch::int(i64::from(sync.close)),
                    batch::text(&now),
                    batch::text(&session.id),
                    batch::int(session.user_id),
                ],
            ),
            // Every `SET` expression reads the row as it was before this
            // statement, so `finished_at` is stamped exactly on the
            // unfinished-to-finished transition, and only while the
            // session moved in this batch.
            batch::exec(
                &format!(
                    "UPDATE playback_progress SET position_ms = ?, \
                     duration_ms = COALESCE(?, duration_ms), \
                     is_finished = CASE WHEN {finishes} THEN 1 ELSE is_finished END, \
                     finished_at = CASE WHEN {finishes} THEN ? ELSE finished_at END, \
                     updated_at = ? \
                     WHERE user_id = ? AND edition_id = ? AND {open_session}"
                ),
                progress_binds,
            ),
        ];
        if let Some(work_id) = work_id {
            statements.extend(work_finished(
                session.user_id,
                work_id,
                &session.edition_id,
                &now,
            ));
        }
        let probe = statements.len();
        statements.push(finished_here_probe(
            session.user_id,
            &session.edition_id,
            &now,
        ));
        // A close bumps; so does a report that finished the edition, as
        // the batch decided it.
        statements.push(if sync.close {
            bump(LiveTopic::Playback)
        } else {
            bump_if_any(
                LiveTopic::Playback,
                finished_here_probe(session.user_id, &session.edition_id, &now),
            )
        });
        let operation_id = format!("playback-sync-{}", uuid::Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        // Statement 1 is the session's own update.
        if batch::rows_affected(&reply, 1) == 0 {
            return Ok(None);
        }
        let became_finished = !batch::rows(&reply, probe).is_empty();
        let progress = self
            .playback_progress(session.user_id, &session.edition_id)
            .await?
            .ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!("playback progress was not stored"))
            })?;
        Ok(Some(PlaybackChange {
            progress,
            became_finished,
        }))
    }

    /// Publishes a playback host event, after the write it describes.
    ///
    /// Best effort: the listening state is already committed, so a failed
    /// publish is logged rather than undoing it.
    ///
    /// Payload: `userId`, `editionId`, `workId` (or null), `sessionId` (or
    /// null when progress was set outright), `positionMs` and `durationMs`
    /// (or null while no player has reported the runtime).
    ///
    /// # Arguments
    ///
    /// * `event` - Which event.
    /// * `progress` - The listener's progress after the write.
    /// * `work_id` - The edition's work.
    /// * `session_id` - The play session, when the write came from one.
    pub async fn publish_playback_event(
        &self,
        event: PlaybackEvent,
        progress: &PlaybackProgress,
        work_id: Option<&str>,
        session_id: Option<&str>,
    ) {
        let payload = serde_json::json!({
            "userId": progress.user_id,
            "editionId": progress.edition_id,
            "workId": work_id,
            "sessionId": session_id,
            "positionMs": progress.position.get(),
            "durationMs": progress.duration.map(PlaybackMs::get),
        });
        let key = format!("{}:{}", progress.user_id, progress.edition_id);
        let spec = crate::PublishDomainEventSpec {
            event_type: event.as_str().into(),
            schema_version: 1,
            correlation_id: session_id.unwrap_or(&progress.edition_id).to_string(),
            // Each write is its own change: an edition can be started,
            // stopped and finished many times.
            dedup_key: format!("{}:{key}:{}", event.as_str(), uuid::Uuid::new_v4()),
            payload: payload.to_string(),
            ordering_key: key,
            ..crate::PublishDomainEventSpec::default()
        };
        if let Err(err) = self.publish_domain_event(spec).await {
            tracing::warn!(
                event = event.as_str(),
                edition = %progress.edition_id,
                error = %err,
                "could not publish a playback event"
            );
        }
    }

    /// One person's bookmarks in one edition, in timeline order.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn list_bookmarks(&self, user_id: i64, edition_id: &str) -> Result<Vec<Bookmark>> {
        use bookmarks::Column as B;
        let select = bookmarks::Entity::find()
            .filter(B::UserId.eq(user_id))
            .filter(B::EditionId.eq(edition_id))
            .order_by_asc(B::PositionMs);
        // Paged like the progress list: the primary key, the bookmark's
        // id, is the tie-break `read_paged` appends.
        Ok(read_paged(self, select, Some(MAX_BOOKMARKS_PER_EDITION))
            .await?
            .into_iter()
            .map(map_bookmark)
            .collect())
    }

    /// One bookmark by id.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn bookmark(&self, id: &str) -> Result<Option<Bookmark>> {
        Ok(bookmarks::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_bookmark))
    }

    /// Adds a bookmark for one person in one edition.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The person.
    /// * `edition_id` - The edition.
    /// * `position` - The place to mark.
    /// * `title` - Their note; at most [`MAX_BOOKMARK_TITLE_BYTES`] bytes
    ///   are kept.
    ///
    /// # Returns
    ///
    /// The bookmark, or `None` when the person already keeps
    /// [`MAX_BOOKMARKS_PER_EDITION`] in the edition.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn add_bookmark(
        &self,
        user_id: i64,
        edition_id: &str,
        position: PlaybackMs,
        title: &str,
    ) -> Result<Option<Bookmark>> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_str();
        let cap = i64::try_from(MAX_BOOKMARKS_PER_EDITION).unwrap_or(i64::MAX);
        let statements = vec![
            // The topic's row is written first, so two additions from
            // different database sessions queue on it and the second
            // counts the first's bookmark: the cap holds across nodes.
            bump(LiveTopic::Playback),
            batch::exec(
                "INSERT INTO bookmarks \
                 (id, user_id, edition_id, position_ms, title, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ? WHERE \
                 (SELECT COUNT(*) FROM bookmarks WHERE user_id = ? AND edition_id = ?) < ?",
                vec![
                    batch::text(&id),
                    batch::int(user_id),
                    batch::text(edition_id),
                    batch::int(position.stored()),
                    batch::text(truncated(title.trim(), MAX_BOOKMARK_TITLE_BYTES)),
                    batch::text(&now),
                    batch::text(&now),
                    batch::int(user_id),
                    batch::text(edition_id),
                    batch::int(cap),
                ],
            ),
        ];
        let operation_id = format!("bookmark-add-{id}");
        let reply = self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        // Statement 1 is the insert.
        if batch::rows_affected(&reply, 1) == 0 {
            return Ok(None);
        }
        self.bookmark(&id).await
    }

    /// Changes one of a person's bookmarks.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The person; another person's bookmark is not found.
    /// * `id` - The bookmark.
    /// * `position` - The new place; `None` keeps it.
    /// * `title` - The new note; `None` keeps it.
    ///
    /// # Returns
    ///
    /// The bookmark after the change, or `None` when the person has no
    /// such bookmark.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn edit_bookmark(
        &self,
        user_id: i64,
        id: &str,
        position: Option<PlaybackMs>,
        title: Option<&str>,
    ) -> Result<Option<Bookmark>> {
        let now = now_str();
        let title = title.map(|title| truncated(title.trim(), MAX_BOOKMARK_TITLE_BYTES));
        let statements = vec![
            batch::exec(
                "UPDATE bookmarks SET position_ms = COALESCE(?, position_ms), \
                 title = COALESCE(?, title), updated_at = ? WHERE id = ? AND user_id = ?",
                vec![
                    ms_opt(position),
                    match title {
                        Some(title) => batch::text(title),
                        None => DbValue::Null(DbType::Text),
                    },
                    batch::text(&now),
                    batch::text(id),
                    batch::int(user_id),
                ],
            ),
            bump(LiveTopic::Playback),
        ];
        let operation_id = format!("bookmark-edit-{}", uuid::Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        if batch::rows_affected(&reply, 0) == 0 {
            return Ok(None);
        }
        self.bookmark(id).await
    }

    /// Removes one of a person's bookmarks.
    ///
    /// # Returns
    ///
    /// True when a bookmark of theirs was removed.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn delete_bookmark(&self, user_id: i64, id: &str) -> Result<bool> {
        let statements = vec![
            batch::exec(
                "DELETE FROM bookmarks WHERE id = ? AND user_id = ?",
                vec![batch::text(id), batch::int(user_id)],
            ),
            bump(LiveTopic::Playback),
        ];
        let operation_id = format!("bookmark-delete-{}", uuid::Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(batch::rows_affected(&reply, 0) > 0)
    }
}

#[cfg(test)]
#[path = "playback_tests.rs"]
mod tests;
