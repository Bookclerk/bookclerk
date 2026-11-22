//! A storage guest's `Destination` capability over an object store.
//!
//! The `s3` and `r2` destination plugins store through an `S3Backend` and
//! the `b2` plugin through a `B2Backend` (each built its own way: endpoint,
//! credentials, change feed). They answer the plugin ABI the same way:
//! streamed writes with optional conditions, staged writes committed by
//! token, and the intent operations through [`crate::guest`].

use std::pin::Pin;

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    ByteRange, ChangesPage, ChangesRequest, CopyResult, Destination, KeyOutcome, ListOptions,
    ListPage, MoveManyPage, MoveManyRequest, MoveOptions, MoveResult, ObjectMetadata, PluginError,
    PutResult, ReadResult, WriteOptions,
};
use tokio::io::AsyncRead;

use crate::guest::{self, object_metadata, plugin_error as map_storage};
use crate::{ChangeFeedPage, ObjectMeta, StorageBackend, StorageError};

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// A change feed kept beside the backend rather than in it, for a feed
/// whose calls are not `Send` (one over the guest's own bindings, such as
/// the `b2` destination's webhook feed).
#[async_trait(?Send)]
pub trait ChangeSource {
    /// One page of changes since `cursor`, or `None` when the feed is not
    /// running now (the host lists instead).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidCursor`] for a cursor the feed does
    /// not accept, and the feed's other failures.
    async fn changes(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> std::result::Result<Option<ChangeFeedPage>, StorageError>;
}

/// The `Destination` capability of an object-store guest.
pub struct ObjectStoreDestination<B> {
    /// The backend every call goes to.
    backend: B,
    /// The change feed, when it is kept beside the backend; otherwise
    /// `changes` asks the backend.
    feed: Option<Box<dyn ChangeSource>>,
}

impl<B: StorageBackend> ObjectStoreDestination<B> {
    /// Serves `backend` as a destination.
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            feed: None,
        }
    }

    /// Answers `changes` from `feed` instead of the backend.
    #[must_use]
    pub fn with_changes(mut self, feed: Box<dyn ChangeSource>) -> Self {
        self.feed = Some(feed);
        self
    }
}

/// The object metadata a write asks for.
fn write_meta(options: &WriteOptions) -> Result<ObjectMeta> {
    let sha256_hex =
        crate::sha256_field_from_raw(options.sha256.as_deref()).map_err(map_storage)?;
    Ok(ObjectMeta {
        content_type: options.content_type.clone(),
        content_length: options.content_length,
        sha256_hex,
        commit_token: options.commit_token.clone(),
        ..Default::default()
    })
}

/// The SHA-256 a write reports, as raw bytes.
fn put_sha(hex: Option<&str>) -> Option<Vec<u8>> {
    hex.and_then(|hex| {
        crate::parse_sha256_hex(hex)
            .ok()
            .map(|bytes| bytes.to_vec())
    })
}

/// Destination-side staging key. Bytes never spool on the host or broker.
fn stage_object_key(token: Option<&str>, key: &str) -> Result<String> {
    let token = token.unwrap_or("");
    if token.is_empty()
        || token.contains('/')
        || token.contains('\\')
        || token.contains("..")
        || token.contains('\0')
    {
        return Err(PluginError::invalid_params(
            "commit_token must be a non-empty identifier without path separators",
        ));
    }
    if key.is_empty() {
        return Err(PluginError::invalid_params("object key required"));
    }
    Ok(format!(".bookclerk-stage/{token}/{key}"))
}

#[async_trait(?Send)]
impl<B: StorageBackend> Destination for ObjectStoreDestination<B> {
    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>> {
        self.backend
            .head(key)
            .await
            .map(|probe| probe.map(object_metadata))
            .map_err(map_storage)
    }

    async fn list(&self, options: ListOptions) -> Result<ListPage> {
        guest::list(&self.backend, options).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<ReadResult> {
        let storage_range = range.map(|r| crate::ByteRange {
            offset: r.offset,
            length: r.length,
        });
        let (probe, body) = self
            .backend
            .get_stream(key, storage_range)
            .await
            .map_err(map_storage)?;
        Ok(ReadResult {
            meta: object_metadata(probe),
            body,
        })
    }

    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> Result<PutResult> {
        let condition = guest::write_condition(&options)?;
        let dest_key = if options.stage_only {
            stage_object_key(options.commit_token.as_deref(), key)?
        } else {
            key.to_string()
        };
        let meta = write_meta(&options)?;
        let written = match &condition {
            Some(condition) => {
                self.backend
                    .put_stream_conditional(&dest_key, body, meta, condition)
                    .await
                    .map_err(map_storage)?
                    .put
            }
            None => self
                .backend
                .put_stream(&dest_key, body, meta)
                .await
                .map_err(map_storage)?,
        };
        Ok(PutResult {
            key: key.into(),
            bytes_written: written.bytes_written,
            etag: options.commit_token.clone().or(written.etag),
            sha256: put_sha(written.sha256_hex.as_deref()),
            ..Default::default()
        })
    }

    async fn copy(&self, from: &str, to: &str) -> Result<CopyResult> {
        let bytes_copied = self
            .backend
            .copy_sized(from, to)
            .await
            .map_err(map_storage)?;
        Ok(CopyResult { bytes_copied })
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.backend.delete(key).await.map_err(map_storage)
    }

    async fn commit(&self, key: &str, commit_token: &str) -> Result<PutResult> {
        let staged = stage_object_key(Some(commit_token), key)?;
        match self.backend.probe(&staged).await {
            Ok(probe) => {
                self.backend.copy(&staged, key).await.map_err(map_storage)?;
                let _ = self.backend.delete(&staged).await;
                Ok(PutResult {
                    key: key.into(),
                    bytes_written: probe.size,
                    etag: Some(commit_token.into()),
                    sha256: put_sha(probe.meta.sha256_hex.as_deref()),
                    ..Default::default()
                })
            }
            Err(StorageError::NotFound(_)) => {
                let probe = self.backend.probe(key).await.map_err(map_storage)?;
                if probe.meta.commit_token.as_deref() != Some(commit_token) {
                    return Err(PluginError::not_found(format!(
                        "staged object missing and `{key}` is not commit {commit_token}"
                    )));
                }
                Ok(PutResult {
                    key: key.into(),
                    bytes_written: probe.size,
                    etag: Some(commit_token.into()),
                    sha256: put_sha(probe.meta.sha256_hex.as_deref()),
                    ..Default::default()
                })
            }
            Err(err) => Err(map_storage(err)),
        }
    }

    async fn move_object(&self, from: &str, to: &str, options: MoveOptions) -> Result<MoveResult> {
        guest::move_object(&self.backend, from, to, options).await
    }

    async fn move_many(&self, request: MoveManyRequest) -> Result<MoveManyPage> {
        guest::move_many(&self.backend, request).await
    }

    async fn delete_many(&self, keys: Vec<String>) -> Result<Vec<KeyOutcome>> {
        guest::delete_many(&self.backend, keys).await
    }

    async fn head_many(&self, keys: Vec<String>) -> Result<Vec<Option<ObjectMetadata>>> {
        guest::head_many(&self.backend, keys).await
    }

    async fn translate_keys(&self, keys: Vec<String>) -> Result<Vec<String>> {
        guest::translate_keys(&self.backend, keys).await
    }

    async fn changes(&self, request: ChangesRequest) -> Result<ChangesPage> {
        match &self.feed {
            Some(feed) => {
                let page = feed
                    .changes(request.cursor.as_deref(), request.limit)
                    .await
                    .map_err(map_storage)?;
                Ok(guest::changes_page(page))
            }
            None => guest::changes(&self.backend, request).await,
        }
    }

    async fn abort_stage(&self, key: &str, commit_token: &str) -> Result<()> {
        let staged = stage_object_key(Some(commit_token), key)?;
        match self.backend.delete(&staged).await {
            Ok(()) => Ok(()),
            Err(StorageError::NotFound(_)) => Ok(()),
            Err(err) => Err(map_storage(err)),
        }
    }
}

#[cfg(test)]
#[cfg(feature = "s3")]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use base64::Engine;
    use bookclerk_plugin_sdk::PluginErrorCode;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::s3::s3_protocol::{new_world, open_backend, serve};

    /// A destination over a fresh in-memory S3 fixture.
    async fn destination(dir: &std::path::Path) -> ObjectStoreDestination<crate::S3Backend> {
        let url = serve(new_world(false)).await;
        ObjectStoreDestination::new(open_backend(&url, dir).await)
    }

    /// A body stream over `bytes`.
    fn body(bytes: &[u8]) -> Pin<Box<dyn AsyncRead + Send>> {
        Box::pin(std::io::Cursor::new(bytes.to_vec()))
    }

    /// Reads a whole object.
    async fn read<B: StorageBackend>(dest: &ObjectStoreDestination<B>, key: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut result = dest.get(key, None).await.expect("get");
        result.body.read_to_end(&mut out).await.expect("read");
        out
    }

    fn sized(len: usize) -> WriteOptions {
        WriteOptions {
            content_length: Some(u64::try_from(len).expect("len")),
            content_type: Some("audio/mp4".into()),
            ..WriteOptions::default()
        }
    }

    #[tokio::test]
    async fn objects_are_written_read_listed_copied_and_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = destination(dir.path()).await;
        assert!(dest.head("a.m4b").await.expect("head").is_none());
        let put = dest
            .put("a.m4b", body(b"abc"), sized(3))
            .await
            .expect("put");
        assert_eq!(put.key, "a.m4b");
        assert_eq!(put.bytes_written, 3);
        let meta = dest.head("a.m4b").await.expect("head").expect("stored");
        assert_eq!(meta.size, 3);
        assert_eq!(read(&dest, "a.m4b").await, b"abc");
        let mut range = dest
            .get(
                "a.m4b",
                Some(ByteRange {
                    offset: 1,
                    length: Some(1),
                }),
            )
            .await
            .expect("range");
        assert_eq!(range.meta.key, "a.m4b");
        let mut ranged = Vec::new();
        range.body.read_to_end(&mut ranged).await.expect("read");
        assert_eq!(ranged, b"b", "a ranged read returns only the range");

        let copied = dest.copy("a.m4b", "b.m4b").await.expect("copy");
        assert_eq!(copied.bytes_copied, 3);
        assert_eq!(read(&dest, "b.m4b").await, b"abc");

        let page = dest.list(ListOptions::default()).await.expect("list");
        let mut keys: Vec<_> = page.objects.iter().map(|o| o.key.as_str()).collect();
        keys.retain(|key| !key.contains(".bookclerk-"));
        keys.sort_unstable();
        assert_eq!(keys, ["a.m4b", "b.m4b"]);

        let heads = dest
            .head_many(vec!["a.m4b".into(), "gone".into()])
            .await
            .expect("head_many");
        assert!(heads[0].is_some() && heads[1].is_none());
        let translated = dest
            .translate_keys(vec!["Author/Title.m4b".into()])
            .await
            .expect("translate");
        assert_eq!(translated, ["Author/Title.m4b"]);

        dest.delete("b.m4b").await.expect("delete");
        assert!(dest.head("b.m4b").await.expect("head").is_none());
        let outcomes = dest
            .delete_many(vec!["a.m4b".into()])
            .await
            .expect("delete_many");
        assert_eq!(outcomes.len(), 1);
        assert!(dest.head("a.m4b").await.expect("head").is_none());
    }

    #[tokio::test]
    async fn conditional_writes_and_moves_follow_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = destination(dir.path()).await;
        dest.put("a", body(b"one"), sized(3)).await.expect("put");
        let refused = dest
            .put(
                "a",
                body(b"two"),
                WriteOptions {
                    if_none_match: true,
                    ..sized(3)
                },
            )
            .await
            .expect_err("an existing object is not replaced");
        assert_eq!(refused.code, PluginErrorCode::Conflict, "{refused}");
        assert_eq!(read(&dest, "a").await, b"one");
        dest.put(
            "fresh",
            body(b"new"),
            WriteOptions {
                if_none_match: true,
                ..sized(3)
            },
        )
        .await
        .expect("a new name takes a no-overwrite write");

        let moved = dest
            .move_object("a", "b", MoveOptions::default())
            .await
            .expect("move");
        assert!(!moved.already_moved);
        assert!(dest.head("a").await.expect("head").is_none());
        assert_eq!(read(&dest, "b").await, b"one");
    }

    #[tokio::test]
    async fn staged_writes_publish_on_commit_and_commit_again_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = destination(dir.path()).await;
        let staged = WriteOptions {
            stage_only: true,
            commit_token: Some("tok1".into()),
            ..sized(5)
        };
        let put = dest
            .put("book.m4b", body(b"hello"), staged)
            .await
            .expect("stage");
        assert_eq!(put.etag.as_deref(), Some("tok1"), "the token is the etag");
        assert!(dest.head("book.m4b").await.expect("head").is_none());

        let committed = dest.commit("book.m4b", "tok1").await.expect("commit");
        assert_eq!(committed.bytes_written, 5);
        assert_eq!(read(&dest, "book.m4b").await, b"hello");
        assert!(dest
            .head(".bookclerk-stage/tok1/book.m4b")
            .await
            .expect("head")
            .is_none());

        let again = dest
            .commit("book.m4b", "tok1")
            .await
            .expect("retried commit");
        assert_eq!(again.bytes_written, 5);
        let other = dest
            .commit("book.m4b", "tok2")
            .await
            .expect_err("another token is not this commit");
        assert_eq!(other.code, PluginErrorCode::NotFound, "{other}");

        dest.abort_stage("book.m4b", "tok3")
            .await
            .expect("aborting nothing is fine");
        let staged = WriteOptions {
            stage_only: true,
            commit_token: Some("tok4".into()),
            ..sized(1)
        };
        dest.put("x", body(b"x"), staged).await.expect("stage");
        dest.abort_stage("x", "tok4").await.expect("abort");
        assert!(dest
            .head(".bookclerk-stage/tok4/x")
            .await
            .expect("head")
            .is_none());
    }

    #[tokio::test]
    async fn bad_stage_tokens_and_conditions_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = destination(dir.path()).await;
        for token in [
            None,
            Some(""),
            Some("a/b"),
            Some("a\\b"),
            Some(".."),
            Some("a\0"),
        ] {
            let options = WriteOptions {
                stage_only: true,
                commit_token: token.map(str::to_string),
                ..sized(1)
            };
            let err = dest
                .put("k", body(b"x"), options)
                .await
                .expect_err("refused");
            assert_eq!(err.code, PluginErrorCode::InvalidParams, "{token:?}: {err}");
        }
        let err = dest.commit("", "tok").await.expect_err("refused");
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        let both = WriteOptions {
            if_none_match: true,
            if_match: Some("e".into()),
            ..sized(1)
        };
        assert!(dest.put("k", body(b"x"), both).await.is_err());
        let bad_sha = WriteOptions {
            sha256: Some(vec![1, 2, 3]),
            ..sized(1)
        };
        assert!(dest.put("k", body(b"x"), bad_sha).await.is_err());
        assert!(put_sha(Some("not hex")).is_none());
        assert_eq!(put_sha(Some(&"00".repeat(32))), Some(vec![0; 32]));
    }

    /// What the fake Cloudflare Queues API holds.
    #[derive(Default)]
    struct FakeQueues {
        /// Messages not yet leased: (body, lease id).
        ready: Vec<(String, String)>,
        /// Leased messages by lease id.
        leased: HashMap<String, String>,
        /// Acknowledged lease ids.
        acked: Vec<String>,
        /// Paths requested.
        paths: Vec<String>,
        /// Bearer tokens seen.
        tokens: Vec<String>,
    }

    /// Serves the pull and ack endpoints of one queue over plain HTTP.
    async fn serve_queues(state: Arc<Mutex<FakeQueues>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 8192];
                    let (head_end, length) = loop {
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&buf[..n]);
                        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&data[..at]).to_ascii_lowercase();
                            let length = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (at + 4, length);
                        }
                    };
                    while data.len() < head_end + length {
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        data.extend_from_slice(&buf[..n]);
                    }
                    let head = String::from_utf8_lossy(&data[..head_end]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let token = head
                        .lines()
                        .find_map(|l| l.strip_prefix("authorization: Bearer "))
                        .or_else(|| {
                            head.lines()
                                .find_map(|l| l.strip_prefix("Authorization: Bearer "))
                        })
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let request: serde_json::Value =
                        serde_json::from_slice(&data[head_end..]).unwrap_or_default();
                    let reply = {
                        let mut state = state.lock().expect("lock");
                        state.paths.push(path.clone());
                        state.tokens.push(token);
                        if !path.starts_with("/accounts/") {
                            serde_json::json!({"success": false, "errors": [{"code": 7003, "message": "no route"}]})
                        } else if path.ends_with("/messages/pull") {
                            let batch = request["batch_size"].as_u64().unwrap_or(1);
                            let take = usize::try_from(batch).unwrap_or(1).min(state.ready.len());
                            let pulled: Vec<_> = state.ready.drain(..take).collect();
                            let messages: Vec<_> = pulled
                                .iter()
                                .map(|(body, lease)| {
                                    serde_json::json!({"body": body, "lease_id": lease, "id": lease})
                                })
                                .collect();
                            for (body, lease) in pulled {
                                state.leased.insert(lease, body);
                            }
                            serde_json::json!({"success": true, "errors": [], "result": {"messages": messages}})
                        } else if path.ends_with("/messages/ack") {
                            for ack in request["acks"].as_array().cloned().unwrap_or_default() {
                                let lease = ack["lease_id"].as_str().unwrap_or("").to_string();
                                state.leased.remove(&lease);
                                state.acked.push(lease);
                            }
                            for retry in request["retries"].as_array().cloned().unwrap_or_default()
                            {
                                let lease = retry["lease_id"].as_str().unwrap_or("").to_string();
                                if let Some(body) = state.leased.remove(&lease) {
                                    state.ready.push((body, lease));
                                }
                            }
                            serde_json::json!({"success": true, "errors": [], "result": {"ackCount": 1}})
                        } else {
                            serde_json::json!({"success": false, "errors": [{"code": 7003, "message": "no route"}]})
                        }
                    };
                    let text = reply.to_string();
                    let status = if path.starts_with("/accounts/") {
                        "200 OK"
                    } else {
                        "404 Not Found"
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    const ACCOUNT: &str = "3f4b7e3dcab231cbfdaa90a6a28bd548";
    const QUEUE: &str = "0123456789abcdef0123456789abcdef";

    fn r2_event(action: &str, key: &str) -> String {
        serde_json::json!({
            "account": ACCOUNT,
            "action": action,
            "bucket": "library",
            "object": {"key": key, "size": 3, "eTag": "abc"},
            "eventTime": "2026-11-21T03:00:00.000Z",
        })
        .to_string()
    }

    #[tokio::test]
    async fn changes_come_from_the_cloudflare_queue_and_are_acked_after_the_next_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queues = Arc::new(Mutex::new(FakeQueues::default()));
        {
            let mut state = queues.lock().expect("lock");
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(r2_event("PutObject", "a.m4b"));
            state.ready.push((encoded, "l1".into()));
            state
                .ready
                .push((r2_event("DeleteObject", "b.m4b"), "l2".into()));
            state.ready.push(("not an event".into(), "l3".into()));
        }
        let api = serve_queues(Arc::clone(&queues)).await;
        let url = serve(new_world(false)).await;
        let backend = open_backend(&url, dir.path())
            .await
            .with_cloudflare_queue_at(
                &api,
                &crate::CloudflareAccountId::parse(ACCOUNT).expect("account"),
                &crate::CloudflareQueueId::parse(QUEUE).expect("queue"),
                crate::CloudflareApiToken::new("queue-token".into()).expect("token"),
                &dir.path().join("queue-state"),
            )
            .expect("queue");
        assert!(backend.has_notification_queue());
        let dest = ObjectStoreDestination::new(backend);

        let start = dest
            .changes(ChangesRequest {
                cursor: None,
                limit: 0,
            })
            .await
            .expect("start");
        assert!(start.changes.is_empty());
        assert!(!start.complete);

        let first = dest
            .changes(ChangesRequest {
                cursor: Some(start.cursor),
                limit: 0,
            })
            .await
            .expect("first");
        let keys: Vec<_> = first.changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["a.m4b", "b.m4b"]);
        assert!(!first.complete, "an unreadable message is a missed change");
        {
            let state = queues.lock().expect("lock");
            assert!(
                state.acked.is_empty(),
                "nothing is acked before the host takes the page"
            );
            assert_eq!(state.leased.len(), 3, "the first page stays leased");
        }

        let second = dest
            .changes(ChangesRequest {
                cursor: Some(first.cursor),
                limit: 0,
            })
            .await
            .expect("second");
        assert!(second.changes.is_empty());
        let state = queues.lock().expect("lock");
        let mut acked = state.acked.clone();
        acked.sort_unstable();
        assert_eq!(
            acked,
            ["l1", "l2", "l3"],
            "every delivery the host took is acked"
        );
        assert!(
            state
                .paths
                .iter()
                .all(|path| path
                    .starts_with(&format!("/accounts/{ACCOUNT}/queues/{QUEUE}/messages/")))
        );
        assert!(state.tokens.iter().all(|token| token == "queue-token"));
    }

    #[tokio::test]
    async fn a_refused_pull_is_an_error_naming_the_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api = serve_queues(Arc::new(Mutex::new(FakeQueues::default()))).await;
        let url = serve(new_world(false)).await;
        // A queue id the fake does not route answers 404 with an error body.
        let backend = open_backend(&url, dir.path())
            .await
            .with_cloudflare_queue_at(
                &format!("{api}/elsewhere"),
                &crate::CloudflareAccountId::parse(ACCOUNT).expect("account"),
                &crate::CloudflareQueueId::parse(QUEUE).expect("queue"),
                crate::CloudflareApiToken::new("t".into()).expect("token"),
                &dir.path().join("queue-state"),
            )
            .expect("queue");
        let dest = ObjectStoreDestination::new(backend);
        let start = dest
            .changes(ChangesRequest {
                cursor: None,
                limit: 0,
            })
            .await
            .expect("start");
        let outcome = dest
            .changes(ChangesRequest {
                cursor: Some(start.cursor),
                limit: 0,
            })
            .await;
        let err = outcome.expect_err("a refused pull fails the call");
        let text = err.to_string();
        assert!(text.contains("notification queue pull"), "{text}");
        assert!(text.contains("404"), "{text}");
        assert!(
            text.contains("7003 no route"),
            "the API's reason is kept: {text}"
        );
    }
}
