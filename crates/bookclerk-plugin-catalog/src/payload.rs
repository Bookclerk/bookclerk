//! Deterministic immutable payload identity for an installed plugin tree.
//!
//! [`payload_root_sha256`] hashes packaged files. Host-owned install receipts
//! (`receipt.json` and its temp/backup names) are excluded. Mutable plugin
//! state lives outside the install tree under `plugin-state/`.
//!
//! The digest proves: these installed bytes match the artifact recorded for this
//! provenance-qualified package. It does **not** prove publisher identity.
//!
//! Hosts recompute it on every plugin discovery, which a busy daemon does
//! many times a minute. Every call walks the tree and checks each entry
//! (symlinks, special files, traversal, added or removed files) again, but
//! on Unix a file whose inode, size, modification time and change time are
//! all unchanged reuses its content digest instead of being read again (see
//! [`FileStat`]).

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::error::{CatalogError, Result};
use crate::extract::sha256_file;

/// How recent a file's last write or status change may be and still have its
/// digest cached.
///
/// A write within the same timestamp tick as the stat could leave both
/// timestamps unchanged, so a file touched this recently is hashed every time
/// until it has been still for this long (git's "racily clean" rule).
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Most file digests kept; the cache is cleared when it would grow past this.
const FILE_DIGEST_CACHE_CAP: usize = 4096;

/// What a cached file digest is valid for: the file's identity and every
/// timestamp a content change moves.
///
/// Rewriting a file moves its `ctime` even when `utimes` puts the `mtime`
/// back, and replacing it by rename changes the inode, so a write through the
/// file system changes this key. A write through a shared memory mapping
/// moves the timestamps only on its first dirtying of a page, so the key
/// relies on installed trees staying read-only once materialized. Only Unix
/// exposes `ctime`; elsewhere every file is hashed each time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStat {
    /// Device the file lives on.
    dev: u64,
    /// Inode number on that device.
    ino: u64,
    /// Length in bytes.
    size: u64,
    /// Last content modification, nanoseconds since the Unix epoch.
    mtime_ns: i128,
    /// Last status change (content, mode, owner, links), nanoseconds since the Unix epoch.
    ctime_ns: i128,
}

impl FileStat {
    /// The cache key of `meta`, or `None` when this platform cannot supply one.
    #[cfg(unix)]
    fn of(meta: &fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let ns = |secs: i64, nanos: i64| i128::from(secs) * 1_000_000_000 + i128::from(nanos);
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
        })
    }

    /// The cache key of `meta`, or `None` when this platform cannot supply one.
    #[cfg(not(unix))]
    fn of(_meta: &fs::Metadata) -> Option<Self> {
        None
    }

    /// True when the file was written or changed within `window` of `now`.
    fn is_racy(&self, now: SystemTime, window: Duration) -> bool {
        let Ok(since_epoch) = now.duration_since(SystemTime::UNIX_EPOCH) else {
            return true;
        };
        let now_ns = i128::try_from(since_epoch.as_nanos()).unwrap_or(i128::MAX);
        let window_ns = i128::try_from(window.as_nanos()).unwrap_or(i128::MAX);
        self.mtime_ns.max(self.ctime_ns) > now_ns.saturating_sub(window_ns)
    }
}

/// Content digests of payload files, keyed by path and valid for one [`FileStat`].
fn file_digest_cache() -> &'static Mutex<HashMap<PathBuf, (FileStat, String)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (FileStat, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// SHA-256 of the payload file at `path`, read only when its [`FileStat`]
/// changed since the last read.
///
/// The digest is cached only when the file's stat is identical before and
/// after the read and the file has not changed within [`RACY_WINDOW`], so a
/// write during or just before the read is never remembered.
///
/// # Errors
///
/// Returns an error when the file cannot be read or stat'ed.
fn payload_file_sha256(path: &Path, meta: &fs::Metadata) -> Result<String> {
    payload_file_sha256_within(path, meta, RACY_WINDOW)
}

/// [`payload_file_sha256`] with the racy window as an argument.
///
/// # Errors
///
/// Returns an error when the file cannot be read or stat'ed.
fn payload_file_sha256_within(path: &Path, meta: &fs::Metadata, racy: Duration) -> Result<String> {
    let Some(before) = FileStat::of(meta) else {
        return sha256_file(path);
    };
    {
        let cache = file_digest_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((stat, digest)) = cache.get(path) {
            if *stat == before {
                return Ok(digest.clone());
            }
        }
    }
    let now = SystemTime::now();
    #[cfg(test)]
    tests::FILE_READS.with(|reads| reads.set(reads.get() + 1));
    let digest = sha256_file(path)?;
    let after = FileStat::of(&fs::symlink_metadata(path)?);
    if after.as_ref() == Some(&before) && !before.is_racy(now, racy) {
        let mut cache = file_digest_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= FILE_DIGEST_CACHE_CAP {
            cache.clear();
        }
        cache.insert(path.to_path_buf(), (before, digest.clone()));
    }
    Ok(digest)
}

/// Directory / file names excluded from the immutable payload (host-owned
/// receipts only). Packaged `data/` and `tmp/` are hashed: mutable plugin
/// state lives under `$FILES_DIR/plugin-state/<PluginKey fs-id>/`, not inside
/// the install tree.
pub const PAYLOAD_SKIP_NAMES: &[&str] = &["receipt.json", "receipt.json.bak", "receipt.json.tmp"];

/// SHA-256 of `plugin.toml` at `plugin_root`.
///
/// # Errors
///
/// Returns an error when the file cannot be read.
pub fn manifest_sha256(plugin_root: &Path) -> Result<String> {
    sha256_file(&plugin_root.join("plugin.toml"))
}

/// Deterministic payload root: sorted relative path + type + content hash.
///
/// Encoding (UTF-8, `\n`-terminated records, paths use `/`):
///
/// - file: `f\t{rel}\t{sha256}`
/// - directory: `d\t{rel}` (children listed separately)
///
/// Symlinks and path traversal (`..`) are rejected. The root directory itself
/// is not listed; an empty tree hashes the empty byte string.
///
/// # Errors
///
/// Returns an error on I/O failure, traversal, or a symlink in the payload.
pub fn payload_root_sha256(plugin_root: &Path) -> Result<String> {
    let mut records = Vec::new();
    collect_payload_records(plugin_root, plugin_root, &mut records)?;
    records.sort();
    let mut hasher = Sha256::new();
    for rec in &records {
        hasher.update(rec.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Collects sorted payload records under `dir` (relative to `root`).
fn collect_payload_records(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    if !dir.starts_with(root) {
        return Err(CatalogError::message(format!(
            "payload dir {} escaped {}",
            dir.display(),
            root.display()
        )));
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| CatalogError::message(format!("read {}: {e}", dir.display())))?
        .map(|e| {
            e.map(|e| e.path())
                .map_err(|err| CatalogError::message(err.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        if !path.starts_with(root) {
            return Err(CatalogError::message(format!(
                "payload path {} escaped {}",
                path.display(),
                root.display()
            )));
        }
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            CatalogError::message(format!("non-UTF-8 payload path {}", path.display()))
        })?;
        if dir == root && PAYLOAD_SKIP_NAMES.contains(&name) {
            continue;
        }
        let rel = path.strip_prefix(root).map_err(|_| {
            CatalogError::message(format!(
                "payload path {} escaped {}",
                path.display(),
                root.display()
            ))
        })?;
        let rel_str = normalize_rel(rel)?;
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(CatalogError::message(format!(
                "refusing symlink in plugin payload: {rel_str}"
            )));
        }
        if meta.is_dir() {
            if !rel_str.is_empty() {
                out.push(format!("d\t{rel_str}"));
            }
            collect_payload_records(root, &path, out)?;
        } else if meta.is_file() {
            let digest = payload_file_sha256(&path, &meta)?;
            out.push(format!("f\t{rel_str}\t{digest}"));
        } else {
            return Err(CatalogError::message(format!(
                "refusing special file in plugin payload: {rel_str}"
            )));
        }
    }
    Ok(())
}

/// Normalizes a relative payload path to `/`-separated UTF-8 without traversal.
fn normalize_rel(rel: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => {
                let s = c.to_str().ok_or_else(|| {
                    CatalogError::message("non-UTF-8 path component in plugin payload")
                })?;
                if s == ".." || s.contains('\0') || s.contains('/') || s.contains('\\') {
                    return Err(CatalogError::message(format!(
                        "refusing ambiguous payload path {}",
                        rel.display()
                    )));
                }
                parts.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CatalogError::message(format!(
                    "refusing traversal/absolute payload path {}",
                    rel.display()
                )));
            }
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::write_file;

    #[test]
    fn payload_hash_is_order_independent_and_skips_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(&root.join("plugin.toml"), b"id = \"echo\"\n").unwrap();
        write_file(&root.join("bin/guest"), b"#!/bin/sh\n").unwrap();
        write_file(&root.join("data/secret"), b"nope").unwrap();
        write_file(&root.join("tmp/scratch"), b"nope").unwrap();
        write_file(&root.join("receipt.json"), b"{}").unwrap();

        let a = payload_root_sha256(root).unwrap();
        let manifest = manifest_sha256(root).unwrap();
        assert_eq!(manifest.len(), 64);

        // Reverse-create equivalent tree, including formerly skipped packaged paths.
        let dir2 = tempfile::tempdir().unwrap();
        let root2 = dir2.path();
        write_file(&root2.join("bin/guest"), b"#!/bin/sh\n").unwrap();
        write_file(&root2.join("plugin.toml"), b"id = \"echo\"\n").unwrap();
        write_file(&root2.join("data/secret"), b"nope").unwrap();
        write_file(&root2.join("tmp/scratch"), b"nope").unwrap();
        let b = payload_root_sha256(root2).unwrap();
        assert_eq!(a, b);

        // Mutating a packaged file under data/ changes identity; receipt does not.
        write_file(&root.join("receipt.json"), b"{changed}").unwrap();
        assert_eq!(a, payload_root_sha256(root).unwrap());
        write_file(&root.join("data/secret"), b"changed").unwrap();
        assert_ne!(a, payload_root_sha256(root).unwrap());
    }

    #[test]
    fn payload_hash_covers_packaged_data_and_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(&root.join("plugin.toml"), b"id = \"echo\"\n").unwrap();
        write_file(&root.join("data/keep"), b"one").unwrap();
        let before = payload_root_sha256(root).unwrap();
        write_file(&root.join("data/keep"), b"two").unwrap();
        assert_ne!(before, payload_root_sha256(root).unwrap());
        write_file(&root.join("tmp/scratch"), b"x").unwrap();
        assert_ne!(before, payload_root_sha256(root).unwrap());
    }

    /// Digest of `path` through the cache with no racy window, and whether
    /// the cache now holds it.
    fn cached_digest(path: &Path) -> (String, bool) {
        let meta = fs::symlink_metadata(path).unwrap();
        let digest = payload_file_sha256_within(path, &meta, Duration::ZERO).unwrap();
        let held = file_digest_cache()
            .lock()
            .unwrap()
            .get(path)
            .is_some_and(|(_, cached)| *cached == digest);
        (digest, held)
    }

    std::thread_local! {
        /// Payload files this test thread has read through the digest cache.
        pub(super) static FILE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Change time of `path`, in nanoseconds since the Unix epoch.
    #[cfg(unix)]
    fn ctime_ns(path: &Path) -> i128 {
        FileStat::of(&fs::metadata(path).unwrap()).unwrap().ctime_ns
    }

    /// Writes `bytes` to `path` until the write lands on a later change-time
    /// tick than `before`, so a coarse-timestamp file system still shows it.
    #[cfg(unix)]
    fn rewrite_on_a_later_tick(path: &Path, bytes: &[u8], before: i128) {
        for _ in 0..300 {
            write_file(path, bytes).unwrap();
            if ctime_ns(path) != before {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the change time of {} never moved", path.display());
    }

    #[cfg(unix)]
    #[test]
    fn a_cached_digest_follows_a_same_size_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("guest");
        write_file(&file, b"one").unwrap();
        let (first, held) = cached_digest(&file);
        assert!(held, "a still file is cached");
        let reads = FILE_READS.with(std::cell::Cell::get);
        assert_eq!(cached_digest(&file).0, first);
        assert_eq!(
            FILE_READS.with(std::cell::Cell::get),
            reads,
            "a cache hit reads nothing"
        );

        rewrite_on_a_later_tick(&file, b"two", ctime_ns(&file));
        let (second, _) = cached_digest(&file);
        assert_ne!(first, second);
        assert_eq!(second, sha256_file(&file).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_restored_mtime_does_not_hide_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("guest");
        write_file(&file, b"one").unwrap();
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        let (first, held) = cached_digest(&file);
        assert!(held);

        rewrite_on_a_later_tick(&file, b"two", ctime_ns(&file));
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), mtime);
        let (second, _) = cached_digest(&file);
        assert_ne!(first, second, "ctime moved, so the file is read again");
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("guest");
        write_file(&file, b"one").unwrap();
        let (first, _) = cached_digest(&file);
        let staged = dir.path().join("staged");
        write_file(&staged, b"two").unwrap();
        fs::rename(&staged, &file).unwrap();
        assert_ne!(first, cached_digest(&file).0);
    }

    #[test]
    fn a_racy_file_is_hashed_but_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("guest");
        write_file(&file, b"fresh").unwrap();
        let meta = fs::symlink_metadata(&file).unwrap();
        let digest = payload_file_sha256_within(&file, &meta, Duration::from_secs(3600)).unwrap();
        assert_eq!(digest, sha256_file(&file).unwrap());
        assert!(file_digest_cache().lock().unwrap().get(&file).is_none());
    }

    #[test]
    fn payload_hash_rejects_dotdot_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("plugin.toml"), b"x").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", dir.path().join("link")).unwrap();
            let err = payload_root_sha256(dir.path()).unwrap_err();
            assert!(err.to_string().contains("symlink"), "{err}");
        }
        let err = normalize_rel(Path::new("../evil")).unwrap_err();
        assert!(err.to_string().contains("traversal") || err.to_string().contains("refusing"));
    }
}
