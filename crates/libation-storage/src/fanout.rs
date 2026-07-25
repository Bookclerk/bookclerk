//! Fan-out storage: write to every enabled destination; read with fallback.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::{Result, StorageError};
use crate::traits::{ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend};

/// Multiplexes one logical storage key across multiple backends.
///
/// Mutations (`put*`, `copy`, `rename`, `delete`, `touch_file`) run on every
/// child. Reads try children in order and succeed on the first hit.
pub struct FanoutBackend {
    backends: Vec<Box<dyn StorageBackend>>,
}

impl FanoutBackend {
    /// Build a fan-out over `backends` (must be non-empty).
    pub fn new(backends: Vec<Box<dyn StorageBackend>>) -> Result<Self> {
        if backends.is_empty() {
            return Err(StorageError::InvalidKey(
                "fan-out storage requires at least one enabled destination".into(),
            ));
        }
        Ok(Self { backends })
    }

    fn is_not_found(err: &StorageError) -> bool {
        matches!(err, StorageError::NotFound(_))
    }
}

#[async_trait]
impl StorageBackend for FanoutBackend {
    fn name(&self) -> &'static str {
        "multi"
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
        for backend in &self.backends {
            backend.put(key, data.clone(), meta.clone()).await?;
        }
        Ok(())
    }

    async fn put_file(&self, key: &str, path: &Path, meta: ObjectMeta) -> Result<()> {
        for backend in &self.backends {
            backend.put_file(key, path, meta.clone()).await?;
        }
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let mut last_not_found = None;
        for backend in &self.backends {
            match backend.get(key).await {
                Ok(data) => return Ok(data),
                Err(err) if Self::is_not_found(&err) => last_not_found = Some(err),
                Err(err) => return Err(err),
            }
        }
        Err(last_not_found.unwrap_or_else(|| StorageError::NotFound(key.into())))
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        for backend in &self.backends {
            if backend.exists(key).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let mut by_key = BTreeMap::new();
        for backend in &self.backends {
            for info in backend.list(prefix).await? {
                by_key.entry(info.key.clone()).or_insert(info);
            }
        }
        Ok(by_key.into_values().collect())
    }

    async fn probe(&self, key: &str) -> Result<ObjectProbe> {
        let mut last_not_found = None;
        for backend in &self.backends {
            match backend.probe(key).await {
                Ok(probe) => return Ok(probe),
                Err(err) if Self::is_not_found(&err) => last_not_found = Some(err),
                Err(err) => return Err(err),
            }
        }
        Err(last_not_found.unwrap_or_else(|| StorageError::NotFound(key.into())))
    }

    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        for backend in &self.backends {
            backend.copy(from, to).await?;
        }
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        for backend in &self.backends {
            backend.rename(from, to).await?;
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        for backend in &self.backends {
            backend.delete(key).await?;
        }
        Ok(())
    }

    async fn touch_file(
        &self,
        key: &str,
        created: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<()> {
        for backend in &self.backends {
            backend.touch_file(key, created, modified).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::LocalFsBackend;
    use tempfile::tempdir;

    #[tokio::test]
    async fn put_writes_to_all_backends() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let fan = FanoutBackend::new(vec![
            Box::new(LocalFsBackend::new(a.path().to_path_buf()).unwrap()),
            Box::new(LocalFsBackend::new(b.path().to_path_buf()).unwrap()),
        ])
        .unwrap();

        fan.put(
            "book.m4b",
            Bytes::from_static(b"audio"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        assert!(a.path().join("book.m4b").is_file());
        assert!(b.path().join("book.m4b").is_file());
        assert!(fan.exists("book.m4b").await.unwrap());
    }

    #[tokio::test]
    async fn get_falls_back_when_missing_on_first() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let first = LocalFsBackend::new(a.path().to_path_buf()).unwrap();
        let second = LocalFsBackend::new(b.path().to_path_buf()).unwrap();
        second
            .put(
                "only-b.m4b",
                Bytes::from_static(b"x"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();

        let fan = FanoutBackend::new(vec![Box::new(first), Box::new(second)]).unwrap();
        let bytes = fan.get("only-b.m4b").await.unwrap();
        assert_eq!(bytes.as_ref(), b"x");
    }

    #[tokio::test]
    async fn list_unions_keys() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let first = LocalFsBackend::new(a.path().to_path_buf()).unwrap();
        let second = LocalFsBackend::new(b.path().to_path_buf()).unwrap();
        first
            .put("a.m4b", Bytes::from_static(b"a"), ObjectMeta::default())
            .await
            .unwrap();
        second
            .put("b.m4b", Bytes::from_static(b"b"), ObjectMeta::default())
            .await
            .unwrap();
        second
            .put("a.m4b", Bytes::from_static(b"dup"), ObjectMeta::default())
            .await
            .unwrap();

        let fan = FanoutBackend::new(vec![Box::new(first), Box::new(second)]).unwrap();
        let keys: Vec<_> = fan
            .list("")
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(keys, vec!["a.m4b".to_string(), "b.m4b".to_string()]);
    }
}
