//! Local filesystem storage backend.

use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use filetime::{set_file_times, FileTime};
use tokio::fs;

use crate::error::{Result, StorageError};
use crate::normalize_prefix;
use crate::traits::{
    bookclerk_meta_sidecar_key, ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend,
};

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
}

impl LocalFsBackend {
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
        std::fs::create_dir_all(&root)?;
        if !prefix.is_empty() {
            std::fs::create_dir_all(root.join(prefix.trim_end_matches('/')))?;
        }
        Ok(Self { root, prefix })
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
        // full_key only prepends a normalized prefix; still reject escape in the
        // combined path.
        if full.contains("..") {
            return Err(StorageError::InvalidKey(key.into()));
        }
        let path = self.root.join(&full);
        // Prevent path escape above root.
        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        if let Ok(canonical) = path.canonicalize() {
            if !canonical.starts_with(&canonical_root) {
                return Err(StorageError::InvalidKey(key.into()));
            }
        } else {
            // Parent must still stay under root when the file does not exist yet.
            if let Some(parent) = path.parent() {
                let parent_canon = if parent.exists() {
                    parent
                        .canonicalize()
                        .unwrap_or_else(|_| parent.to_path_buf())
                } else {
                    parent.to_path_buf()
                };
                if parent_canon.is_absolute()
                    && !parent_canon.starts_with(&canonical_root)
                    && !path.starts_with(&self.root)
                {
                    return Err(StorageError::InvalidKey(key.into()));
                }
            }
        }
        Ok(path)
    }
}

/// Rejects empty keys, absolute keys, and any `..` segment.
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('/') || key.contains("..") {
        return Err(StorageError::InvalidKey(key.into()));
    }
    Ok(())
}

#[async_trait]
impl StorageBackend for LocalFsBackend {
    fn name(&self) -> &'static str {
        "local"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
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
        // Prefer hard-link/copy without loading the whole audiobook into RAM.
        match fs::hard_link(source, &dest).await {
            Ok(()) => {}
            Err(_) => {
                fs::copy(source, &dest).await?;
            }
        }
        write_local_meta_sidecar(self, key, &meta).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let path = self.resolve(key)?;
        let data = fs::read(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(key.into())
            } else {
                StorageError::Io(err)
            }
        })?;
        Ok(Bytes::from(data))
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let path = self.resolve(key)?;
        Ok(fs::try_exists(&path).await?)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        validate_key(prefix).or_else(|_| {
            if prefix.is_empty() {
                Ok(())
            } else {
                Err(StorageError::InvalidKey(prefix.into()))
            }
        })?;
        let mut out = Vec::new();
        let full_prefix = self.full_key(prefix);
        list_recursive(&self.root, &self.root, &full_prefix, &mut out).await?;
        // Strip the storage prefix so returned keys match library storage_key
        // values (same as S3Backend).
        if !self.prefix.is_empty() {
            out = out
                .into_iter()
                .filter_map(|obj| {
                    obj.key.strip_prefix(&self.prefix).map(|rest| ObjectInfo {
                        key: rest.to_string(),
                        size: obj.size,
                    })
                })
                .collect();
        }
        Ok(out)
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
        let mut probe = ObjectProbe {
            key: key.to_string(),
            size: file_meta.len(),
            content_type: None,
            meta: ObjectMeta {
                content_length: Some(file_meta.len()),
                ..Default::default()
            },
        };
        // Cheap sidecar read — never opens the audio body.
        let meta_key = bookclerk_meta_sidecar_key(key);
        if let Ok(bytes) = self.get(&meta_key).await {
            if let Ok(parsed) = serde_json::from_slice::<ObjectMeta>(&bytes) {
                probe.meta.asin = parsed.asin.or(probe.meta.asin);
                probe.meta.title = parsed.title.or(probe.meta.title);
                probe.meta.creation_time = parsed.creation_time.or(probe.meta.creation_time);
                probe.meta.last_write_time = parsed.last_write_time.or(probe.meta.last_write_time);
                probe.content_type = parsed.content_type.or(probe.content_type);
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
        let limit = if limit == 0 { 256 } else { limit as usize };
        let want = limit.saturating_add(1);
        let mut heap: BinaryHeap<(String, u64)> = BinaryHeap::new();
        let mut found_cursor = cursor.is_none();
        let full_prefix = self.full_key(prefix);
        bounded_list_page_walk(
            &self.root,
            &self.root,
            &full_prefix,
            &self.prefix,
            cursor,
            want,
            &mut heap,
            &mut found_cursor,
        )
        .await?;
        if cursor.is_some() && !found_cursor {
            return Err(StorageError::InvalidCursor(
                "stale or unknown list cursor".into(),
            ));
        }
        let collected: Vec<(String, u64)> = heap.into_sorted_vec();
        let next_cursor = if collected.len() > limit {
            collected
                .get(limit.saturating_sub(1))
                .map(|(key, _)| key.clone())
        } else {
            None
        };
        Ok(crate::ListPage {
            objects: collected
                .into_iter()
                .take(limit)
                .map(|(key, size)| ObjectInfo { key, size })
                .collect(),
            next_cursor,
        })
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
        if let Some(range) = range {
            file.seek(std::io::SeekFrom::Start(range.offset)).await?;
            if let Some(len) = range.length {
                return Ok((probe, Box::pin(file.take(len))));
            }
        }
        Ok((probe, Box::pin(file)))
    }

    async fn put_stream(
        &self,
        key: &str,
        mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> Result<crate::PutStreamResult> {
        use tokio::io::AsyncWriteExt;
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let tmp = sibling_temp_path(&path);
        let put = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let bytes_written = tokio::io::copy(&mut body, &mut file).await?;
            if let Some(expected) = meta.content_length {
                if bytes_written != expected {
                    return Err(StorageError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "put_stream length mismatch: wrote {bytes_written}, expected {expected}"
                        ),
                    )));
                }
            }
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&tmp, &path).await?;
            Ok(bytes_written)
        };
        match put.await {
            Ok(bytes_written) => {
                write_local_meta_sidecar(self, key, &meta).await?;
                Ok(crate::PutStreamResult {
                    bytes_written,
                    etag: None,
                })
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp).await;
                Err(err)
            }
        }
    }
}

/// Deterministic directory listing (sorted by path).
async fn sorted_dir_entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut read_dir = match fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(StorageError::Io(err)),
    };
    let mut entries = Vec::new();
    while let Some(entry) = read_dir.next_entry().await? {
        entries.push(entry);
    }
    entries.sort_by_key(|e| e.path());
    Ok(entries)
}

/// Walks `dir` and appends files whose relative key starts with `prefix`.
async fn list_recursive(
    root: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<ObjectInfo>,
) -> Result<()> {
    let entries = sorted_dir_entries(dir).await?;
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            Box::pin(list_recursive(root, &path, prefix, out)).await?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.') && n.contains(".bookclerk-tmp-"))
        {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| StorageError::InvalidKey(path.display().to_string()))?;
        let key = rel.to_string_lossy().replace('\\', "/");
        if !prefix.is_empty() && !key.starts_with(prefix) {
            continue;
        }
        let meta = fs::metadata(&path).await?;
        out.push(ObjectInfo {
            key,
            size: meta.len(),
        });
    }
    Ok(())
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

/// Opaque, bounded directory walk. Memory is O(depth + page), not O(namespace).
///
/// Entries are scanned unsorted. Only the smallest `want` keys strictly after
/// `cursor` are retained (max-heap of page size). Concurrent mutation is weakly
/// consistent. A cursor that is not observed during this walk fails closed as
/// [`StorageError::InvalidCursor`].
#[allow(clippy::too_many_arguments)]
async fn bounded_list_page_walk(
    root: &Path,
    dir: &Path,
    full_prefix: &str,
    storage_prefix: &str,
    cursor: Option<&str>,
    want: usize,
    heap: &mut BinaryHeap<(String, u64)>,
    found_cursor: &mut bool,
) -> Result<()> {
    let mut read_dir = match fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(StorageError::Io(err)),
    };
    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            Box::pin(bounded_list_page_walk(
                root,
                &path,
                full_prefix,
                storage_prefix,
                cursor,
                want,
                heap,
                found_cursor,
            ))
            .await?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.') && n.contains(".bookclerk-tmp-"))
        {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| StorageError::InvalidKey(path.display().to_string()))?;
        let key_full = rel.to_string_lossy().replace('\\', "/");
        if !full_prefix.is_empty() && !key_full.starts_with(full_prefix) {
            continue;
        }
        let key = if storage_prefix.is_empty() {
            key_full
        } else {
            match key_full.strip_prefix(storage_prefix) {
                Some(rest) => rest.to_string(),
                None => continue,
            }
        };
        if cursor == Some(key.as_str()) {
            *found_cursor = true;
            continue;
        }
        if cursor.is_some_and(|c| key.as_str() <= c) {
            continue;
        }
        if heap.len() < want {
            let meta = fs::metadata(&path).await?;
            heap.push((key, meta.len()));
            track_list_page_retained(heap.len());
            continue;
        }
        if heap
            .peek()
            .is_some_and(|(top, _)| key.as_str() < top.as_str())
        {
            let meta = fs::metadata(&path).await?;
            heap.pop();
            heap.push((key, meta.len()));
            track_list_page_retained(heap.len());
        }
    }
    Ok(())
}

// Test-only high-water mark of keys retained by `bounded_list_page_walk`.
// Thread-local so parallel `cargo test` workers do not share a counter.
#[cfg(test)]
thread_local! {
    static LIST_PAGE_MAX_RETAINED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Records how many keys the page heap held after an insert (tests assert this).
fn track_list_page_retained(n: usize) {
    #[cfg(test)]
    LIST_PAGE_MAX_RETAINED.with(|held| held.set(held.get().max(n)));
    #[cfg(not(test))]
    let _ = n;
}

/// Writes a `.bookclerk-meta.json` sidecar when ASIN or title is present.
async fn write_local_meta_sidecar(
    backend: &LocalFsBackend,
    key: &str,
    meta: &ObjectMeta,
) -> Result<()> {
    // Skip recursive meta-for-meta; only persist meaningful identity tags.
    if key.ends_with(".bookclerk-meta.json") {
        return Ok(());
    }
    if meta.asin.is_none() && meta.title.is_none() {
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
        LIST_PAGE_MAX_RETAINED.with(|held| held.set(0));
        let page = backend.list_page("", None, 7).await.unwrap();
        assert_eq!(page.objects.len(), 7);
        let retained = LIST_PAGE_MAX_RETAINED.with(|held| held.get());
        assert!(
            retained <= 8,
            "list_page heap retained {retained} entries (limit+1 is 8) over a 400-object flat dir"
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
}
