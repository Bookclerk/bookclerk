//! Deterministic S3 HTTP fixture for multipart upload, copy conditions, and
//! journal reclaim. This is protocol evidence, not live AWS conformance.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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

#[derive(Clone)]
struct StreamSpec {
    /// `Content-Length` to advertise. `None` omits the header.
    claim: Option<u64>,
    /// Bytes to try to write. May exceed `claim`.
    total: u64,
}

pub(crate) struct World {
    objects: HashMap<String, Obj>,
    uploads: HashMap<String, Upload>,
    aborts: Vec<String>,
    completes: Vec<String>,
    copy_parts: Vec<(String, String)>,
    hold_parts: bool,
    fail_complete: bool,
    /// Integrity PUTs wait until the test releases them, one permit at a time.
    hold_integrity: bool,
    /// Integrity PUTs fail after the body is already complete.
    fail_integrity: bool,
    held_integrity: Vec<Arc<Notify>>,
    integrity_arrived: Arc<Notify>,
    /// After CopyObject commits, wait so a test can replace the destination.
    hold_copy: bool,
    copies_held: usize,
    release_copy: bool,
    /// When set, GET of an integrity key streams this body instead of the store.
    stream_integrity: Option<StreamSpec>,
    stream_bytes: Arc<AtomicUsize>,
    part_gate: Arc<Notify>,
    first_part_started: Arc<Notify>,
    /// When false the store accepts `If-None-Match` / `If-Match` on writes
    /// and deletes but ignores them, as some S3-compatible stores do.
    enforce_conditions: bool,
    /// Requests that ignore their conditions even while
    /// `enforce_conditions` is set, for stores that enforce some kinds only.
    ignore_conditions_on: Vec<CondOp>,
}

/// A request kind that can carry `If-None-Match` / `If-Match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CondOp {
    /// `PutObject`.
    Put,
    /// `CompleteMultipartUpload`.
    Complete,
    /// `CopyObject`.
    Copy,
    /// `DeleteObject`.
    Delete,
    /// `x-amz-copy-source-if-match` on `CopyObject`.
    CopySource,
    /// `x-amz-copy-source-if-match` on `UploadPartCopy`.
    PartCopySource,
}

/// True when a store that honours `op` refuses a copy whose source has
/// `etag` under the request's `x-amz-copy-source-if-match`.
fn refused_source(
    world: &World,
    op: CondOp,
    etag: &str,
    headers: &HashMap<String, String>,
) -> bool {
    if world.ignore_conditions_on.contains(&op) {
        return false;
    }
    headers
        .get("x-amz-copy-source-if-match")
        .is_some_and(|expected| expected != etag)
}

const PRECONDITION_FAILED: &str = "HTTP/1.1 412 Precondition Failed\r\nContent-Type: application/xml\r\nContent-Length: 48\r\nConnection: close\r\n\r\n<Error><Code>PreconditionFailed</Code></Error>";

/// The 412 a store that enforces conditions answers for a write or delete of
/// `key` under the request's `If-None-Match` / `If-Match`, or `None` when the
/// write may go ahead.
fn refused_condition(
    world: &World,
    op: CondOp,
    key: &str,
    headers: &HashMap<String, String>,
) -> Option<String> {
    if !world.enforce_conditions || world.ignore_conditions_on.contains(&op) {
        return None;
    }
    let current = world.objects.get(key);
    if headers.get("if-none-match").map(String::as_str) == Some("*") && current.is_some() {
        return Some(PRECONDITION_FAILED.into());
    }
    if let Some(expected) = headers.get("if-match") {
        if current.map(|obj| obj.etag.as_str()) != Some(expected.as_str()) {
            return Some(PRECONDITION_FAILED.into());
        }
    }
    None
}

fn etag_for(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    format!(
        "\"{:x}\"",
        u64::from_be_bytes(digest[..8].try_into().unwrap())
    )
}

pub(crate) async fn serve(world: Arc<Mutex<World>>) -> String {
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
                let key = percent_decode(path.trim_start_matches('/'));
                let stream = {
                    let world = world.lock().await;
                    if method == "GET" && key.contains(".bookclerk-integrity/") {
                        world.stream_integrity.clone()
                    } else {
                        None
                    }
                };
                if let Some(spec) = stream {
                    stream_integrity(&mut socket, &spec, &world).await;
                    return;
                }
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
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

async fn stream_integrity(
    socket: &mut tokio::net::TcpStream,
    spec: &StreamSpec,
    world: &Arc<Mutex<World>>,
) {
    use tokio::io::AsyncWriteExt;
    let header = match spec.claim {
        Some(len) => {
            format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")
        }
        None => "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".into(),
    };
    if socket.write_all(header.as_bytes()).await.is_err() {
        return;
    }
    let chunk = vec![b'x'; 4096];
    let mut sent = 0u64;
    while sent < spec.total {
        let n = std::cmp::min(chunk.len() as u64, spec.total - sent) as usize;
        if socket.write_all(&chunk[..n]).await.is_err() {
            break;
        }
        sent += n as u64;
        world
            .lock()
            .await
            .stream_bytes
            .fetch_add(n, Ordering::SeqCst);
    }
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
            let source_key = percent_decode(source.trim_start_matches('/'));
            let if_match = headers
                .get("x-amz-copy-source-if-match")
                .cloned()
                .unwrap_or_default();
            let mut world = world.lock().await;
            let Some(obj) = world.objects.get(&source_key) else {
                return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .into();
            };
            if refused_source(&world, CondOp::PartCopySource, &obj.etag, headers) {
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
        if world.lock().await.fail_complete {
            return "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into();
        }
        let upload_id = q.get("uploadId").cloned().unwrap_or_default();
        let mut world = world.lock().await;
        let Some(upload_key) = world.uploads.get(&upload_id).map(|u| u.key.clone()) else {
            return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        };
        if let Some(refused) = refused_condition(&world, CondOp::Complete, &upload_key, headers) {
            return refused;
        }
        let Some(upload) = world.uploads.remove(&upload_id) else {
            return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        };
        let mut body = Vec::new();
        for part in upload.parts.values() {
            body.extend_from_slice(part);
        }
        // Content-derived, like a real multipart ETag, and not the SHA-256.
        let etag = etag_for(&body);
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
    if method == "DELETE" && !q.contains_key("uploadId") {
        let mut world = world.lock().await;
        // Deleting a missing key succeeds, conditions or not.
        if world.objects.contains_key(key) {
            if let Some(refused) = refused_condition(&world, CondOp::Delete, key, headers) {
                return refused;
            }
        }
        world.objects.remove(key);
        return "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into();
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
    if method == "GET" && q.contains_key("list-type") {
        let prefix = q.get("prefix").cloned().unwrap_or_default();
        let world = world.lock().await;
        let mut xml = String::from(
            "<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>false</IsTruncated>",
        );
        for key in world.objects.keys() {
            if prefix.is_empty() || key.contains(&prefix) {
                let name = key.strip_prefix("library/").unwrap_or(key);
                let obj = &world.objects[key];
                xml.push_str(&format!(
                    "<Contents><Key>{name}</Key><Size>{}</Size><ETag>{}</ETag><LastModified>2026-11-17T12:00:00.000Z</LastModified></Contents>",
                    obj.body.len(),
                    obj.etag.replace('"', "&quot;")
                ));
            }
        }
        xml.push_str("</ListBucketResult>");
        return xml_ok(&xml);
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
        if let Some(range) = headers.get("range") {
            // A ranged read answers 206 with only the bytes asked for.
            let bytes = slice_range(&obj.body, range);
            let start = range
                .strip_prefix("bytes=")
                .and_then(|spec| spec.split_once('-'))
                .and_then(|(from, _)| from.parse::<usize>().ok())
                .unwrap_or(0);
            return format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: {}\r\nConnection: close\r\n\r\n{}",
                bytes.len(),
                start,
                (start + bytes.len()).saturating_sub(1),
                obj.body.len(),
                obj.etag,
                String::from_utf8_lossy(&bytes)
            );
        }
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
        let source_key = percent_decode(source.trim_start_matches('/'));
        let mut guard = world.lock().await;
        let Some(obj) = guard.objects.get(&source_key).cloned_obj() else {
            return "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        };
        if refused_source(&guard, CondOp::CopySource, &obj.etag, headers) {
            return "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 48\r\nConnection: close\r\n\r\n<Error><Code>PreconditionFailed</Code></Error>"
                .into();
        }
        if let Some(refused) = refused_condition(&guard, CondOp::Copy, key, headers) {
            return refused;
        }
        let etag = obj.etag.clone();
        guard.objects.insert(key.to_string(), obj);
        let hold = guard.hold_copy;
        if hold {
            guard.copies_held += 1;
        }
        drop(guard);
        if hold {
            loop {
                if world.lock().await.release_copy {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        let xml = format!("<CopyObjectResult><ETag>{etag}</ETag></CopyObjectResult>");
        return xml_ok(&xml);
    }
    if method == "PUT" && key.contains(".bookclerk-integrity/") {
        let (fail, hold, release) = {
            let mut world = world.lock().await;
            if world.fail_integrity {
                (true, false, None)
            } else if world.hold_integrity {
                let release = Arc::new(Notify::new());
                world.held_integrity.push(Arc::clone(&release));
                world.integrity_arrived.notify_one();
                (false, true, Some(release))
            } else {
                (false, false, None)
            }
        };
        if fail {
            return "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
        }
        if hold {
            if let Some(release) = release {
                release.notified().await;
            }
        }
    }
    if method == "PUT" {
        let mut meta = HashMap::new();
        for (name, value) in headers {
            if let Some(rest) = name.strip_prefix("x-amz-meta-") {
                meta.insert(rest.to_string(), value.clone());
            }
        }
        let etag = etag_for(body);
        let mut world = world.lock().await;
        if let Some(refused) = refused_condition(&world, CondOp::Put, key, headers) {
            return refused;
        }
        world.objects.insert(
            key.to_string(),
            Obj {
                body: body.to_vec(),
                etag: etag.clone(),
                meta,
            },
        );
        return format!(
            "HTTP/1.1 200 OK\r\nETag: {etag}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
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

pub(crate) async fn open_backend(url: &str, journal: &std::path::Path) -> S3Backend {
    open_sized(url, journal, Some(4)).await
}

async fn open_sized(url: &str, journal: &std::path::Path, part_size: Option<usize>) -> S3Backend {
    let cfg = S3Settings {
        bucket: "library".into(),
        region: "us-east-1".into(),
        endpoint: Some(url.into()),
        force_path_style: true,
    };
    let creds = S3Credentials {
        access_key_id: "test-key".into(),
        secret_access_key: "test-secret".into(),
        session_token: None,
    };
    let backend = S3Backend::from_parts_with_journal(&cfg, "", Some(&creds), journal)
        .await
        .unwrap();
    match part_size {
        Some(part_size) => backend.with_part_size(part_size),
        None => backend,
    }
}

pub(crate) fn new_world(hold_parts: bool) -> Arc<Mutex<World>> {
    Arc::new(Mutex::new(World {
        objects: HashMap::new(),
        uploads: HashMap::new(),
        aborts: Vec::new(),
        completes: Vec::new(),
        copy_parts: Vec::new(),
        hold_parts,
        fail_complete: false,
        hold_integrity: false,
        fail_integrity: false,
        held_integrity: Vec::new(),
        integrity_arrived: Arc::new(Notify::new()),
        hold_copy: false,
        copies_held: 0,
        release_copy: false,
        stream_integrity: None,
        stream_bytes: Arc::new(AtomicUsize::new(0)),
        part_gate: Arc::new(Notify::new()),
        first_part_started: Arc::new(Notify::new()),
        enforce_conditions: true,
        ignore_conditions_on: Vec::new(),
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
    assert!(probe.etag.as_deref().is_some_and(|etag| !etag.is_empty()));
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

fn sha(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

#[tokio::test]
async fn failed_completion_does_not_attach_the_new_digest_to_the_old_object() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&url, dir.path()).await;
    let body_a = b"aaaaaaaaaa";
    backend
        .put_stream(
            "book.m4b",
            Box::pin(std::io::Cursor::new(body_a.to_vec())),
            ObjectMeta {
                content_length: Some(body_a.len() as u64),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap();
    world.lock().await.fail_complete = true;
    let body_b = b"bbbbbbbbbb";
    let err = backend
        .put_stream(
            "book.m4b",
            Box::pin(std::io::Cursor::new(body_b.to_vec())),
            ObjectMeta {
                content_length: Some(body_b.len() as u64),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("500") || matches!(err, StorageError::S3(_)),
        "{err}"
    );
    let probe = backend.probe("book.m4b").await.unwrap();
    let body = backend.get("book.m4b").await.unwrap();
    assert_eq!(body.as_ref(), body_a);
    assert_eq!(probe.meta.sha256_hex.as_deref(), Some(sha(body_a).as_str()));
    assert_eq!(sha(body.as_ref()), sha(body_a));
    assert_ne!(probe.meta.sha256_hex.as_deref(), Some(sha(body_b).as_str()));
}

#[tokio::test]
async fn small_copy_and_delete_keep_integrity_bound_to_the_body() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_sized(&url, dir.path(), None).await;
    let body = b"audiobook";
    backend
        .put_stream(
            "stage.m4b",
            Box::pin(std::io::Cursor::new(body.to_vec())),
            ObjectMeta {
                content_length: Some(body.len() as u64),
                commit_token: Some("tok-1".into()),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap();
    backend.copy("stage.m4b", "final.m4b").await.unwrap();
    drop(backend);
    let again = open_sized(&url, dir.path(), None).await;
    let probe = again.probe("final.m4b").await.unwrap();
    assert_eq!(probe.meta.sha256_hex.as_deref(), Some(sha(body).as_str()));
    assert_eq!(probe.meta.commit_token.as_deref(), Some("tok-1"));
    again.delete("final.m4b").await.unwrap();
    again
        .put(
            "final.m4b",
            Bytes::from_static(b"audiobook"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
    let replaced = again.probe("final.m4b").await.unwrap();
    assert_eq!(again.get("final.m4b").await.unwrap().as_ref(), body);
    assert!(replaced.meta.sha256_hex.is_none());
    assert!(replaced.meta.commit_token.is_none());
}

#[tokio::test]
async fn opposite_completion_order_keeps_the_visible_bodys_digest() {
    let world = new_world(false);
    world.lock().await.hold_integrity = true;
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&url, dir.path()).await;
    let body_a = b"aaaaaaaaaa".to_vec();
    let body_b = b"bbbbbbbbbb".to_vec();
    let first = tokio::spawn({
        let backend = backend.clone();
        let body_a = body_a.clone();
        async move {
            backend
                .put_stream(
                    "book.m4b",
                    Box::pin(std::io::Cursor::new(body_a)),
                    ObjectMeta {
                        content_length: Some(10),
                        commit_token: Some("tok-a".into()),
                        ..ObjectMeta::default()
                    },
                )
                .await
        }
    });
    let second = tokio::spawn({
        let backend = backend.clone();
        let body_b = body_b.clone();
        async move {
            backend
                .put_stream(
                    "book.m4b",
                    Box::pin(std::io::Cursor::new(body_b)),
                    ObjectMeta {
                        content_length: Some(10),
                        commit_token: Some("tok-b".into()),
                        ..ObjectMeta::default()
                    },
                )
                .await
        }
    });
    loop {
        let ready = {
            let world = world.lock().await;
            let _arrived = &world.integrity_arrived;
            world.completes.len() >= 2 && world.held_integrity.len() >= 2
        };
        if ready {
            break;
        }
        if first.is_finished() && second.is_finished() {
            panic!(
                "uploads finished before both integrity records were held: {:?}",
                world.lock().await.completes
            );
        }
        tokio::task::yield_now().await;
    }
    let visible = {
        let world = world.lock().await;
        world
            .objects
            .get("library/book.m4b")
            .expect("completed object")
            .body
            .clone()
    };
    let releases = {
        let mut world = world.lock().await;
        std::mem::take(&mut world.held_integrity)
    };
    for release in releases.into_iter().rev() {
        release.notify_one();
    }
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let probe = backend.probe("book.m4b").await.unwrap();
    let got = backend.get("book.m4b").await.unwrap();
    assert_eq!(got.as_ref(), visible.as_slice());
    assert_eq!(
        probe.meta.sha256_hex.as_deref(),
        Some(sha(&visible).as_str())
    );
    assert_eq!(sha(got.as_ref()), sha(&visible));
    if visible.as_slice() == body_a.as_slice() {
        assert_eq!(probe.meta.commit_token.as_deref(), Some("tok-a"));
        assert_ne!(
            probe.meta.sha256_hex.as_deref(),
            Some(sha(&body_b).as_str())
        );
    } else {
        assert_eq!(probe.meta.commit_token.as_deref(), Some("tok-b"));
        assert_ne!(
            probe.meta.sha256_hex.as_deref(),
            Some(sha(&body_a).as_str())
        );
    }
}

#[tokio::test]
async fn integrity_write_failure_does_not_keep_the_previous_digest() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&url, dir.path()).await;
    let body_a = b"aaaaaaaaaa";
    backend
        .put_stream(
            "book.m4b",
            Box::pin(std::io::Cursor::new(body_a.to_vec())),
            ObjectMeta {
                content_length: Some(body_a.len() as u64),
                commit_token: Some("tok-a".into()),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap();
    world.lock().await.fail_integrity = true;
    let body_b = b"bbbbbbbbbb";
    let err = backend
        .put_stream(
            "book.m4b",
            Box::pin(std::io::Cursor::new(body_b.to_vec())),
            ObjectMeta {
                content_length: Some(body_b.len() as u64),
                commit_token: Some("tok-b".into()),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::S3(_)), "{err}");
    let got = backend.get("book.m4b").await.unwrap();
    assert_eq!(got.as_ref(), body_b);
    let probe = backend.probe("book.m4b").await.unwrap();
    assert!(probe.meta.sha256_hex.is_none(), "{probe:?}");
    assert!(probe.meta.commit_token.is_none(), "{probe:?}");
    assert_ne!(probe.meta.sha256_hex.as_deref(), Some(sha(body_a).as_str()));
}

#[tokio::test]
async fn unmatched_integrity_record_is_not_trusted() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&url, dir.path()).await;
    let body = b"aaaaaaaaaa";
    backend
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
    let etag = backend.probe("book.m4b").await.unwrap().etag.unwrap();
    let key = bound_integrity_key("book.m4b", &etag);
    backend
        .put(
            &key,
            Bytes::from_static(br#"{"etag":"other-version","sha256_hex":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","commit_token":"stale"}"#),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
    let probe = backend.probe("book.m4b").await.unwrap();
    assert_eq!(backend.get("book.m4b").await.unwrap().as_ref(), body);
    assert!(probe.meta.sha256_hex.is_none(), "{probe:?}");
    assert!(probe.meta.commit_token.is_none(), "{probe:?}");
}

#[tokio::test]
async fn deleting_one_key_leaves_another_keys_integrity() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_sized(&url, dir.path(), None).await;
    for (key, token) in [("a.m4b", "tok-a"), ("b.m4b", "tok-b")] {
        let body = key.as_bytes().to_vec();
        backend
            .put_stream(
                key,
                Box::pin(std::io::Cursor::new(body)),
                ObjectMeta {
                    content_length: Some(key.len() as u64),
                    commit_token: Some(token.into()),
                    ..ObjectMeta::default()
                },
            )
            .await
            .unwrap();
    }
    backend.delete("a.m4b").await.unwrap();
    let kept = backend.probe("b.m4b").await.unwrap();
    assert_eq!(
        kept.meta.sha256_hex.as_deref(),
        Some(sha(b"b.m4b").as_str())
    );
    assert_eq!(kept.meta.commit_token.as_deref(), Some("tok-b"));
}

#[tokio::test]
async fn copy_binds_integrity_to_the_copy_response_not_a_later_head() {
    let world = new_world(false);
    world.lock().await.hold_copy = true;
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_sized(&url, dir.path(), None).await;
    let source = b"audiobook";
    backend
        .put_stream(
            "stage.m4b",
            Box::pin(std::io::Cursor::new(source.to_vec())),
            ObjectMeta {
                content_length: Some(source.len() as u64),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap();
    let source_sha = sha(source);
    let copy = tokio::spawn({
        let backend = backend.clone();
        async move { backend.copy("stage.m4b", "final.m4b").await }
    });
    loop {
        if world.lock().await.copies_held >= 1 {
            break;
        }
        if copy.is_finished() {
            panic!(
                "copy finished before the committed barrier: {:?}",
                copy.await
            );
        }
        tokio::task::yield_now().await;
    }
    let replacement = b"REPLACED!";
    assert_eq!(replacement.len(), source.len());
    {
        let mut world = world.lock().await;
        let obj = world
            .objects
            .get_mut("library/final.m4b")
            .expect("copy committed");
        obj.body = replacement.to_vec();
        obj.etag = "\"replaced\"".into();
        obj.meta.clear();
        world.release_copy = true;
    }
    copy.await.unwrap().unwrap();
    let probe = backend.probe("final.m4b").await.unwrap();
    let got = backend.get("final.m4b").await.unwrap();
    assert_eq!(got.as_ref(), replacement);
    assert_ne!(sha(got.as_ref()), source_sha);
    assert!(
        probe.meta.sha256_hex.is_none(),
        "replacement HEAD reported the copied source digest: {probe:?}"
    );
    assert_ne!(probe.meta.sha256_hex.as_deref(), Some(source_sha.as_str()));
}

struct CountingReader {
    left: usize,
    pulled: Arc<AtomicUsize>,
    chunk: u8,
}

impl tokio::io::AsyncRead for CountingReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.left == 0 || buf.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        let n = std::cmp::min(self.left, buf.remaining());
        let byte = self.chunk;
        buf.put_slice(&vec![byte; n]);
        self.left -= n;
        self.pulled.fetch_add(n, Ordering::SeqCst);
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn integrity_reads_stop_at_the_metadata_cap() {
    let cap = INTEGRITY_RECORD_MAX_BYTES;
    let record = br#"{"etag":"\"abc\"","sha256_hex":"abcd","commit_token":"t"}"#;
    let parsed = read_capped_record(
        Some(record.len() as u64),
        Box::pin(std::io::Cursor::new(record.to_vec())),
    )
    .await
    .unwrap();
    assert_eq!(parsed.len(), record.len());

    let pulled = Arc::new(AtomicUsize::new(0));
    let err = read_capped_record(
        Some(cap + 1),
        Box::pin(CountingReader {
            left: (cap as usize) * 8,
            pulled: Arc::clone(&pulled),
            chunk: b'A',
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StorageError::PayloadTooLarge(_)), "{err}");
    assert_eq!(
        pulled.load(Ordering::SeqCst),
        0,
        "oversize hint must not be read"
    );

    let pulled = Arc::new(AtomicUsize::new(0));
    let err = read_capped_record(
        None,
        Box::pin(CountingReader {
            left: (cap as usize) * 8,
            pulled: Arc::clone(&pulled),
            chunk: b'B',
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StorageError::PayloadTooLarge(_)), "{err}");
    let absent = pulled.load(Ordering::SeqCst) as u64;
    assert!(
        absent > cap && absent <= cap + 8192,
        "absent length pulled {absent}, cap {cap}"
    );

    let pulled = Arc::new(AtomicUsize::new(0));
    let err = read_capped_record(
        Some(16),
        Box::pin(CountingReader {
            left: (cap as usize) * 8,
            pulled: Arc::clone(&pulled),
            chunk: b'C',
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StorageError::PayloadTooLarge(_)), "{err}");
    let understated = pulled.load(Ordering::SeqCst) as u64;
    assert!(
        understated > cap && understated <= cap + 8192,
        "understated length pulled {understated}, cap {cap}"
    );
}

async fn probe_streamed_integrity(spec: StreamSpec) -> (StorageError, usize) {
    let world = new_world(false);
    {
        let mut world = world.lock().await;
        world.stream_integrity = Some(spec);
        world.stream_bytes.store(0, Ordering::SeqCst);
    }
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_sized(&url, dir.path(), None).await;
    backend
        .put_stream(
            "book.m4b",
            Box::pin(std::io::Cursor::new(b"audiobook".to_vec())),
            ObjectMeta {
                content_length: Some(9),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap();
    world.lock().await.stream_bytes.store(0, Ordering::SeqCst);
    let err = backend.probe("book.m4b").await.unwrap_err();
    let sent = world.lock().await.stream_bytes.load(Ordering::SeqCst);
    (err, sent)
}

#[tokio::test]
async fn streamed_integrity_records_are_not_fully_buffered() {
    let cap = INTEGRITY_RECORD_MAX_BYTES;
    let total = 8 * 1024 * 1024;
    let (err, sent) = probe_streamed_integrity(StreamSpec {
        claim: Some(cap + 1),
        total,
    })
    .await;
    assert!(matches!(err, StorageError::PayloadTooLarge(_)), "{err}");
    assert!(
        (sent as u64) < 1024 * 1024,
        "limit+1 response sent {sent} of {total}"
    );

    let (err, sent) = probe_streamed_integrity(StreamSpec { claim: None, total }).await;
    assert!(matches!(err, StorageError::PayloadTooLarge(_)), "{err}");
    assert!(
        (sent as u64) * 2 < total,
        "absent Content-Length sent {sent} of {total}"
    );

    let (err, sent) = probe_streamed_integrity(StreamSpec {
        claim: Some(16),
        total,
    })
    .await;
    assert!(
        matches!(
            err,
            StorageError::PayloadTooLarge(_) | StorageError::Integrity(_)
        ),
        "{err}"
    );
    assert!(
        (sent as u64) < 1024 * 1024,
        "understated Content-Length sent {sent} of {total}"
    );
}

fn put_meta(body: &[u8]) -> ObjectMeta {
    ObjectMeta {
        content_length: Some(body.len() as u64),
        ..ObjectMeta::default()
    }
}

async fn put_bytes(backend: &S3Backend, key: &str, body: &[u8]) {
    backend
        .put_stream(
            key,
            Box::pin(std::io::Cursor::new(body.to_vec())),
            put_meta(body),
        )
        .await
        .unwrap();
}

async fn put_if(
    backend: &S3Backend,
    key: &str,
    body: &[u8],
    condition: &WriteCondition,
) -> Result<ConditionalPutResult> {
    backend
        .put_stream_conditional(
            key,
            Box::pin(std::io::Cursor::new(body.to_vec())),
            put_meta(body),
            condition,
        )
        .await
}

async fn no_probe_objects_left(world: &Arc<Mutex<World>>) {
    let world = world.lock().await;
    assert!(
        world
            .objects
            .keys()
            .all(|key| !key.contains(".bookclerk-probe/")),
        "probe object left behind: {:?}",
        world.objects.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn the_probe_trusts_only_a_store_that_refuses_the_second_write() {
    for enforce in [true, false] {
        let world = new_world(false);
        world.lock().await.enforce_conditions = enforce;
        let url = serve(Arc::clone(&world)).await;
        let dir = tempfile::tempdir().unwrap();
        let backend = open_sized(&url, dir.path(), None).await;
        let support = backend.conditional_support().await;
        assert_eq!(
            support,
            ConditionalSupport {
                writes: enforce,
                completes: enforce,
                copies: enforce,
                copy_pins: true,
                part_copy_pins: true,
                deletes: enforce,
            }
        );
        assert!(
            world.lock().await.uploads.is_empty(),
            "probe upload left open"
        );
        no_probe_objects_left(&world).await;
        // Remembered: a second call does not probe again.
        world.lock().await.enforce_conditions = !enforce;
        assert_eq!(backend.clone().conditional_support().await, support);
    }
}

#[tokio::test]
async fn each_request_kind_is_trusted_only_on_its_own_evidence() {
    for ignored in [
        CondOp::Put,
        CondOp::Complete,
        CondOp::Copy,
        CondOp::Delete,
        CondOp::CopySource,
        CondOp::PartCopySource,
    ] {
        let world = new_world(false);
        world.lock().await.ignore_conditions_on = vec![ignored];
        let url = serve(Arc::clone(&world)).await;
        let dir = tempfile::tempdir().unwrap();
        let backend = open_sized(&url, dir.path(), None).await;
        let support = backend.conditional_support().await;
        assert_eq!(
            support,
            ConditionalSupport {
                writes: ignored != CondOp::Put,
                completes: ignored != CondOp::Complete,
                copies: ignored != CondOp::Copy,
                copy_pins: ignored != CondOp::CopySource,
                part_copy_pins: ignored != CondOp::PartCopySource,
                deletes: ignored != CondOp::Delete,
            },
            "ignoring {ignored:?}"
        );
        no_probe_objects_left(&world).await;
        assert!(
            world.lock().await.uploads.is_empty(),
            "probe upload left open"
        );

        // A store that ignores one kind gets the host's algorithm wherever
        // that kind would carry the guarantee, so nothing is overwritten.
        put_bytes(&backend, "a.m4b", b"book").await;
        put_bytes(&backend, "taken.m4b", b"other").await;
        let no_overwrite = MoveOptions {
            no_overwrite: true,
            ..MoveOptions::default()
        };
        let refused = backend
            .move_object("a.m4b", "taken.m4b", &no_overwrite)
            .await;
        assert!(
            matches!(refused, Err(StorageError::Conflict(_))),
            "ignoring {ignored:?}: {refused:?}"
        );
        assert_eq!(backend.get("taken.m4b").await.unwrap().as_ref(), b"other");
        assert_eq!(backend.get("a.m4b").await.unwrap().as_ref(), b"book");

        let put = put_if(&backend, "new.json", b"one", &WriteCondition::IfNoneMatch)
            .await
            .unwrap();
        assert_eq!(
            put.atomic,
            !matches!(ignored, CondOp::Put | CondOp::Complete),
            "ignoring {ignored:?}"
        );
        let again = put_if(&backend, "new.json", b"two", &WriteCondition::IfNoneMatch).await;
        assert!(
            matches!(again, Err(StorageError::Conflict(_))),
            "ignoring {ignored:?}: {again:?}"
        );
        assert_eq!(backend.get("new.json").await.unwrap().as_ref(), b"one");
    }
}

#[tokio::test]
async fn conditional_puts_are_atomic_only_on_an_enforcing_store() {
    for enforce in [true, false] {
        let world = new_world(false);
        world.lock().await.enforce_conditions = enforce;
        let url = serve(Arc::clone(&world)).await;
        let dir = tempfile::tempdir().unwrap();
        let backend = open_sized(&url, dir.path(), None).await;

        let first = put_if(&backend, "a.json", b"one", &WriteCondition::IfNoneMatch)
            .await
            .unwrap();
        assert_eq!(first.atomic, enforce);
        let etag = backend.probe("a.json").await.unwrap().etag;
        assert_eq!(first.put.etag, etag);
        let again = put_if(&backend, "a.json", b"two", &WriteCondition::IfNoneMatch).await;
        assert!(matches!(again, Err(StorageError::Conflict(_))), "{again:?}");

        let stale = put_if(
            &backend,
            "a.json",
            b"three",
            &WriteCondition::IfMatch("\"stale\"".into()),
        )
        .await;
        assert!(matches!(stale, Err(StorageError::Conflict(_))), "{stale:?}");
        let fresh = put_if(
            &backend,
            "a.json",
            b"four",
            &WriteCondition::IfMatch(etag.unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(fresh.atomic, enforce);
        assert_eq!(backend.get("a.json").await.unwrap().as_ref(), b"four");
    }
}

#[tokio::test]
async fn the_store_refuses_a_conditional_put_that_lost_a_race() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_sized(&url, dir.path(), Some(4)).await;
    assert!(backend.conditional_support().await.writes);
    // The pre-check passes; another writer lands before the upload
    // completes, and the store refuses the completion.
    world.lock().await.hold_parts = true;
    let racing = tokio::spawn({
        let backend = backend.clone();
        async move {
            put_if(
                &backend,
                "race.json",
                b"mine-and-long",
                &WriteCondition::IfNoneMatch,
            )
            .await
        }
    });
    let (started, gate) = {
        let world = world.lock().await;
        (
            Arc::clone(&world.first_part_started),
            Arc::clone(&world.part_gate),
        )
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
        .await
        .expect("the upload reached its first part");
    world.lock().await.objects.insert(
        "library/race.json".into(),
        Obj {
            body: b"theirs".to_vec(),
            etag: etag_for(b"theirs"),
            meta: HashMap::new(),
        },
    );
    gate.notify_one();
    let lost = racing.await.unwrap();
    assert!(matches!(lost, Err(StorageError::Conflict(_))), "{lost:?}");
    assert_eq!(backend.get("race.json").await.unwrap().as_ref(), b"theirs");
}

#[tokio::test]
async fn moves_copy_then_delete_the_source_only_while_it_is_unchanged() {
    for enforce in [true, false] {
        let world = new_world(false);
        world.lock().await.enforce_conditions = enforce;
        let url = serve(Arc::clone(&world)).await;
        let dir = tempfile::tempdir().unwrap();
        let backend = open_sized(&url, dir.path(), None).await;
        put_bytes(&backend, "a.m4b", b"book").await;
        put_bytes(&backend, "taken.m4b", b"other").await;

        let moved = backend
            .move_object("a.m4b", "b/a.m4b", &MoveOptions::default())
            .await
            .unwrap();
        assert!(!moved.atomic && !moved.already_moved);
        assert!(!backend.exists("a.m4b").await.unwrap());
        assert_eq!(backend.get("b/a.m4b").await.unwrap().as_ref(), b"book");

        let again = backend
            .move_object("a.m4b", "b/a.m4b", &MoveOptions::default())
            .await
            .unwrap();
        assert!(again.already_moved);

        let no_overwrite = MoveOptions {
            no_overwrite: true,
            ..MoveOptions::default()
        };
        let refused = backend
            .move_object("b/a.m4b", "taken.m4b", &no_overwrite)
            .await;
        assert!(
            matches!(refused, Err(StorageError::Conflict(_))),
            "{refused:?}"
        );
        assert_eq!(backend.get("taken.m4b").await.unwrap().as_ref(), b"other");
        assert!(backend.exists("b/a.m4b").await.unwrap());

        let stale = backend
            .move_object(
                "b/a.m4b",
                "c.m4b",
                &MoveOptions {
                    if_source_match: Some("\"stale\"".into()),
                    ..MoveOptions::default()
                },
            )
            .await;
        assert!(matches!(stale, Err(StorageError::Conflict(_))), "{stale:?}");
        no_probe_objects_left(&world).await;
    }
}

#[tokio::test]
async fn a_source_rewritten_during_a_move_is_kept() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_sized(&url, dir.path(), None).await;
    put_bytes(&backend, "a.m4b", b"book").await;
    assert!(backend.conditional_support().await.deletes);
    world.lock().await.hold_copy = true;
    let moving = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .move_object("a.m4b", "b.m4b", &MoveOptions::default())
                .await
        }
    });
    loop {
        if world.lock().await.copies_held >= 1 {
            break;
        }
        assert!(
            !moving.is_finished(),
            "move finished before the copy barrier"
        );
        tokio::task::yield_now().await;
    }
    {
        let mut world = world.lock().await;
        let source = world.objects.get_mut("library/a.m4b").expect("source");
        source.body = b"newer".to_vec();
        source.etag = etag_for(b"newer");
        world.release_copy = true;
    }
    let kept = moving.await.unwrap();
    assert!(matches!(kept, Err(StorageError::Conflict(_))), "{kept:?}");
    assert_eq!(backend.get("a.m4b").await.unwrap().as_ref(), b"newer");
    assert_eq!(backend.get("b.m4b").await.unwrap().as_ref(), b"book");
}

#[tokio::test]
async fn hinted_listings_carry_signatures_and_filter_by_time() {
    let world = new_world(false);
    let url = serve(Arc::clone(&world)).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = open_sized(&url, dir.path(), None).await;
    put_bytes(&backend, "x/a.json", b"a").await;
    let request = |modified_since_ms| ListRequest {
        prefix: "x/".into(),
        cursor: None,
        limit: 10,
        with_signatures: true,
        modified_since_ms,
    };
    let objects = |page: &HintedListPage| -> Vec<ObjectInfo> {
        // Leave out the integrity record the digest is bound to.
        page.page
            .objects
            .iter()
            .filter(|object| !object.key.contains(".bookclerk-integrity/"))
            .cloned()
            .collect()
    };
    let page = backend.list_hinted(&request(None)).await.unwrap();
    assert!(page.signatures_included && !page.modified_since_applied);
    let probe = backend.probe("x/a.json").await.unwrap();
    let listed = &objects(&page)[0];
    assert_eq!(listed.key, "x/a.json");
    assert_eq!(listed.signature, probe.etag);
    assert_eq!(listed.etag, probe.etag);
    let listed_at = listed.modified_at_ms.expect("listed modification time");
    let newer = backend
        .list_hinted(&request(Some(listed_at + 1)))
        .await
        .unwrap();
    assert!(newer.modified_since_applied && objects(&newer).is_empty());
    let same = backend
        .list_hinted(&request(Some(listed_at)))
        .await
        .unwrap();
    assert_eq!(objects(&same).len(), 1);
}
