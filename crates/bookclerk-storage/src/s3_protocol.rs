//! Deterministic S3 HTTP fixture for multipart upload, copy conditions, and
//! journal reclaim. This is protocol evidence, not live AWS conformance.

use std::collections::HashMap;
use std::sync::Arc;

use bookclerk_config::OutputS3Config;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

use super::*;

struct Obj {
    body: Vec<u8>,
    etag: String,
    meta: HashMap<String, String>,
}

struct Upload {
    key: String,
    parts: std::collections::BTreeMap<i32, Vec<u8>>,
}

struct World {
    objects: HashMap<String, Obj>,
    uploads: HashMap<String, Upload>,
    aborts: Vec<String>,
    completes: Vec<String>,
    copy_parts: Vec<(String, String)>,
    hold_parts: bool,
    part_gate: Arc<Notify>,
    first_part_started: Arc<Notify>,
}

fn etag_for(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    format!(
        "\"{:x}\"",
        u64::from_be_bytes(digest[..8].try_into().unwrap())
    )
}

async fn serve(world: Arc<Mutex<World>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let world = Arc::clone(&world);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                let mut collected = Vec::new();
                let header_end = loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    collected.extend_from_slice(&buf[..n]);
                    if let Some(pos) = collected.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&collected[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let request = lines.next().unwrap_or("");
                let mut headers = HashMap::new();
                for line in lines {
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap_or("");
                let target = parts.next().unwrap_or("/");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let content_len = headers
                    .get("content-length")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = collected[header_end..].to_vec();
                while body.len() < content_len {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&buf[..n]);
                }
                body.truncate(content_len);
                let key = path.trim_start_matches('/').to_string();
                let response = handle(method, &key, query, &headers, &body, &world).await;
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

fn query_map(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (k.to_string(), v.to_string())
        })
        .collect()
}

fn xml_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn handle(
    method: &str,
    key: &str,
    query: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
    world: &Arc<Mutex<World>>,
) -> String {
    let q = query_map(query);
    if method == "POST" && q.contains_key("uploads") {
        let id = format!("up-{}", world.lock().await.uploads.len() + 1);
        world.lock().await.uploads.insert(
            id.clone(),
            Upload {
                key: key.to_string(),
                parts: std::collections::BTreeMap::new(),
            },
        );
        let xml = format!(
            "<?xml version=\"1.0\"?><InitiateMultipartUploadResult><Bucket>library</Bucket><Key>{key}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
        );
        return xml_ok(&xml);
    }
    if method == "PUT" && q.contains_key("uploadId") && q.contains_key("partNumber") {
        let upload_id = q.get("uploadId").cloned().unwrap_or_default();
        let part: i32 = q
            .get("partNumber")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let hold = {
            let world = world.lock().await;
            world.hold_parts
        };
        if hold && part == 1 {
            let gate = {
                let world = world.lock().await;
                world.first_part_started.notify_one();
                Arc::clone(&world.part_gate)
            };
            gate.notified().await;
        }
        if let Some(source) = headers.get("x-amz-copy-source") {
            let source_key = source.trim_start_matches('/').to_string();
            let if_match = headers
                .get("x-amz-copy-source-if-match")
                .cloned()
                .unwrap_or_default();
            let mut world = world.lock().await;
            let Some(obj) = world.objects.get(&source_key) else {
                return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .into();
            };
            if obj.etag != if_match {
                return "HTTP/1.1 412 Precondition Failed\r\nContent-Type: application/xml\r\nContent-Length: 48\r\nConnection: close\r\n\r\n<Error><Code>PreconditionFailed</Code></Error>"
                    .into();
            }
            let range = headers
                .get("x-amz-copy-source-range")
                .cloned()
                .unwrap_or_default();
            let bytes = slice_range(&obj.body, &range);
            world.copy_parts.push((source_key, if_match));
            if let Some(upload) = world.uploads.get_mut(&upload_id) {
                upload.parts.insert(part, bytes);
            }
            let etag = format!("\"part-{part}\"");
            let xml = format!("<CopyPartResult><ETag>{etag}</ETag></CopyPartResult>");
            return xml_ok(&xml);
        }
        let etag = format!("\"part-{part}\"");
        world
            .lock()
            .await
            .uploads
            .get_mut(&upload_id)
            .unwrap()
            .parts
            .insert(part, body.to_vec());
        return format!(
            "HTTP/1.1 200 OK\r\nETag: {etag}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }
    if method == "POST" && q.contains_key("uploadId") {
        let upload_id = q.get("uploadId").cloned().unwrap_or_default();
        let mut world = world.lock().await;
        let Some(upload) = world.uploads.remove(&upload_id) else {
            return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        };
        let mut body = Vec::new();
        for part in upload.parts.values() {
            body.extend_from_slice(part);
        }
        let etag = format!("\"multipart-{}\"", upload.parts.len());
        world.objects.insert(
            upload.key,
            Obj {
                body,
                etag: etag.clone(),
                meta: HashMap::new(),
            },
        );
        world.completes.push(upload_id);
        let xml = format!(
            "<CompleteMultipartUploadResult><ETag>{etag}</ETag></CompleteMultipartUploadResult>"
        );
        return xml_ok(&xml);
    }
    if method == "DELETE" && q.contains_key("uploadId") {
        let upload_id = q.get("uploadId").cloned().unwrap_or_default();
        let mut world = world.lock().await;
        if world.uploads.remove(&upload_id).is_some() {
            world.aborts.push(upload_id);
            return "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        }
        return "HTTP/1.1 404 NoSuchUpload\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .into();
    }
    if method == "HEAD" {
        let world = world.lock().await;
        let Some(obj) = world.objects.get(key) else {
            return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        };
        let mut extra = String::new();
        for (name, value) in &obj.meta {
            extra.push_str(&format!("x-amz-meta-{name}: {value}\r\n"));
        }
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {}\r\n{extra}Connection: close\r\n\r\n",
            obj.body.len(),
            obj.etag
        );
    }
    if method == "GET" {
        let world = world.lock().await;
        let Some(obj) = world.objects.get(key) else {
            return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        };
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {}\r\nConnection: close\r\n\r\n{}",
            obj.body.len(),
            obj.etag,
            String::from_utf8_lossy(&obj.body)
        );
    }
    if method == "PUT" && headers.contains_key("x-amz-copy-source") {
        let source = headers
            .get("x-amz-copy-source")
            .cloned()
            .unwrap_or_default();
        let source_key = source.trim_start_matches('/').to_string();
        let if_match = headers
            .get("x-amz-copy-source-if-match")
            .cloned()
            .unwrap_or_default();
        let mut world = world.lock().await;
        let Some(obj) = world.objects.get(&source_key).cloned_obj() else {
            return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        };
        if obj.etag != if_match {
            return "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 48\r\nConnection: close\r\n\r\n<Error><Code>PreconditionFailed</Code></Error>"
                .into();
        }
        let etag = obj.etag.clone();
        world.objects.insert(key.to_string(), obj);
        let xml = format!("<CopyObjectResult><ETag>{etag}</ETag></CopyObjectResult>");
        return xml_ok(&xml);
    }
    if method == "PUT" {
        let mut meta = HashMap::new();
        for (name, value) in headers {
            if let Some(rest) = name.strip_prefix("x-amz-meta-") {
                meta.insert(rest.to_string(), value.clone());
            }
        }
        let etag = etag_for(body);
        world.lock().await.objects.insert(
            key.to_string(),
            Obj {
                body: body.to_vec(),
                etag,
                meta,
            },
        );
        return "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into();
    }
    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
}

trait ClonedObj {
    fn cloned_obj(&self) -> Option<Obj>;
}

impl ClonedObj for Option<&Obj> {
    fn cloned_obj(&self) -> Option<Obj> {
        self.map(|obj| Obj {
            body: obj.body.clone(),
            etag: obj.etag.clone(),
            meta: obj.meta.clone(),
        })
    }
}

fn slice_range(body: &[u8], header: &str) -> Vec<u8> {
    let Some(spec) = header.strip_prefix("bytes=") else {
        return body.to_vec();
    };
    let (start, end) = spec.split_once('-').unwrap_or(("0", ""));
    let start: usize = start.parse().unwrap_or(0);
    let end: usize = end.parse().unwrap_or(body.len().saturating_sub(1));
    body.get(start..=end).unwrap_or(&[]).to_vec()
}

async fn open_backend(url: &str, journal: &std::path::Path) -> S3Backend {
    let cfg = OutputS3Config {
        enabled: true,
        bucket: "library".into(),
        prefix: String::new(),
        region: "us-east-1".into(),
        endpoint: Some(url.into()),
        force_path_style: true,
        naming: Default::default(),
        plugin: String::new(),
    };
    let creds = crate::s3_credentials::S3Credentials {
        access_key_id: "test-key".into(),
        secret_access_key: "test-secret".into(),
        session_token: None,
        label: None,
    };
    S3Backend::from_parts_with_journal(&cfg, "", Some(&creds), journal)
        .await
        .unwrap()
        .with_part_size(4)
}

fn new_world(hold_parts: bool) -> Arc<Mutex<World>> {
    Arc::new(Mutex::new(World {
        objects: HashMap::new(),
        uploads: HashMap::new(),
        aborts: Vec::new(),
        completes: Vec::new(),
        copy_parts: Vec::new(),
        hold_parts,
        part_gate: Arc::new(Notify::new()),
        first_part_started: Arc::new(Notify::new()),
    }))
}

#[tokio::test]
async fn multipart_digest_is_visible_to_a_reconstructed_backend() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&url, dir.path()).await;
    let body = b"abcdefghij";
    let written = backend
        .put_stream(
            "book.m4b",
            Box::pin(std::io::Cursor::new(body.to_vec())),
            ObjectMeta {
                content_length: Some(body.len() as u64),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap();
    let expect = hex::encode(Sha256::digest(body));
    assert_eq!(written.sha256_hex.as_deref(), Some(expect.as_str()));
    drop(backend);
    let again = open_backend(&url, dir.path()).await;
    let probe = again.probe("book.m4b").await.unwrap();
    assert_eq!(probe.meta.sha256_hex.as_deref(), Some(expect.as_str()));
    assert_ne!(probe.etag.as_deref(), Some(expect.as_str()));
    assert!(probe.etag.as_deref().unwrap_or("").contains("multipart"));
}

#[tokio::test]
async fn live_upload_survives_a_second_backend_and_abandoned_one_is_aborted() {
    let world = new_world(true);
    let gate = Arc::clone(&world.lock().await.part_gate);
    let started = Arc::clone(&world.lock().await.first_part_started);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let first = open_backend(&url, dir.path()).await;
    let upload = tokio::spawn({
        let first = first.clone();
        async move {
            first
                .put_stream(
                    "live.m4b",
                    Box::pin(std::io::Cursor::new(b"abcdefghij".to_vec())),
                    ObjectMeta {
                        content_length: Some(10),
                        ..ObjectMeta::default()
                    },
                )
                .await
        }
    });
    started.notified().await;
    let _second = open_backend(&url, dir.path()).await;
    assert!(
        world.lock().await.aborts.is_empty(),
        "a second backend must not abort a live upload"
    );
    gate.notify_one();
    upload.await.unwrap().unwrap();
    assert!(world.lock().await.aborts.is_empty());
    assert_eq!(world.lock().await.completes.len(), 1);

    let abandoned = new_world(true);
    let gate = Arc::clone(&abandoned.lock().await.part_gate);
    let started = Arc::clone(&abandoned.lock().await.first_part_started);
    let url = serve(Arc::clone(&abandoned)).await;
    let dir = tempfile::tempdir().unwrap();
    SKIP_DROP_ABORT.store(true, std::sync::atomic::Ordering::SeqCst);
    let owner = open_backend(&url, dir.path()).await;
    let task = tokio::spawn({
        let owner = owner.clone();
        async move {
            owner
                .put_stream(
                    "dead.m4b",
                    Box::pin(std::io::Cursor::new(b"abcdefghij".to_vec())),
                    ObjectMeta {
                        content_length: Some(10),
                        ..ObjectMeta::default()
                    },
                )
                .await
        }
    });
    started.notified().await;
    task.abort();
    let _ = task.await;
    drop(owner);
    SKIP_DROP_ABORT.store(false, std::sync::atomic::Ordering::SeqCst);
    let _reclaimer = open_backend(&url, dir.path()).await;
    let _ = gate;
    assert_eq!(
        abandoned.lock().await.aborts.len(),
        1,
        "an upload whose owner lock is gone is aborted"
    );
}

#[tokio::test]
async fn multipart_copy_rejects_a_source_replaced_between_parts() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&url, dir.path()).await;
    let original = Bytes::from_static(b"aaaaaaaabbbb");
    backend
        .put("src.bin", original.clone(), ObjectMeta::default())
        .await
        .unwrap();
    let replacing = ReplacingCopy {
        inner: backend.clone(),
        world: Arc::clone(&world),
        flipped: std::sync::atomic::AtomicBool::new(false),
    };
    let err = replacing.copy("src.bin", "dst.bin").await.unwrap_err();
    assert!(matches!(err, StorageError::Integrity(_)), "{err}");
    assert!(world.lock().await.completes.is_empty());
    assert!(!world.lock().await.objects.contains_key("dst.bin"));
    assert!(world.lock().await.copy_parts.len() <= 1 || world.lock().await.aborts.len() == 1);
}

struct ReplacingCopy {
    inner: S3Backend,
    world: Arc<Mutex<World>>,
    flipped: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl StorageBackend for ReplacingCopy {
    fn name(&self) -> &'static str {
        "replacing"
    }
    fn instance_id(&self) -> String {
        self.inner.instance_id()
    }
    fn supports_server_copy(&self) -> bool {
        true
    }
    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(Self {
            inner: self.inner.clone(),
            world: Arc::clone(&self.world),
            flipped: std::sync::atomic::AtomicBool::new(
                self.flipped.load(std::sync::atomic::Ordering::SeqCst),
            ),
        })
    }
    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
        self.inner.put(key, data, meta).await
    }
    async fn get(&self, key: &str) -> Result<Bytes> {
        self.inner.get(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        self.inner.exists(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<crate::ObjectInfo>> {
        self.inner.list(prefix).await
    }
    async fn probe(&self, key: &str) -> Result<ObjectProbe> {
        self.inner.probe(key).await
    }
    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        let world = Arc::clone(&self.world);
        let flip = tokio::spawn(async move {
            loop {
                if !world.lock().await.copy_parts.is_empty() {
                    let mut world = world.lock().await;
                    if let Some(obj) = world.objects.get_mut("library/src.bin") {
                        obj.body = b"ZZZZZZZZbbbb".to_vec();
                        obj.etag = "\"changed\"".into();
                    }
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
        let result = self.inner.copy(from, to).await;
        flip.abort();
        drop(flip);
        result
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
        ObjectProbe,
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
