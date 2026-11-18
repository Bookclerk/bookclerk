//! The change feed of a local root: a file-system watcher that records what
//! changed in a bounded in-memory journal, read back through
//! [`crate::StorageBackend::changes`].
//!
//! The journal lives only as long as the process that watches. A cursor
//! names the watcher it came from, so a cursor from an earlier process (or an
//! earlier watcher in this one) reads as "may have missed changes" and the
//! caller backs the feed with a full listing. The same answer comes back
//! when the journal dropped entries the cursor had not read, when the
//! operating system reported that it lost events, and when a whole directory
//! moved, because the watcher cannot name the objects that went with it.
//!
//! The watcher runs only where the operating system's notifications can be
//! trusted: on a local disk (see [`crate::fs_kind`]). Network file systems
//! and FUSE mounts often deliver no notifications for changes made by other
//! machines, so on those the feed is never live and every page says so.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::fs_kind;
use crate::{ChangeFeedPage, ObjectChange, ObjectChangeKind, Result, StorageError};

/// Changes kept in memory before the oldest are dropped. A cursor older
/// than the oldest kept change reads as incomplete. About 100 bytes each.
const JOURNAL_CAPACITY: usize = 16_384;

/// Most changes one page returns (the ABI's `maxChangesPage`).
const MAX_PAGE: usize = 256;

/// Most files a directory walk records for one new directory before it
/// gives up and marks a gap instead.
const MAX_WALK_FILES: usize = 4_096;

/// Prefix of every cursor this module hands out; a cursor without it is not
/// one of ours.
const CURSOR_TAG: &str = "w1";

/// Why a feed is not live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotWatching {
    /// The root is on a network file system, a FUSE mount, or a file system
    /// whose kind is unknown.
    NotLocalDisk,
    /// The operating system refused the watch, or stopped it (for example
    /// the per-user watch limit was reached).
    WatcherFailed,
}

/// The journal the watcher thread writes and [`LocalWatch::changes`] reads.
struct Journal {
    /// Random id of this watcher; cursors carry it.
    id: String,
    /// Recorded changes with their sequence numbers, oldest first.
    entries: VecDeque<(u64, ObjectChange)>,
    /// Sequence number the next change gets; also the head cursor.
    next_seq: u64,
    /// A cursor below this may have missed changes.
    gap_before: u64,
    /// `None` while the watcher runs; otherwise why it does not.
    stopped: Option<NotWatching>,
}

impl Journal {
    /// Records a change to `key`.
    fn push(&mut self, key: String, kind: ObjectChangeKind) {
        if self.entries.len() >= JOURNAL_CAPACITY {
            if let Some((dropped, _)) = self.entries.pop_front() {
                self.gap_before = self.gap_before.max(dropped + 1);
            }
        }
        self.entries.push_back((
            self.next_seq,
            ObjectChange {
                key,
                kind,
                info: None,
            },
        ));
        self.next_seq += 1;
    }

    /// Marks that changes may have been missed: every cursor handed out so
    /// far reads as incomplete.
    fn gap(&mut self) {
        self.next_seq += 1;
        self.gap_before = self.next_seq;
    }

    /// The cursor for sequence position `seq`.
    fn cursor(&self, seq: u64) -> String {
        format!("{CURSOR_TAG}.{}.{seq}", self.id)
    }

    /// A page that says the feed may have missed changes, with the head
    /// cursor to continue from after a full listing.
    fn incomplete(&self) -> ChangeFeedPage {
        ChangeFeedPage {
            changes: Vec::new(),
            cursor: self.cursor(self.next_seq),
            more: false,
            complete: false,
            live_watch: false,
        }
    }
}

/// Locks the journal; a poisoned lock still yields it, because every update
/// leaves it consistent before anything that could panic.
fn lock(journal: &Mutex<Journal>) -> MutexGuard<'_, Journal> {
    journal.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A watcher on one local root and the journal it fills.
pub(crate) struct LocalWatch {
    /// Shared with the watcher's event handler.
    journal: Arc<Mutex<Journal>>,
    /// Kept alive for as long as the feed should run; `None` when it does
    /// not.
    _watcher: Option<RecommendedWatcher>,
}

impl std::fmt::Debug for LocalWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let journal = lock(&self.journal);
        f.debug_struct("LocalWatch")
            .field("id", &journal.id)
            .field("next_seq", &journal.next_seq)
            .field("stopped", &journal.stopped)
            .finish_non_exhaustive()
    }
}

impl LocalWatch {
    /// Starts watching `dir` recursively, mapping paths under it to keys
    /// that start with `key_prefix`.
    ///
    /// Never fails: when `dir` is not on a local disk, or the operating
    /// system refuses the watch, the feed is created stopped and every page
    /// says it may have missed changes.
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory the keys live under (the root joined with
    ///   the key prefix). Created when missing.
    /// * `key_prefix` - The normalized key prefix, empty or ending in `/`.
    /// * `kind` - The file-system kind to assume instead of classifying
    ///   `dir` (tests).
    pub(crate) fn start(dir: &Path, key_prefix: &str, kind: Option<fs_kind::FsKind>) -> Self {
        let journal = Arc::new(Mutex::new(Journal {
            id: uuid::Uuid::new_v4().simple().to_string(),
            entries: VecDeque::new(),
            next_seq: 0,
            gap_before: 0,
            stopped: None,
        }));
        let _ = std::fs::create_dir_all(dir);
        let kind = kind.unwrap_or_else(|| fs_kind::classify(dir));
        if !kind.is_local() {
            lock(&journal).stopped = Some(NotWatching::NotLocalDisk);
            tracing::info!(
                dir = %dir.display(),
                ?kind,
                "not watching the output root: it is not on a local disk"
            );
            return Self {
                journal,
                _watcher: None,
            };
        }
        // Canonical, so event paths (which the OS reports resolved) strip.
        let base = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let handler = EventSink {
            journal: Arc::clone(&journal),
            base: base.clone(),
            prefix: key_prefix.to_string(),
        };
        let watcher = notify::recommended_watcher(move |event| handler.handle(event)).and_then(
            |mut watcher| {
                watcher.watch(&base, RecursiveMode::Recursive)?;
                Ok(watcher)
            },
        );
        match watcher {
            Ok(watcher) => Self {
                journal,
                _watcher: Some(watcher),
            },
            Err(err) => {
                lock(&journal).stopped = Some(NotWatching::WatcherFailed);
                tracing::warn!(
                    dir = %base.display(),
                    error = %err,
                    "could not watch the output root; changes are found by listing"
                );
                Self {
                    journal,
                    _watcher: None,
                }
            }
        }
    }

    /// Why the feed is not live, or `None` while it is.
    #[cfg(test)]
    pub(crate) fn stopped(&self) -> Option<NotWatching> {
        lock(&self.journal).stopped
    }

    /// Changes after `cursor`, oldest first.
    ///
    /// No cursor (or an empty one) asks for the current position: the page
    /// carries the head cursor and no changes. A cursor from another watcher,
    /// one older than the journal reaches back, one from before a gap, and
    /// any cursor while the watcher is stopped get an empty page with
    /// `complete: false` and the head cursor.
    ///
    /// # Errors
    ///
    /// [`StorageError::InvalidCursor`] for a string that is not a cursor
    /// this module made.
    pub(crate) fn changes(&self, cursor: Option<&str>, limit: u32) -> Result<ChangeFeedPage> {
        let journal = lock(&self.journal);
        let live = journal.stopped.is_none();
        let Some(cursor) = cursor.filter(|c| !c.is_empty()) else {
            return Ok(ChangeFeedPage {
                changes: Vec::new(),
                cursor: journal.cursor(journal.next_seq),
                more: false,
                complete: live,
                live_watch: live,
            });
        };
        let (id, seq) = parse_cursor(cursor)?;
        if !live || id != journal.id || seq > journal.next_seq || seq < journal.gap_before {
            return Ok(journal.incomplete());
        }
        let limit = match usize::try_from(limit) {
            Ok(0) | Err(_) => MAX_PAGE,
            Ok(n) => n.min(MAX_PAGE),
        };
        let mut changes = Vec::new();
        let mut next = seq;
        let mut more = false;
        for (entry_seq, change) in journal.entries.iter().filter(|(s, _)| *s >= seq) {
            if changes.len() == limit {
                more = true;
                break;
            }
            changes.push(change.clone());
            next = entry_seq + 1;
        }
        if !more {
            next = journal.next_seq;
        }
        Ok(ChangeFeedPage {
            changes,
            cursor: journal.cursor(next),
            more,
            complete: true,
            live_watch: true,
        })
    }
}

/// Splits a cursor into its watcher id and sequence position.
///
/// # Errors
///
/// [`StorageError::InvalidCursor`] when `cursor` is not `w1.<id>.<seq>`.
fn parse_cursor(cursor: &str) -> Result<(&str, u64)> {
    let invalid = || StorageError::InvalidCursor("not a local change-feed cursor".into());
    let mut parts = cursor.splitn(3, '.');
    if parts.next() != Some(CURSOR_TAG) {
        return Err(invalid());
    }
    let id = parts
        .next()
        .filter(|id| !id.is_empty())
        .ok_or_else(invalid)?;
    let seq = parts
        .next()
        .and_then(|seq| seq.parse::<u64>().ok())
        .ok_or_else(invalid)?;
    Ok((id, seq))
}

/// Turns watcher events into journal entries.
struct EventSink {
    /// The journal to fill.
    journal: Arc<Mutex<Journal>>,
    /// The canonical watched directory; event paths are made relative to it.
    base: PathBuf,
    /// Key prefix prepended to every relative path.
    prefix: String,
}

impl EventSink {
    /// Records one watcher event (or error).
    fn handle(&self, event: notify::Result<Event>) {
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                let mut journal = lock(&self.journal);
                journal.gap();
                if matches!(err.kind, notify::ErrorKind::MaxFilesWatch) {
                    journal.stopped = Some(NotWatching::WatcherFailed);
                    tracing::warn!(
                        error = %err,
                        "stopped watching the output root: the watch limit was reached"
                    );
                } else {
                    tracing::debug!(error = %err, "output root watcher reported an error");
                }
                return;
            }
        };
        if event.need_rescan() {
            lock(&self.journal).gap();
            return;
        }
        match event.kind {
            EventKind::Access(_) => {}
            EventKind::Remove(kind) => {
                // A removed folder's files report their own removals.
                if kind != RemoveKind::Folder {
                    self.record_all(&event.paths, ObjectChangeKind::Removed);
                }
            }
            EventKind::Create(CreateKind::Folder) => {
                for path in &event.paths {
                    self.walk_new_dir(path);
                }
            }
            EventKind::Modify(ModifyKind::Name(mode)) => self.renamed(mode, &event.paths),
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any | EventKind::Other => {
                for path in &event.paths {
                    if path.is_dir() {
                        self.walk_new_dir(path);
                    } else {
                        self.record(path, ObjectChangeKind::Changed);
                    }
                }
            }
        }
    }

    /// Records a rename. A file renamed inside the root is a removal and a
    /// change; a directory renamed in or out, or a rename whose source may
    /// have been a directory, is a gap, because the objects that moved with
    /// it are not named.
    fn renamed(&self, mode: RenameMode, paths: &[PathBuf]) {
        match (mode, paths) {
            (RenameMode::Both, [from, to]) => {
                if to.is_dir() {
                    lock(&self.journal).gap();
                } else {
                    self.record(from, ObjectChangeKind::Removed);
                    self.record(to, ObjectChangeKind::Changed);
                }
            }
            (RenameMode::To, _) => {
                for path in paths {
                    if path.is_dir() {
                        self.walk_new_dir(path);
                    } else {
                        self.record(path, ObjectChangeKind::Changed);
                    }
                }
            }
            // Moved out of the root: the source is gone, so whether it was
            // a directory cannot be told any more.
            (RenameMode::From, _) => {
                self.record_all(paths, ObjectChangeKind::Removed);
                lock(&self.journal).gap();
            }
            _ => lock(&self.journal).gap(),
        }
    }

    /// Records every file under a directory that just appeared. Files can
    /// land in it before the watcher adds a watch, so the walk names them.
    /// A directory too large to walk is a gap instead.
    fn walk_new_dir(&self, dir: &Path) {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&next) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    pending.push(entry.path());
                } else if file_type.is_file() {
                    if found.len() == MAX_WALK_FILES {
                        lock(&self.journal).gap();
                        return;
                    }
                    found.push(entry.path());
                }
            }
        }
        self.record_all(&found, ObjectChangeKind::Changed);
    }

    /// Records the same kind of change for every path that maps to a key.
    fn record_all(&self, paths: &[PathBuf], kind: ObjectChangeKind) {
        for path in paths {
            self.record(path, kind);
        }
    }

    /// Records a change to the object at `path`, when it names one.
    fn record(&self, path: &Path, kind: ObjectChangeKind) {
        if let Some(key) = key_for(&self.base, &self.prefix, path) {
            lock(&self.journal).push(key, kind);
        }
    }
}

/// The object key for `path` under `base`, or `None` for a path outside it,
/// the base itself, a name that is not UTF-8, or a temporary file the local
/// backend writes before renaming it into place.
fn key_for(base: &Path, prefix: &str, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(base).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    let name = parts.last()?;
    if name.starts_with('.') && name.contains(".bookclerk-tmp-") {
        return None;
    }
    Some(format!("{prefix}{}", parts.join("/")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A journal with no watcher behind it, for driving the sink by hand.
    fn sink(dir: &Path, prefix: &str) -> (LocalWatch, EventSink) {
        let watch = LocalWatch {
            journal: Arc::new(Mutex::new(Journal {
                id: "test".into(),
                entries: VecDeque::new(),
                next_seq: 0,
                gap_before: 0,
                stopped: None,
            })),
            _watcher: None,
        };
        let sink = EventSink {
            journal: Arc::clone(&watch.journal),
            base: dir.to_path_buf(),
            prefix: prefix.into(),
        };
        (watch, sink)
    }

    fn event(kind: EventKind, paths: &[&Path]) -> notify::Result<Event> {
        let mut event = Event::new(kind);
        for path in paths {
            event = event.add_path(path.to_path_buf());
        }
        Ok(event)
    }

    fn keys(page: &ChangeFeedPage) -> Vec<(String, ObjectChangeKind)> {
        page.changes
            .iter()
            .map(|c| (c.key.clone(), c.kind))
            .collect()
    }

    #[test]
    fn keys_strip_the_base_add_the_prefix_and_skip_temporary_files() {
        let base = Path::new("/lib");
        assert_eq!(
            key_for(base, "p/", Path::new("/lib/A/b.m4b")).as_deref(),
            Some("p/A/b.m4b")
        );
        assert_eq!(key_for(base, "", Path::new("/lib")), None);
        assert_eq!(key_for(base, "", Path::new("/elsewhere/x")), None);
        assert_eq!(
            key_for(base, "", Path::new("/lib/A/.b.m4b.bookclerk-tmp-12")),
            None
        );
        assert_eq!(
            key_for(base, "", Path::new("/lib/A/.hidden")).as_deref(),
            Some("A/.hidden")
        );
    }

    #[test]
    fn cursors_read_back_in_pages_and_reject_strangers() {
        let dir = tempfile::tempdir().unwrap();
        let (watch, sink) = sink(dir.path(), "");
        let start = watch.changes(None, 0).unwrap();
        assert!(start.complete && start.live_watch && start.changes.is_empty());
        for name in ["a", "b", "c"] {
            sink.handle(event(
                EventKind::Create(CreateKind::File),
                &[&dir.path().join(name)],
            ));
        }
        let first = watch.changes(Some(&start.cursor), 2).unwrap();
        assert_eq!(
            keys(&first),
            vec![
                ("a".into(), ObjectChangeKind::Changed),
                ("b".into(), ObjectChangeKind::Changed)
            ]
        );
        assert!(first.more && first.complete);
        let second = watch.changes(Some(&first.cursor), 2).unwrap();
        assert_eq!(keys(&second), vec![("c".into(), ObjectChangeKind::Changed)]);
        assert!(!second.more);
        let idle = watch.changes(Some(&second.cursor), 2).unwrap();
        assert!(idle.changes.is_empty() && idle.complete);
        assert_eq!(idle.cursor, second.cursor);

        let foreign = watch.changes(Some("w1.other.0"), 2).unwrap();
        assert!(!foreign.complete && !foreign.live_watch);
        assert_eq!(foreign.cursor, idle.cursor);
        assert!(matches!(
            watch.changes(Some("nonsense"), 2),
            Err(StorageError::InvalidCursor(_))
        ));
        assert!(matches!(
            watch.changes(Some("w1.test.x"), 2),
            Err(StorageError::InvalidCursor(_))
        ));
    }

    #[test]
    fn lost_events_and_directory_moves_make_older_cursors_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let (watch, sink) = sink(dir.path(), "");
        let before = watch.changes(None, 0).unwrap().cursor;
        sink.handle(Err(notify::Error::generic("queue overflow")));
        let after = watch.changes(Some(&before), 0).unwrap();
        assert!(!after.complete);
        assert!(watch.changes(Some(&after.cursor), 0).unwrap().complete);

        std::fs::create_dir(dir.path().join("moved")).unwrap();
        sink.handle(event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[&dir.path().join("old"), &dir.path().join("moved")],
        ));
        assert!(!watch.changes(Some(&after.cursor), 0).unwrap().complete);

        let now = watch.changes(None, 0).unwrap().cursor;
        sink.handle(event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &[&dir.path().join("gone")],
        ));
        assert!(!watch.changes(Some(&now), 0).unwrap().complete);
    }

    #[test]
    fn a_file_rename_is_a_removal_and_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let (watch, sink) = sink(dir.path(), "");
        let start = watch.changes(None, 0).unwrap().cursor;
        std::fs::write(dir.path().join("new.m4b"), b"x").unwrap();
        sink.handle(event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[&dir.path().join("old.m4b"), &dir.path().join("new.m4b")],
        ));
        sink.handle(event(
            EventKind::Remove(RemoveKind::File),
            &[&dir.path().join("new.m4b")],
        ));
        let page = watch.changes(Some(&start), 0).unwrap();
        assert!(page.complete);
        assert_eq!(
            keys(&page),
            vec![
                ("old.m4b".into(), ObjectChangeKind::Removed),
                ("new.m4b".into(), ObjectChangeKind::Changed),
                ("new.m4b".into(), ObjectChangeKind::Removed),
            ]
        );
    }

    #[test]
    fn a_new_directory_names_the_files_already_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let (watch, sink) = sink(dir.path(), "lib/");
        let start = watch.changes(None, 0).unwrap().cursor;
        std::fs::create_dir_all(dir.path().join("Author/Title")).unwrap();
        std::fs::write(dir.path().join("Author/Title/book.m4b"), b"x").unwrap();
        sink.handle(event(
            EventKind::Create(CreateKind::Folder),
            &[&dir.path().join("Author")],
        ));
        let page = watch.changes(Some(&start), 0).unwrap();
        assert_eq!(
            keys(&page),
            vec![(
                "lib/Author/Title/book.m4b".into(),
                ObjectChangeKind::Changed
            )]
        );
    }

    #[test]
    fn a_full_journal_drops_the_oldest_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (watch, sink) = sink(dir.path(), "");
        let start = watch.changes(None, 0).unwrap().cursor;
        for i in 0..=JOURNAL_CAPACITY {
            sink.record(&dir.path().join(format!("f{i}")), ObjectChangeKind::Changed);
        }
        assert!(!watch.changes(Some(&start), 0).unwrap().complete);
        let head = watch.changes(None, 0).unwrap().cursor;
        assert!(watch.changes(Some(&head), 0).unwrap().complete);
    }

    #[test]
    fn a_root_that_is_not_on_a_local_disk_is_never_watched() {
        let dir = tempfile::tempdir().unwrap();
        let watch = LocalWatch::start(dir.path(), "", Some(fs_kind::FsKind::Remote));
        assert_eq!(watch.stopped(), Some(NotWatching::NotLocalDisk));
        let page = watch.changes(None, 0).unwrap();
        assert!(!page.complete && !page.live_watch);
        let again = watch.changes(Some(&page.cursor), 0).unwrap();
        assert!(!again.complete);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_watcher_reports_a_file_written_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let watch = LocalWatch::start(dir.path(), "", None);
        if watch.stopped().is_some() {
            // A temp dir on a network or unknown file system: nothing to
            // watch here.
            return;
        }
        let start = watch.changes(None, 0).unwrap().cursor;
        std::fs::create_dir(dir.path().join("A")).unwrap();
        std::fs::write(dir.path().join("A/b.m4b"), b"x").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let page = watch.changes(Some(&start), 0).unwrap();
            assert!(page.complete && page.live_watch, "{page:?}");
            if page.changes.iter().any(|c| c.key == "A/b.m4b") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no event: {page:?}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
