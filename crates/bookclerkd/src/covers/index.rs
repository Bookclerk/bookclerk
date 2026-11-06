//! The cover cache's journal index: what Bookclerk knows about each entry
//! in `cache/covers/` that its files don't say.
//!
//! - **One file per cache.** `index.journal` sits beside the entry
//!   directories and holds one JSON record per line: an entry was written,
//!   served or dropped. It is never served, since only entry directories
//!   are, and no entry carries a sidecar file.
//! - **In memory.** The daemon reads the journal once when it opens the
//!   cache and keeps the index in memory; changes are appended as they
//!   happen. Once the journal holds many more records than entries, it is
//!   rewritten to a temporary file and renamed into place.
//! - **Rebuilt when lost.** A missing, torn or unreadable journal is not an
//!   error. On open, entries found on disk but not in the journal are
//!   indexed as stale with no validators, so an image is fetched again in
//!   full the next time it is checked, and records for entries no longer on
//!   disk are dropped.
//!
//! Records are written only by this module, so reading them is not an
//! untrusted-input boundary; a line that does not parse is skipped.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bookclerk_cover::http_cache::Validators;
use serde::{Deserialize, Serialize};

/// The journal's file name inside the cache directory.
pub(super) const JOURNAL_FILE: &str = "index.journal";

/// Where a compacted journal is written before it is renamed into place.
const COMPACT_FILE: &str = "index.journal.tmp";

/// Records the journal may hold beyond twice the entries before it is
/// compacted.
const COMPACT_SLACK: usize = 1024;

/// A served entry is journaled as recently used at most this often, so
/// serving an image rarely writes to disk.
pub(super) const SERVED_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// A point in time as whole seconds since the Unix epoch, as the journal
/// stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Stamp(u64);

impl Stamp {
    /// The current time.
    pub(super) fn now() -> Self {
        Self::from_time(SystemTime::now())
    }

    /// `time`, rounded down to the second; the epoch for anything before it.
    pub(super) fn from_time(time: SystemTime) -> Self {
        Self(
            time.duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
        )
    }

    /// The time this stamp names.
    pub(super) fn to_time(self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.0)
    }

    /// The stamp `after` later, saturating.
    pub(super) fn after(self, after: Duration) -> Self {
        Self(self.0.saturating_add(after.as_secs()))
    }

    /// Time from `earlier` to this stamp; zero when `earlier` is later.
    pub(super) fn since(self, earlier: Self) -> Duration {
        Duration::from_secs(self.0.saturating_sub(earlier.0))
    }
}

/// What the index knows about one cached image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    /// When the image was last downloaded or confirmed unchanged.
    pub(super) fetched: Stamp,
    /// Until when the image is fresh. An image that can change at its
    /// address is revalidated after this; a cover never is.
    pub(super) fresh_until: Stamp,
    /// Upstream's entity tag, sent back as `If-None-Match`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) etag: Option<String>,
    /// Upstream's `Last-Modified`, sent back as `If-Modified-Since`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_modified: Option<Stamp>,
    /// When the entry was last served, to within [`SERVED_INTERVAL`].
    pub(super) served: Stamp,
    /// Bytes of the entry's files on disk.
    pub(super) bytes: u64,
}

impl Entry {
    /// The validators to revalidate this image with.
    pub(super) fn validators(&self) -> Validators {
        Validators::new(self.etag.as_deref(), self.last_modified.map(Stamp::to_time))
    }

    /// Whether the image is past its freshness lifetime at `now`.
    pub(super) fn is_stale(&self, now: Stamp) -> bool {
        self.fresh_until <= now
    }
}

/// One line of the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Record {
    /// The entry for `key` was written or revalidated: this replaces
    /// whatever the index held for it.
    Put {
        /// The entry's cache key.
        key: String,
        /// Everything known about it.
        entry: Entry,
    },
    /// The entry for `key` was served at `at`.
    Served {
        /// The entry's cache key.
        key: String,
        /// When it was served.
        at: Stamp,
    },
    /// The entry for `key` was deleted.
    Drop {
        /// The entry's cache key.
        key: String,
    },
}

/// The in-memory index of one cover cache and its journal.
#[derive(Debug)]
pub(super) struct Index {
    /// The cache directory, or `None` for an index that keeps nothing on
    /// disk.
    dir: Option<PathBuf>,
    /// Entries by cache key.
    entries: HashMap<String, Entry>,
    /// Sum of every entry's `bytes`.
    total: u64,
    /// Append handle on the journal, when it could be opened.
    journal: Option<std::fs::File>,
    /// Records in the journal file, to decide when to compact it.
    records: usize,
}

impl Index {
    /// An index with no cache directory: it tracks entries in memory only.
    pub(super) fn detached() -> Self {
        Self {
            dir: None,
            entries: HashMap::new(),
            total: 0,
            journal: None,
            records: 0,
        }
    }

    /// Opens the index of the cache in `dir`: replays its journal, indexes
    /// entries on disk the journal does not know as stale, forgets entries
    /// no longer on disk, and writes a compacted journal.
    ///
    /// Never fails: without a readable journal the index is rebuilt from
    /// the entry directories, and without a writable one it is kept in
    /// memory only.
    pub(super) fn open(dir: &Path) -> Self {
        let (mut entries, _) = std::fs::read(dir.join(JOURNAL_FILE))
            .map(|bytes| replay(&bytes))
            .unwrap_or_default();
        let on_disk = entry_dirs(dir);
        entries.retain(|key, _| on_disk.contains_key(key));
        for (key, (bytes, modified)) in on_disk {
            entries.entry(key).or_insert_with(|| Entry {
                fetched: modified,
                // Unknown freshness: an image is checked again in full the
                // next time it is served.
                fresh_until: Stamp(0),
                etag: None,
                last_modified: None,
                served: modified,
                bytes,
            });
        }
        let total = entries.values().map(|entry| entry.bytes).sum();
        let mut index = Self {
            dir: Some(dir.to_path_buf()),
            entries,
            total,
            journal: None,
            records: 0,
        };
        index.compact();
        index
    }

    /// The entry for `key`, if indexed.
    pub(super) fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// Bytes of every indexed entry.
    pub(super) fn total(&self) -> u64 {
        self.total
    }

    /// Records `entry` for `key`, replacing what was there.
    pub(super) fn put(&mut self, key: &str, entry: Entry) {
        self.total = self.total.saturating_add(entry.bytes);
        if let Some(old) = self.entries.insert(key.to_owned(), entry.clone()) {
            self.total = self.total.saturating_sub(old.bytes);
        }
        self.append(&Record::Put {
            key: key.to_owned(),
            entry,
        });
    }

    /// Notes that `key` was served at `now`; journaled only when the last
    /// noted time is [`SERVED_INTERVAL`] old. An unindexed key is ignored.
    pub(super) fn served(&mut self, key: &str, now: Stamp) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if now.since(entry.served) < SERVED_INTERVAL {
            return;
        }
        entry.served = now;
        self.append(&Record::Served {
            key: key.to_owned(),
            at: now,
        });
    }

    /// Forgets `key`.
    ///
    /// # Returns
    ///
    /// The entry it held, if any.
    pub(super) fn remove(&mut self, key: &str) -> Option<Entry> {
        let removed = self.entries.remove(key)?;
        self.total = self.total.saturating_sub(removed.bytes);
        self.append(&Record::Drop {
            key: key.to_owned(),
        });
        Some(removed)
    }

    /// Forgets the least recently served entries until the index holds at
    /// most `target` bytes.
    ///
    /// # Returns
    ///
    /// The keys forgotten, whose directories the caller deletes.
    pub(super) fn evict_to(&mut self, target: u64) -> Vec<String> {
        if self.total <= target {
            return Vec::new();
        }
        let mut by_age: Vec<(Stamp, String)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.served, key.clone()))
            .collect();
        by_age.sort();
        let mut evicted = Vec::new();
        for (_, key) in by_age {
            if self.total <= target {
                break;
            }
            if self.remove(&key).is_some() {
                evicted.push(key);
            }
        }
        evicted
    }

    /// Appends `record` to the journal, compacting it when it has grown
    /// well past the entries it describes.
    fn append(&mut self, record: &Record) {
        let Some(journal) = self.journal.as_mut() else {
            return;
        };
        let written = serde_json::to_vec(record)
            .map_err(std::io::Error::other)
            .and_then(|mut line| {
                line.push(b'\n');
                journal.write_all(&line)
            });
        match written {
            Ok(()) => self.records += 1,
            Err(err) => tracing::warn!(error = %err, "could not append to the cover cache index"),
        }
        if self.records > self.entries.len().saturating_mul(2) + COMPACT_SLACK {
            self.compact();
        }
    }

    /// Rewrites the journal as one `Put` per entry, through a temporary
    /// file renamed into place, and reopens it for appending.
    fn compact(&mut self) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        self.journal = None;
        let mut body = Vec::new();
        for (key, entry) in &self.entries {
            let record = Record::Put {
                key: key.clone(),
                entry: entry.clone(),
            };
            if let Ok(line) = serde_json::to_vec(&record) {
                body.extend_from_slice(&line);
                body.push(b'\n');
            }
        }
        let temp = dir.join(COMPACT_FILE);
        let path = dir.join(JOURNAL_FILE);
        let rewritten = std::fs::write(&temp, &body).and_then(|()| std::fs::rename(&temp, &path));
        if let Err(err) = rewritten {
            let _ = std::fs::remove_file(&temp);
            tracing::warn!(error = %err, "could not rewrite the cover cache index");
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => {
                self.journal = Some(file);
                self.records = self.entries.len();
            }
            Err(err) => {
                tracing::warn!(error = %err, "cover cache index kept in memory only");
            }
        }
    }

    /// Makes `key` stale now, as if its lifetime had run out.
    #[cfg(test)]
    pub(super) fn expire(&mut self, key: &str) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.fresh_until = Stamp(0);
        }
    }
}

/// Whether `name` is a cache key: 64 lowercase hex digits.
pub(super) fn is_key(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Replays journal bytes into entries; later records win. Lines that do not
/// parse, and records whose key is not a cache key, are skipped.
///
/// # Returns
///
/// The entries and the number of records read.
fn replay(bytes: &[u8]) -> (HashMap<String, Entry>, usize) {
    let mut entries = HashMap::new();
    let mut records = 0;
    for line in bytes.split(|&b| b == b'\n') {
        let Ok(record) = serde_json::from_slice::<Record>(line) else {
            continue;
        };
        records += 1;
        match record {
            Record::Put { key, entry } if is_key(&key) => {
                entries.insert(key, entry);
            }
            Record::Served { key, at } => {
                if let Some(entry) = entries.get_mut(&key) {
                    entry.served = entry.served.max(at);
                }
            }
            Record::Drop { key } => {
                entries.remove(&key);
            }
            Record::Put { .. } => {}
        }
    }
    (entries, records)
}

/// The entry directories in `dir`, with their bytes on disk and newest
/// file modification time.
fn entry_dirs(dir: &Path) -> HashMap<String, (u64, Stamp)> {
    let mut found = HashMap::new();
    let Ok(listing) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in listing.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_key(&name) || !entry.path().is_dir() {
            continue;
        }
        let mut bytes = 0u64;
        let mut newest = SystemTime::UNIX_EPOCH;
        for file in std::fs::read_dir(entry.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Ok(meta) = file.metadata() {
                bytes = bytes.saturating_add(meta.len());
                if let Ok(modified) = meta.modified() {
                    newest = newest.max(modified);
                }
            }
        }
        found.insert(name, (bytes, Stamp::from_time(newest)));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn entry(served: u64, bytes: u64) -> Entry {
        Entry {
            fetched: Stamp(served),
            fresh_until: Stamp(served + 3600),
            etag: Some("\"v1\"".into()),
            last_modified: Some(Stamp(1_000)),
            served: Stamp(served),
            bytes,
        }
    }

    fn make_dir(dir: &Path, key: &str, bytes: usize) {
        std::fs::create_dir_all(dir.join(key)).expect("dir");
        std::fs::write(dir.join(key).join("128.jpg"), vec![0u8; bytes]).expect("file");
    }

    #[test]
    fn the_journal_survives_a_restart_and_a_torn_tail() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        for n in 0..3 {
            make_dir(dir, &key(n), 10);
        }
        let mut index = Index::open(dir);
        assert_eq!(index.total(), 30, "found on disk");
        assert!(index.get(&key(0)).expect("rebuilt").is_stale(Stamp::now()));
        index.put(&key(0), entry(5_000, 10));
        index.put(&key(1), entry(6_000, 10));
        index.served(&key(1), Stamp(6_000 + 3600));
        index.remove(&key(2));
        drop(index);
        // A crash mid-append leaves half a line.
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(JOURNAL_FILE))
            .expect("journal");
        journal.write_all(b"{\"op\":\"put\",\"ke").expect("torn");

        let index = Index::open(dir);
        assert_eq!(index.get(&key(0)), Some(&entry(5_000, 10)));
        assert_eq!(index.get(&key(1)).expect("kept").served, Stamp(9_600));
        // Dropped from the index but still on disk: indexed again as stale.
        let rebuilt = index.get(&key(2)).expect("rebuilt");
        assert_eq!(rebuilt.etag, None);
        assert!(rebuilt.is_stale(Stamp::now()));
        assert_eq!(index.total(), 30);
        // The compacted journal has one record per entry and no torn line.
        let text = std::fs::read_to_string(dir.join(JOURNAL_FILE)).expect("read");
        assert_eq!(text.lines().count(), 3);
        assert!(text.ends_with('\n'));
        assert!(!dir.join(COMPACT_FILE).exists());
    }

    #[test]
    fn entries_missing_on_disk_are_forgotten() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        make_dir(dir, &key(1), 4);
        let mut index = Index::open(dir);
        index.put(&key(1), entry(1, 4));
        index.put(&key(2), entry(1, 7));
        assert_eq!(index.total(), 11);
        drop(index);
        let index = Index::open(dir);
        assert!(index.get(&key(2)).is_none());
        assert_eq!(index.total(), 4);
    }

    #[test]
    fn serving_is_journaled_at_most_hourly() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(temp.path());
        index.put(&key(1), entry(10_000, 1));
        let before = index.records;
        index.served(&key(1), Stamp(10_000 + 60));
        assert_eq!(index.records, before);
        assert_eq!(index.get(&key(1)).expect("entry").served, Stamp(10_000));
        index.served(&key(1), Stamp(10_000 + 3600));
        assert_eq!(index.records, before + 1);
        index.served(&key(9), Stamp(99_999));
        assert_eq!(index.records, before + 1, "unknown keys are ignored");
    }

    #[test]
    fn eviction_drops_the_least_recently_served_first() {
        let mut index = Index::detached();
        for n in 0..4u8 {
            index.put(&key(n), entry(u64::from(10 - n) * 1000, 10));
        }
        assert!(index.evict_to(40).is_empty());
        let evicted = index.evict_to(25);
        assert_eq!(evicted, vec![key(3), key(2)]);
        assert_eq!(index.total(), 20);
        // Replacing an entry replaces its bytes.
        index.put(&key(0), entry(1, 3));
        assert_eq!(index.total(), 13);
    }

    #[test]
    fn a_long_journal_is_compacted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(temp.path());
        for round in 0..COMPACT_SLACK + 3 {
            index.put(&key(1), entry(u64::try_from(round).expect("small"), 1));
        }
        assert!(index.records <= 2, "{}", index.records);
        let text = std::fs::read_to_string(temp.path().join(JOURNAL_FILE)).expect("read");
        assert!(text.lines().count() <= 2);
        assert_eq!(Index::open(temp.path()).get(&key(1)), None, "not on disk");
    }

    #[test]
    fn replay_skips_foreign_lines_and_keys() {
        let good = key(7);
        let text = format!(
            "not json\n{{\"op\":\"put\",\"key\":\"../x\",\"entry\":{entry}}}\n\
             {{\"op\":\"put\",\"key\":\"{good}\",\"entry\":{entry}}}\n\
             {{\"op\":\"served\",\"key\":\"{good}\",\"at\":1}}\n\
             {{\"op\":\"what\"}}\n",
            entry = serde_json::to_string(&entry(50, 2)).expect("json"),
        );
        let (entries, records) = replay(text.as_bytes());
        assert_eq!(records, 3);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[&good].served, Stamp(50), "older serve ignored");
        let (entries, _) = replay(format!("{{\"op\":\"drop\",\"key\":\"{good}\"}}").as_bytes());
        assert!(entries.is_empty());
    }

    #[test]
    fn stamps_and_validators() {
        let stamp = Stamp::from_time(SystemTime::UNIX_EPOCH + Duration::from_millis(2_500));
        assert_eq!(stamp, Stamp(2));
        assert_eq!(
            Stamp::from_time(SystemTime::UNIX_EPOCH - Duration::from_secs(1)),
            Stamp(0)
        );
        assert_eq!(stamp.after(Duration::from_secs(3)), Stamp(5));
        assert_eq!(
            Stamp(u64::MAX).after(Duration::from_secs(3)),
            Stamp(u64::MAX)
        );
        assert_eq!(Stamp(1).since(Stamp(5)), Duration::ZERO);
        let validators = entry(1, 1).validators();
        assert_eq!(validators.if_none_match(), Some("\"v1\""));
        assert_eq!(
            validators.last_modified(),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000))
        );
        assert!(!is_key("ABCDEF".repeat(11).get(..64).expect("64")));
    }
}
