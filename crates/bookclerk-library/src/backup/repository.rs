//! Content-addressed backup object repository.
//!
//! Layout (under `$BOOKCLERK_FILES_DIR/backups/`):
//!
//! ```text
//! manifests/<recovery-point-id>.json
//! manifests/<recovery-point-id>.sha256
//! objects/ab/cdef...   # SHA-256 hex of uncompressed canonical JSON
//! ```
//!
//! Objects are immutable. A recovery point restores from its own manifest
//! without replaying older manifests. Garbage collection deletes objects that
//! no retained manifest references.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;

use super::encode::{
    decode_canonical_object, encode_canonical_object, sha256_hex, unwrap_stored_object,
    wrap_stored_object, CanonicalObject,
};
use super::{BackupManifest, BACKUPS_DIR};
use crate::error::{LibraryError, Result};

/// On-disk backup repository rooted at `files_dir/backups`.
#[derive(Debug, Clone)]
pub struct BackupRepository {
    /// `$BOOKCLERK_FILES_DIR/backups`.
    root: PathBuf,
    /// In-process reentrant exclusive lock; flocks `backups/.lock` at depth 0.
    lock: Arc<RepoLock>,
}

/// In-process reentrant exclusive lock; flocks `backups/.lock` at depth 0.
#[derive(Debug)]
struct RepoLock {
    /// Mutex protecting owner/depth and the flocked file handle.
    inner: Mutex<RepoLockInner>,
    /// Wakes threads waiting for the in-process exclusive lock.
    cv: Condvar,
}

/// Owner, nesting depth, and optional flocked `backups/.lock` handle.
#[derive(Debug)]
struct RepoLockInner {
    /// Open lock file while `depth > 0`.
    file: Option<File>,
    /// Same-thread reentry count.
    depth: u32,
    /// Thread that currently holds the in-process lock.
    owner: Option<ThreadId>,
}

/// Held exclusive backup-repository lock (publication vs GC/prune).
pub struct RepoLockGuard<'a> {
    /// Repository whose lock this guard releases on drop.
    repo: &'a BackupRepository,
}

impl Drop for RepoLockGuard<'_> {
    fn drop(&mut self) {
        let mut inner = self
            .repo
            .lock
            .inner
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        inner.depth = inner.depth.saturating_sub(1);
        if inner.depth == 0 {
            inner.owner = None;
            inner.file = None;
            self.repo.lock.cv.notify_all();
        }
    }
}

impl BackupRepository {
    /// Opens (creating) the repository under `files_dir/backups`.
    ///
    /// # Errors
    ///
    /// Returns when the backups directory cannot be created.
    pub fn open(files_dir: &Path) -> Result<Self> {
        Self::open_root(&files_dir.join(BACKUPS_DIR))
    }

    /// Opens `root` as a backups repository (`manifests/` + `objects/`).
    ///
    /// # Errors
    ///
    /// Returns when the directories cannot be created.
    pub fn open_root(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.join("manifests"))?;
        fs::create_dir_all(root.join("objects"))?;
        Ok(Self {
            root: root.to_path_buf(),
            lock: Arc::new(RepoLock {
                inner: Mutex::new(RepoLockInner {
                    file: None,
                    depth: 0,
                    owner: None,
                }),
                cv: Condvar::new(),
            }),
        })
    }

    /// Repository root (`…/backups`).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Exclusive lock covering object publication and GC/prune.
    ///
    /// Same-thread reentry is allowed so [`Self::put_object`] can nest under a
    /// backup that already holds the lock. Other threads wait. Other processes
    /// block on `backups/.lock`.
    ///
    /// # Errors
    ///
    /// Returns when the lock file cannot be created or flocked.
    pub fn lock_exclusive(&self) -> Result<RepoLockGuard<'_>> {
        let me = std::thread::current().id();
        let mut inner = self
            .lock
            .inner
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        loop {
            if inner.depth == 0 {
                let file = OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .truncate(false)
                    .open(self.root.join(".lock"))?;
                file.lock()?;
                inner.file = Some(file);
                inner.depth = 1;
                inner.owner = Some(me);
                return Ok(RepoLockGuard { repo: self });
            }
            if inner.owner == Some(me) {
                inner.depth = inner.depth.saturating_add(1);
                return Ok(RepoLockGuard { repo: self });
            }
            inner = self
                .lock
                .cv
                .wait(inner)
                .unwrap_or_else(|err| err.into_inner());
        }
    }

    /// Writes `object` if missing. Returns the uncompressed SHA-256 hex.
    ///
    /// The write uses a unique same-directory temp file, fsyncs it, then
    /// installs with a no-clobber hard link. An existing object is reused only
    /// after its bytes verify; a truncated or corrupt file is replaced.
    ///
    /// # Errors
    ///
    /// Returns when encoding, compression, or the filesystem write fails.
    pub fn put_object(&self, object: &CanonicalObject) -> Result<String> {
        let _lock = self.lock_exclusive()?;
        self.put_object_locked(object)
    }

    /// Writes `object` assuming [`Self::lock_exclusive`] is already held.
    fn put_object_locked(&self, object: &CanonicalObject) -> Result<String> {
        let uncompressed = encode_canonical_object(object)?;
        let digest = sha256_hex(&uncompressed);
        let path = self.object_path(&digest)?;
        if path.is_file() {
            match self.get_object(&digest) {
                Ok(_) => return Ok(digest),
                Err(_) => {
                    fs::remove_file(&path)?;
                }
            }
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            fsync_dir(parent);
        }
        let stored = wrap_stored_object(&uncompressed)?;
        let tmp = unique_tmp_path(&path);
        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(&stored)?;
            f.sync_all()?;
        }
        install_no_clobber(&tmp, &path)?;
        if path.is_file() {
            match self.get_object(&digest) {
                Ok(_) => Ok(digest),
                Err(err) => {
                    let _ = fs::remove_file(&path);
                    Err(err)
                }
            }
        } else {
            Err(LibraryError::Schema(format!(
                "backup object `{digest}` was not installed"
            )))
        }
    }

    /// Loads and verifies one object by uncompressed digest.
    ///
    /// # Errors
    ///
    /// Returns when the object is missing, corrupt, or the digest mismatches.
    pub fn get_object(&self, digest: &str) -> Result<CanonicalObject> {
        let path = self.object_path(digest)?;
        if !path.is_file() {
            return Err(LibraryError::Schema(format!(
                "backup object `{digest}` is missing"
            )));
        }
        let stored = fs::read(&path)?;
        let uncompressed = unwrap_stored_object(&stored)?;
        let actual = sha256_hex(&uncompressed);
        if actual != digest {
            return Err(LibraryError::Schema(format!(
                "backup object `{digest}` content does not match its address (`{actual}`)"
            )));
        }
        decode_canonical_object(&uncompressed)
    }

    /// True when an object file exists (does not imply the bytes are valid).
    #[must_use]
    pub fn object_exists(&self, digest: &str) -> bool {
        self.object_path(digest)
            .map(|p| p.is_file())
            .unwrap_or(false)
    }

    /// Publishes `manifest` only after all referenced objects exist **and**
    /// verify.
    ///
    /// Writes JSON then a sibling `.sha256` of the exact file bytes. Incomplete
    /// staging uses `manifests/.staging-<id>.json` and is ignored by list/GC.
    ///
    /// # Errors
    ///
    /// Returns when a referenced object is missing or corrupt, or the
    /// filesystem write fails.
    pub fn publish_manifest(&self, manifest: &BackupManifest) -> Result<PathBuf> {
        let _lock = self.lock_exclusive()?;
        for digest in manifest.referenced_objects() {
            self.get_object(&digest).map_err(|err| {
                LibraryError::Schema(format!("cannot publish backup `{}`: {err}", manifest.id))
            })?;
        }
        let json = serde_json::to_vec_pretty(manifest)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("backup manifest json: {err}")))?;
        let dir = self.root.join("manifests");
        fs::create_dir_all(&dir)?;
        let staging = unique_tmp_path(&dir.join(format!("{}.json", manifest.id)));
        let final_path = dir.join(format!("{}.json", manifest.id));
        let hash_path = dir.join(format!("{}.sha256", manifest.id));
        write_tmp_fsync(&staging, &json)?;
        let digest = sha256_hex(&json);
        let hash_tmp = unique_tmp_path(&hash_path);
        write_tmp_fsync(&hash_tmp, format!("{digest}\n").as_bytes())?;
        fs::rename(&hash_tmp, &hash_path)?;
        fsync_dir(&dir);
        fs::rename(&staging, &final_path)?;
        fsync_dir(&dir);
        Ok(final_path)
    }

    /// Reads and integrity-checks a published manifest.
    ///
    /// # Errors
    ///
    /// Returns when the file is missing, JSON is malformed, the sidecar hash
    /// mismatches, or the embedded id does not match the filename.
    pub fn read_manifest(&self, id: &str) -> Result<BackupManifest> {
        if !manifest_id_ok(id) {
            return Err(LibraryError::Schema(format!(
                "backup recovery-point id `{id}` is not a safe path"
            )));
        }
        let path = self.root.join("manifests").join(format!("{id}.json"));
        let hash_path = self.root.join("manifests").join(format!("{id}.sha256"));
        if !path.is_file() {
            return Err(LibraryError::Schema(format!(
                "backup recovery point `{id}` is missing"
            )));
        }
        let json = fs::read(&path)?;
        let expected = fs::read_to_string(&hash_path)
            .map_err(|_| {
                LibraryError::Schema(format!(
                    "backup recovery point `{id}` is missing its integrity sidecar"
                ))
            })?
            .trim()
            .to_ascii_lowercase();
        let actual = sha256_hex(&json);
        if actual != expected {
            return Err(LibraryError::Schema(format!(
                "backup recovery point `{id}` manifest digest mismatch"
            )));
        }
        let manifest: BackupManifest = serde_json::from_slice(&json).map_err(|err| {
            LibraryError::Schema(format!("backup recovery point `{id}` json: {err}"))
        })?;
        if manifest.id != id {
            return Err(LibraryError::Schema(format!(
                "backup manifest id `{}` does not match file `{id}`",
                manifest.id
            )));
        }
        Ok(manifest)
    }

    /// Lists published recovery-point manifests oldest-first. Staging files
    /// and unreadable manifests are skipped.
    #[must_use]
    pub fn list_manifests(&self) -> Vec<BackupManifest> {
        let dir = self.root.join("manifests");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for ent in entries.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            let id = name.trim_end_matches(".json");
            if let Ok(manifest) = self.read_manifest(id) {
                out.push(manifest);
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        out
    }

    /// Lists every published recovery-point manifest, failing if any cannot
    /// be verified. Staging files (dot-prefixed) are ignored.
    ///
    /// Use this for prune/GC. [`Self::list_manifests`] stays best-effort for
    /// non-destructive listing.
    ///
    /// # Errors
    ///
    /// Returns when the manifests directory cannot be read, a published
    /// `*.json` fails [`Self::read_manifest`], or an unexpected non-sidecar
    /// file is present.
    pub fn list_manifests_strict(&self) -> Result<Vec<BackupManifest>> {
        let dir = self.root.join("manifests");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let entries = fs::read_dir(&dir)?;
        let mut out = Vec::new();
        for ent in entries {
            let ent = ent?;
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if name.ends_with(".sha256") {
                continue;
            }
            if !name.ends_with(".json") {
                return Err(LibraryError::Schema(format!(
                    "backup manifests directory contains unexpected file `{name}`"
                )));
            }
            let id = name.trim_end_matches(".json");
            out.push(self.read_manifest(id)?);
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    /// Deletes one published manifest (not its objects). Returns whether a
    /// file was removed.
    ///
    /// # Errors
    ///
    /// Returns when the id is unsafe or unlink fails for a reason other than
    /// not found.
    pub fn delete_manifest(&self, id: &str) -> Result<bool> {
        let _lock = self.lock_exclusive()?;
        if !manifest_id_ok(id) {
            return Err(LibraryError::Schema(format!(
                "backup recovery-point id `{id}` is not a safe path"
            )));
        }
        let json = self.root.join("manifests").join(format!("{id}.json"));
        let hash = self.root.join("manifests").join(format!("{id}.sha256"));
        let mut removed = false;
        for path in [json, hash] {
            match fs::remove_file(&path) {
                Ok(()) => removed = true,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(removed)
    }

    /// Deletes objects not referenced by any retained published manifest.
    ///
    /// Reachability is computed from a **strict** enumeration of published
    /// manifests. An unreadable retained manifest fails closed instead of
    /// treating its objects as garbage.
    ///
    /// # Errors
    ///
    /// Returns when a published manifest cannot be verified, a directory
    /// cannot be read, or an unreferenced object cannot be unlinked.
    pub fn gc_unreferenced_objects(&self) -> Result<usize> {
        let _lock = self.lock_exclusive()?;
        let mut live = BTreeSet::new();
        for manifest in self.list_manifests_strict()? {
            live.extend(manifest.referenced_objects());
        }
        let objects = self.root.join("objects");
        let mut deleted = 0usize;
        let Ok(prefixes) = fs::read_dir(&objects) else {
            return Ok(0);
        };
        for prefix in prefixes.flatten() {
            let path = prefix.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&path) else {
                continue;
            };
            for file in files.flatten() {
                let file_path = file.path();
                let Some(name) = file_path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with('.') {
                    let _ = fs::remove_file(&file_path);
                    continue;
                }
                let Some(dir) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let digest = format!("{dir}{name}");
                if live.contains(&digest) {
                    continue;
                }
                fs::remove_file(&file_path)?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Content-addressed path `objects/<aa>/<rest>` for a SHA-256 hex digest.
    fn object_path(&self, digest: &str) -> Result<PathBuf> {
        if !object_digest_ok(digest) {
            return Err(LibraryError::Schema(format!(
                "backup object digest `{digest}` is not a SHA-256 hex id"
            )));
        }
        Ok(self
            .root
            .join("objects")
            .join(&digest[..2])
            .join(&digest[2..]))
    }
}

/// Unique same-directory temp path (`.{name}.tmp-<pid>-<nonce>-<n>`).
fn unique_tmp_path(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("object");
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nonce = rand::random::<u64>();
    final_path.with_file_name(format!(
        ".{name}.tmp-{}-{nonce:016x}-{n}",
        std::process::id()
    ))
}

/// Writes `bytes` to `path` (must not exist) and `sync_all`s the file.
fn write_tmp_fsync(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Installs `tmp` onto `final_path` without clobbering a winner; fsyncs the parent.
fn install_no_clobber(tmp: &Path, final_path: &Path) -> Result<()> {
    match fs::hard_link(tmp, final_path) {
        Ok(()) => {
            let _ = fs::remove_file(tmp);
            if let Some(parent) = final_path.parent() {
                fsync_dir(parent);
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(tmp);
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_file(tmp);
            Err(err.into())
        }
    }
}

/// Best-effort directory fsync so a rename/hard-link is durable.
fn fsync_dir(dir: &Path) {
    if let Ok(file) = File::open(dir) {
        let _ = file.sync_all();
    }
}

/// True when `digest` is a 64-character SHA-256 hex id.
fn object_digest_ok(digest: &str) -> bool {
    digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit())
}

/// True when `id` is a safe recovery-point file name (no path separators).
fn manifest_id_ok(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('/')
        && !id.contains('\\')
        && id != "."
        && id != ".."
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
