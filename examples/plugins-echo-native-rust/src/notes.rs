//! A small `notes` table on the plugin's own `[[databases]]` binding.
//!
//! Written the way plugin database code should be: sized from `limits()`,
//! split with `chunk()`, read with `iterate()` when the session streams, and
//! paged by key when it does not. Typed `limit_exceeded` errors are handled
//! by the cap they name ([`PluginError::limit`]), never by matching the
//! message.
//!
//! `tests/notes.rs` drives this module through
//! `bookclerk_db_testkit::BindingDatabase` under the profile
//! `bookclerk-plugin test` names, so the same code is proven on the shipped
//! adapters, on D1 and on the smallest advertisement a host accepts.
//!
//! See `docs/plugins/database-limits.md`.

use std::collections::HashMap;

use bookclerk_plugin_sdk::{
    DatabaseBinding, DbValue, LimitExceeded, PluginError, PluginMigration, PluginMigrationOp,
    RowStream,
};

/// The `[[databases]]` binding name in `plugin.toml` that holds the notes.
pub const NOTES_BINDING: &str = "DB";

/// The `database_migrations` registration for [`NOTES_BINDING`].
///
/// Applied once, in order, before the binding is handed to the plugin.
#[must_use]
pub fn migrations() -> Vec<PluginMigration> {
    vec![PluginMigration {
        id: "notes-v1".into(),
        operations: vec![PluginMigrationOp::Schema(
            "CREATE TABLE IF NOT EXISTS notes \
             (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL)"
                .into(),
        )],
    }]
}

/// The `database_migrations` answer for `binding`.
///
/// # Returns
///
/// [`migrations`] for [`NOTES_BINDING`], nothing for any other binding.
#[must_use]
pub fn migrations_for(binding: &str) -> Vec<PluginMigration> {
    if binding == NOTES_BINDING {
        migrations()
    } else {
        Vec::new()
    }
}

/// One stored note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Identity the database assigned, increasing in insertion order.
    pub id: i64,
    /// The note text as written.
    pub body: String,
}

/// A note refused before it is sent: storable, but never returnable.
///
/// A reply cell is held to `maxCellBytes`, so a longer note could be written
/// under a larger `maxPayloadBytes` and then never read back.
///
/// # Arguments
///
/// * `used` - The note's size in UTF-8 bytes.
/// * `cap` - The session's `maxCellBytes`.
///
/// # Returns
///
/// A typed `limit_exceeded` error, read with [`PluginError::limit`] like the
/// binding's own.
#[must_use]
pub fn note_too_large(used: u64, cap: u64) -> PluginError {
    PluginError::limit_exceeded(LimitExceeded::new("maxCellBytes", cap, used))
}

/// Reads one stored row back into a [`Note`].
///
/// # Errors
///
/// Returns `invalid_params` when a column is missing or has an unexpected
/// type.
pub fn note_from_row(row: &HashMap<String, DbValue>) -> Result<Note, PluginError> {
    match (row.get("id"), row.get("body")) {
        (Some(DbValue::Int64(id)), Some(DbValue::Text(body))) => Ok(Note {
            id: *id,
            body: body.clone(),
        }),
        _ => Err(PluginError::invalid_params(
            "notes row has an unexpected shape",
        )),
    }
}

/// Stores `bodies` as new notes.
///
/// Every body is first checked against the session's `maxCellBytes`: a note
/// the database would accept but could never return is refused up front.
/// Then `chunk()` splits the inserts into batches that each fit one request
/// under the session's `maxStatements` and `maxRequestBytes`. Each batch
/// commits atomically; the loop does not, so when a later batch fails the
/// notes of the earlier ones stay stored. `on_committed` hears the running
/// total after each batch, so a caller can say how far a failed write got.
///
/// # Arguments
///
/// * `db` - The plugin's notes binding.
/// * `bodies` - Note texts, stored in order.
/// * `on_committed` - Called with the number of notes stored so far after
///   each batch commits.
///
/// # Returns
///
/// How many notes were stored.
///
/// # Errors
///
/// Returns `limit_exceeded` before anything is written when a body passes
/// `maxCellBytes`, and the binding's error otherwise, `limit_exceeded`
/// included when one statement passes `maxPayloadBytes` or
/// `maxRequestBytes`.
pub async fn add_notes(
    db: &DatabaseBinding,
    bodies: &[String],
    mut on_committed: impl FnMut(usize),
) -> Result<usize, PluginError> {
    let caps = db.limits().await?;
    let cell_cap = u64::from(caps.max_cell_bytes);
    for body in bodies {
        let used = u64::try_from(body.len()).unwrap_or(u64::MAX);
        if used > cell_cap {
            return Err(note_too_large(used, cell_cap));
        }
    }
    let inserts = bodies
        .iter()
        .map(|body| {
            db.prepare("INSERT INTO notes (body) VALUES (?)")
                .bind(vec![DbValue::Text(body.clone())])
                .as_run()
        })
        .collect();
    let mut stored = 0;
    for batch in db.chunk(inserts, None).await? {
        let len = batch.len();
        db.batch(batch, None).await?;
        stored += len;
        on_committed(stored);
    }
    Ok(stored)
}

/// Counts the stored notes.
///
/// # Errors
///
/// Returns the binding's error, or `invalid_params` when the count does not
/// come back as one INTEGER.
pub async fn count_notes(db: &DatabaseBinding) -> Result<i64, PluginError> {
    let row = db
        .prepare("SELECT count(*) AS n FROM notes")
        .first(None)
        .await?;
    let n = row.and_then(|cols| cols.into_iter().find(|(name, _)| name == "n"));
    match n {
        Some((_, DbValue::Int64(n))) => Ok(n),
        _ => Err(PluginError::invalid_params(
            "notes count has an unexpected shape",
        )),
    }
}

/// Reads the notes in id order without holding them all in memory.
///
/// When the session streams ([`bookclerk_plugin_sdk::DbCapabilities::streams_results`]),
/// one `iterate()` reads the whole table as a single snapshot, a frame at a
/// time. When it does not, every read is held to the inline caps, so the
/// notes are read in pages keyed on `id` (never `OFFSET`), each sized from
/// `maxResultRows`, halved whenever a page's bytes pass `maxResultBytes`, and
/// doubled again (up to `maxResultRows`) after each full page, so one run of
/// wide notes does not slow the rest of the walk. With a limit, no read asks
/// for more rows than are still wanted.
///
/// Dropping the reader ends the read.
pub struct NotesReader {
    /// The binding the reads go through.
    db: DatabaseBinding,
    /// Notes still wanted, `None` for all of them.
    left: Option<usize>,
    /// Where the read is.
    state: ReaderState,
}

impl std::fmt::Debug for NotesReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotesReader")
            .field("left", &self.left)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// Where a [`NotesReader`] is.
#[derive(Debug)]
enum ReaderState {
    /// Nothing has been read yet; the session's limits pick the mode.
    Unopened,
    /// One streamed read of the whole table.
    Streaming(RowStream),
    /// Key-ordered pages.
    Paging {
        /// The largest page: the session's `maxResultRows`, at least one.
        max_page: usize,
        /// Rows the next page asks for.
        page_size: usize,
        /// The id of the last note handed out; the next page starts after it.
        after: i64,
        /// Rows of the current page not handed out yet, oldest last.
        rows: Vec<HashMap<String, DbValue>>,
        /// Whether the current page was short, so no page follows it.
        last: bool,
    },
    /// The read is over (the last note, the limit, or an error).
    Done,
}

impl NotesReader {
    /// A reader over `db`'s notes, oldest first.
    ///
    /// # Arguments
    ///
    /// * `db` - The plugin's notes binding.
    /// * `limit` - The most notes to hand out, or `None` for all of them.
    #[must_use]
    pub fn new(db: &DatabaseBinding, limit: Option<usize>) -> Self {
        let state = if limit == Some(0) {
            ReaderState::Done
        } else {
            ReaderState::Unopened
        };
        Self {
            db: db.clone(),
            left: limit,
            state,
        }
    }

    /// The next note, `None` once the read is over.
    ///
    /// # Errors
    ///
    /// Yields (once, then `None`) the binding's error: `limit_exceeded` when
    /// even the streamed budget is too small or one note alone passes the
    /// inline caps, or `invalid_params` for a row of the wrong shape.
    pub async fn next(&mut self) -> Option<Result<Note, PluginError>> {
        match self.step().await {
            Ok(Some(note)) => {
                if let Some(left) = self.left.as_mut() {
                    *left -= 1;
                    if *left == 0 {
                        self.state = ReaderState::Done;
                    }
                }
                Some(Ok(note))
            }
            Ok(None) => {
                self.state = ReaderState::Done;
                None
            }
            Err(err) => {
                self.state = ReaderState::Done;
                Some(Err(err))
            }
        }
    }

    /// Reads up to the next note, whatever the mode.
    async fn step(&mut self) -> Result<Option<Note>, PluginError> {
        loop {
            match &mut self.state {
                ReaderState::Done => return Ok(None),
                ReaderState::Unopened => {
                    let caps = self.db.limits().await?;
                    self.state = if caps.streams_results() {
                        // With a limit the read asks for no more rows
                        // than are wanted.
                        let read = match self.left {
                            Some(left) => self
                                .db
                                .prepare("SELECT id, body FROM notes ORDER BY id LIMIT ?")
                                .bind(vec![DbValue::Int64(
                                    i64::try_from(left).unwrap_or(i64::MAX),
                                )]),
                            None => self.db.prepare("SELECT id, body FROM notes ORDER BY id"),
                        };
                        ReaderState::Streaming(read.iterate(None))
                    } else {
                        let max_page = usize::try_from(caps.max_result_rows)
                            .unwrap_or(usize::MAX)
                            .max(1);
                        ReaderState::Paging {
                            max_page,
                            page_size: max_page,
                            after: 0,
                            rows: Vec::new(),
                            last: false,
                        }
                    };
                }
                ReaderState::Streaming(stream) => {
                    return match stream.next().await {
                        Some(row) => note_from_row(&row?).map(Some),
                        None => Ok(None),
                    };
                }
                ReaderState::Paging {
                    max_page,
                    page_size,
                    after,
                    rows,
                    last,
                } => {
                    if let Some(row) = rows.pop() {
                        let note = note_from_row(&row)?;
                        *after = note.id;
                        return Ok(Some(note));
                    }
                    if *last {
                        return Ok(None);
                    }
                    if let Some(left) = self.left {
                        *page_size = (*page_size).min(left);
                    }
                    let size = i64::try_from(*page_size).unwrap_or(i64::MAX);
                    let read = self
                        .db
                        .prepare("SELECT id, body FROM notes WHERE id > ? ORDER BY id LIMIT ?")
                        .bind(vec![DbValue::Int64(*after), DbValue::Int64(size)])
                        .all(None)
                        .await;
                    let page = match read {
                        Ok(page) => page.results.unwrap_or_default(),
                        Err(err) => {
                            let wide = err
                                .limit()
                                .is_some_and(|limit| limit.limit == "maxResultBytes");
                            if wide && *page_size > 1 {
                                *page_size /= 2;
                                continue;
                            }
                            return Err(err);
                        }
                    };
                    if page.len() < *page_size {
                        *last = true;
                    } else {
                        *page_size = (*page_size * 2).min(*max_page);
                    }
                    *rows = page;
                    rows.reverse();
                }
            }
        }
    }
}
