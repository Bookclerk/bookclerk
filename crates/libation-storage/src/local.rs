//! Local filesystem storage backend.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use filetime::{set_file_times, FileTime};
use tokio::fs;

use crate::error::{Result, StorageError};
use crate::traits::{ObjectInfo, ObjectMeta, StorageBackend};

/// Stores objects under a root directory; keys map to relative paths.
#[derive(Debug, Clone)]
pub struct LocalFsBackend {
    root: PathBuf,
}

impl LocalFsBackend {
    /// Create a backend rooted at `root`, creating the directory if needed.
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn resolve(&self, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        let path = self.root.join(key);
        // Prevent path escape.
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

    async fn put(&self, key: &str, data: Bytes, _meta: ObjectMeta) -> Result<()> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, &data).await?;
        Ok(())
    }

    async fn put_file(&self, key: &str, source: &Path, _meta: ObjectMeta) -> Result<()> {
        let dest = self.resolve(key)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Prefer hard-link/copy without loading the whole audiobook into RAM.
        match fs::hard_link(source, &dest).await {
            Ok(()) => Ok(()),
            Err(_) => {
                fs::copy(source, &dest).await?;
                Ok(())
            }
        }
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
        list_recursive(&self.root, &self.root, prefix, &mut out).await?;
        Ok(out)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.resolve(key)?;
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(StorageError::Io(err)),
        }
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
}

async fn list_recursive(
    root: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<ObjectInfo>,
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
            Box::pin(list_recursive(root, &path, prefix, out)).await?;
            continue;
        }
        if !file_type.is_file() {
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
            .put(key, Bytes::from_static(b"audio"), ObjectMeta::default())
            .await
            .unwrap();
        assert!(backend.exists(key).await.unwrap());
        assert_eq!(backend.get(key).await.unwrap().as_ref(), b"audio");
        let listed = backend.list("Author/").await.unwrap();
        assert_eq!(listed.len(), 1);
        backend.delete(key).await.unwrap();
        assert!(!backend.exists(key).await.unwrap());
    }

    #[tokio::test]
    async fn put_file_copies_without_bytes_api() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().join("store")).unwrap();
        let src = dir.path().join("src.m4b");
        tokio::fs::write(&src, b"audiobook-bytes").await.unwrap();
        backend
            .put_file("A/T/book.m4b", &src, ObjectMeta::default())
            .await
            .unwrap();
        assert_eq!(
            backend.get("A/T/book.m4b").await.unwrap().as_ref(),
            b"audiobook-bytes"
        );
    }

    #[tokio::test]
    async fn rejects_path_escape() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let err = backend
            .put("../escape", Bytes::from_static(b"x"), ObjectMeta::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }
}
