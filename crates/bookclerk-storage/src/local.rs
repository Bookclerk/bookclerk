//! Local filesystem storage backend.

use std::path::{Path, PathBuf};
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
        })
    }

    /// Overrides the host placement reported for scan adoption.
    ///
    /// Production backends leave this unset and use [`crate::host_placement_id`].
    #[must_use]
    pub fn with_scan_placement(mut self, placement: impl Into<String>) -> Self {
        self.placement_override = Some(placement.into());
        self
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

/// Rejects empty keys, absolute keys, and any `ParentDir` segment.
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('/') {
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
                .unwrap_or_else(crate::host_placement_id),
        )
    }

    fn supports_server_copy(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
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
        let mut probe = ObjectProbe {
            key: key.to_string(),
            size: file_meta.len(),
            content_type: None,
            etag: None,
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
        mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        mut meta: ObjectMeta,
    ) -> Result<crate::PutStreamResult> {
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
            tokio::fs::rename(&tmp, &path).await?;
            guard.disarm();
            Ok(bytes_written)
        };
        match put.await {
            Ok(bytes_written) => {
                write_local_meta_sidecar(self, key, &meta).await?;
                Ok(crate::PutStreamResult {
                    bytes_written,
                    etag: None,
                    sha256_hex: meta.sha256_hex,
                })
            }
            Err(err) => Err(err),
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
}
