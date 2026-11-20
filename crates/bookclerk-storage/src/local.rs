//! Local filesystem storage backend.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use filetime::{set_file_times, FileTime};
use sha2::Digest;
use tokio::fs;

use crate::bounded::{
    clamp_page_limit, ensure_scalar_len, normalize_range, parse_sha256_hex, read_scalar_body,
    reject_scalar_hint, ReadSpan, TempGuard, MAX_SCALAR_OBJECT_BYTES,
};
use crate::error::{Result, StorageError};
use crate::fs_kind::{self, FsKind};
use crate::intent::{
    require_disjoint_prefixes, ConditionalPutResult, HintedListPage, KeyResult, ListRequest,
    MoveOptions, MovePrefixPage, MoveResult, StorageCapabilities, WriteCondition,
};
use crate::list_index::list_page_indexed;
use crate::normalize_prefix;
use crate::traits::{bookclerk_meta_sidecar_key, ObjectMeta, ObjectProbe, StorageBackend};

/// Stores objects under a root directory; keys map to relative paths.
///
/// An optional key prefix is prepended to every key (same model as S3),
/// so library `storage_key` values stay relative to the prefix.
#[derive(Debug, Clone)]
pub struct LocalFsBackend {
    /// Filesystem root; object keys are resolved under this directory.
    root: PathBuf,
    /// Normalized key prefix (same model as S3); stripped from list results.
    prefix: String,
    /// Test override for [`StorageBackend::scan_placement`].
    placement_override: Option<String>,
    /// Serializes `ifMatch` writes (and `ifNoneMatch` writes on a file
    /// system without hard links) through this backend and its clones, so
    /// two of them never both pass the same check.
    check_then_write: Arc<tokio::sync::Mutex<()>>,
    /// Test override for the file-system kind of every path (see
    /// [`Self::with_fs_kind`]); `None` classifies each path.
    fs_kind_override: Option<FsKind>,
    /// The change feed, when [`Self::with_watch`] started one; shared by
    /// clones.
    #[cfg(feature = "watch")]
    watch: Option<Arc<crate::local_watch::LocalWatch>>,
}

impl LocalFsBackend {
    /// What every local backend does natively: moves are renames, batches
    /// run in one call, and conditional writes are checked here. A backend
    /// built with [`Self::with_watch`] (feature `watch`) also has a change
    /// feed.
    pub const CAPABILITIES: StorageCapabilities = StorageCapabilities {
        move_object: true,
        move_many: true,
        delete_many: true,
        head_many: true,
        conditional_write: true,
        changes: false,
    };

    /// Create a backend rooted at `root` with no key prefix.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn new(root: PathBuf) -> Result<Self> {
        Self::with_prefix(root, "")
    }

    /// Create a backend rooted at `root` with an optional key prefix
    /// (e.g. `library/`). The prefix directory is created when needed.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn with_prefix(root: PathBuf, prefix: &str) -> Result<Self> {
        let prefix = normalize_prefix(prefix);
        // Reject ParentDir (and absolute/root components) in the *prefix* before
        // any join/mkdir. Operator `root` may still contain `..` and is
        // canonicalized below.
        if !prefix.is_empty() {
            validate_key(prefix.trim_end_matches('/'))?;
        }
        // Operator-configured storage root may include lexical `..` (joined onto
        // `files_dir` by config resolution). Reject NUL, create, then canonicalize
        // so later joins use a realpath identity for containment.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            if root.as_os_str().as_bytes().contains(&0) {
                return Err(StorageError::InvalidKey(root.display().to_string()));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            if root.as_os_str().encode_wide().any(|c| c == 0) {
                return Err(StorageError::InvalidKey(root.display().to_string()));
            }
        }
        std::fs::create_dir_all(&root)?;
        let root = std::fs::canonicalize(&root).map_err(StorageError::Io)?;
        if !prefix.is_empty() {
            let prefix_rel = prefix.trim_end_matches('/');
            let prefix_dir = root.join(prefix_rel);
            if !prefix_dir.starts_with(&root) {
                return Err(StorageError::InvalidKey(prefix));
            }
            // Walk ancestors under root and refuse symlink components before mkdir
            // so `root/link -> /outside` + missing child cannot create outside.
            let mut cursor = root.clone();
            for comp in Path::new(prefix_rel).components() {
                cursor = cursor.join(comp.as_os_str());
                if !cursor.starts_with(&root) {
                    return Err(StorageError::InvalidKey(prefix.clone()));
                }
                match std::fs::symlink_metadata(&cursor) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(StorageError::InvalidKey(format!(
                            "refusing symlink in storage prefix path: {prefix}"
                        )));
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            std::fs::create_dir_all(&prefix_dir)?;
            let prefix_canon = std::fs::canonicalize(&prefix_dir).map_err(StorageError::Io)?;
            if !prefix_canon.starts_with(&root) {
                return Err(StorageError::InvalidKey(prefix));
            }
        }
        Ok(Self {
            root,
            prefix,
            placement_override: None,
            check_then_write: Arc::new(tokio::sync::Mutex::new(())),
            fs_kind_override: None,
            #[cfg(feature = "watch")]
            watch: None,
        })
    }

    /// Starts a change feed for this backend's root (below its prefix),
    /// served by [`StorageBackend::changes`] for as long as the backend or
    /// a clone of it lives.
    ///
    /// The watcher runs only on a local disk; on a network file system or a
    /// FUSE mount (see [`crate::fs_kind`]), or when the operating system
    /// refuses it, every page says the feed may have missed changes, so the
    /// caller lists instead. Call it after [`Self::with_fs_kind`] for the
    /// override to apply.
    #[cfg(feature = "watch")]
    #[must_use]
    pub fn with_watch(mut self) -> Self {
        let dir = self.root.join(self.prefix.trim_end_matches('/'));
        self.watch = Some(Arc::new(crate::local_watch::LocalWatch::start(
            &dir,
            "",
            self.fs_kind_override,
        )));
        self
    }

    /// Treats every path as living on a file system of `kind`, instead of
    /// classifying it.
    ///
    /// Tests use it to drive the network-mount paths on a local disk.
    /// Production backends leave it unset.
    #[must_use]
    pub fn with_fs_kind(mut self, kind: FsKind) -> Self {
        self.fs_kind_override = Some(kind);
        self
    }

    /// The file-system kind of `path` (see [`kind_of`]), or the override.
    async fn fs_kind(&self, path: &Path) -> FsKind {
        match self.fs_kind_override {
            Some(kind) => kind,
            None => kind_of(path).await,
        }
    }

    /// The change signature of the file at `path` whose metadata is `meta`.
    async fn signature_at(&self, path: &Path, meta: &std::fs::Metadata) -> Option<String> {
        signature_of(path, meta, self.fs_kind(path).await).await
    }

    /// Overrides the host placement reported for scan adoption.
    ///
    /// Production backends leave this unset and use [`crate::node_placement_id`].
    #[must_use]
    pub fn with_scan_placement(mut self, placement: impl Into<String>) -> Self {
        self.placement_override = Some(placement.into());
        self
    }

    /// True when [`Self::with_watch`] started a change feed (live or not).
    fn has_watch(&self) -> bool {
        #[cfg(feature = "watch")]
        {
            self.watch.is_some()
        }
        #[cfg(not(feature = "watch"))]
        {
            false
        }
    }

    /// Prepends the storage prefix to `key` (no-op when the prefix is empty).
    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{key}", self.prefix)
        }
    }

    /// Maps a key to an absolute path, rejecting `..` and escapes above `root`.
    fn resolve(&self, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        let full = self.full_key(key);
        if Path::new(&full)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(StorageError::InvalidKey(key.into()));
        }
        let path = self.root.join(&full);
        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        if let Ok(canonical) = path.canonicalize() {
            if !canonical.starts_with(&canonical_root) {
                return Err(StorageError::InvalidKey(key.into()));
            }
            return Ok(canonical);
        }
        // Canonicalize failed: distinguish a dangling/unresolvable symlink from a
        // genuinely missing component. Treating a dangling leaf as "missing" lets
        // later `fs::write` follow the link and create the outside target.
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(StorageError::InvalidKey(format!(
                    "refusing dangling or unresolvable symlink: {key}"
                )));
            }
            Ok(_) => {
                return Err(StorageError::InvalidKey(format!(
                    "could not canonicalize existing path for key: {key}"
                )));
            }
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(StorageError::Io(err));
            }
            Err(_) => {}
        }
        // Missing leaf/intermediates: canonicalize nearest existing ancestor and
        // rejoin the suffix (rejects symlink-parent escapes).
        let mut suffix = Vec::new();
        let mut cursor = path.clone();
        loop {
            match cursor.canonicalize() {
                Ok(canon) => {
                    if !canon.starts_with(&canonical_root) {
                        return Err(StorageError::InvalidKey(key.into()));
                    }
                    let mut out = canon;
                    for part in suffix.iter().rev() {
                        out.push(part);
                    }
                    if !out.starts_with(&canonical_root) {
                        return Err(StorageError::InvalidKey(key.into()));
                    }
                    return Ok(out);
                }
                Err(canon_err) => {
                    match std::fs::symlink_metadata(&cursor) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            return Err(StorageError::InvalidKey(format!(
                                "refusing dangling or unresolvable symlink in key path: {key}"
                            )));
                        }
                        Ok(_) => {
                            return Err(StorageError::InvalidKey(format!(
                                "could not canonicalize path for key {key}: {canon_err}"
                            )));
                        }
                        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                            return Err(StorageError::Io(err));
                        }
                        Err(_) => {}
                    }
                    let name = cursor
                        .file_name()
                        .ok_or_else(|| StorageError::InvalidKey(key.into()))?;
                    suffix.push(name.to_os_string());
                    match cursor.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => {
                            cursor = parent.to_path_buf();
                        }
                        _ => return Err(StorageError::InvalidKey(key.into())),
                    }
                }
            }
        }
    }
}

/// A file's modification time in milliseconds since the Unix epoch, when the
/// platform reports one at or after the epoch.
fn modified_ms(meta: &std::fs::Metadata) -> Option<u64> {
    let since = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    u64::try_from(since.as_millis()).ok()
}

/// [`file_signature`] for the file at `path`, adding its file index on a
/// local Windows disk.
///
/// Windows stamps file times from a clock that can tick only every few
/// milliseconds, so two same-size writes in one tick would otherwise get one
/// signature and an `ifMatch` check could not tell them apart. Each write
/// publishes a new file by rename, which gets a new index, as a new inode
/// does on Unix.
async fn signature_of(path: &Path, meta: &std::fs::Metadata, kind: FsKind) -> Option<String> {
    #[cfg(windows)]
    if kind.is_local() {
        let path = path.to_path_buf();
        let index = tokio::task::spawn_blocking(move || fs_kind::file_index(&path))
            .await
            .ok()
            .flatten();
        if let Some(index) = index {
            return file_signature(meta, kind).map(|base| format!("{index:x}-{base}"));
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    file_signature(meta, kind)
}

/// A file's change signature, also used as its etag: size and modification
/// time, plus inode and status-change time on a local Unix disk (and the
/// file index on a local Windows disk, see [`signature_of`]). Reading the
/// file leaves it unchanged; writing, replacing, renaming onto it or touching
/// it changes it. `None` when the platform reports no modification time.
///
/// On network and FUSE file systems (`kind` not local) inode numbers and
/// status-change times can change after a remount or a cache eviction
/// without the file changing, so the signature there is size and
/// modification time only.
fn file_signature(meta: &std::fs::Metadata, kind: FsKind) -> Option<String> {
    #[cfg(unix)]
    if kind.is_local() {
        use std::os::unix::fs::MetadataExt;
        return Some(format!(
            "{:x}-{:x}-{}.{:09}-{}.{:09}",
            meta.ino(),
            meta.size(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec()
        ));
    }
    #[cfg(not(unix))]
    let _ = kind;
    let since = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(format!(
        "{:x}-{}.{:09}",
        meta.len(),
        since.as_secs(),
        since.subsec_nanos()
    ))
}

/// Classifies the file system holding `path` (or, when it does not exist
/// yet, its nearest existing ancestor) off the async runtime, since a
/// `statfs` on a stalled network mount can block.
async fn kind_of(path: &Path) -> FsKind {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || fs_kind::classify_nearest(&path))
        .await
        .unwrap_or(FsKind::Unknown)
}

/// The device id of the file system holding `path`, off the async runtime.
async fn device_of(path: &Path) -> Option<u64> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || fs_kind::device_of(&path))
        .await
        .ok()
        .flatten()
}

/// Renames `from` to `to`, settling the answer a network file system can
/// give for a retried request: a `NotFound` while `from` is gone and `to`
/// exists means an earlier attempt already did the rename.
///
/// # Errors
///
/// Returns the rename's I/O error otherwise.
async fn rename_settled(from: &Path, to: &Path) -> std::io::Result<()> {
    match fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let from_gone = matches!(fs::symlink_metadata(from).await, Err(e) if e.kind() == std::io::ErrorKind::NotFound);
            if from_gone && fs::symlink_metadata(to).await.is_ok() {
                Ok(())
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

/// True when `a` and `b` are names of one file (same device and inode on
/// Unix). Always false elsewhere, where callers only reach this after a
/// failed hard link, which left no second name.
async fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a).await, fs::metadata(b).await) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

/// Flushes a directory's entries to disk after a rename into it, so the new
/// name survives a crash. Best effort: some file systems refuse to sync a
/// directory, and Windows has no directory handle to sync.
async fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = fs::File::open(dir).await {
        let _ = handle.sync_all().await;
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Maps a `NotFound` I/O error to [`StorageError::NotFound`] for `key`.
fn io_for(key: &str) -> impl FnOnce(std::io::Error) -> StorageError + '_ {
    move |err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            StorageError::NotFound(key.into())
        } else {
            StorageError::Io(err)
        }
    }
}

/// Metadata for `path`, or `None` when nothing is there.
async fn metadata_if_present(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match fs::metadata(path).await {
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(StorageError::Io(err)),
    }
}

/// Rejects empty keys, absolute keys, and any `ParentDir` segment.
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('/') {
        return Err(StorageError::InvalidKey(key.into()));
    }
    // On Windows a `:` is never part of a file name: `C:x` is a path on
    // another drive and `name:x` an NTFS alternate data stream of `name`
    // (possibly another title's file). The Windows naming rules replace
    // `:` in every title, so only a key planned elsewhere (a host naming
    // with POSIX rules) can carry one, and it is refused here rather than
    // written somewhere else.
    if cfg!(windows) && key.contains(':') {
        return Err(StorageError::InvalidKey(key.into()));
    }
    if Path::new(key).components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        return Err(StorageError::InvalidKey(key.into()));
    }
    Ok(())
}

#[async_trait]
impl StorageBackend for LocalFsBackend {
    fn name(&self) -> &'static str {
        "local"
    }

    fn instance_id(&self) -> String {
        format!("local:{}:{}", self.root.display(), self.prefix)
    }

    fn scan_placement(&self) -> Option<String> {
        Some(
            self.placement_override
                .clone()
                .unwrap_or_else(crate::node_placement_id),
        )
    }

    fn supports_server_copy(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            changes: self.has_watch(),
            ..Self::CAPABILITIES
        }
    }

    /// The watcher's journal (see [`Self::with_watch`]); `None` without
    /// one.
    async fn changes(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Option<crate::ChangeFeedPage>> {
        #[cfg(feature = "watch")]
        if let Some(watch) = &self.watch {
            return watch.changes(cursor, limit).map(Some);
        }
        let _ = (cursor, limit);
        Ok(None)
    }

    /// Renames the file (and its meta sidecar).
    ///
    /// - Source and target on different devices (a folder inside the
    ///   library can be another mount): [`crate::intent::fallback_move`],
    ///   which copies, syncs, verifies and then deletes.
    /// - Both on a local disk: with `no_overwrite` the file is hard-linked
    ///   to the target first, which fails when anything is already there,
    ///   and then unlinked from the source; the move is atomic. Without
    ///   hard links it falls back as above.
    /// - On a network or FUSE file system: no hard links. `no_overwrite`
    ///   checks the target just before the rename under the backend's
    ///   check-then-write lock, and the move reports `atomic: false`.
    ///
    /// A retried network rename that answers "not found" after it already
    /// happened counts as done.
    async fn move_object(&self, from: &str, to: &str, options: &MoveOptions) -> Result<MoveResult> {
        let src = self.resolve(from)?;
        let dest = self.resolve(to)?;
        let Some(source) = metadata_if_present(&src).await? else {
            if src != dest && metadata_if_present(&dest).await?.is_some() {
                return Ok(MoveResult {
                    atomic: true,
                    already_moved: true,
                });
            }
            return Err(StorageError::NotFound(from.into()));
        };
        if !source.is_file() {
            return Err(StorageError::InvalidKey(format!("{from} is not an object")));
        }
        if let Some(expected) = &options.if_source_match {
            if self.signature_at(&src, &source).await.as_deref() != Some(expected.as_str()) {
                return Err(StorageError::Conflict(format!(
                    "{from} changed since it was read"
                )));
            }
        }
        if src == dest {
            return Ok(MoveResult {
                atomic: true,
                already_moved: false,
            });
        }
        let dest_dir = dest.parent().unwrap_or(&dest).to_path_buf();
        fs::create_dir_all(&dest_dir).await?;
        // A folder inside the library can be another mount. A rename or a
        // hard link cannot cross into it, so go straight to the copy,
        // verify and delete path instead of relying on the error code.
        if let (Some(a), Some(b)) = (device_of(&src).await, device_of(&dest_dir).await) {
            if a != b {
                return crate::intent::fallback_move(self, from, to, options).await;
            }
        }
        let src_dir = src.parent().unwrap_or(&src).to_path_buf();
        let trusted =
            self.fs_kind(&src_dir).await.is_local() && self.fs_kind(&dest_dir).await.is_local();
        if options.no_overwrite && trusted {
            match fs::hard_link(&src, &dest).await {
                Ok(()) => {
                    if let Err(err) = fs::remove_file(&src).await {
                        // The source is still there (on Windows another
                        // program may hold it open): undo the link so the
                        // object keeps exactly one name, and report it.
                        let _ = fs::remove_file(&dest).await;
                        return Err(io_for(from)(err));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(StorageError::Conflict(format!("{to} already exists")));
                }
                // No hard links on this file system.
                Err(_) => return crate::intent::fallback_move(self, from, to, options).await,
            }
        } else {
            let _held = if options.no_overwrite {
                let held = self.check_then_write.lock().await;
                if metadata_if_present(&dest).await?.is_some() {
                    return Err(StorageError::Conflict(format!("{to} already exists")));
                }
                Some(held)
            } else {
                None
            };
            match rename_settled(&src, &dest).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
                    return crate::intent::fallback_move(self, from, to, options).await;
                }
                Err(err) => return Err(io_for(from)(err)),
            }
        }
        self.move_sidecar(from, to).await;
        sync_dir(&dest_dir).await;
        if src_dir != dest_dir {
            sync_dir(&src_dir).await;
        }
        Ok(MoveResult {
            atomic: trusted,
            already_moved: false,
        })
    }

    async fn move_prefix(
        &self,
        from_prefix: &str,
        to_prefix: &str,
        cursor: Option<&str>,
        limit: u32,
        options: &MoveOptions,
    ) -> Result<MovePrefixPage> {
        require_disjoint_prefixes(from_prefix, to_prefix)?;
        let page = self.list_page(from_prefix, cursor, limit).await?;
        let options = MoveOptions {
            if_source_match: None,
            ..options.clone()
        };
        let mut out = MovePrefixPage {
            results: Vec::with_capacity(page.objects.len()),
            next_cursor: page.next_cursor,
            atomic: true,
            objects_moved: 0,
        };
        for object in page.objects {
            let Some(rest) = object.key.strip_prefix(from_prefix) else {
                continue;
            };
            let target = format!("{to_prefix}{rest}");
            match self.move_object(&object.key, &target, &options).await {
                Ok(moved) => {
                    out.atomic &= moved.atomic;
                    out.objects_moved = out.objects_moved.saturating_add(1);
                    out.results.push(KeyResult::from_result(object.key, Ok(())));
                }
                Err(err) => out
                    .results
                    .push(KeyResult::from_result(object.key, Err(err))),
            }
        }
        Ok(out)
    }

    async fn put_stream_conditional(
        &self,
        key: &str,
        body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        meta: ObjectMeta,
        condition: &WriteCondition,
    ) -> Result<ConditionalPutResult> {
        self.write_stream(key, body, meta, Some(condition)).await
    }

    /// Fills each entry's etag, signature and modification time from the
    /// file, and leaves out files modified before `modified_since_ms`.
    async fn list_hinted(&self, request: &ListRequest) -> Result<HintedListPage> {
        let mut page = self
            .list_page(&request.prefix, request.cursor.as_deref(), request.limit)
            .await?;
        let wants_stat = request.with_signatures || request.modified_since_ms.is_some();
        if wants_stat {
            let mut kept = Vec::with_capacity(page.objects.len());
            for mut object in page.objects {
                let Some(meta) = metadata_if_present(&self.resolve(&object.key)?).await? else {
                    // Removed since the listing; the next listing drops it.
                    continue;
                };
                object.size = meta.len();
                object.modified_at_ms = modified_ms(&meta);
                object.signature = self.signature_at(&self.resolve(&object.key)?, &meta).await;
                object.etag.clone_from(&object.signature);
                let older = matches!(
                    (request.modified_since_ms, object.modified_at_ms),
                    (Some(since), Some(at)) if at < since
                );
                if !older {
                    kept.push(object);
                }
            }
            page.objects = kept;
        }
        Ok(HintedListPage {
            page,
            signatures_included: wants_stat,
            modified_since_applied: request.modified_since_ms.is_some(),
        })
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
        ensure_scalar_len(data.len(), MAX_SCALAR_OBJECT_BYTES)?;
        if let Some(expected) = meta.sha256_hex.as_deref() {
            let want = parse_sha256_hex(expected)?;
            let got = sha2::Sha256::digest(data.as_ref());
            if want.as_slice() != got.as_slice() {
                return Err(StorageError::Integrity(
                    "scalar put sha256 does not match the buffer".into(),
                ));
            }
        }
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, &data).await?;
        write_local_meta_sidecar(self, key, &meta).await?;
        Ok(())
    }

    async fn put_file(&self, key: &str, source: &Path, meta: ObjectMeta) -> Result<()> {
        let dest = self.resolve(key)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Prefer a hard link, then a copy, without loading the whole
        // audiobook into RAM. Links are only trusted on a local disk: a
        // retried NFS `LINK` can fail after it succeeded, and copying a file
        // onto a link to itself would truncate it.
        let linked =
            self.fs_kind(&dest).await.is_local() && fs::hard_link(source, &dest).await.is_ok();
        if !linked && !same_file(source, &dest).await {
            fs::copy(source, &dest).await?;
        }
        write_local_meta_sidecar(self, key, &meta).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let path = self.resolve(key)?;
        let info = fs::metadata(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(key.into())
            } else {
                StorageError::Io(err)
            }
        })?;
        reject_scalar_hint(info.len(), MAX_SCALAR_OBJECT_BYTES)?;
        let file = fs::File::open(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(key.into())
            } else {
                StorageError::Io(err)
            }
        })?;
        read_scalar_body(Box::pin(file), MAX_SCALAR_OBJECT_BYTES).await
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let path = self.resolve(key)?;
        Ok(fs::try_exists(&path).await?)
    }

    async fn probe(&self, key: &str) -> Result<ObjectProbe> {
        let path = self.resolve(key)?;
        let file_meta = fs::metadata(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(key.into())
            } else {
                StorageError::Io(err)
            }
        })?;
        let signature = self.signature_at(&path, &file_meta).await;
        let mut probe = ObjectProbe {
            key: key.to_string(),
            size: file_meta.len(),
            content_type: None,
            etag: signature.clone(),
            meta: ObjectMeta {
                content_length: Some(file_meta.len()),
                ..Default::default()
            },
            modified_at_ms: modified_ms(&file_meta),
            signature,
            ..ObjectProbe::default()
        };
        // Cheap sidecar read — never opens the audio body.
        let meta_key = bookclerk_meta_sidecar_key(key);
        if let Ok(bytes) = self.get(&meta_key).await {
            if let Ok(parsed) = serde_json::from_slice::<ObjectMeta>(&bytes) {
                probe.meta.asin = parsed.asin.or(probe.meta.asin);
                probe.meta.title = parsed.title.or(probe.meta.title);
                probe.meta.creation_time = parsed.creation_time.or(probe.meta.creation_time);
                probe.meta.last_write_time = parsed.last_write_time.or(probe.meta.last_write_time);
                probe.meta.sha256_hex = parsed.sha256_hex.or(probe.meta.sha256_hex);
                probe.meta.commit_token = parsed.commit_token.or(probe.meta.commit_token);
                probe.content_type = parsed.content_type.clone().or(probe.content_type);
                probe.meta.content_type = parsed.content_type.or(probe.meta.content_type);
                if parsed.content_length.is_some() {
                    probe.meta.content_length = parsed.content_length;
                }
            }
        }
        Ok(probe)
    }

    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let src = self.resolve(from)?;
        let dest = self.resolve(to)?;
        let len = fs::metadata(&src)
            .await
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    StorageError::NotFound(from.into())
                } else {
                    StorageError::Io(err)
                }
            })?
            .len();
        if len > crate::bounded::MAX_SUPPORTED_OBJECT_BYTES {
            return Err(StorageError::PayloadTooLarge(format!(
                "object length {len} exceeds {}",
                crate::bounded::MAX_SUPPORTED_OBJECT_BYTES
            )));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(&src, &dest).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(from.into())
            } else {
                StorageError::Io(err)
            }
        })?;
        // Move companion meta sidecar when present.
        let from_meta = bookclerk_meta_sidecar_key(from);
        let to_meta = bookclerk_meta_sidecar_key(to);
        if self.exists(&from_meta).await? {
            let meta_src = self.resolve(&from_meta)?;
            let meta_dest = self.resolve(&to_meta)?;
            if let Some(parent) = meta_dest.parent() {
                fs::create_dir_all(parent).await?;
            }
            let _ = fs::copy(&meta_src, &meta_dest).await;
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.resolve(key)?;
        match fs::remove_file(&path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(StorageError::Io(err)),
        }
        // Best-effort remove companion meta when deleting the primary object.
        if !key.ends_with(".bookclerk-meta.json") {
            let meta_key = bookclerk_meta_sidecar_key(key);
            let _ = self.delete(&meta_key).await;
        }
        Ok(())
    }

    async fn touch_file(
        &self,
        key: &str,
        created: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<()> {
        let path = self.resolve(key)?;
        if !path.exists() {
            return Ok(());
        }
        let created = created.map(FileTime::from_system_time);
        let modified = modified.map(FileTime::from_system_time);
        match (created, modified) {
            (Some(c), Some(m)) => set_file_times(&path, c, m).map_err(StorageError::Io)?,
            (None, Some(m)) => {
                let meta = std::fs::metadata(&path).map_err(StorageError::Io)?;
                let c = FileTime::from_last_modification_time(&meta);
                set_file_times(&path, c, m).map_err(StorageError::Io)?;
            }
            (Some(c), None) => {
                let meta = std::fs::metadata(&path).map_err(StorageError::Io)?;
                let m = FileTime::from_last_modification_time(&meta);
                set_file_times(&path, c, m).map_err(StorageError::Io)?;
            }
            (None, None) => {}
        }
        Ok(())
    }

    async fn list_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<crate::ListPage> {
        if !prefix.is_empty() {
            validate_key(prefix)?;
        }
        let limit = clamp_page_limit(limit);
        let root = self.root.clone();
        let storage_prefix = self.prefix.clone();
        let list_prefix = prefix.to_string();
        let cursor = cursor.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            list_page_indexed(
                &root,
                &storage_prefix,
                &list_prefix,
                cursor.as_deref(),
                limit,
            )
        })
        .await
        .map_err(|err| StorageError::Other(anyhow::anyhow!("list index task: {err}")))?
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<crate::ByteRange>,
    ) -> Result<(
        ObjectProbe,
        std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    )> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let probe = self.probe(key).await?;
        let path = self.resolve(key)?;
        let mut file = tokio::fs::File::open(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(key.into())
            } else {
                StorageError::Io(err)
            }
        })?;
        let span = normalize_range(range, Some(probe.size))?;
        if let Some(span) = span {
            let offset = match span {
                ReadSpan::ToEnd { offset } | ReadSpan::Exact { offset, .. } => offset,
            };
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            if let ReadSpan::Exact { length, .. } = span {
                return Ok((probe, Box::pin(file.take(length))));
            }
        }
        Ok((probe, Box::pin(file)))
    }

    async fn put_stream(
        &self,
        key: &str,
        body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> Result<crate::PutStreamResult> {
        self.write_stream(key, body, meta, None)
            .await
            .map(|written| written.put)
    }
}

impl LocalFsBackend {
    /// Streams `body` into a sibling temp file, then publishes it under `key`
    /// when `condition` holds (always when it is `None`).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Conflict`] when `condition` fails, integrity
    /// and size errors for the body, and I/O errors.
    async fn write_stream(
        &self,
        key: &str,
        mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        mut meta: ObjectMeta,
        condition: Option<&WriteCondition>,
    ) -> Result<ConditionalPutResult> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;
        if let Some(expected) = meta.sha256_hex.as_deref() {
            let _ = parse_sha256_hex(expected)?;
        }
        if let Some(len) = meta.content_length {
            if len > crate::bounded::MAX_SUPPORTED_OBJECT_BYTES {
                return Err(StorageError::PayloadTooLarge(format!(
                    "object length {len} exceeds {}",
                    crate::bounded::MAX_SUPPORTED_OBJECT_BYTES
                )));
            }
        }
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let tmp = sibling_temp_path(&path);
        if !tmp.starts_with(&self.root) && !path.starts_with(&self.root) {
            return Err(StorageError::InvalidKey(key.into()));
        }
        let mut guard = TempGuard::arm(tmp.clone());
        let put = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let mut hasher = Sha256::new();
            let mut bytes_written = 0u64;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = tokio::io::AsyncReadExt::read(&mut body, &mut buf).await?;
                if n == 0 {
                    break;
                }
                let next = bytes_written.saturating_add(n as u64);
                if let Some(expected) = meta.content_length {
                    if next > expected {
                        return Err(StorageError::Integrity(format!(
                            "put_stream wrote past declared length {expected}"
                        )));
                    }
                }
                if next > crate::bounded::MAX_SUPPORTED_OBJECT_BYTES {
                    return Err(StorageError::PayloadTooLarge(format!(
                        "object exceeded {}",
                        crate::bounded::MAX_SUPPORTED_OBJECT_BYTES
                    )));
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n]).await?;
                bytes_written = next;
            }
            if let Some(expected) = meta.content_length {
                if bytes_written != expected {
                    return Err(StorageError::Integrity(format!(
                        "put_stream length mismatch: wrote {bytes_written}, expected {expected}"
                    )));
                }
            }
            let digest = hex::encode(hasher.finalize());
            if let Some(expected) = meta.sha256_hex.as_deref() {
                if !expected.eq_ignore_ascii_case(&digest) {
                    return Err(StorageError::Integrity(
                        "put_stream sha256 does not match the body".into(),
                    ));
                }
            }
            meta.sha256_hex = Some(digest);
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            let atomic = self.publish(&tmp, &path, key, condition).await?;
            guard.disarm();
            Ok((bytes_written, atomic))
        };
        let (bytes_written, atomic) = put.await?;
        write_local_meta_sidecar(self, key, &meta).await?;
        let etag = match metadata_if_present(&path).await? {
            Some(meta) => self.signature_at(&path, &meta).await,
            None => None,
        };
        Ok(ConditionalPutResult {
            put: crate::PutStreamResult {
                bytes_written,
                etag,
                sha256_hex: meta.sha256_hex,
            },
            atomic,
        })
    }

    /// Moves the finished temp file `tmp` to `path` when `condition` holds.
    /// Returns whether the check and the publish were one step.
    ///
    /// On a local disk `IfNoneMatch` hard-links the temp file into place,
    /// which fails if anything is already there. `IfMatch`, and
    /// `IfNoneMatch` on a network or FUSE file system or one without hard
    /// links, checks just before the rename while holding
    /// [`Self::check_then_write`], so another conditional write through
    /// Bookclerk cannot pass the same check; a write from outside Bookclerk
    /// in that instant can. A plain publish counts as atomic only on a
    /// local disk. The directory is synced after the rename.
    async fn publish(
        &self,
        tmp: &Path,
        path: &Path,
        key: &str,
        condition: Option<&WriteCondition>,
    ) -> Result<bool> {
        let dir = path.parent().unwrap_or(path);
        let kind = self.fs_kind(dir).await;
        let atomic = match condition {
            None => {
                rename_settled(tmp, path).await?;
                kind.is_local()
            }
            // Only a local disk answers a hard link reliably: a retried NFS
            // `LINK` can say "exists" after it succeeded, and SMB, AFP and
            // many FUSE file systems fake links or have none.
            Some(WriteCondition::IfNoneMatch) if kind.is_local() => {
                match fs::hard_link(tmp, path).await {
                    Ok(()) => {
                        // The object is published; a leftover temp name is harmless.
                        let _ = fs::remove_file(tmp).await;
                        true
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Err(StorageError::Conflict(format!("{key} already exists")));
                    }
                    Err(_) => self.publish_if_absent(tmp, path, key).await?,
                }
            }
            Some(WriteCondition::IfNoneMatch) => self.publish_if_absent(tmp, path, key).await?,
            Some(WriteCondition::IfMatch(expected)) => {
                let _held = self.check_then_write.lock().await;
                let current = match metadata_if_present(path).await? {
                    Some(meta) => signature_of(path, &meta, kind).await,
                    None => None,
                };
                if current.as_deref() != Some(expected.as_str()) {
                    return Err(StorageError::Conflict(format!(
                        "{key} changed since it was read"
                    )));
                }
                rename_settled(tmp, path).await?;
                false
            }
        };
        sync_dir(dir).await;
        Ok(atomic)
    }

    /// Checks that nothing is at `path` and renames `tmp` there, holding
    /// [`Self::check_then_write`]. Not atomic against writers outside
    /// Bookclerk; returns `false` to say so.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Conflict`] when something is already there,
    /// and I/O errors from the check or the rename.
    async fn publish_if_absent(&self, tmp: &Path, path: &Path, key: &str) -> Result<bool> {
        let _held = self.check_then_write.lock().await;
        if metadata_if_present(path).await?.is_some() {
            return Err(StorageError::Conflict(format!("{key} already exists")));
        }
        rename_settled(tmp, path).await?;
        Ok(false)
    }

    /// Moves the companion meta sidecar of `from` to `to`, or removes a
    /// stale sidecar at `to` when `from` has none. Best effort: the object
    /// itself has already moved.
    async fn move_sidecar(&self, from: &str, to: &str) {
        let (Ok(src), Ok(dest)) = (
            self.resolve(&bookclerk_meta_sidecar_key(from)),
            self.resolve(&bookclerk_meta_sidecar_key(to)),
        ) else {
            return;
        };
        if fs::rename(&src, &dest).await.is_err() {
            let _ = fs::remove_file(&dest).await;
        }
    }
}

/// Unique sibling temp path used for atomic [`LocalFsBackend::put_stream`].
fn sibling_temp_path(final_path: &Path) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "object".into());
    final_path.with_file_name(format!(".{name}.bookclerk-tmp-{nonce}"))
}

/// Writes a `.bookclerk-meta.json` sidecar when identity or integrity fields exist.
async fn write_local_meta_sidecar(
    backend: &LocalFsBackend,
    key: &str,
    meta: &ObjectMeta,
) -> Result<()> {
    // Skip recursive meta-for-meta; only persist meaningful identity tags.
    if key.ends_with(".bookclerk-meta.json") {
        return Ok(());
    }
    if meta.asin.is_none()
        && meta.title.is_none()
        && meta.sha256_hex.is_none()
        && meta.commit_token.is_none()
        && meta.creation_time.is_none()
        && meta.last_write_time.is_none()
    {
        return Ok(());
    }
    let sidecar = bookclerk_meta_sidecar_key(key);
    let payload =
        serde_json::to_vec(meta).map_err(|err| StorageError::Io(std::io::Error::other(err)))?;
    let path = backend.resolve(&sidecar)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&path, payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn put_get_exists_delete() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let key = "Author/Title/book.m4b";
        assert!(!backend.exists(key).await.unwrap());
        backend
            .put(
                key,
                Bytes::from_static(b"audio"),
                ObjectMeta {
                    asin: Some("B00X".into()),
                    title: Some("Book".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(backend.exists(key).await.unwrap());
        assert_eq!(backend.get(key).await.unwrap().as_ref(), b"audio");
        let probe = backend.probe(key).await.unwrap();
        assert_eq!(probe.meta.asin.as_deref(), Some("B00X"));
        assert_eq!(probe.meta.title.as_deref(), Some("Book"));
        let listed = backend.list_audio("").await.unwrap();
        assert_eq!(listed.len(), 1);
        backend.delete(key).await.unwrap();
        assert!(!backend.exists(key).await.unwrap());
    }

    #[tokio::test]
    async fn rename_moves_audio_and_meta_sidecar() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        backend
            .put(
                "Old/book.m4b",
                Bytes::from_static(b"audio"),
                ObjectMeta {
                    asin: Some("B00X".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        backend
            .rename("Old/book.m4b", "New/book.m4b")
            .await
            .unwrap();
        assert!(!backend.exists("Old/book.m4b").await.unwrap());
        assert!(backend.exists("New/book.m4b").await.unwrap());
        let probe = backend.probe("New/book.m4b").await.unwrap();
        assert_eq!(probe.meta.asin.as_deref(), Some("B00X"));
        assert!(!backend
            .exists(&bookclerk_meta_sidecar_key("Old/book.m4b"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn put_file_copies_without_bytes_api() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().join("store")).unwrap();
        let src = dir.path().join("src.m4b");
        std::fs::write(&src, b"from-file").unwrap();
        backend
            .put_file("A/B.m4b", &src, ObjectMeta::default())
            .await
            .unwrap();
        assert_eq!(backend.get("A/B.m4b").await.unwrap().as_ref(), b"from-file");
    }

    /// A `:` in a key is a title character on POSIX, but a drive or an
    /// alternate data stream on Windows, where it is refused.
    #[tokio::test]
    async fn a_colon_in_a_key_is_refused_only_on_windows() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        for key in ["Star Wars: X/Book.m4b", "A/Book.m4b:stream"] {
            let put = backend
                .put(key, Bytes::from_static(b"x"), ObjectMeta::default())
                .await;
            if cfg!(windows) {
                assert!(matches!(put, Err(StorageError::InvalidKey(_))), "{key}");
            } else {
                put.unwrap();
                assert_eq!(backend.get(key).await.unwrap().as_ref(), b"x");
            }
        }
    }

    #[tokio::test]
    async fn rejects_path_escape() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let err = backend
            .put(
                "../escape.m4b",
                Bytes::from_static(b"x"),
                ObjectMeta::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[tokio::test]
    async fn rejects_dangling_symlink_leaf_without_creating_outside() {
        let dir = tempdir().unwrap();
        let store = dir.path().join("store");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let backend = LocalFsBackend::new(store.clone()).unwrap();
        let link = store.join("new.txt");
        let target = outside.join("new.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();
        assert!(!target.exists());
        let err = backend
            .put(
                "new.txt",
                Bytes::from_static(b"payload"),
                ObjectMeta::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)), "{err:?}");
        assert!(
            !target.exists(),
            "dangling symlink must not create the outside target"
        );
    }

    #[tokio::test]
    async fn prefix_scopes_keys_under_root() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::with_prefix(dir.path().to_path_buf(), "library/").unwrap();
        backend
            .put(
                "Author/Book.m4b",
                Bytes::from_static(b"audio"),
                ObjectMeta {
                    asin: Some("B00X".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(dir.path().join("library/Author/Book.m4b").is_file());
        assert!(backend.exists("Author/Book.m4b").await.unwrap());
        let listed = backend.list("").await.unwrap();
        assert!(
            listed.iter().any(|o| o.key == "Author/Book.m4b"),
            "list should return keys relative to prefix: {listed:?}"
        );
        assert!(
            !listed.iter().any(|o| o.key.starts_with("library/")),
            "list must strip storage prefix from returned keys"
        );
        // Objects outside the prefix are invisible.
        std::fs::write(dir.path().join("other.m4b"), b"nope").unwrap();
        let listed = backend.list_audio("").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "Author/Book.m4b");
    }

    #[test]
    fn with_prefix_rejects_parent_dir_components() {
        let dir = tempdir().unwrap();
        let parent = dir.path().parent().unwrap();
        let marker = parent.join(format!(
            "bookclerk-prefix-escape-marker-{}",
            std::process::id()
        ));
        assert!(LocalFsBackend::with_prefix(dir.path().to_path_buf(), "../outside/new").is_err());
        assert!(LocalFsBackend::with_prefix(dir.path().to_path_buf(), "foo/../bar").is_err());
        assert!(
            !marker.exists(),
            "ParentDir prefix must not create siblings outside root"
        );
        assert!(!parent.join("outside").exists());
        assert!(!dir.path().join("foo").exists());
        assert!(!dir.path().join("bar").exists());
        assert!(!dir.path().join("outside").exists());
    }

    #[cfg(unix)]
    #[test]
    fn with_prefix_rejects_symlink_ancestor_with_missing_child() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let link = dir.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let before: Vec<_> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let err =
            LocalFsBackend::with_prefix(dir.path().to_path_buf(), "escape/newchild").unwrap_err();
        assert!(
            matches!(err, StorageError::InvalidKey(_)),
            "expected InvalidKey, got {err:?}"
        );
        let after: Vec<_> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after, "must not mkdir through symlink ancestor");
        assert!(!outside.path().join("newchild").exists());
    }

    #[tokio::test]
    async fn put_stream_roundtrip_does_not_buffer_get() {
        use tokio::io::AsyncReadExt;
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let key = "stream.bin";
        let payload = vec![0xA5u8; 1024 * 64];
        backend
            .put_stream(
                key,
                Box::pin(std::io::Cursor::new(payload.clone())),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let (probe, mut body) = backend.get_stream(key, None).await.unwrap();
        assert_eq!(probe.size, payload.len() as u64);
        let mut out = Vec::new();
        body.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, payload);
        let page = backend.list_page("", None, 10).await.unwrap();
        assert!(page.objects.iter().any(|o| o.key == key));
    }

    #[tokio::test]
    async fn scalar_get_rejects_limit_plus_one_without_returning_a_prefix() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let big = vec![7u8; crate::bounded::MAX_SCALAR_OBJECT_BYTES as usize + 1];
        backend
            .put_stream(
                "big.bin",
                Box::pin(std::io::Cursor::new(big)),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let err = backend.get("big.bin").await.unwrap_err();
        assert!(matches!(err, StorageError::PayloadTooLarge(_)), "{err:?}");
    }

    #[tokio::test]
    async fn dropped_put_stream_deletes_the_temp_file() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let backend_task = backend.clone();
        let task = tokio::spawn(async move {
            let body = std::pin::pin!(tokio::io::empty());
            // Block after the temp file is created by using a reader that waits.
            struct WaitRead;
            impl tokio::io::AsyncRead for WaitRead {
                fn poll_read(
                    self: std::pin::Pin<&mut Self>,
                    _cx: &mut std::task::Context<'_>,
                    _buf: &mut tokio::io::ReadBuf<'_>,
                ) -> std::task::Poll<std::io::Result<()>> {
                    std::task::Poll::Pending
                }
            }
            let _ = backend_task
                .put_stream(
                    "slow.bin",
                    Box::pin(WaitRead),
                    ObjectMeta {
                        content_length: Some(1024),
                        ..Default::default()
                    },
                )
                .await;
            let _ = body;
        });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        task.abort();
        let _ = task.await;
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("bookclerk-tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[tokio::test]
    #[ignore = "large storage resource"]
    async fn list_page_one_hundred_thousand_objects() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        let started = std::time::Instant::now();
        for i in 0..100_000u32 {
            std::fs::write(root.join(format!("f{i:06}.bin")), b"x").unwrap();
        }
        let nested = root.join("nest").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("z.bin"), b"z").unwrap();
        eprintln!(
            "created 100001 files in {} ms",
            started.elapsed().as_millis()
        );
        let backend = LocalFsBackend::new(root).unwrap();
        let _ = crate::list_index::take_index_stats();
        let mut cursor = None;
        let mut total = 0usize;
        let mut pages = 0u32;
        let scan = std::time::Instant::now();
        loop {
            let page = backend.list_page("", cursor.as_deref(), 256).await.unwrap();
            assert!(page.objects.len() <= 256);
            total += page.objects.len();
            pages += 1;
            match page.next_cursor {
                Some(next) => {
                    assert_ne!(cursor.as_deref(), Some(next.as_str()));
                    cursor = Some(next);
                }
                None => break,
            }
        }
        let (builds, chunk) = crate::list_index::take_index_stats();
        eprintln!(
            "paged {total} objects in {pages} pages builds={builds} chunk={chunk} elapsed_ms={}",
            scan.elapsed().as_millis()
        );
        assert!(total >= 100_000);
        assert_eq!(builds, 1, "continued pages must reuse one index build");
        assert!(chunk <= crate::list_index::INDEX_CHUNK);
        assert!(scan.elapsed().as_secs() < 120);

        let fan =
            crate::fanout::FanoutBackend::new(vec![Box::new(backend.clone()), Box::new(backend)])
                .unwrap();
        let mut cursor = None;
        let mut fan_total = 0usize;
        let fan_started = std::time::Instant::now();
        loop {
            let page = fan.list_page("", cursor.as_deref(), 256).await.unwrap();
            fan_total += page.objects.len();
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        eprintln!(
            "fan-out paged {fan_total} unique keys in {} ms",
            fan_started.elapsed().as_millis()
        );
        assert_eq!(fan_total, total);
    }

    #[tokio::test]
    async fn list_page_nested_prefix_includes_audio() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        backend
            .put(
                "Misc/Cool Book/book.m4b",
                Bytes::from_static(b"a"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let page = backend
            .list_page("Misc/Cool Book/", None, 10)
            .await
            .unwrap();
        assert!(
            page.objects
                .iter()
                .any(|o| o.key == "Misc/Cool Book/book.m4b"),
            "prefix page = {:?}",
            page.objects
        );
    }

    #[tokio::test]
    async fn list_page_stale_cursor_is_invalid() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        backend
            .put("a.bin", Bytes::from_static(b"a"), ObjectMeta::default())
            .await
            .unwrap();
        let err = backend
            .list_page("", Some("missing-cursor"), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidCursor(_)));
    }

    #[tokio::test]
    async fn list_page_is_bounded_over_large_namespace() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        for i in 0..80 {
            backend
                .put(
                    &format!("n{i:03}.bin"),
                    Bytes::from_static(b"x"),
                    ObjectMeta::default(),
                )
                .await
                .unwrap();
        }
        let mut cursor = None;
        let mut total = 0usize;
        loop {
            let page = backend.list_page("", cursor.as_deref(), 10).await.unwrap();
            assert!(page.objects.len() <= 10);
            total += page.objects.len();
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert!(total >= 80, "paged through {total} objects");
    }

    #[tokio::test]
    async fn list_page_flat_dir_retains_only_page_sized_heap() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        for i in 0..400 {
            backend
                .put(
                    &format!("f{i:04}.bin"),
                    Bytes::from_static(b"x"),
                    ObjectMeta::default(),
                )
                .await
                .unwrap();
        }
        let _ = crate::list_index::take_index_stats();
        let page = backend.list_page("", None, 7).await.unwrap();
        assert_eq!(page.objects.len(), 7);
        let (_builds, retained) = crate::list_index::take_index_stats();
        assert!(
            retained <= crate::list_index::INDEX_CHUNK,
            "list index chunk retained {retained} entries (cap {})",
            crate::list_index::INDEX_CHUNK
        );
        let _ = backend
            .list_page("", page.next_cursor.as_deref(), 7)
            .await
            .unwrap();
        let (builds_after, _) = crate::list_index::take_index_stats();
        assert_eq!(
            builds_after, 0,
            "a continued page must not rebuild the directory index"
        );
        assert_eq!(page.objects[0].key, "f0000.bin");
        assert_eq!(page.next_cursor.as_deref(), Some("f0006.bin"));
    }

    #[tokio::test]
    async fn put_stream_does_not_truncate_existing_on_failure() {
        use tokio::io::AsyncRead;
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        backend
            .put(
                "keep.bin",
                Bytes::from_static(b"original"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        struct FailRead;
        impl AsyncRead for FailRead {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::other("source exploded")))
            }
        }
        let err = backend
            .put_stream(
                "keep.bin",
                Box::pin(FailRead),
                ObjectMeta {
                    content_length: Some(100),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Io(_)));
        let got = backend.get("keep.bin").await.unwrap();
        assert_eq!(&got[..], b"original");
    }

    #[tokio::test]
    async fn probe_round_trips_digest_and_commit_token_for_the_exact_object() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let audio = b"audio-body";
        let cover = b"jpeg-body";
        backend
            .put_stream(
                "Title/book.m4b",
                Box::pin(std::io::Cursor::new(audio.to_vec())),
                ObjectMeta {
                    content_length: Some(audio.len() as u64),
                    content_type: Some("audio/mp4".into()),
                    commit_token: Some("audio-token".into()),
                    asin: Some("B00AUDIO".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        backend
            .put_stream(
                "Title/book.jpg",
                Box::pin(std::io::Cursor::new(cover.to_vec())),
                ObjectMeta {
                    content_length: Some(cover.len() as u64),
                    content_type: Some("image/jpeg".into()),
                    commit_token: Some("cover-token".into()),
                    title: Some("Cover".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let audio_meta = bookclerk_meta_sidecar_key("Title/book.m4b");
        let cover_meta = bookclerk_meta_sidecar_key("Title/book.jpg");
        assert_ne!(audio_meta, cover_meta);
        assert!(backend.exists(&audio_meta).await.unwrap());
        assert!(backend.exists(&cover_meta).await.unwrap());

        let rebuilt = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let audio_probe = rebuilt.probe("Title/book.m4b").await.unwrap();
        let cover_probe = rebuilt.probe("Title/book.jpg").await.unwrap();
        assert_eq!(
            audio_probe.meta.commit_token.as_deref(),
            Some("audio-token")
        );
        assert_eq!(
            cover_probe.meta.commit_token.as_deref(),
            Some("cover-token")
        );
        assert!(audio_probe.meta.sha256_hex.is_some());
        assert_ne!(audio_probe.meta.sha256_hex, cover_probe.meta.sha256_hex);
        assert_eq!(audio_probe.meta.asin.as_deref(), Some("B00AUDIO"));
        assert_eq!(cover_probe.meta.title.as_deref(), Some("Cover"));

        rebuilt
            .copy("Title/book.m4b", "Other/book.m4b")
            .await
            .unwrap();
        let copied = rebuilt.probe("Other/book.m4b").await.unwrap();
        assert_eq!(copied.meta.commit_token.as_deref(), Some("audio-token"));
        assert_eq!(copied.meta.sha256_hex, audio_probe.meta.sha256_hex);

        rebuilt
            .rename("Other/book.m4b", "Moved/book.m4b")
            .await
            .unwrap();
        assert!(!rebuilt
            .exists(&bookclerk_meta_sidecar_key("Other/book.m4b"))
            .await
            .unwrap());
        let moved = rebuilt.probe("Moved/book.m4b").await.unwrap();
        assert_eq!(moved.meta.sha256_hex, audio_probe.meta.sha256_hex);
        rebuilt.delete("Moved/book.m4b").await.unwrap();
        assert!(!rebuilt
            .exists(&bookclerk_meta_sidecar_key("Moved/book.m4b"))
            .await
            .unwrap());

        let missing = rebuilt.probe("Title/book.jpg").await.unwrap();
        assert_eq!(missing.size, cover.len() as u64);
        std::fs::write(
            dir.path().join("Title/book.jpg.bookclerk-meta.json"),
            b"{not-json",
        )
        .unwrap();
        let corrupt = rebuilt.probe("Title/book.jpg").await.unwrap();
        assert_eq!(corrupt.size, cover.len() as u64);
        assert!(corrupt.meta.sha256_hex.is_none());
    }

    #[tokio::test]
    async fn copy_rejects_an_oversized_sparse_source_without_replacing_the_destination() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        backend
            .put(
                "dest.m4b",
                Bytes::from_static(b"keeper"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let src = dir.path().join("huge.m4b");
        let file = std::fs::File::create(&src).unwrap();
        // NTFS allocates the whole length unless the file is marked sparse.
        #[cfg(windows)]
        mark_sparse(&file);
        file.set_len(crate::bounded::MAX_SUPPORTED_OBJECT_BYTES + 1)
            .unwrap();
        let err = backend.copy("huge.m4b", "dest.m4b").await.unwrap_err();
        assert!(
            matches!(err, crate::StorageError::PayloadTooLarge(_)),
            "{err}"
        );
        assert_eq!(backend.get("dest.m4b").await.unwrap().as_ref(), b"keeper");
        assert!(src.metadata().unwrap().len() > crate::bounded::MAX_SUPPORTED_OBJECT_BYTES);
    }

    /// Marks `file` sparse so extending it allocates no disk space, as a
    /// length change does on Unix file systems.
    #[cfg(windows)]
    // One DeviceIoControl on a handle `file` owns; the SAFETY note covers it.
    #[allow(unsafe_code)]
    fn mark_sparse(file: &std::fs::File) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
        use windows_sys::Win32::System::IO::DeviceIoControl;
        let mut returned = 0_u32;
        // SAFETY: the handle is open for the lifetime of `file`; FSCTL_SET_SPARSE
        // takes no input buffer (meaning "set") and writes no output buffer.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            ok,
            0,
            "FSCTL_SET_SPARSE: {}",
            std::io::Error::last_os_error()
        );
    }

    /// A local backend in a fresh directory.
    fn intent_backend() -> (tempfile::TempDir, LocalFsBackend) {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        (dir, backend)
    }

    /// Writes `body` under `key` with a digest, so a meta sidecar exists.
    async fn put_with_sidecar(backend: &LocalFsBackend, key: &str, body: &'static [u8]) {
        backend
            .put(
                key,
                Bytes::from_static(body),
                ObjectMeta {
                    sha256_hex: Some(hex::encode(sha2::Sha256::digest(body))),
                    ..ObjectMeta::default()
                },
            )
            .await
            .unwrap();
    }

    /// Streams `body` under `key` with `condition`.
    async fn put_if(
        backend: &LocalFsBackend,
        key: &str,
        body: &'static [u8],
        condition: &WriteCondition,
    ) -> Result<ConditionalPutResult> {
        backend
            .put_stream_conditional(
                key,
                Box::pin(std::io::Cursor::new(body)),
                ObjectMeta::default(),
                condition,
            )
            .await
    }

    #[tokio::test]
    async fn a_native_move_renames_the_file_and_its_sidecar_and_repeats_safely() {
        let (_dir, b) = intent_backend();
        put_with_sidecar(&b, "Old/book.m4b", b"audio").await;
        let moved = b
            .move_object("Old/book.m4b", "New/book.m4b", &MoveOptions::default())
            .await
            .unwrap();
        assert!(moved.atomic && !moved.already_moved);
        assert_eq!(b.get("New/book.m4b").await.unwrap().as_ref(), b"audio");
        assert!(!b.exists("Old/book.m4b").await.unwrap());
        assert!(b
            .exists(&bookclerk_meta_sidecar_key("New/book.m4b"))
            .await
            .unwrap());
        assert!(!b
            .exists(&bookclerk_meta_sidecar_key("Old/book.m4b"))
            .await
            .unwrap());
        let again = b
            .move_object("Old/book.m4b", "New/book.m4b", &MoveOptions::default())
            .await
            .unwrap();
        assert!(again.already_moved);
        let missing = b
            .move_object("Gone/x", "Else/x", &MoveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(missing, StorageError::NotFound(_)), "{missing}");
    }

    #[tokio::test]
    async fn a_move_without_a_sidecar_clears_a_stale_one_at_the_target() {
        let (_dir, b) = intent_backend();
        put_with_sidecar(&b, "to.m4b", b"old").await;
        b.put(
            "from.m4b",
            Bytes::from_static(b"new"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        b.move_object("from.m4b", "to.m4b", &MoveOptions::default())
            .await
            .unwrap();
        assert_eq!(b.get("to.m4b").await.unwrap().as_ref(), b"new");
        assert!(!b
            .exists(&bookclerk_meta_sidecar_key("to.m4b"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn no_overwrite_keeps_an_existing_target_and_the_source() {
        let (_dir, b) = intent_backend();
        b.put("a", Bytes::from_static(b"a"), ObjectMeta::default())
            .await
            .unwrap();
        b.put("b", Bytes::from_static(b"b"), ObjectMeta::default())
            .await
            .unwrap();
        let options = MoveOptions {
            no_overwrite: true,
            ..MoveOptions::default()
        };
        let err = b.move_object("a", "b", &options).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "{err}");
        assert_eq!(b.get("a").await.unwrap().as_ref(), b"a");
        assert_eq!(b.get("b").await.unwrap().as_ref(), b"b");
        let moved = b.move_object("a", "c/d", &options).await.unwrap();
        assert!(moved.atomic);
        assert_eq!(b.get("c/d").await.unwrap().as_ref(), b"a");
        assert!(!b.exists("a").await.unwrap());
    }

    #[tokio::test]
    async fn if_source_match_moves_only_the_object_that_was_read() {
        let (_dir, b) = intent_backend();
        b.put("a", Bytes::from_static(b"one"), ObjectMeta::default())
            .await
            .unwrap();
        let read = b.head("a").await.unwrap().unwrap().etag.unwrap();
        b.put("a", Bytes::from_static(b"two"), ObjectMeta::default())
            .await
            .unwrap();
        let stale = MoveOptions {
            if_source_match: Some(read),
            ..MoveOptions::default()
        };
        let err = b.move_object("a", "b", &stale).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "{err}");
        let current = MoveOptions {
            if_source_match: b.head("a").await.unwrap().unwrap().etag,
            ..MoveOptions::default()
        };
        b.move_object("a", "b", &current).await.unwrap();
        assert_eq!(b.get("b").await.unwrap().as_ref(), b"two");
    }

    #[tokio::test]
    async fn moving_a_directory_key_is_refused() {
        let (_dir, b) = intent_backend();
        b.put("dir/x", Bytes::from_static(b"x"), ObjectMeta::default())
            .await
            .unwrap();
        let err = b
            .move_object("dir", "other", &MoveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)), "{err}");
        assert!(b.exists("dir/x").await.unwrap());
    }

    #[tokio::test]
    async fn the_etag_changes_when_the_file_is_rewritten_and_not_when_it_is_read() {
        let (_dir, b) = intent_backend();
        b.put("a", Bytes::from_static(b"same"), ObjectMeta::default())
            .await
            .unwrap();
        let first = b.head("a").await.unwrap().unwrap();
        assert!(first.etag.is_some());
        assert_eq!(first.etag, first.signature);
        let _ = b.get("a").await.unwrap();
        assert_eq!(b.head("a").await.unwrap().unwrap().etag, first.etag);
        b.put("a", Bytes::from_static(b"diff"), ObjectMeta::default())
            .await
            .unwrap();
        assert_ne!(b.head("a").await.unwrap().unwrap().etag, first.etag);
    }

    #[tokio::test]
    async fn if_none_match_publishes_once_and_atomically() {
        let (_dir, b) = intent_backend();
        let first = put_if(&b, "k", b"first", &WriteCondition::IfNoneMatch)
            .await
            .unwrap();
        assert!(first.atomic);
        assert_eq!(first.put.etag, b.head("k").await.unwrap().unwrap().etag);
        let err = put_if(&b, "k", b"second", &WriteCondition::IfNoneMatch)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "{err}");
        assert_eq!(b.get("k").await.unwrap().as_ref(), b"first");
        let leftovers: Vec<_> = std::fs::read_dir(&b.root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("bookclerk-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[tokio::test]
    async fn if_match_writes_only_over_the_object_that_was_read() {
        let (_dir, b) = intent_backend();
        b.put("k", Bytes::from_static(b"v1"), ObjectMeta::default())
            .await
            .unwrap();
        let read = b.head("k").await.unwrap().unwrap().etag.unwrap();
        let written = put_if(&b, "k", b"v2", &WriteCondition::IfMatch(read.clone()))
            .await
            .unwrap();
        assert!(!written.atomic, "ifMatch is a check just before the rename");
        let err = put_if(&b, "k", b"v3", &WriteCondition::IfMatch(read))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "{err}");
        assert_eq!(b.get("k").await.unwrap().as_ref(), b"v2");
        let absent = put_if(&b, "new", b"x", &WriteCondition::IfMatch("e".into()))
            .await
            .unwrap_err();
        assert!(matches!(absent, StorageError::Conflict(_)), "{absent}");
        assert!(!b.exists("new").await.unwrap());
    }

    #[tokio::test]
    async fn a_hinted_listing_fills_signatures_and_filters_by_time() {
        let (dir, b) = intent_backend();
        b.put("p/old", Bytes::from_static(b"o"), ObjectMeta::default())
            .await
            .unwrap();
        b.put("p/new", Bytes::from_static(b"n"), ObjectMeta::default())
            .await
            .unwrap();
        let past = FileTime::from_unix_time(1_000_000, 0);
        set_file_times(dir.path().join("p/old"), past, past).unwrap();
        let since = 2_000_000_000;
        let page = b
            .list_hinted(&ListRequest {
                prefix: "p/".into(),
                limit: 10,
                with_signatures: true,
                modified_since_ms: Some(since),
                ..ListRequest::default()
            })
            .await
            .unwrap();
        assert!(page.signatures_included && page.modified_since_applied);
        let keys: Vec<_> = page.page.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["p/new"]);
        let entry = &page.page.objects[0];
        assert!(entry.signature.is_some() && entry.modified_at_ms.is_some());
        assert_eq!(
            entry.etag,
            b.head("p/new").await.unwrap().unwrap().etag,
            "a listing and a probe agree"
        );
        let plain = b
            .list_hinted(&ListRequest {
                prefix: "p/".into(),
                limit: 10,
                ..ListRequest::default()
            })
            .await
            .unwrap();
        assert!(!plain.signatures_included && !plain.modified_since_applied);
        assert_eq!(plain.page.objects.len(), 2);
    }

    #[tokio::test]
    async fn a_prefix_move_renames_a_page_and_refuses_overlap() {
        let (_dir, b) = intent_backend();
        for key in ["a/1", "a/2", "a/sub/3"] {
            b.put(key, Bytes::from_static(b"x"), ObjectMeta::default())
                .await
                .unwrap();
        }
        let page = b
            .move_prefix("a/", "b/", None, 100, &MoveOptions::default())
            .await
            .unwrap();
        assert!(page.atomic);
        assert_eq!(page.objects_moved, 3);
        assert!(page.results.iter().all(|r| r.error.is_none()));
        for key in ["b/1", "b/2", "b/sub/3"] {
            assert!(b.exists(key).await.unwrap(), "{key}");
        }
        let err = b
            .move_prefix("b/", "b/sub/", None, 100, &MoveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)), "{err}");
    }

    #[test]
    fn the_backend_reports_its_native_operations() {
        let (_dir, b) = intent_backend();
        assert_eq!(b.capabilities(), LocalFsBackend::CAPABILITIES);
    }

    fn put_if_absent_body(
        body: &'static [u8],
    ) -> (
        std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        ObjectMeta,
    ) {
        (
            Box::pin(std::io::Cursor::new(body.to_vec())),
            ObjectMeta {
                content_length: Some(body.len() as u64),
                ..ObjectMeta::default()
            },
        )
    }

    #[tokio::test]
    async fn network_mounts_never_use_hard_links_and_say_so() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf())
            .unwrap()
            .with_fs_kind(FsKind::Remote);
        let (body, meta) = put_if_absent_body(b"one");
        let first = backend
            .put_stream_conditional("a.m4b", body, meta, &WriteCondition::IfNoneMatch)
            .await
            .unwrap();
        assert!(!first.atomic);
        let (body, meta) = put_if_absent_body(b"two");
        let again = backend
            .put_stream_conditional("a.m4b", body, meta, &WriteCondition::IfNoneMatch)
            .await;
        assert!(matches!(again, Err(StorageError::Conflict(_))), "{again:?}");
        assert_eq!(backend.get("a.m4b").await.unwrap().as_ref(), b"one");

        backend
            .put(
                "taken.m4b",
                Bytes::from_static(b"other"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let no_overwrite = MoveOptions {
            no_overwrite: true,
            ..MoveOptions::default()
        };
        let refused = backend
            .move_object("a.m4b", "taken.m4b", &no_overwrite)
            .await;
        assert!(
            matches!(refused, Err(StorageError::Conflict(_))),
            "{refused:?}"
        );
        assert_eq!(backend.get("taken.m4b").await.unwrap().as_ref(), b"other");

        let moved = backend
            .move_object("a.m4b", "b/a.m4b", &no_overwrite)
            .await
            .unwrap();
        assert!(!moved.atomic);
        assert!(!backend.exists("a.m4b").await.unwrap());
        assert_eq!(backend.get("b/a.m4b").await.unwrap().as_ref(), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let nlink = std::fs::metadata(dir.path().join("b/a.m4b"))
                .unwrap()
                .nlink();
            assert_eq!(nlink, 1, "a network-mount move left a second name");
        }
    }

    #[tokio::test]
    async fn network_mount_signatures_leave_out_inode_and_ctime() {
        let dir = tempdir().unwrap();
        let local = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        local
            .put("a.json", Bytes::from_static(b"{}"), ObjectMeta::default())
            .await
            .unwrap();
        let remote = local.clone().with_fs_kind(FsKind::Remote);
        let signature = remote.probe("a.json").await.unwrap().signature.unwrap();
        assert_eq!(signature.split('-').count(), 2, "{signature}");
        assert!(signature.starts_with("2-"), "size first: {signature}");
        let page = remote
            .list_hinted(&ListRequest {
                prefix: String::new(),
                limit: 10,
                with_signatures: true,
                ..ListRequest::default()
            })
            .await
            .unwrap();
        let listed = page
            .page
            .objects
            .iter()
            .find(|o| o.key == "a.json")
            .unwrap();
        assert_eq!(listed.signature.as_deref(), Some(signature.as_str()));
        // An ifMatch write on the mount compares against the same form.
        let (body, meta) = put_if_absent_body(b"[]");
        remote
            .put_stream_conditional("a.json", body, meta, &WriteCondition::IfMatch(signature))
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn put_file_onto_a_link_to_its_own_source_keeps_the_bytes() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        let backend = LocalFsBackend::new(dir.path().join("lib"))
            .unwrap()
            .with_fs_kind(FsKind::Remote);
        let source = dir.path().join("scratch.m4b");
        std::fs::write(&source, b"audiobook").unwrap();
        // What a retried NFS link leaves behind: the target already names
        // the source.
        std::fs::hard_link(&source, dir.path().join("lib/book.m4b")).unwrap();
        backend
            .put_file("book.m4b", &source, ObjectMeta::default())
            .await
            .unwrap();
        assert_eq!(
            backend.get("book.m4b").await.unwrap().as_ref(),
            b"audiobook"
        );
    }

    #[tokio::test]
    async fn a_retried_rename_that_already_happened_counts_as_done() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        std::fs::write(&to, b"moved").unwrap();
        rename_settled(&from, &to).await.unwrap();
        let neither = rename_settled(&from, &dir.path().join("nowhere")).await;
        assert_eq!(neither.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }
}
