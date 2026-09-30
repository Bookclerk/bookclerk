//! Fan-out storage: write to every enabled destination; read with fallback.
//!
//! # Pagination
//!
//! Pages walk children in configuration order. The cursor is
//! `bcf1\\u{1}{child}\\u{1}{child_cursor}` and is opaque. A sorted merge is not
//! used: S3 and plugin cursors are not lexicographic keys. Duplicate keys keep
//! the earlier child's object (source precedence) and later copies are skipped
//! via `exists` on previous children, so the page does not retain other
//! children's inventories. An empty child page that still has a cursor is
//! followed at most [`EMPTY_PAGE_HOPS`] times; a repeated cursor is
//! [`StorageError::InvalidCursor`].
//!
//! # Writes
//!
//! `put_stream` tees a 64 KiB window into one task per child. Dropping the
//! parent sets a cancel flag and aborts those tasks (`JoinHandle::abort`, not
//! a detached drop). A failed child fails the call. Children that already
//! published keep their object; replay overwrites. This is not a distributed
//! transaction.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};

use sha2::Digest;

use crate::bounded::{
    ensure_scalar_len, read_scalar_body, reject_scalar_hint, MAX_SCALAR_OBJECT_BYTES,
};
use crate::error::{Result, StorageError};
use crate::traits::{ObjectMeta, ObjectProbe, StorageBackend};

/// How many empty-but-continued child pages to follow before failing the cursor.
const EMPTY_PAGE_HOPS: u32 = 8;

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

    fn instance_id(&self) -> String {
        format!("fanout:{}", fanout_identity(&self.backends))
    }

    fn scan_placement(&self) -> Option<String> {
        let parts: Vec<Option<String>> = self
            .backends
            .iter()
            .map(|backend| backend.scan_placement())
            .collect();
        if parts.iter().all(Option::is_none) {
            return None;
        }
        // A local child makes the whole fan-out node-bound. Empty slots are
        // portable children and stay in the vector so order is part of identity.
        Some(encode_ordered_ids(
            &parts
                .into_iter()
                .map(|part| part.unwrap_or_default())
                .collect::<Vec<_>>(),
        ))
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(Self {
            backends: self.backends.iter().map(|b| b.clone_box()).collect(),
        })
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
        ensure_scalar_len(data.len(), MAX_SCALAR_OBJECT_BYTES)?;
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
            match backend.head(key).await {
                Ok(Some(probe)) => {
                    reject_scalar_hint(probe.size, MAX_SCALAR_OBJECT_BYTES)?;
                    let (_probe, body) = backend.get_stream(key, None).await?;
                    return read_scalar_body(body, MAX_SCALAR_OBJECT_BYTES).await;
                }
                Ok(None) => last_not_found = Some(StorageError::NotFound(key.into())),
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
        let limit = crate::bounded::clamp_page_limit(limit);
        let (mut child_index, mut child_cursor) =
            decode_fanout_cursor(cursor, &fanout_identity(&self.backends))?;
        if child_index >= self.backends.len() {
            return Err(StorageError::InvalidCursor(
                "fan-out cursor names a missing child".into(),
            ));
        }
        let mut objects = Vec::new();
        let mut empty_hops = 0u32;
        while child_index < self.backends.len() && objects.len() < limit {
            let want = u32::try_from(limit - objects.len()).unwrap_or(u32::MAX);
            let page = self.backends[child_index]
                .list_page(prefix, child_cursor.as_deref(), want)
                .await?;
            if page.objects.len() > usize::try_from(want).unwrap_or(usize::MAX) {
                return Err(StorageError::PayloadTooLarge(
                    "fan-out child returned a page larger than the requested limit".into(),
                ));
            }
            if page.objects.is_empty() {
                match page.next_cursor {
                    Some(next) => {
                        if child_cursor.as_deref() == Some(next.as_str()) {
                            return Err(StorageError::InvalidCursor(
                                "fan-out child cursor did not advance".into(),
                            ));
                        }
                        empty_hops += 1;
                        if empty_hops > EMPTY_PAGE_HOPS {
                            return Err(StorageError::InvalidCursor(
                                "fan-out child returned empty pages without finishing".into(),
                            ));
                        }
                        child_cursor = Some(next);
                        continue;
                    }
                    None => {
                        child_index += 1;
                        child_cursor = None;
                        empty_hops = 0;
                        continue;
                    }
                }
            }
            empty_hops = 0;
            if let Some(prev) = child_cursor.as_deref() {
                if page.next_cursor.as_deref() == Some(prev) {
                    return Err(StorageError::InvalidCursor(
                        "fan-out child cursor did not advance".into(),
                    ));
                }
            }
            for obj in page.objects {
                if child_index > 0
                    && earlier_has_key(&self.backends[..child_index], &obj.key).await?
                {
                    continue;
                }
                objects.push(obj);
            }
            match page.next_cursor {
                Some(next) => child_cursor = Some(next),
                None => {
                    child_index += 1;
                    child_cursor = None;
                }
            }
        }
        let next_cursor = if child_index < self.backends.len() {
            Some(encode_fanout_cursor(
                &fanout_identity(&self.backends),
                child_index,
                child_cursor.as_deref(),
            ))
        } else {
            None
        };
        Ok(crate::ListPage {
            objects,
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
        use tokio::io::AsyncReadExt;
        if self.backends.len() == 1 {
            return self.backends[0].put_stream(key, body, meta).await;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut joins = tokio::task::JoinSet::new();
        let mut writers = Vec::new();
        let mut aborts = Vec::new();
        for (index, backend) in self.backends.iter().enumerate() {
            let (reader, writer) = tokio::io::duplex(64 * 1024);
            let boxed = backend.clone_box();
            let key = key.to_string();
            let meta = meta.clone();
            let cancel = Arc::clone(&cancel);
            aborts.push(joins.spawn(async move {
                let body = CancellableRead {
                    inner: reader,
                    cancel,
                };
                let result = boxed.put_stream(&key, Box::pin(body), meta).await;
                (index, result)
            }));
            writers.push(writer);
        }
        let mut guard = FanoutGuard {
            cancel: Arc::clone(&cancel),
            aborts,
            armed: true,
        };
        let copied = async {
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
            Ok::<u64, StorageError>(total)
        }
        .await;
        let total = match copied {
            Ok(total) => total,
            Err(err) => {
                guard.cancel_now();
                while joins.join_next().await.is_some() {}
                return Err(err);
            }
        };
        // Children may still be flushing after source EOF. Completions are
        // observed as they arrive so a later child's error aborts an earlier
        // child that is still finalizing. The guard stays armed until every
        // task has been joined.
        let mut chosen: Option<(usize, crate::PutStreamResult)> = None;
        let mut failure = None;
        while let Some(joined) = joins.join_next().await {
            match joined {
                Ok((index, Ok(result))) => {
                    if result.bytes_written != total {
                        if failure.is_none() {
                            failure = Some(StorageError::Integrity(format!(
                                "fan-out child {index} reported {} bytes, source wrote {total}",
                                result.bytes_written
                            )));
                            guard.cancel_now();
                        }
                    } else if failure.is_none() {
                        match &chosen {
                            Some((prev, _)) if *prev <= index => {}
                            _ => chosen = Some((index, result)),
                        }
                    }
                }
                Ok((index, Err(err))) => {
                    if failure.is_none() {
                        failure = Some(err);
                        guard.cancel_now();
                        let _ = index;
                    }
                }
                Err(err) if err.is_cancelled() => {}
                Err(err) => {
                    if failure.is_none() {
                        failure = Some(StorageError::Other(anyhow::anyhow!(
                            "fan-out put_stream task: {err}"
                        )));
                        guard.cancel_now();
                    }
                }
            }
        }
        if let Some(err) = failure {
            return Err(err);
        }
        let Some((_, mut last)) = chosen else {
            return Err(StorageError::Other(anyhow::anyhow!(
                "fan-out put_stream finished without a child result"
            )));
        };
        guard.disarm();
        last.bytes_written = total;
        Ok(last)
    }

    fn supports_server_copy(&self) -> bool {
        self.backends.iter().all(|b| b.supports_server_copy())
    }
}

/// Length-prefixed child ids in configuration order.
///
/// Sorting would hide first-child precedence. A raw join is ambiguous when an
/// id contains the separator.
fn encode_ordered_ids(ids: &[String]) -> String {
    let mut out = String::from("bcid1");
    for id in ids {
        out.push('\u{1e}');
        out.push_str(&id.len().to_string());
        out.push(':');
        out.push_str(id);
    }
    out
}

fn fanout_identity(backends: &[Box<dyn StorageBackend>]) -> String {
    let ids: Vec<String> = backends
        .iter()
        .map(|backend| backend.instance_id())
        .collect();
    encode_ordered_ids(&ids)
}

fn identity_token(identity: &str) -> String {
    hex::encode(sha2::Sha256::digest(identity.as_bytes()))
}

fn encode_fanout_cursor(identity: &str, child: usize, cursor: Option<&str>) -> String {
    format!(
        "bcf2\u{1}{}\u{1}{child}\u{1}{}",
        identity_token(identity),
        cursor.unwrap_or("")
    )
}

fn decode_fanout_cursor(cursor: Option<&str>, identity: &str) -> Result<(usize, Option<String>)> {
    let Some(cursor) = cursor else {
        return Ok((0, None));
    };
    let rest = cursor.strip_prefix("bcf2\u{1}").ok_or_else(|| {
        StorageError::InvalidCursor("fan-out cursor is not from this backend".into())
    })?;
    let (token, rest) = rest.split_once('\u{1}').ok_or_else(|| {
        StorageError::InvalidCursor("fan-out cursor is missing its child order".into())
    })?;
    if token != identity_token(identity) {
        return Err(StorageError::InvalidCursor(
            "fan-out cursor was issued for a different child order".into(),
        ));
    }
    let (index, child_cursor) = rest
        .split_once('\u{1}')
        .ok_or_else(|| StorageError::InvalidCursor("fan-out cursor is truncated".into()))?;
    let child = index
        .parse::<usize>()
        .map_err(|_| StorageError::InvalidCursor("fan-out cursor child index is invalid".into()))?;
    let child_cursor = if child_cursor.is_empty() {
        None
    } else {
        Some(child_cursor.to_string())
    };
    Ok((child, child_cursor))
}

async fn earlier_has_key(earlier: &[Box<dyn StorageBackend>], key: &str) -> Result<bool> {
    for backend in earlier {
        if backend.exists(key).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

struct FanoutGuard {
    cancel: Arc<AtomicBool>,
    aborts: Vec<tokio::task::AbortHandle>,
    armed: bool,
}

impl FanoutGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }

    fn cancel_now(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        for abort in &self.aborts {
            abort.abort();
        }
        self.armed = false;
    }
}

impl Drop for FanoutGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.store(true, Ordering::SeqCst);
            for abort in &self.aborts {
                abort.abort();
            }
        }
    }
}

struct CancellableRead<R> {
    inner: R,
    cancel: Arc<AtomicBool>,
}

impl<R: AsyncRead + Unpin> AsyncRead for CancellableRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.cancel.load(Ordering::SeqCst) {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "fan-out transfer cancelled",
            )));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
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

    struct GatePut {
        fail: bool,
        entered: Arc<std::sync::atomic::AtomicUsize>,
        ready: Arc<tokio::sync::Barrier>,
        release: Arc<tokio::sync::Notify>,
        published: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl StorageBackend for GatePut {
        fn name(&self) -> &'static str {
            "gate"
        }

        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                fail: self.fail,
                entered: Arc::clone(&self.entered),
                ready: Arc::clone(&self.ready),
                release: Arc::clone(&self.release),
                published: Arc::clone(&self.published),
            })
        }

        async fn put(&self, _: &str, _: Bytes, _: ObjectMeta) -> Result<()> {
            Err(StorageError::Other(anyhow::anyhow!("unused")))
        }

        async fn get(&self, key: &str) -> Result<Bytes> {
            Err(StorageError::NotFound(key.into()))
        }

        async fn exists(&self, _: &str) -> Result<bool> {
            Ok(false)
        }

        async fn list(&self, _: &str) -> Result<Vec<crate::ObjectInfo>> {
            Ok(Vec::new())
        }

        async fn probe(&self, key: &str) -> Result<crate::ObjectProbe> {
            Err(StorageError::NotFound(key.into()))
        }

        async fn copy(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }

        async fn delete(&self, _: &str) -> Result<()> {
            Ok(())
        }

        async fn list_page(&self, _: &str, _: Option<&str>, _: u32) -> Result<crate::ListPage> {
            Ok(crate::ListPage::default())
        }

        async fn get_stream(
            &self,
            key: &str,
            _: Option<crate::ByteRange>,
        ) -> Result<(
            crate::ObjectProbe,
            std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        )> {
            Err(StorageError::NotFound(key.into()))
        }

        async fn put_stream(
            &self,
            _: &str,
            mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            _: ObjectMeta,
        ) -> Result<crate::PutStreamResult> {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 32];
            loop {
                if body.read(&mut buf).await? == 0 {
                    break;
                }
            }
            self.entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.ready.wait().await;
            if self.fail {
                return Err(StorageError::Io(std::io::Error::other(
                    "child failed during finalization",
                )));
            }
            self.release.notified().await;
            self.published
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::PutStreamResult {
                bytes_written: 4,
                ..crate::PutStreamResult::default()
            })
        }
    }

    fn gate(
        fail: bool,
        entered: &Arc<std::sync::atomic::AtomicUsize>,
        ready: &Arc<tokio::sync::Barrier>,
        release: &Arc<tokio::sync::Notify>,
        published: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> GatePut {
        GatePut {
            fail,
            entered: Arc::clone(entered),
            ready: Arc::clone(ready),
            release: Arc::clone(release),
            published: Arc::clone(published),
        }
    }

    async fn wait_entered(entered: &std::sync::atomic::AtomicUsize, n: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while entered.load(std::sync::atomic::Ordering::SeqCst) < n {
            if tokio::time::Instant::now() > deadline {
                panic!("children did not reach finalization");
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn drop_after_source_eof_aborts_children_still_finalizing() {
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ready = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Notify::new());
        let published = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fan = FanoutBackend::new(vec![
            Box::new(gate(false, &entered, &ready, &release, &published)),
            Box::new(gate(false, &entered, &ready, &release, &published)),
        ])
        .unwrap();
        let entered_wait = Arc::clone(&entered);
        let task = tokio::spawn(async move {
            fan.put_stream(
                "book.m4b",
                Box::pin(std::io::Cursor::new(b"data".to_vec())),
                ObjectMeta::default(),
            )
            .await
        });
        wait_entered(entered_wait.as_ref(), 2).await;
        task.abort();
        let _ = task.await;
        tokio::task::yield_now().await;
        assert_eq!(
            published.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "aborted children must not publish after the parent is dropped"
        );
    }

    #[tokio::test]
    async fn child_finalization_error_aborts_blocked_sibling() {
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ready = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Notify::new());
        let published = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fan = FanoutBackend::new(vec![
            Box::new(gate(true, &entered, &ready, &release, &published)),
            Box::new(gate(false, &entered, &ready, &release, &published)),
        ])
        .unwrap();
        let err = fan
            .put_stream(
                "book.m4b",
                Box::pin(std::io::Cursor::new(b"data".to_vec())),
                ObjectMeta::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Io(_)), "{err}");
        assert_eq!(published.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn later_child_finalization_error_aborts_the_blocked_first_child() {
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ready = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Notify::new());
        let published = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fan = FanoutBackend::new(vec![
            Box::new(gate(false, &entered, &ready, &release, &published)),
            Box::new(gate(true, &entered, &ready, &release, &published)),
        ])
        .unwrap();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fan.put_stream(
                "book.m4b",
                Box::pin(std::io::Cursor::new(b"data".to_vec())),
                ObjectMeta::default(),
            ),
        )
        .await
        .expect("parent waited for the stalled first child")
        .unwrap_err();
        assert!(matches!(err, StorageError::Io(_)), "{err}");
        assert_eq!(published.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn child_byte_count_mismatch_is_integrity_and_stops_the_sibling() {
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ready = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Notify::new());
        let published = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fan = FanoutBackend::new(vec![
            Box::new(LieCount {
                short: false,
                entered: Arc::clone(&entered),
                ready: Arc::clone(&ready),
                release: Arc::clone(&release),
                published: Arc::clone(&published),
            }),
            Box::new(LieCount {
                short: true,
                entered: Arc::clone(&entered),
                ready: Arc::clone(&ready),
                release: Arc::clone(&release),
                published: Arc::clone(&published),
            }),
        ])
        .unwrap();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fan.put_stream(
                "book.m4b",
                Box::pin(std::io::Cursor::new(b"data".to_vec())),
                ObjectMeta::default(),
            ),
        )
        .await
        .expect("parent waited for the stalled child")
        .unwrap_err();
        assert!(matches!(err, StorageError::Integrity(_)), "{err}");
        assert_eq!(published.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    struct LieCount {
        short: bool,
        entered: Arc<std::sync::atomic::AtomicUsize>,
        ready: Arc<tokio::sync::Barrier>,
        release: Arc<tokio::sync::Notify>,
        published: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl StorageBackend for LieCount {
        fn name(&self) -> &'static str {
            "lie"
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                short: self.short,
                entered: Arc::clone(&self.entered),
                ready: Arc::clone(&self.ready),
                release: Arc::clone(&self.release),
                published: Arc::clone(&self.published),
            })
        }
        async fn put(&self, _: &str, _: Bytes, _: ObjectMeta) -> Result<()> {
            Ok(())
        }
        async fn get(&self, key: &str) -> Result<Bytes> {
            Err(StorageError::NotFound(key.into()))
        }
        async fn exists(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn list(&self, _: &str) -> Result<Vec<crate::ObjectInfo>> {
            Ok(Vec::new())
        }
        async fn probe(&self, key: &str) -> Result<crate::ObjectProbe> {
            Err(StorageError::NotFound(key.into()))
        }
        async fn copy(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn list_page(&self, _: &str, _: Option<&str>, _: u32) -> Result<crate::ListPage> {
            Ok(crate::ListPage::default())
        }
        async fn get_stream(
            &self,
            key: &str,
            _: Option<crate::ByteRange>,
        ) -> Result<(
            crate::ObjectProbe,
            std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        )> {
            Err(StorageError::NotFound(key.into()))
        }
        async fn put_stream(
            &self,
            _: &str,
            mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            _: ObjectMeta,
        ) -> Result<crate::PutStreamResult> {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 32];
            let mut total = 0u64;
            loop {
                let n = body.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
            self.entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.ready.wait().await;
            if self.short {
                return Ok(crate::PutStreamResult {
                    bytes_written: total.saturating_sub(1),
                    ..crate::PutStreamResult::default()
                });
            }
            self.release.notified().await;
            self.published
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::PutStreamResult {
                bytes_written: total,
                ..crate::PutStreamResult::default()
            })
        }
    }

    #[tokio::test]
    async fn child_order_is_part_of_identity_and_duplicate_keys_keep_the_first_child() {
        let dir = tempfile::tempdir().unwrap();
        let left = LocalFsBackend::new(dir.path().join("left"))
            .unwrap()
            .with_scan_placement("node-left");
        let right = LocalFsBackend::new(dir.path().join("right"))
            .unwrap()
            .with_scan_placement("node-right");
        let weird = LocalFsBackend::new(dir.path().join("a|b")).unwrap();
        left.put(
            "book.m4b",
            Bytes::from_static(b"from-left"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
        right
            .put(
                "book.m4b",
                Bytes::from_static(b"from-right"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        right
            .put(
                "only-right.m4b",
                Bytes::from_static(b"right"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let forward =
            FanoutBackend::new(vec![Box::new(left.clone()), Box::new(right.clone())]).unwrap();
        let reverse =
            FanoutBackend::new(vec![Box::new(right.clone()), Box::new(left.clone())]).unwrap();
        assert_ne!(forward.instance_id(), reverse.instance_id());
        assert_ne!(forward.scan_placement(), reverse.scan_placement());
        assert!(forward.scan_placement().is_some());
        let single = FanoutBackend::new(vec![Box::new(weird)]).unwrap();
        let split = FanoutBackend::new(vec![
            Box::new(LocalFsBackend::new(dir.path().join("a")).unwrap()),
            Box::new(LocalFsBackend::new(dir.path().join("b")).unwrap()),
        ])
        .unwrap();
        assert_ne!(single.instance_id(), split.instance_id());
        assert_eq!(
            forward.get("book.m4b").await.unwrap().as_ref(),
            b"from-left"
        );
        let page = forward.list_page("", None, 1).await.unwrap();
        let cursor = page.next_cursor.expect("child cursor");
        let err = reverse.list_page("", Some(&cursor), 1).await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidCursor(_)), "{err}");
        let listed = forward.list_page("", None, 10).await.unwrap();
        let keys: Vec<_> = listed.objects.iter().map(|obj| obj.key.as_str()).collect();
        assert_eq!(keys.iter().filter(|key| **key == "book.m4b").count(), 1);
        assert!(keys.contains(&"only-right.m4b"));
    }

    #[tokio::test]
    async fn later_child_duplicate_checks_one_head_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let left = LocalFsBackend::new(dir.path().join("left")).unwrap();
        let right = LocalFsBackend::new(dir.path().join("right")).unwrap();
        for name in ["a.m4b", "b.m4b", "c.m4b"] {
            left.put(name, Bytes::from_static(b"l"), ObjectMeta::default())
                .await
                .unwrap();
            right
                .put(name, Bytes::from_static(b"r"), ObjectMeta::default())
                .await
                .unwrap();
        }
        let heads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fan = FanoutBackend::new(vec![
            Box::new(CountExists {
                inner: left,
                heads: Arc::clone(&heads),
            }),
            Box::new(right),
        ])
        .unwrap();
        let page = fan.list_page("", None, 10).await.unwrap();
        assert_eq!(page.objects.len(), 3);
        assert_eq!(heads.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    struct CountExists {
        inner: LocalFsBackend,
        heads: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl StorageBackend for CountExists {
        fn name(&self) -> &'static str {
            "count-exists"
        }
        fn instance_id(&self) -> String {
            self.inner.instance_id()
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                heads: Arc::clone(&self.heads),
            })
        }
        async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
            self.inner.put(key, data, meta).await
        }
        async fn get(&self, key: &str) -> Result<Bytes> {
            self.inner.get(key).await
        }
        async fn exists(&self, key: &str) -> Result<bool> {
            self.heads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.exists(key).await
        }
        async fn list(&self, prefix: &str) -> Result<Vec<crate::ObjectInfo>> {
            self.inner.list(prefix).await
        }
        async fn probe(&self, key: &str) -> Result<crate::ObjectProbe> {
            self.inner.probe(key).await
        }
        async fn copy(&self, from: &str, to: &str) -> Result<()> {
            self.inner.copy(from, to).await
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key).await
        }
        async fn list_page(
            &self,
            prefix: &str,
            cursor: Option<&str>,
            limit: u32,
        ) -> Result<crate::ListPage> {
            self.inner.list_page(prefix, cursor, limit).await
        }
        async fn get_stream(
            &self,
            key: &str,
            range: Option<crate::ByteRange>,
        ) -> Result<(
            crate::ObjectProbe,
            std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        )> {
            self.inner.get_stream(key, range).await
        }
        async fn put_stream(
            &self,
            key: &str,
            body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            meta: ObjectMeta,
        ) -> Result<crate::PutStreamResult> {
            self.inner.put_stream(key, body, meta).await
        }
    }
}
