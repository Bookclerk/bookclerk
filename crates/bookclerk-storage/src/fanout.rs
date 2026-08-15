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
    /// Enabled destinations; writes hit every child, reads succeed on the first hit.
    backends: Vec<Box<dyn StorageBackend>>,
}

impl FanoutBackend {
    /// Build a fan-out over `backends` (must be non-empty).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn new(backends: Vec<Box<dyn StorageBackend>>) -> Result<Self> {
        if backends.is_empty() {
            return Err(StorageError::InvalidKey(
                "fan-out storage requires at least one enabled destination".into(),
            ));
        }
        Ok(Self { backends })
    }

    /// True when a child reported [`StorageError::NotFound`], allowing fallback to the next backend.
    fn is_not_found(err: &StorageError) -> bool {
        matches!(err, StorageError::NotFound(_))
    }
}

#[async_trait]
impl StorageBackend for FanoutBackend {
    fn name(&self) -> &'static str {
        "multi"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(Self {
            backends: self.backends.iter().map(|b| b.clone_box()).collect(),
        })
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

    async fn list_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<crate::ListPage> {
        let all = self.list(prefix).await?;
        if let Some(c) = cursor {
            if !all.iter().any(|o| o.key.as_str() == c) {
                return Err(StorageError::InvalidCursor(
                    "stale or unknown list cursor".into(),
                ));
            }
        }
        let start = cursor
            .and_then(|c| all.iter().position(|o| o.key.as_str() == c).map(|i| i + 1))
            .unwrap_or(0);
        let limit = if limit == 0 { 256 } else { limit as usize };
        let slice: Vec<_> = all.into_iter().skip(start).take(limit + 1).collect();
        let next_cursor = if slice.len() > limit {
            slice.get(limit.saturating_sub(1)).map(|o| o.key.clone())
        } else {
            None
        };
        Ok(crate::ListPage {
            objects: slice.into_iter().take(limit).collect(),
            next_cursor,
        })
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<crate::ByteRange>,
    ) -> Result<(
        crate::ObjectProbe,
        std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    )> {
        let mut last_not_found = None;
        for backend in &self.backends {
            match backend.get_stream(key, range).await {
                Ok(got) => return Ok(got),
                Err(err) if Self::is_not_found(&err) => last_not_found = Some(err),
                Err(err) => return Err(err),
            }
        }
        Err(last_not_found.unwrap_or_else(|| StorageError::NotFound(key.into())))
    }

    async fn put_stream(
        &self,
        key: &str,
        mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> Result<crate::PutStreamResult> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        if self.backends.len() == 1 {
            return self.backends[0].put_stream(key, body, meta).await;
        }
        let mut joins = Vec::new();
        let mut writers = Vec::new();
        for backend in &self.backends {
            let (reader, writer) = tokio::io::duplex(64 * 1024);
            let boxed = backend.clone_box();
            let key = key.to_string();
            let meta = meta.clone();
            joins.push(tokio::spawn(async move {
                boxed.put_stream(&key, Box::pin(reader), meta).await
            }));
            writers.push(writer);
        }
        let mut total = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = body.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            total += n as u64;
            for writer in &mut writers {
                writer.write_all(&buf[..n]).await?;
            }
        }
        drop(writers);
        let mut last = crate::PutStreamResult {
            bytes_written: total,
            etag: None,
        };
        for join in joins {
            last = join.await.map_err(|err| {
                StorageError::Other(anyhow::anyhow!("fan-out put_stream: {err}"))
            })??;
        }
        Ok(last)
    }

    fn supports_server_copy(&self) -> bool {
        self.backends.iter().all(|b| b.supports_server_copy())
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
